//! Audio output side: the audio thread and the functions the other threads
//! call into it.

pub mod hold;
pub mod pump;

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use irl_core::consts;

use crate::shared::{AudioState, Shared};

pub use pump::AudioPump;

/// Audio thread body: up to `AUDIO_PUMP_BURST` pump iterations per wakeup,
/// until the run is stopped.
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
            // The pump reports when it next has work rather than being polled
            // at a fixed 1ms: once primed that is a deadline off the output
            // clock, and in the steady state it is "when the next chunk is
            // due", which is the only moment this thread matters.
            obs::time::sleep_ms(pump.idle_sleep_ms());
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

/// Output clock, playout mapping, fades and concealment back to the
/// not-yet-primed state. Caller holds `audio_state`.
pub fn reset_audio_timing_state(state: &mut AudioState) {
    state.clock.stand_down();
    state.conceal_fade_pending = false;
    state.out_last = irl_core::LastSample::default();
    state.offset_baseline_ns = 0;
    state.offset_baseline_set = false;
    state.recovery_until_us = 0;
    state.speed_carry.reset();
    state.align_read_pending = false;
    // Priming waits for the whole target, hold included, so nothing is left
    // to build once it re-primes.
    state.hold_unbuilt_ns = 0;
    state.latest_audio_stream_pts_ns = 0;
    state.latest_buffered_end_pts_ns = 0;
    state.latest_obs_end_ts_ns = 0;
    state.decoded_frame_samples = 0;
    state.startup_warmup_remaining_ms = 0;
    state.drain = irl_core::DrainWatch::default();

    // The receiver-thread half (decode-error counters, last-sample memory)
    // lives in `ReceiverFlags` / `AudioIntake`, cleared by their owners at the
    // same call sites.
}

/// The audio reset plus the video-side mirrors in `ConnStats` and the stream
/// PTS trackers. Caller holds `audio_state`.
pub fn reset_stream_timing_state(shared: &Shared, state: &mut AudioState) {
    reset_audio_timing_state(state);

    // The trim is a property of the sender, so it deliberately survives the
    // audio-only reset above (a throttled decoder flush must not cost two
    // minutes of relearning). It does not survive this one: a PTS-repair reset
    // means the timeline broke badly enough that the level no longer maps to
    // the sender's clock, and a reconnect may not even be the same encoder.
    state.speed_trim.reset();
    // The skew readings straddle the break; the hold they sized stays, and a
    // release window takes it back if the sender no longer needs it.
    hold::forget_readings(state);

    // The fallback clock re-anchors, and the frame interval is re-measured
    // for the new stream.
    shared.conn.video_ts_init.store(false, Relaxed);
    shared.conn.video_frame_interval_ns.store(0, Relaxed);

    // The controller on the audio thread re-arms from 1.0 while playback is
    // unprimed, which is exactly the window a reset opens.
    shared.conn.set_current_speed(1.0);
}

/// Extend recovery to at least `duration_us` from now.
pub fn mark_audio_recovery(state: &mut AudioState, now_us: u64, duration_us: u64) {
    let until_us = now_us + duration_us;
    if until_us > state.recovery_until_us {
        state.recovery_until_us = until_us;
    }
}

pub fn audio_recovery_active(state: &AudioState, now_us: u64) -> bool {
    state.recovery_until_us != 0 && now_us < state.recovery_until_us
}
