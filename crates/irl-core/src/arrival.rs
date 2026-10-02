//! Whether video has stopped catching up to live.
//!
//! How late video reaches the plugin against its audio is the sender's skew
//! only once video is *live*. A relay hands a new subscriber video from its
//! last keyframe next to live audio, so at connection start a stream with no
//! skew can read more than a second late.
//!
//! Catching up means arriving faster than real time, so a packet's arrival
//! time minus its PTS falls with every frame and then hovers around a floor;
//! a sender that really is late (#33) holds a steady offset from its first
//! packet. Video counts as live once that floor has not dropped by more than
//! a tolerance across a lookback, or after a bounded wait. A batching relay
//! passes, since the newest frame of each batch lands on the same floor.

use std::collections::VecDeque;

use crate::consts;

/// The floor of `arrival - PTS` over a connection, with enough history to say
/// whether it is still falling.
#[derive(Debug)]
pub struct ArrivalFloor {
    lookback_ns: u64,
    tolerance_ns: i64,
    max_wait_ns: u64,
    first_ns: Option<u64>,
    floor_ns: i64,
    /// `(arrival, floor as of that arrival)`, oldest first. Trimmed to one
    /// entry at or before the lookback horizon.
    history: VecDeque<(u64, i64)>,
}

impl ArrivalFloor {
    /// Live once the floor fell by at most `tolerance_ns` across the last
    /// `lookback_ns`, or `max_wait_ns` after the first packet regardless.
    pub fn new(lookback_ns: u64, tolerance_ns: u64, max_wait_ns: u64) -> Self {
        Self {
            lookback_ns,
            tolerance_ns: tolerance_ns as i64,
            max_wait_ns,
            first_ns: None,
            floor_ns: i64::MAX,
            history: VecDeque::new(),
        }
    }

    /// Forget everything: a new connection or a cleared source.
    pub fn reset(&mut self) {
        self.first_ns = None;
        self.floor_ns = i64::MAX;
        self.history.clear();
    }

    /// A video packet with `pts_ns` arrived at `received_ns`.
    pub fn note(&mut self, received_ns: u64, pts_ns: i64) {
        self.first_ns.get_or_insert(received_ns);
        self.floor_ns = self.floor_ns.min(received_ns as i64 - pts_ns);
        self.history.push_back((received_ns, self.floor_ns));
        // Keep exactly one entry at or before the horizon: it is the floor
        // "a lookback ago" that `live` compares against.
        let horizon_ns = received_ns.saturating_sub(self.lookback_ns);
        while self.history.len() > 1 && self.history[1].0 <= horizon_ns {
            self.history.pop_front();
        }
    }

    /// Whether video arriving now is live rather than still catching up.
    pub fn live(&self, now_ns: u64) -> bool {
        let Some(first_ns) = self.first_ns else {
            return false;
        };
        if now_ns.saturating_sub(first_ns) >= self.max_wait_ns {
            return true;
        }
        let (Some(&(oldest_ns, floor_then_ns)), Some(&(newest_ns, _))) =
            (self.history.front(), self.history.back())
        else {
            return false;
        };
        if newest_ns.saturating_sub(oldest_ns) < self.lookback_ns {
            return false; // not a lookback of history yet
        }
        floor_then_ns - self.floor_ns <= self.tolerance_ns
    }
}

impl Default for ArrivalFloor {
    /// The plugin's floor, from the `VIDEO_LIVE_*` constants.
    fn default() -> Self {
        Self::new(
            consts::VIDEO_LIVE_LOOKBACK_MS * 1_000_000,
            consts::VIDEO_LIVE_TOLERANCE_MS * 1_000_000,
            consts::VIDEO_LIVE_MAX_WAIT_MS * 1_000_000,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: u64 = 33_333_333;
    const LOOKBACK: u64 = 250_000_000;
    const TOLERANCE: u64 = 16_000_000;
    const MAX_WAIT: u64 = 2_000_000_000;

    fn floor() -> ArrivalFloor {
        ArrivalFloor::new(LOOKBACK, TOLERANCE, MAX_WAIT)
    }

    #[test]
    fn nothing_is_live_without_history() {
        let mut f = floor();
        assert!(!f.live(0));
        f.note(1_000_000_000, 5_000_000_000);
        assert!(!f.live(1_000_000_000));
    }

    #[test]
    fn a_steady_sender_is_live_after_a_lookback_however_late_it_is() {
        // Real-time arrival: the offset never moves, whatever it is.
        let mut f = floor();
        let t0 = 10_000_000_000u64;
        for i in 0..=8u64 {
            f.note(t0 + i * FRAME, (i * FRAME) as i64);
            let expect = i * FRAME >= LOOKBACK;
            assert_eq!(f.live(t0 + i * FRAME), expect, "frame {i}");
        }
    }

    #[test]
    fn video_catching_up_is_not_live_until_it_stops() {
        let mut f = floor();
        let t0 = 10_000_000_000u64;
        // 1.3 s of video arrives at four times real time: 39 frames in 325 ms.
        let mut now = t0;
        for i in 0..39u64 {
            now = t0 + i * FRAME / 4;
            f.note(now, (i * FRAME) as i64);
            assert!(!f.live(now), "frame {i} is still catching up");
        }
        // Then live. Not at once: the floor was still falling a lookback ago.
        let live_from = now;
        let mut went_live_at = None;
        for i in 39..60u64 {
            now = live_from + (i - 38) * FRAME;
            f.note(now, (i * FRAME) as i64);
            if f.live(now) && went_live_at.is_none() {
                went_live_at = Some(now - live_from);
            }
        }
        let after = went_live_at.expect("went live");
        assert!(
            (LOOKBACK..LOOKBACK + 2 * FRAME).contains(&after),
            "live {after}ns after the catch-up ended"
        );
    }

    #[test]
    fn a_batching_relay_is_live() {
        // Half-second batches: fifteen frames stamped with one arrival.
        let mut f = floor();
        let t0 = 10_000_000_000u64;
        let mut live_in_batch = Vec::new();
        for batch in 0..4u64 {
            let now = t0 + batch * 500_000_000;
            for k in 0..15u64 {
                f.note(now, ((batch * 15 + k) * FRAME) as i64);
            }
            live_in_batch.push(f.live(now));
        }
        assert_eq!(live_in_batch, [false, true, true, true]);
    }

    #[test]
    fn jitter_within_the_tolerance_does_not_unsettle_it() {
        let mut f = floor();
        let t0 = 10_000_000_000u64;
        for i in 0..60u64 {
            // Arrival wobbles by +-10 ms around real time.
            let wobble = if i % 2 == 0 { 10_000_000 } else { 0 };
            let now = t0 + i * FRAME + wobble;
            f.note(now, (i * FRAME) as i64);
            if i * FRAME >= LOOKBACK + FRAME {
                assert!(f.live(now), "frame {i}");
            }
        }
    }

    #[test]
    fn a_stream_that_never_stops_catching_up_is_let_through() {
        let mut f = floor();
        let t0 = 10_000_000_000u64;
        let mut now = t0;
        let mut i = 0u64;
        while now - t0 < MAX_WAIT {
            assert!(!f.live(now));
            now = t0 + i * FRAME / 2; // twice real time, for ever
            f.note(now, (i * FRAME) as i64);
            i += 1;
        }
        assert!(f.live(now));
    }

    #[test]
    fn reset_starts_over() {
        let mut f = floor();
        for i in 0..20u64 {
            f.note(i * FRAME, (i * FRAME) as i64);
        }
        assert!(f.live(20 * FRAME));
        f.reset();
        assert!(!f.live(20 * FRAME));
    }
}
