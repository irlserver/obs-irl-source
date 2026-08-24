//! Audio output side (port of the audio-thread half of `receiver-audio.c`).
//! W2-B owns this module. The functions here are the ones the other threads
//! call; their signatures are frozen.

pub mod pump;

use std::sync::Arc;

use crate::shared::{AudioState, Shared};

pub use pump::AudioPump;

/// Audio thread body: 16 pump iterations per wakeup, 1 ms sleep, until the
/// run is stopped (`receiver.c: irl_audio_thread`).
pub fn audio_thread(shared: Arc<Shared>) {
    let _ = shared;
    todo!("W2-B")
}

/// Where emitted audio goes. Production is [`obs::SourceHandle`]; tests use a
/// recording sink, since no libobs is running under `cargo test`.
pub trait AudioSink: Send {
    fn output_audio(&self, audio: &obs::AudioFrame<'_>);
}

impl AudioSink for obs::SourceHandle {
    fn output_audio(&self, audio: &obs::AudioFrame<'_>) {
        obs::SourceHandle::output_audio(self, audio)
    }
}

/// `irl_audio_output_claim`: reserve `frames` on the sample-counter clock and
/// return the OBS timestamp for them. Caller holds `audio_state`.
pub fn output_claim(state: &mut AudioState, frames: u32, rate: u32) -> u64 {
    let _ = (state, frames, rate);
    todo!("W2-B")
}

/// `irl_reset_audio_timing_state`: output clock, playout mapping, fades and
/// concealment back to the not-yet-primed state. Caller holds `audio_state`.
pub fn reset_audio_timing_state(state: &mut AudioState) {
    let _ = state;
    todo!("W2-B")
}

/// `irl_reset_stream_timing_state`: the audio reset plus the video-side
/// mirrors in `ConnStats` and the stream PTS trackers. Caller holds
/// `audio_state`.
pub fn reset_stream_timing_state(shared: &Shared, state: &mut AudioState) {
    let _ = (shared, state);
    todo!("W2-B")
}

/// `irl_mark_audio_recovery`: hold recovery for `duration_us` from now.
pub fn mark_audio_recovery(state: &mut AudioState, now_us: u64, duration_us: u64) {
    let _ = (state, now_us, duration_us);
    todo!("W2-B")
}

/// `irl_audio_recovery_active`.
pub fn audio_recovery_active(state: &AudioState, now_us: u64) -> bool {
    let _ = (state, now_us);
    todo!("W2-B")
}
