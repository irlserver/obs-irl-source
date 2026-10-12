//! Receiver thread.
//!
//! The receiver thread owns demux and the audio decoder. Decoded audio goes
//! to [`audio_in::AudioIntake`], a plain struct the receiver holds and calls;
//! video packets go onto the video channel undecoded. State that both the
//! packet path and the audio intake touch lives in [`ReceiverFlags`] and is
//! passed as `&mut`.

pub mod audio_in;
pub mod decode;
pub mod probe;
pub mod read_ahead;
pub mod stream;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use irl_core::consts;

use crate::receiver::audio_in::AudioIntake;
use crate::receiver::read_ahead::{Next, ReadAhead};
use crate::shared::Shared;
use crate::sync::control::SyncControl;

/// Receiver-thread state shared between the packet path (`decode.rs`) and
/// the audio intake. Every field is receiver-thread-only.
#[derive(Debug, Default)]
pub struct ReceiverFlags {
    /// Which streams the current connection carries; set at open, cleared at
    /// close.
    pub has_audio_stream: bool,
    pub has_video_stream: bool,
    /// Consecutive audio decode errors (the audio decoder flushes after a
    /// burst).
    pub audio_decode_errors: i32,
    /// Throttles (FFmpeg µs domain).
    pub audio_last_decoder_flush_time_us: u64,
    pub audio_last_decoder_warning_time_us: u64,
}

impl ReceiverFlags {
    /// Put every field back to the fresh-connection state.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Everything the receiver thread owns for one run: the FFmpeg objects, the
/// per-connection flags, the two frame intakes, and the reusable packet and
/// frame.
///
/// The stream properties the decode path needs (`audio_tb`, `video_tb`,
/// `video_codec_id`) are cached here at open time on purpose: a
/// [`ffmpeg::StreamRef`] borrows the [`ffmpeg::FormatContext`] immutably while
/// `read_frame` needs it mutably, so nothing may hold a `StreamRef` across the
/// read loop.
pub struct Receiver {
    shared: Arc<Shared>,
    fmt: Option<ffmpeg::FormatContext>,
    audio_dec: Option<ffmpeg::CodecContext>,
    /// Created with the connection and released with it: a device kept across
    /// reconnects makes the probe loop skip and attach a stale device, so a
    /// reconnect would behave differently from a fresh connect.
    hw_device: Option<ffmpeg::HwDeviceContext>,
    audio_stream_idx: i32,
    video_stream_idx: i32,
    audio_tb: ffmpeg::Rational,
    video_tb: ffmpeg::Rational,
    /// What the previous session on this thread carried, so a reconnect can
    /// probe fast. Cleared at thread start, never by `close_ffmpeg`, so a
    /// settings-forced restart always probes in full.
    prev_had_video: bool,
    prev_had_audio: bool,
    using_hw_decode: bool,
    flags: ReceiverFlags,
    audio_in: AudioIntake,
    /// Timecode sync: the delay line in front of the decoders and its
    /// controller. Every packet the demuxer delivers goes through it.
    sync: SyncControl,
    last_stats_time: u64,
    frame: ffmpeg::Frame,
}

/// Why the reading of one connection ended.
enum ConnectionEnd {
    /// The run was stopped.
    Stopped,
    /// The read failed; the connection is torn down and reopened.
    Failed(ffmpeg::Error),
    /// The reader thread is gone without saying why.
    ReaderLost,
}

impl Receiver {
    /// Allocate the frame the audio decode reuses. `None` when the allocation
    /// fails.
    fn new(shared: Arc<Shared>) -> Option<Self> {
        let frame = ffmpeg::Frame::new().ok()?;
        let audio_in = AudioIntake::default();
        let sync = SyncControl::new(Arc::clone(&shared));
        Some(Self {
            shared,
            fmt: None,
            audio_dec: None,
            hw_device: None,
            audio_stream_idx: -1,
            video_stream_idx: -1,
            audio_tb: ffmpeg::Rational::new(0, 1),
            video_tb: ffmpeg::Rational::new(0, 1),
            prev_had_video: false,
            prev_had_audio: false,
            using_hw_decode: false,
            flags: ReceiverFlags::default(),
            audio_in,
            sync,
            last_stats_time: 0,
            frame,
        })
    }

    /// Current jitter-buffer fill. Takes the buffer mutex on its own, which
    /// the lock contract allows (`audio_state` is only ever taken *above* it,
    /// never below).
    fn audio_fill_ms(&self) -> i32 {
        self.shared
            .audio_buf()
            .as_ref()
            .map_or(0, irl_core::AudioBuffer::fill_ms)
    }

    /// Connect, read and reconnect until the run stops.
    fn run(&mut self) {
        crate::log::log_input_url("Receiver thread started for", &self.shared.cfg.url);

        // A new thread means a new stream configuration (create, or a
        // restart-forcing settings edit): nothing learned about the previous
        // stream applies, so the first connect always probes in full.
        self.prev_had_video = false;
        self.prev_had_audio = false;

        while self.shared.is_active() {
            if !self.open_stream() {
                if !self.wait_for_reconnect() {
                    break;
                }
                continue;
            }
            self.prepare_new_connection();
            self.read_connection();
        }

        self.close_ffmpeg();
        // The run is over: drop the delay line and tell the registry, and
        // free the queued packets rather than leave them behind on the
        // shared state.
        self.sync.reset();
        self.shared.video.drain();
    }

    /// Read the open connection until it fails or the run stops. The reader
    /// thread borrows the demuxer for exactly this long.
    fn read_connection(&mut self) {
        let Some(mut fmt) = self.fmt.take() else {
            return;
        };
        let active = Arc::clone(&self.shared.flags.thread_active);
        let mut eagain_since_us = 0;
        let end = read_ahead::run(
            consts::RECEIVER_READ_AHEAD_PACKETS,
            || read_packet(&mut fmt, &active, &mut eagain_since_us),
            |packets| self.consume(packets),
        );
        self.fmt = Some(fmt);

        match end {
            ConnectionEnd::Stopped => {}
            ConnectionEnd::Failed(err) => self.handle_stream_read_error(err),
            ConnectionEnd::ReaderLost => {
                if self.shared.is_active() {
                    irl_error!("Reader thread exited unexpectedly; stopping the stream");
                    self.shared.flags.thread_active.store(false, Relaxed);
                    self.shared.video.wake_all();
                }
            }
        }
    }

    /// The receiver's half of the read loop: take each packet off the reader,
    /// and release held packets on time whether or not anything arrives.
    fn consume(&mut self, packets: &ReadAhead<ffmpeg::Packet, ffmpeg::Error>) -> ConnectionEnd {
        while self.shared.is_active() {
            if !self.apply_read_backpressure() || !self.sync_wait_for_room() {
                break;
            }

            match packets.next(self.read_wait()) {
                // Timecode sync sees every packet first: it measures the
                // timecode and either keeps the packet in its delay line or
                // hands it back.
                Next::Packet(pkt) => {
                    if !self.sync.intercept(&pkt) {
                        self.dispatch_packet(&pkt);
                    }
                }
                Next::Failed(err) => return ConnectionEnd::Failed(err),
                Next::Idle => {}
                Next::Closed => return ConnectionEnd::ReaderLost,
            }

            self.sync_drain();
            self.log_receiver_stats();
        }
        ConnectionEnd::Stopped
    }

    /// How long to wait for the next packet: until the delay line's next
    /// release, and no longer than the idle wait.
    fn read_wait(&self) -> Duration {
        let idle_ns = consts::RECEIVER_IDLE_WAIT_MS * 1_000_000;
        let wait_ns = self.sync.next_release_ns().map_or(idle_ns, |release_ns| {
            release_ns
                .saturating_sub(obs::time::gettime_ns())
                .min(idle_ns)
        });
        Duration::from_nanos(wait_ns)
    }

    /// Hand one packet to its decoder path: audio is
    /// decoded here, video goes to the video thread.
    fn dispatch_packet(&mut self, pkt: &ffmpeg::Packet) {
        let index = pkt.stream_index();
        if index == self.audio_stream_idx && self.audio_dec.is_some() {
            self.handle_audio_packet(pkt);
        } else if index == self.video_stream_idx {
            self.push_video_packet(pkt);
        }
    }

    /// Dispatch every held packet that has come due.
    fn sync_drain(&mut self) {
        let now_ns = obs::time::gettime_ns();
        while let Some(pkt) = self.sync.pop_due(now_ns) {
            self.dispatch_packet(&pkt);
        }
    }

    /// While the delay line is at its ceiling, stop taking packets and drain. Draining inside the wait is what makes the room, so
    /// this cannot deadlock while packets are becoming due. Returns false when
    /// the run was stopped while waiting.
    fn sync_wait_for_room(&mut self) -> bool {
        while self.shared.is_active() && self.sync.is_full() {
            self.sync_drain();
            obs::time::sleep_ms(2);
        }
        self.shared.is_active()
    }

    /// Backlog backpressure: above the fill ceiling, stop reading so the
    /// transport holds the excess and playback bleeds it off via speed.
    /// Bounded by buffer capacity so a burst between checks can never force
    /// the ring buffer to drop audible data.
    ///
    /// Returns false when the run was stopped while waiting.
    fn apply_read_backpressure(&mut self) -> bool {
        if !self.flags.has_audio_stream || self.shared.cfg.low_latency_audio {
            return true;
        }

        let buffer_max_ms = self.shared.hot.watermarks().max_ms;
        let mut pace_ms = buffer_max_ms.saturating_mul(3);
        if pace_ms > consts::BLEED_PACE_FILL_MS {
            pace_ms = consts::BLEED_PACE_FILL_MS;
        }
        // The flat cap above is an absolute latency guard, but it must never
        // fall to where playback cannot prime: priming waits for target + the
        // OBS output lead, and a ceiling below that stops the read loop before
        // the buffer ever reaches it, so the source would sit silent forever.
        // buffer_max is target + 200, so this floor clears the prime threshold
        // by ~220ms at every target.
        if pace_ms < buffer_max_ms + 100 {
            pace_ms = buffer_max_ms + 100;
        }

        while self.shared.is_active() && self.audio_fill_ms() > pace_ms {
            obs::time::sleep_ms(5);
        }
        self.shared.is_active()
    }
}

/// The reader thread's read: the next packet, the error that ends the
/// connection, or `None` once the run has stopped.
fn read_packet(
    fmt: &mut ffmpeg::FormatContext,
    active: &AtomicBool,
    eagain_since_us: &mut u64,
) -> Option<Result<ffmpeg::Packet, ffmpeg::Error>> {
    let mut pkt = match ffmpeg::Packet::new() {
        Ok(pkt) => pkt,
        Err(err) => return Some(Err(err)),
    };
    while active.load(Relaxed) {
        // `read_frame` arms the interrupt watch, which aborts the read once
        // the run stops.
        match fmt.read_frame(&mut pkt) {
            Ok(()) => {
                *eagain_since_us = 0;
                return Some(Ok(pkt));
            }
            Err(err)
                if err.is_eagain()
                    && retry_eagain(eagain_since_us, ffmpeg::gettime_us() as u64) =>
            {
                ffmpeg::usleep(1000);
            }
            Err(err) => return Some(Err(err)),
        }
    }
    None
}

/// Handle `av_read_frame` returning EAGAIN. Returns true when the caller
/// should retry rather than treat it as a read error.
///
/// EAGAIN is a non-blocking demuxer saying "nothing yet", not a failure.
/// Treating it as one would tear down a healthy connection (close the
/// input, reset PTS repair, fade out the buffered audio, clear the
/// source) for a normal empty poll.
///
/// Bounded, because a retry re-arms the interrupt watch on every pass and
/// the watch only measures one `av_read_frame` call: unbounded, a demuxer
/// wedged in permanent EAGAIN would spin here forever, past the point where
/// the same wedge on a blocking read would have been aborted and
/// reconnected. Past that timeout it falls through to the error path.
fn retry_eagain(since_us: &mut u64, now_us: u64) -> bool {
    if *since_us == 0 {
        *since_us = now_us;
    }
    now_us - *since_us < consts::IO_STALL_TIMEOUT_US
}

/// Receiver thread body. Opens the stream, runs the `av_read_frame` loop with
/// backpressure, reconnects, and on exit leaves the shared state clean for the
/// next run.
pub fn receiver_thread(shared: Arc<Shared>) {
    let Some(mut receiver) = Receiver::new(shared.clone()) else {
        irl_error!("Failed to allocate a frame, receiver exiting");
        shared.flags.thread_active.store(false, Relaxed);
        return;
    };
    receiver.run();
}
