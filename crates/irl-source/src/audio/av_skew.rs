//! The audio hold for a sender whose video is stamped behind its audio: the
//! plugin half of [`irl_core::av_skew`].
//!
//! The receiver measures the skew as it pushes each video packet
//! ([`observe_video_packet`]), the pump refuses to prime until that has
//! happened once or a bounded wait runs out ([`prime_may_wait`]), and a new
//! connection forgets the hold ([`reset`]). The hold is folded into the
//! published watermarks — the jitter buffer's target *is* the user's Target
//! Buffer plus the hold — so the prime threshold, the speed controller, the
//! hidden trim and the video thread's anchor wait all follow it without
//! knowing it exists. `Config::apply_hot` composes the same way, so a
//! Target Buffer edit mid-stream keeps the hold.

use std::sync::atomic::Ordering::Relaxed;

use irl_core::{Watermarks, av_skew, consts};

use crate::config::publish_watermarks;
use crate::shared::{AudioState, Shared};

/// Whether the hold applies to this connection at all. Low-latency mode
/// keeps no cushion to hold audio in, and the setting is the user's say.
fn enabled(shared: &Shared) -> bool {
    shared.cfg.compensate_av_skew && !shared.cfg.low_latency_audio
}

/// A video packet with `video_pts_ns` reached the receiver. Measures it
/// against the newest audio PTS decoded before it and raises the hold if the
/// uncovered part of that skew has grown. Only before priming; afterwards the
/// standing video delay owns any change.
///
/// Takes `audio_state` and, through the publish, the jitter buffer and the
/// watermark mutex, in that order. The caller must hold none of them.
pub fn observe_video_packet(shared: &Shared, video_pts_ns: i64) {
    if !enabled(shared) {
        return;
    }
    let state = shared.audio_state();
    if state.primed {
        return;
    }
    let audio_pts_ns = state.latest_audio_stream_pts_ns;
    if audio_pts_ns == 0 {
        // No audio admitted yet: nothing to measure against. The pump is not
        // primeable without audio either, so no wait is running.
        return;
    }
    // From here the pump may prime: a skew has been measured, whatever it
    // came to.
    shared.conn.av_skew_seen.store(true, Relaxed);

    let skew_ms = (audio_pts_ns - video_pts_ns) / 1_000_000;
    let current_hold_ms = shared.conn.av_skew_hold_ms.load(Relaxed);
    let published = shared.hot.watermarks();
    let user_target_ms = published.target_ms - current_hold_ms;
    let need_ms = av_skew::hold_ms(
        skew_ms,
        user_target_ms,
        consts::AUDIO_OUT_LEAD_MS,
        consts::AV_SKEW_HOLD_MARGIN_MS,
        consts::AV_SKEW_HOLD_MAX_MS,
    );
    if need_ms <= current_hold_ms {
        return;
    }

    let next = Watermarks::derive(user_target_ms + need_ms);
    // `derive` clamps to BUFFER_TARGET_MAX_MS; the hold is whatever of it
    // the user's target left room for.
    let applied_ms = next.target_ms - user_target_ms;
    if applied_ms <= current_hold_ms {
        return;
    }
    if !publish_watermarks(shared, &state, next) {
        irl_warn!(
            "Could not grow the jitter buffer to hold audio {applied_ms}ms longer; video will run {skew_ms}ms behind its audio"
        );
        return;
    }
    drop(state);
    shared.conn.av_skew_hold_ms.store(applied_ms, Relaxed);
    irl_info!(
        "Video is stamped {skew_ms}ms behind its audio; holding audio {applied_ms}ms longer to keep lip sync (Target Buffer {user_target_ms}ms, {}ms in effect)",
        next.target_ms
    );
    // Uncovered skew, if the ceiling or the Target Buffer maximum cut the
    // hold short: the standing video delay paces it, but it is a lip-sync
    // error the user should hear about.
    let residual_ms = skew_ms + i64::from(consts::AV_SKEW_HOLD_MARGIN_MS)
        - i64::from(user_target_ms)
        - i64::from(consts::AUDIO_OUT_LEAD_MS)
        - i64::from(applied_ms);
    if residual_ms > 0 {
        irl_warn!(
            "The skew is more than the audio hold can cover; video will run about {residual_ms}ms behind its audio"
        );
    }
}

/// Whether a primeable audio buffer should keep waiting for the skew
/// measurement. The connection carries video but no video packet has been
/// measured yet, and the wait since the buffer first became primeable is
/// within [`consts::AV_SKEW_WAIT_MS`]. Logs once when the wait runs out.
///
/// Caller holds `audio_state`.
pub fn prime_may_wait(shared: &Shared, state: &mut AudioState, now_ns: u64) -> bool {
    if !enabled(shared) || !shared.flags.video_present.load(Relaxed) {
        return false;
    }
    if shared.conn.av_skew_seen.load(Relaxed) {
        return false;
    }
    if state.skew_wait_since_ns == 0 {
        state.skew_wait_since_ns = now_ns;
        return true;
    }
    if now_ns.saturating_sub(state.skew_wait_since_ns) < consts::AV_SKEW_WAIT_MS * 1_000_000 {
        return true;
    }
    // Stop asking: a video packet after this still measures and can still
    // raise the hold while unprimed, but nothing waits for it any more.
    shared.conn.av_skew_seen.store(true, Relaxed);
    irl_info!(
        "No video packet within {}ms of audio being ready; priming without a lip-sync measurement",
        consts::AV_SKEW_WAIT_MS
    );
    false
}

/// Forget the hold: a new connection measures its own. Restores the user's
/// Target Buffer as the published target; the ring keeps its size, which is
/// only ever headroom.
///
/// Caller holds `audio_state`.
pub fn reset(shared: &Shared, state: &mut AudioState) {
    state.skew_wait_since_ns = 0;
    shared.conn.av_skew_seen.store(false, Relaxed);
    let hold_ms = shared.conn.av_skew_hold_ms.swap(0, Relaxed);
    if hold_ms > 0 {
        let published = shared.hot.watermarks();
        let user = Watermarks::derive(published.target_ms - hold_ms);
        publish_watermarks(shared, state, user);
    }
}
