//! Video timestamp mapping (port of `video-handler.c:285-411`).
//!
//! The arithmetic of `irl_video_due_time` and `video_record_lead`, extracted
//! from the locking and logging around it: what those C functions do beyond
//! this is snapshot audio-thread state under `audio_state_lock`, publish the
//! lead stats and throttle a warning line, all of which belongs to the plugin
//! crate.

use crate::consts;

/// Map a stream PTS through the audio playout mapping: the OBS timestamp at
/// which audio with `buffered_end_pts_ns` will play is `obs_end_ts_ns`, so
/// `pts` lands at `obs_end + (pts − buffered_end)`. Negative results clamp to
/// zero.
///
/// Ports the mapped branch of `irl_video_due_time` (`video-handler.c:360-369`)
/// and the offset form used by `irl_video_playout_offset`. The caller decides
/// whether a mapping exists at all: the C test is `obs_end_ts_ns != 0 &&
/// buffered_end_pts_ns > 0`, and it holds the last known offset for
/// `VIDEO_OFFSET_HOLD_NS` after that stops being true.
pub fn map_through_playout(pts_ns: i64, obs_end_ts_ns: u64, buffered_end_pts_ns: i64) -> u64 {
    let mapped = pts_ns + (obs_end_ts_ns as i64 - buffered_end_pts_ns);
    if mapped < 0 { 0 } else { mapped as u64 }
}

/// The stream-PTS → OBS-clock offset the mapping implies
/// (`irl_video_playout_offset`), for re-deriving queued frames' due times.
pub fn playout_offset_ns(obs_end_ts_ns: u64, buffered_end_pts_ns: i64) -> i64 {
    obs_end_ts_ns as i64 - buffered_end_pts_ns
}

/// Video-only fallback anchored at the first frame: drift below −500 ms shows
/// now, drift above +500 ms caps at now + 200 ms.
///
/// Ports `video-handler.c:371-388`. The anchor deliberately stays put — a
/// re-anchor would be a visible timeline jump, while clamping lets ordinary
/// frames self-correct after a burst.
pub fn fallback_anchor(pts_ns: i64, pts_base_ns: i64, sys_base_ns: u64, now_ns: u64) -> u64 {
    let computed = sys_base_ns.wrapping_add((pts_ns - pts_base_ns) as u64);
    let drift = computed as i64 - now_ns as i64;

    if drift < -consts::VIDEO_TS_CLAMP_NS {
        now_ns
    } else if drift > consts::VIDEO_TS_CLAMP_NS {
        now_ns + consts::VIDEO_TS_CAP_NS
    } else {
        computed
    }
}

/// Lead libobs can absorb: `OBS_ASYNC_FRAME_BUDGET × frame_interval`, floored
/// at the audio re-anchor margin.
///
/// Ports the budget half of `video_record_lead` (`video-handler.c:293-297`).
/// The caller adds `buffer_target_ms × 1e6` — the jitter buffer's own
/// contribution to the lead — to get the C's `queue_safe_ns`. The budget is
/// expressed in frames because that is what libobs counts: the same 400 ms is
/// 12 frames at 30 fps and 48 at 120 fps.
pub fn queue_safe_ns(frame_interval_ns: i64) -> i64 {
    let interval = if frame_interval_ns <= 0 {
        consts::VIDEO_INTERVAL_DEFAULT_NS
    } else {
        frame_interval_ns
    };
    let budget_ns = consts::OBS_ASYNC_FRAME_BUDGET * interval;
    let floor_ns = consts::AUDIO_OFFSET_REANCHOR_MARGIN_MS * 1_000_000;
    if budget_ns < floor_ns {
        floor_ns
    } else {
        budget_ns
    }
}

/// EMA (1/8 step) of PTS deltas, ignoring deltas outside the 4–100 ms window.
///
/// Ports the interval estimator in `irl_handle_video_frame`
/// (`receiver-video.c:420-437`). Measured rather than taken from
/// `avg_frame_rate`, which live SRT/RTMP demuxers routinely leave unset or
/// wrong; out-of-range deltas (PTS repair, discontinuities, reordering) are
/// skipped rather than smoothed in.
pub fn interval_ema(prev_ns: i64, delta_ns: i64) -> i64 {
    let usable = consts::VIDEO_INTERVAL_MIN_NS..=consts::VIDEO_INTERVAL_MAX_NS;
    if !usable.contains(&delta_ns) {
        return prev_ns;
    }
    if prev_ns == 0 {
        return delta_ns;
    }
    prev_ns + (delta_ns - prev_ns) / 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_shifts_the_pts_onto_the_obs_clock() {
        // Audio whose stream PTS ends at 10 s plays out at 3 s on the OBS
        // clock: everything is shifted back by 7 s.
        let obs_end = 3_000_000_000u64;
        let buffered_end = 10_000_000_000i64;
        assert_eq!(
            map_through_playout(10_000_000_000, obs_end, buffered_end),
            3_000_000_000
        );
        assert_eq!(
            map_through_playout(10_016_000_000, obs_end, buffered_end),
            3_016_000_000
        );
        assert_eq!(playout_offset_ns(obs_end, buffered_end), -7_000_000_000);
    }

    #[test]
    fn negative_mapped_pts_clamps_to_zero() {
        // A frame from before the audio epoch would map before the OBS clock
        // started; OBS takes 0 as "now".
        assert_eq!(map_through_playout(0, 1_000_000_000, 10_000_000_000), 0);
        assert_eq!(map_through_playout(-5, 1, 1), 0);
    }

    #[test]
    fn fallback_passes_a_frame_inside_the_drift_window() {
        let now = 10_000_000_000u64;
        // Anchored at now with a PTS 40 ms past the base.
        assert_eq!(fallback_anchor(40_000_000, 0, now, now), now + 40_000_000);
        // 499 ms early is still inside the window.
        let computed = fallback_anchor(0, 499_000_000, now, now);
        assert_eq!(computed, now - 499_000_000);
    }

    #[test]
    fn fallback_shows_a_far_late_frame_now() {
        let now = 10_000_000_000u64;
        // Drift of -501 ms: display immediately.
        assert_eq!(fallback_anchor(0, 501_000_000, now, now), now);
        assert_eq!(fallback_anchor(0, 5_000_000_000, now, now), now);
    }

    #[test]
    fn fallback_caps_a_far_early_frame() {
        let now = 10_000_000_000u64;
        // Drift of +501 ms: cap at now + 200 ms rather than let OBS hold the
        // previous frame that long.
        assert_eq!(
            fallback_anchor(501_000_000, 0, now, now),
            now + consts::VIDEO_TS_CAP_NS
        );
        assert_eq!(
            fallback_anchor(60_000_000_000, 0, now, now),
            now + consts::VIDEO_TS_CAP_NS
        );
        // Exactly 500 ms is still inside the window.
        assert_eq!(fallback_anchor(500_000_000, 0, now, now), now + 500_000_000);
    }

    #[test]
    fn queue_safe_floors_at_the_reanchor_margin() {
        // 24 frames at 120 fps is 200 ms, below the 400 ms floor.
        assert_eq!(queue_safe_ns(8_333_333), 400_000_000);
        // At 60 fps, 24 frames is a hair under 400 ms, so the floor still wins.
        assert_eq!(queue_safe_ns(16_666_666), 400_000_000);
        // At 30 fps it is 800 ms, and the budget wins.
        assert_eq!(queue_safe_ns(33_333_333), 24 * 33_333_333);
        // No measurement yet: the 30 fps default stands in.
        assert_eq!(queue_safe_ns(0), 24 * consts::VIDEO_INTERVAL_DEFAULT_NS);
        assert_eq!(queue_safe_ns(-1), 24 * consts::VIDEO_INTERVAL_DEFAULT_NS);
    }

    #[test]
    fn interval_ema_takes_the_first_usable_delta_whole() {
        assert_eq!(interval_ema(0, 16_666_666), 16_666_666);
    }

    #[test]
    fn interval_ema_steps_one_eighth() {
        // From 33.3 ms towards 16.6 ms: one eighth of the way.
        let next = interval_ema(33_333_333, 16_666_666);
        assert_eq!(next, 33_333_333 + (16_666_666 - 33_333_333) / 8);

        // Converges on the real interval. Integer division truncates, so the
        // step stops once the residual is under 8 ns — which is where it
        // settles, well inside a nanosecond-irrelevant margin.
        let mut interval = 33_333_333;
        for _ in 0..200 {
            interval = interval_ema(interval, 16_666_666);
        }
        assert!((interval - 16_666_666).abs() < 8, "got {interval}");
    }

    #[test]
    fn interval_ema_skips_out_of_range_deltas() {
        // Faster than 250 fps, or slower than 10 fps: a discontinuity, not a
        // frame rate.
        assert_eq!(interval_ema(16_666_666, 3_999_999), 16_666_666);
        assert_eq!(interval_ema(16_666_666, 100_000_001), 16_666_666);
        assert_eq!(interval_ema(16_666_666, -5_000_000), 16_666_666);
        assert_eq!(interval_ema(0, 2_000_000_000), 0);
        // The bounds themselves are usable.
        assert_eq!(interval_ema(0, consts::VIDEO_INTERVAL_MIN_NS), 4_000_000);
        assert_eq!(interval_ema(0, consts::VIDEO_INTERVAL_MAX_NS), 100_000_000);
    }
}
