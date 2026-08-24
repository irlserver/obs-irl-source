//! Decoded audio intake on the receiver thread (port of
//! `irl_handle_audio_frame`, `receiver-audio.c:906-1127`). W2-B.

use irl_core::PtsRepair;

use crate::receiver::ReceiverFlags;
use crate::shared::{Shared, StreamConfig};

/// Receiver-thread audio state: the input resampler, its scratch buffer, the
/// PTS repair state machine and the last-sample memory used for silence
/// shaping.
pub struct AudioIntake {
    _private: (),
}

impl AudioIntake {
    /// Fresh intake for a run; PTS-repair thresholds come from `cfg`.
    pub fn new(cfg: &StreamConfig) -> Self {
        let _ = cfg;
        todo!("W2-B")
    }

    /// Per-connection reset (`irl_prepare_new_connection` for the audio
    /// fields): drops the resampler, resets PTS repair.
    pub fn reset(&mut self) {
        todo!("W2-B")
    }

    /// (Re)initialise PTS repair for the audio stream's time base
    /// (`pts_repair_init` at decoder open and after a decoder flush).
    pub fn init_pts_repair(&mut self, cfg: &StreamConfig, tb: ffmpeg::Rational) {
        let _ = (cfg, tb);
        todo!("W2-B")
    }

    /// The PTS repair state (the decode path calls `reset` on it after a
    /// decoder flush).
    pub fn pts_repair(&mut self) -> Option<&mut PtsRepair> {
        todo!("W2-B")
    }

    /// One decoded frame: format change handling, PTS repair, warm-up
    /// discard, silence insertion, resample to interleaved f32, pre-keyframe
    /// discard, write to the jitter buffer, publish
    /// `decoded_frame_samples` / `latest_audio_stream_pts_ns`.
    pub fn handle_frame(&mut self, shared: &Shared, flags: &mut ReceiverFlags, frame: &ffmpeg::Frame, tb: ffmpeg::Rational) {
        let _ = (shared, flags, frame, tb);
        todo!("W2-B")
    }
}
