//! Receiver thread (port of `src/receiver.c`; the thread body is W2-A's).
//!
//! The receiver thread owns demux/decode. Decoded audio goes to
//! [`audio_in::AudioIntake`], decoded video to
//! [`crate::video::intake::VideoIntake`]; both are plain structs the receiver
//! holds and calls. State that more than one of decode / audio intake / video
//! intake touches lives in [`ReceiverFlags`] and is passed as `&mut`.

pub mod audio_in;
pub mod decode;
pub mod probe;
pub mod stream;

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use irl_core::consts;

use crate::receiver::audio_in::AudioIntake;
use crate::shared::Shared;

/// Receiver-thread state shared between the packet path (`decode.rs`) and
/// the two frame intakes. Every field is receiver-thread-only; the C kept
/// them on `struct irl_source` and reset them in `irl_prepare_new_connection`
/// / `irl_reset_stream_timing_state`.
#[derive(Debug, Default)]
pub struct ReceiverFlags {
    /// Which streams the current connection carries (`audio_stream_idx >= 0`
    /// / `video_stream_idx >= 0` in C); set at open, cleared at close.
    pub has_audio_stream: bool,
    pub has_video_stream: bool,
    /// Consecutive audio decode errors (the audio decoder flushes after a
    /// burst). Video decode is not on this thread; its state lives in
    /// [`crate::video::DecodeState`].
    pub audio_decode_errors: i32,
    /// Throttles (FFmpeg µs domain).
    pub audio_last_decoder_flush_time_us: u64,
    pub audio_last_decoder_warning_time_us: u64,
}

impl ReceiverFlags {
    /// `irl_prepare_new_connection` + `irl_reset_stream_timing_state` for the
    /// receiver-only fields: everything back to the fresh-connection state.
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
    /// Created with the connection and released with it: keeping a device
    /// across reconnects made the probe loop silently skip and attach a stale
    /// device, so reconnects behaved differently from fresh connects.
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
    /// Start of the current unbroken run of EAGAIN reads (FFmpeg µs), or 0.
    eagain_since_us: u64,
    using_hw_decode: bool,
    flags: ReceiverFlags,
    audio_in: AudioIntake,
    last_stats_time: u64,
    pkt: ffmpeg::Packet,
    frame: ffmpeg::Frame,
}

impl Receiver {
    /// Allocate the packet and frame the read loop reuses. `None` mirrors the
    /// C's "failed to allocate packet/frame" bail-out.
    fn new(shared: Arc<Shared>) -> Option<Self> {
        let pkt = ffmpeg::Packet::new().ok()?;
        let frame = ffmpeg::Frame::new().ok()?;
        let audio_in = AudioIntake::new(&shared.cfg);
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
            eagain_since_us: 0,
            using_hw_decode: false,
            flags: ReceiverFlags::default(),
            audio_in,
            last_stats_time: 0,
            pkt,
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

    /// Handle `av_read_frame` returning EAGAIN. Returns true when the caller
    /// should retry rather than treat it as a read error.
    ///
    /// EAGAIN is a non-blocking demuxer saying "nothing yet", not a failure.
    /// Treating it as one tore down a healthy connection — closing the input,
    /// resetting PTS repair, fading the buffered audio out and clearing the
    /// source — for a normal empty poll.
    ///
    /// Bounded, because a retry re-arms the interrupt watch on every pass and
    /// the watch only measures one `av_read_frame` call: unbounded, a demuxer
    /// wedged in permanent EAGAIN would spin here forever, past the point where
    /// the same wedge on a blocking read would have been aborted and
    /// reconnected. Past that timeout it falls through to the error path.
    fn retry_eagain(&mut self, now_us: u64) -> bool {
        if self.eagain_since_us == 0 {
            self.eagain_since_us = now_us;
        }
        now_us - self.eagain_since_us < consts::IO_STALL_TIMEOUT_US
    }

    /// The `av_read_frame` loop.
    fn run(&mut self) {
        crate::log::log_input_url("Receiver thread started for", &self.shared.cfg.url);

        // A new thread means a new stream configuration (create, or a
        // restart-forcing settings edit): nothing learned about the previous
        // stream applies, so the first connect always probes in full.
        self.prev_had_video = false;
        self.prev_had_audio = false;

        while self.shared.is_active() {
            if self.fmt.is_none() {
                self.eagain_since_us = 0;
                if !self.open_stream() {
                    if !self.wait_for_reconnect() {
                        break;
                    }
                    continue;
                }
                self.prepare_new_connection();
            }

            if !self.apply_read_backpressure() {
                break;
            }

            let read = {
                let Self { fmt, pkt, .. } = self;
                let Some(fmt) = fmt.as_mut() else { continue };
                // `read_frame` arms the interrupt watch, which is the C's
                // `ctx->io_start_us = av_gettime()` before `av_read_frame`.
                fmt.read_frame(pkt)
            };
            match &read {
                Err(err) if err.is_eagain() => {
                    if self.retry_eagain(ffmpeg::gettime_us() as u64) {
                        self.pkt.unref();
                        ffmpeg::usleep(1000);
                        continue;
                    }
                }
                _ => self.eagain_since_us = 0,
            }
            if let Err(err) = read {
                self.handle_stream_read_error(err);
                continue;
            }

            let index = self.pkt.stream_index();
            if index == self.audio_stream_idx && self.audio_dec.is_some() {
                self.handle_audio_packet();
            } else if index == self.video_stream_idx {
                self.push_video_packet();
            }

            self.pkt.unref();
            self.log_receiver_stats();
        }

        self.close_ffmpeg();
        // Queued frames pin decoder surfaces; the run is over, so free them
        // rather than leave them behind on the shared state.
        self.shared.video.drain();
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

/// Receiver thread body. Opens the stream, runs the `av_read_frame` loop with
/// backpressure, reconnects, and on exit leaves the shared state clean for the
/// next run.
pub fn receiver_thread(shared: Arc<Shared>) {
    let Some(mut receiver) = Receiver::new(shared.clone()) else {
        irl_error!("Failed to allocate packet/frame, receiver exiting");
        shared.flags.thread_active.store(false, Relaxed);
        return;
    };
    receiver.run();
}
