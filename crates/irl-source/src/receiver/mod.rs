//! Receiver thread (port of `src/receiver.c`; the thread body is W2-A's).
//!
//! The receiver thread owns demux/decode. Decoded audio goes to
//! [`audio_in::AudioIntake`], decoded video to
//! [`crate::video::intake::VideoIntake`]; both are plain structs the receiver
//! holds and calls. State that more than one of decode / audio intake / video
//! intake touches lives in [`ReceiverFlags`] and is passed as `&mut`.

pub mod audio_in;
pub mod decode;
pub mod stream;

use std::sync::Arc;

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
    /// Frame-level keyframe gate: decoded video is shown, and audio is
    /// admitted, only after the first keyframe.
    pub first_keyframe_received: bool,
    /// Packet-level keyframe gate: the video decoder is not fed until a key
    /// packet arrives.
    pub video_pkt_gate_open: bool,
    /// When the packet gate started waiting (FFmpeg µs domain).
    pub video_pkt_gate_start_us: u64,
    /// Set when `send_packet` failed for video; cleared on the next keyframe.
    pub video_corrupted: bool,
    /// Log throttles.
    pub video_skip_logged: bool,
    pub video_hold_logged: bool,
    /// Consecutive decode errors (audio flushes after a burst; video never).
    pub audio_decode_errors: i32,
    pub video_decode_errors: i32,
    /// Throttles (FFmpeg µs domain).
    pub audio_last_decoder_flush_time_us: u64,
    pub audio_last_decoder_warning_time_us: u64,
    pub video_last_decoder_warning_time_us: u64,
    /// Previous decoded video PTS (ns) for the frame-interval EMA.
    pub video_prev_pts_ns: i64,
    /// Last seen video dimensions, for mid-stream resolution changes.
    pub last_video_width: i32,
    pub last_video_height: i32,
}

impl ReceiverFlags {
    /// `irl_prepare_new_connection` + `irl_reset_stream_timing_state` for the
    /// receiver-only fields: everything back to the fresh-connection state.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Receiver thread body (W2-A). Opens the stream, runs the `av_read_frame`
/// loop with backpressure, reconnects, and on exit leaves the shared state
/// clean for the next run.
pub fn receiver_thread(shared: Arc<Shared>) {
    let _ = shared;
    todo!("W2-A")
}
