//! The demuxer read, on a thread of its own.
//!
//! `av_read_frame` blocks for as long as the network delivers nothing. The
//! receiver has work that falls due on a clock rather than on arrival
//! (timecode sync releases held packets at their release times), and that work
//! cannot wait out a delivery gap behind a blocked read. So one reader thread
//! per connection does nothing but read, and the receiver takes what it reads
//! off a bounded channel with a timeout.
//!
//! The channel is small on purpose. It decouples the two threads; it is not a
//! buffer. When the receiver stops taking packets (read backpressure, a full
//! delay line) the reader blocks on the send, stops reading, and the transport
//! holds the excess as it did with one thread.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

/// What the receiver gets from [`ReadAhead::next`].
#[derive(Debug)]
pub enum Next<T, E> {
    /// The next packet, in demux order.
    Packet(T),
    /// The read failed and the reader has stopped.
    Failed(E),
    /// Nothing arrived within the wait.
    Idle,
    /// The reader is gone without an error: it was told to stop, or it
    /// panicked.
    Closed,
}

/// The receiving end of the reader thread.
pub struct ReadAhead<T, E> {
    rx: Receiver<Result<T, E>>,
}

impl<T, E> ReadAhead<T, E> {
    /// Wait at most `wait` for the next packet.
    pub fn next(&self, wait: Duration) -> Next<T, E> {
        match self.rx.recv_timeout(wait) {
            Ok(Ok(packet)) => Next::Packet(packet),
            Ok(Err(err)) => Next::Failed(err),
            Err(RecvTimeoutError::Timeout) => Next::Idle,
            Err(RecvTimeoutError::Disconnected) => Next::Closed,
        }
    }
}

/// Run `read` on a reader thread and `consume` on this one, until `consume`
/// returns. `read` returns the next packet, a read error (which ends the
/// reader), or `None` when the run has been stopped.
///
/// Before the reader is joined its channel is dropped, so a reader blocked on
/// a full channel exits at once. A reader blocked in the read itself has to be
/// woken by the read returning, which the FFmpeg interrupt callback does when
/// the run stops; `read` must therefore never block past a stop.
pub fn run<T, E, R>(
    capacity: usize,
    read: impl FnMut() -> Option<Result<T, E>> + Send,
    consume: impl FnOnce(&ReadAhead<T, E>) -> R,
) -> R
where
    T: Send,
    E: Send,
{
    let (tx, rx) = mpsc::sync_channel(capacity);
    std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .name("irl-read".to_owned())
            .spawn_scoped(scope, move || read_into(read, &tx));
        if let Err(err) = &spawned {
            irl_error!("Could not start the reader thread: {err}");
        }
        let packets = ReadAhead { rx };
        let result = consume(&packets);
        drop(packets);
        if let Ok(reader) = spawned {
            // `read_into` catches its own panics, so the join cannot fail.
            let _ = reader.join();
        }
        result
    })
}

/// The reader thread's body. A panic in `read` ends the reader like a stop:
/// the sender drops, and the receiver sees [`Next::Closed`].
fn read_into<T, E>(mut read: impl FnMut() -> Option<Result<T, E>>, tx: &SyncSender<Result<T, E>>) {
    let body = std::panic::AssertUnwindSafe(|| {
        while let Some(result) = read() {
            let failed = result.is_err();
            if tx.send(result).is_err() || failed {
                return;
            }
        }
    });
    if let Err(payload) = std::panic::catch_unwind(body) {
        let msg = obs::panic::payload_message(payload.as_ref());
        irl_error!("Reader thread panicked: {msg}");
    }
}
