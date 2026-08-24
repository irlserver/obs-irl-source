//! Decoded video intake on the receiver thread (port of
//! `irl_handle_video_frame`, `receiver-video.c:289-438`). W2-C.

use crate::receiver::ReceiverFlags;
use crate::shared::Shared;

/// Receiver-thread video intake state (beyond [`ReceiverFlags`]).
pub struct VideoIntake {
    _private: (),
}

impl VideoIntake {
    pub fn new() -> Self {
        todo!("W2-C")
    }

    /// Per-connection reset.
    pub fn reset(&mut self) {
        todo!("W2-C")
    }

    /// One decoded frame: PTS validation, keyframe gate, HEVC corrupt hold,
    /// corrupt-frame accounting, resolution-change detection, rescale to ns,
    /// frame-interval EMA publish, push onto `shared.video`.
    pub fn handle_frame(&mut self, shared: &Shared, flags: &mut ReceiverFlags, frame: &ffmpeg::Frame, tb: ffmpeg::Rational, codec_id: ffmpeg::AVCodecID) {
        let _ = (shared, flags, frame, tb, codec_id);
        todo!("W2-C")
    }
}

impl Default for VideoIntake {
    fn default() -> Self {
        Self::new()
    }
}
