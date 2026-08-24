//! Video timestamp mapping (port of `video-handler.c:285-411`).

/// Map a stream PTS through the audio playout mapping: the OBS timestamp at
/// which audio with `buffered_end_pts_ns` will play is `obs_end_ts_ns`, so
/// `pts` lands at `obs_end + (pts − buffered_end)`. `None` when the mapping is
/// unavailable; negative results clamp to zero.
pub fn map_through_playout(pts_ns: i64, obs_end_ts_ns: u64, buffered_end_pts_ns: i64) -> u64 {
    let _ = (pts_ns, obs_end_ts_ns, buffered_end_pts_ns);
    todo!("W1-C")
}

/// Video-only fallback anchored at the first frame: drift below −500 ms shows
/// now, drift above +500 ms caps at now + 200 ms.
pub fn fallback_anchor(pts_ns: i64, pts_base_ns: i64, sys_base_ns: u64, now_ns: u64) -> u64 {
    let _ = (pts_ns, pts_base_ns, sys_base_ns, now_ns);
    todo!("W1-C")
}

/// Lead libobs can absorb: `OBS_ASYNC_FRAME_BUDGET × frame_interval`, floored
/// at the audio re-anchor margin.
pub fn queue_safe_ns(frame_interval_ns: i64) -> i64 {
    let _ = frame_interval_ns;
    todo!("W1-C")
}

/// EMA (1/8 step) of PTS deltas, ignoring deltas outside the 4–100 ms window.
pub fn interval_ema(prev_ns: i64, delta_ns: i64) -> i64 {
    let _ = (prev_ns, delta_ns);
    todo!("W1-C")
}
