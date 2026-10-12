//! The reader thread the receiver reads through (`src/receiver/read_ahead.rs`),
//! driven with stand-in reads: what it promises is that a read blocked on the
//! network never blocks the receiver, and that every way a connection ends
//! lets both threads go.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::time::{Duration, Instant};

use obs_irl_source::receiver::read_ahead::{self, Next};

const WAIT: Duration = Duration::from_millis(5);

#[test]
fn packets_arrive_in_order_and_a_failed_read_ends_the_reader() {
    let mut n = 0;
    let read = move || {
        n += 1;
        Some(if n <= 5 { Ok(n) } else { Err("read failed") })
    };

    let (got, end) = read_ahead::run(2, read, |packets| {
        let mut got = Vec::new();
        loop {
            match packets.next(Duration::from_secs(5)) {
                Next::Packet(n) => got.push(n),
                Next::Idle => {}
                end => return (got, end),
            }
        }
    });

    assert_eq!(got, vec![1, 2, 3, 4, 5]);
    assert!(matches!(end, Next::Failed("read failed")));
}

/// The case the reader thread exists for: nothing arrives for a while, and
/// the receiver keeps waking on its own schedule instead of sitting inside
/// the read.
#[test]
fn a_blocked_read_does_not_block_the_receiver() {
    let active = Arc::new(AtomicBool::new(true));
    let reader_active = Arc::clone(&active);
    // Blocks until the run stops, as a read with no data does until the
    // interrupt callback aborts it.
    let read = move || -> Option<Result<u32, ()>> {
        while reader_active.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
        }
        None
    };

    let wakeups = AtomicU32::new(0);
    let end = read_ahead::run(4, read, |packets| {
        let started = Instant::now();
        loop {
            match packets.next(WAIT) {
                Next::Idle => {
                    wakeups.fetch_add(1, Relaxed);
                    if started.elapsed() > Duration::from_millis(100) {
                        active.store(false, Relaxed);
                    }
                }
                end => return end,
            }
        }
    });

    assert!(
        wakeups.load(Relaxed) >= 10,
        "woke {} times while the read was blocked",
        wakeups.load(Relaxed)
    );
    assert!(matches!(end, Next::Closed), "a stopped reader closes");
}

/// A receiver that stops taking packets (backpressure, then a stop) leaves the
/// reader blocked on a full channel. Returning from the receiver side has to
/// release it, or the join would hang the receiver thread.
#[test]
fn a_reader_blocked_on_a_full_channel_is_released_when_the_receiver_returns() {
    let reads = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&reads);
    let read = move || -> Option<Result<u32, ()>> { Some(Ok(counted.fetch_add(1, Relaxed))) };

    read_ahead::run(1, read, |packets| {
        // Let the reader fill the channel and block on the next send.
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(packets.next(WAIT), Next::Packet(0)));
    });

    // Back here means the reader was joined; it read at most the packet
    // taken, the one in the channel and the one it was blocked sending.
    assert!(reads.load(Relaxed) <= 4);
}

#[test]
fn a_reader_that_panics_closes_the_channel() {
    let read = || -> Option<Result<u32, ()>> { panic!("demuxer bug") };

    let end = read_ahead::run(1, read, |packets| packets.next(Duration::from_secs(5)));

    assert!(matches!(end, Next::Closed));
}
