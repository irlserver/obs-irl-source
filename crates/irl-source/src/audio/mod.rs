//! Audio output side (port of the audio-thread half of `receiver-audio.c`).
//! W2-B owns this module. The functions here are the ones the other threads
//! call; their signatures are frozen.

pub mod pump;

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use irl_core::consts;
use irl_core::timing;

use crate::shared::{AudioState, Shared};

pub use pump::AudioPump;

/// Audio thread body: 16 pump iterations per wakeup, 1 ms sleep, until the
/// run is stopped (`receiver.c: irl_audio_thread`).
pub fn audio_thread(shared: Arc<Shared>) {
    let mut pump = AudioPump::new(shared.clone());

    while shared.flags.thread_active.load(Relaxed) {
        if shared.flags.reconnecting.load(Relaxed) {
            obs::time::sleep_ms(1);
            continue;
        }

        let mut pumped = false;
        for _ in 0..consts::AUDIO_PUMP_BURST {
            if !shared.flags.thread_active.load(Relaxed) {
                break;
            }
            // The whole pump runs under the audio state lock (taken inside
            // `pump_once`), so nothing it calls may take that lock again: the
            // mutex is not recursive and a nested acquire hangs this thread,
            // and with it the video thread waiting behind it.
            if !pump.pump_once() {
                break;
            }
            pumped = true;
        }

        if !pumped {
            obs::time::sleep_ms(consts::AUDIO_PUMP_SLEEP_MS);
        }
    }
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
    let ts = timing::output_next_ts(state.anchor_ns, state.samples, rate);
    state.samples += frames as u64;
    ts
}

/// `irl_reset_audio_timing_state`: output clock, playout mapping, fades and
/// concealment back to the not-yet-primed state. Caller holds `audio_state`.
pub fn reset_audio_timing_state(state: &mut AudioState) {
    state.primed = false;
    state.anchor_ns = 0;
    state.samples = 0;
    state.conceal_fade_pending = false;
    state.out_last = irl_core::LastSample::default();
    state.offset_baseline_ns = 0;
    state.offset_baseline_set = false;
    state.recovery_until_us = 0;
    state.latest_audio_stream_pts_ns = 0;
    state.latest_buffered_end_pts_ns = 0;
    state.latest_obs_end_ts_ns = 0;
    state.decoded_frame_samples = 0;
    state.startup_warmup_remaining_ms = 0;
    state.drain = irl_core::DrainWatch::default();
    // The C also zeroed `audio_last_obs_lead_ns`, the two last-chunk
    // durations, `audio_last_frames_out` and `audio_last_samples_per_sec`
    // (stats now in `ConnStats`) plus the receiver-thread decode-error
    // counters and the last-sample memory. Those have no `Shared` here;
    // `reset_stream_timing_state` clears the stats half, and the receiver's
    // own `ReceiverFlags` / `AudioIntake` carry the rest.
}

/// `irl_reset_stream_timing_state`: the audio reset plus the video-side
/// mirrors in `ConnStats` and the stream PTS trackers. Caller holds
/// `audio_state`.
pub fn reset_stream_timing_state(shared: &Shared, state: &mut AudioState) {
    reset_audio_timing_state(state);

    // The output-side stats the audio reset owned in C.
    shared.conn.last_obs_lead_ns.store(0, Relaxed);
    shared.conn.last_chunk_stream_ns.store(0, Relaxed);
    shared.conn.last_chunk_obs_ns.store(0, Relaxed);
    shared.conn.last_frames_out.store(0, Relaxed);
    shared.conn.last_samples_per_sec.store(0, Relaxed);

    state.latest_video_stream_pts_ns = 0;

    // State, not counters: the interval has to be re-measured for the new
    // stream, and a stale lead would be reported until the first frame
    // arrives. video_lead_excess is cumulative for the source, like the other
    // quality counters.
    shared.conn.video_ts_init.store(false, Relaxed);
    shared.conn.video_sys_base.store(0, Relaxed);
    shared.conn.video_pts_base.store(0, Relaxed);
    shared.conn.video_frame_interval_ns.store(0, Relaxed);
    shared.conn.video_lead_ns.store(0, Relaxed);

    // Every C call site set `current_speed = 1.0f` immediately after this
    // call (`irl-source.c:234-235`, `receiver-stream.c:671-678`); the
    // controller itself lives on the audio thread and re-arms from 1.0 while
    // playback is unprimed, which is exactly the window a reset opens.
    shared.conn.set_current_speed(1.0);
}

/// `irl_mark_audio_recovery`: hold recovery for `duration_us` from now.
pub fn mark_audio_recovery(state: &mut AudioState, now_us: u64, duration_us: u64) {
    let until_us = now_us + duration_us;
    if until_us > state.recovery_until_us {
        state.recovery_until_us = until_us;
    }
}

/// `irl_audio_recovery_active`.
pub fn audio_recovery_active(state: &AudioState, now_us: u64) -> bool {
    state.recovery_until_us != 0 && now_us < state.recovery_until_us
}
