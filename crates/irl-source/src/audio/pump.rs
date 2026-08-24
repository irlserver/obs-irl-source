//! `irl_pump_audio_once` and helpers (port of `src/receiver-audio.c`). W2-B.

use std::sync::Arc;

use crate::audio::AudioSink;
use crate::shared::Shared;

/// Audio-thread-owned state: the speed resampler, scratch buffers and the
/// speed controller.
pub struct AudioPump {
    _private: (),
}

impl AudioPump {
    /// Production pump emitting to the source.
    pub fn new(shared: Arc<Shared>) -> Self {
        Self::with_sink(shared.clone(), Box::new(shared.source))
    }

    /// Pump with an explicit sink (tests).
    pub fn with_sink(shared: Arc<Shared>, sink: Box<dyn AudioSink>) -> Self {
        let _ = (shared, sink);
        todo!("W2-B")
    }

    /// One pump iteration. Takes `audio_state` exactly once for the whole
    /// call; returns whether audio (or concealment) was emitted.
    pub fn pump_once(&mut self) -> bool {
        todo!("W2-B")
    }
}
