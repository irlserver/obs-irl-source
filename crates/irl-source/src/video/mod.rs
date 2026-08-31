//! Video path (port of `receiver-video.c` + `video-handler.c`). W2-C owns
//! this module; the signatures here are frozen.

pub mod decode;
pub mod intake;
pub mod output;
pub mod thread;

use std::sync::Arc;

use crate::shared::Shared;

pub use intake::DecodeState;
pub use thread::VideoThread;

/// Where output frames go. Production is [`obs::SourceHandle`]; tests use a
/// recording sink.
pub trait VideoSink: Send {
    fn output_video(&self, frame: &obs::VideoFrame<'_>);
    fn output_video_none(&self);
}

impl VideoSink for obs::SourceHandle {
    fn output_video(&self, frame: &obs::VideoFrame<'_>) {
        obs::SourceHandle::output_video(self, frame)
    }
    fn output_video_none(&self) {
        obs::SourceHandle::output_video_none(self)
    }
}

/// Video thread body: decode queued packets as they come due, transfer to
/// system memory, pace, convert and output; consume clear requests; drain on
/// exit.
pub fn video_thread(shared: Arc<Shared>) {
    VideoThread::new(shared).run();
}
