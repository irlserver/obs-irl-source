//! The audio hold (`irl_core::audio_hold`) wired into the plugin.
//!
//! The receiver reads the skew as it pushes each video packet and moves the
//! hold ([`observe_video_packet`]). The pump waits a bounded time for the
//! first reading before it primes ([`prime_may_wait`]) and credits what it
//! builds afterwards ([`credit_build`]), which the video thread takes out of
//! its standing delay. A new connection forgets the hold
//! ([`reset_connection`]).
//!
//! Outside low-latency mode the hold is folded into the published watermarks,
//! whose target *is* Target Buffer plus the hold, so the prime threshold, the
//! speed controller, the hidden trim and the video thread's anchor wait all
//! follow it without knowing it exists, and `Config::apply_hot` composes a
//! Target Buffer edit with it. Low-latency mode has neither a target to fold
//! it into nor a speed controller to build it with: the pump primes once it
//! holds the hold's worth of audio and keeps that much queued, and the hold is
//! sized before priming only.

use std::sync::atomic::Ordering::Relaxed;

use irl_core::{HoldChange, Watermarks, consts, timing};

use crate::config::publish_watermarks;
use crate::shared::{AudioState, Shared};

/// A video packet with decode timestamp `dts_ns` reached the receiver at
/// `received_ns`. Reads it against the newest audio PTS decoded before it and
/// moves the hold if the readings call for it.
///
/// Takes `audio_state` and, through a publish, the jitter buffer and the
/// watermark mutex, in that order. The caller must hold none of them.
pub fn observe_video_packet(shared: &Shared, received_ns: u64, dts_ns: i64) {
    let mut state = shared.audio_state();
    state.hold_live.note(received_ns, dts_ns);
    let audio_pts_ns = state.latest_audio_stream_pts_ns;
    if audio_pts_ns == 0 || !state.hold_live.live(received_ns) {
        return;
    }
    state.hold.observe(received_ns, audio_pts_ns - dts_ns);

    let covered_ms = covered_ms(shared, &state);
    let change = if !state.clock.is_primed() {
        state.hold.before_prime(state.hold_ms, covered_ms)
    } else if regulates(shared) {
        state.hold.regulate(received_ns, state.hold_ms, covered_ms)
    } else {
        None
    };
    if let Some(change) = change {
        apply(shared, &mut state, change);
    }
}

/// Whether audio that is ready to prime should keep waiting for a skew
/// reading: the connection carries video, nothing has been read yet, and the
/// wait is within [`consts::AUDIO_HOLD_PRIME_WAIT_MS`]. Priming without one
/// is not wrong, only slower to settle: a hold found afterwards is built at
/// -2 % while the video delay covers the gap, instead of being in force from
/// the first sample.
///
/// Caller holds `audio_state`.
pub fn prime_may_wait(shared: &Shared, state: &mut AudioState, now_ns: u64) -> bool {
    if state.hold_wait_done || !shared.flags.video_present.load(Relaxed) {
        return false;
    }
    if state.hold.measured() {
        state.hold_wait_done = true;
        return false;
    }
    if state.hold_wait_since_ns == 0 {
        state.hold_wait_since_ns = now_ns;
        return true;
    }
    if now_ns.saturating_sub(state.hold_wait_since_ns)
        < consts::AUDIO_HOLD_PRIME_WAIT_MS * 1_000_000
    {
        return true;
    }
    state.hold_wait_done = true;
    irl_info!(
        "No live video within {}ms of audio being ready; starting audio without a lip-sync reading",
        consts::AUDIO_HOLD_PRIME_WAIT_MS
    );
    false
}

/// The pump played `played_ns` of OBS time for `consumed_ns` of stream: the
/// playout offset grew by the difference. While a raise made after priming is
/// still being built, that growth is the hold building, and is credited to
/// [`AudioState::hold_built_ns`] for the video thread.
///
/// Caller holds `audio_state`.
pub fn credit_build(state: &mut AudioState, played_ns: u64, consumed_ns: u64) {
    if state.hold_unbuilt_ns == 0 || played_ns <= consumed_ns {
        return;
    }
    let built_ns = (played_ns - consumed_ns).min(state.hold_unbuilt_ns);
    state.hold_unbuilt_ns -= built_ns;
    state.hold_built_ns += built_ns;
}

/// How far the hold has moved since the playout offset baseline was taken,
/// which the offset is expected to follow rather than read as drift.
pub fn moved_since_baseline_ns(state: &AudioState) -> i64 {
    i64::from(state.hold_ms - state.offset_baseline_hold_ms) * 1_000_000
}

/// Forget the readings, keeping the hold in force: the audio timeline broke,
/// so readings from before it no longer compare with readings after it, but
/// the sender is the same one.
///
/// Caller holds `audio_state`.
pub fn forget_readings(state: &mut AudioState) {
    state.hold.reset();
    state.hold_live.reset();
    state.hold_unbuilt_ns = 0;
}

/// Back to no hold: a new connection measures its own. Restores Target
/// Buffer as the published target; the ring keeps its size, which is only
/// ever headroom.
///
/// Caller holds `audio_state`.
pub fn reset_connection(shared: &Shared, state: &mut AudioState) {
    forget_readings(state);
    state.hold_wait_since_ns = 0;
    state.hold_wait_done = false;
    if state.hold_ms == 0 {
        return;
    }
    if !shared.cfg.low_latency_audio {
        let user = Watermarks::derive(user_target_ms(shared, state));
        // A smaller target never needs the ring to grow, so this cannot fail.
        publish_watermarks(shared, state, user);
    }
    state.hold_ms = 0;
    shared.conn.audio_hold_ms.store(0, Relaxed);
}

/// Whether the hold may move once audio plays: the buffer is regulated by
/// playback speed, which is what builds and releases it without a seam.
fn regulates(shared: &Shared) -> bool {
    !shared.cfg.low_latency_audio && shared.hot.adaptive_speed.load(Relaxed)
}

/// Target Buffer as the user set it: the published target less the hold
/// folded into it.
fn user_target_ms(shared: &Shared, state: &AudioState) -> i32 {
    let published_ms = shared.hot.watermarks().target_ms;
    if shared.cfg.low_latency_audio {
        published_ms
    } else {
        published_ms - state.hold_ms
    }
}

/// How long audio already waits between arriving and playing without any
/// hold: Target Buffer plus the output lead, or the lead alone in
/// low-latency mode, which keeps no cushion.
fn covered_ms(shared: &Shared, state: &AudioState) -> i32 {
    let low_latency = shared.cfg.low_latency_audio;
    let rate = shared.audio_buf().as_ref().map_or(0, |b| b.sample_rate());
    let lead_ms =
        (timing::output_lead_ns(state.decoded_frame_samples, rate, low_latency) / 1_000_000) as i32;
    if low_latency {
        lead_ms
    } else {
        user_target_ms(shared, state) + lead_ms
    }
}

fn apply(shared: &Shared, state: &mut AudioState, change: HoldChange) {
    let from_ms = state.hold_ms;
    let mut to_ms = change.to_ms;
    let user_ms = user_target_ms(shared, state);
    if !shared.cfg.low_latency_audio {
        let next = Watermarks::derive(user_ms + to_ms);
        // `derive` clamps to BUFFER_TARGET_MAX_MS; the hold is whatever of it
        // Target Buffer left room for.
        to_ms = next.target_ms - user_ms;
        if to_ms == from_ms {
            return;
        }
        if !publish_watermarks(shared, state, next) {
            irl_warn!(
                "Could not grow the jitter buffer to hold audio back {to_ms}ms; video arriving {}ms behind its audio stays covered by delaying video",
                change.skew_ms
            );
            return;
        }
    }

    if state.clock.is_primed() {
        let moved_ns = u64::from(to_ms.abs_diff(from_ms)) * 1_000_000;
        state.hold_unbuilt_ns = if to_ms > from_ms {
            state.hold_unbuilt_ns + moved_ns
        } else {
            state.hold_unbuilt_ns.saturating_sub(moved_ns)
        };
    }
    state.hold_ms = to_ms;
    shared.conn.audio_hold_ms.store(to_ms, Relaxed);

    let in_effect = if shared.cfg.low_latency_audio {
        String::new()
    } else {
        format!(
            " (Target Buffer {user_ms}ms, {}ms in effect)",
            user_ms + to_ms
        )
    };
    let skew_ms = change.skew_ms;
    if to_ms < from_ms {
        irl_info!(
            "Video has arrived at most {skew_ms}ms behind its audio for {}s; releasing the audio hold from {from_ms}ms to {to_ms}ms{in_effect}",
            consts::AUDIO_HOLD_RELAX_WINDOW_MS / 1000
        );
    } else if state.clock.is_primed() {
        irl_info!(
            "Video has arrived {skew_ms}ms behind its audio for {}s; holding audio back {to_ms}ms instead of {from_ms}ms to keep lip sync, built up by playing up to 2% slow{in_effect}",
            consts::AUDIO_HOLD_RAISE_WINDOW_MS / 1000
        );
    } else {
        irl_info!(
            "Video arrives {skew_ms}ms behind its audio; holding audio back {to_ms}ms to keep lip sync{in_effect}"
        );
    }
    if change.capped {
        irl_warn!(
            "Video arrives {skew_ms}ms behind its audio, more than the {}ms audio hold covers; the rest is covered by delaying video, and the sound runs ahead of the picture by that much",
            consts::AUDIO_HOLD_MAX_MS
        );
    }
}
