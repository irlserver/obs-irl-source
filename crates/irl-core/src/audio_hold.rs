//! The audio hold: how much longer than Target Buffer audio waits, so that
//! video which leaves the sender behind its audio is in hand when the two are
//! due.
//!
//! Many senders hand the transport their audio before the video of the same
//! instant (pocketSRT queues audio ~300 ms early, a stabiliser sends video a
//! second or more late, #33). Once the skew exceeds Target Buffer plus the
//! output lead, the picture for a sound is not here when the sound plays, and
//! no video schedule can fix that: the audio has to wait. So the jitter
//! buffer's target is raised by the uncovered part of the skew plus a margin.
//! Before priming that only starts audio later; after it the speed controller
//! builds the cushion at -2 % while the video delay ([`crate::video_delay`])
//! bridges the gap.
//!
//! The skew is measured in mux order (each video packet's timestamp against
//! the newest audio PTS decoded before it), so loss and stalls, which delay
//! both streams together, leave it alone. The caller measures only once video
//! is live (`crate::arrival`), which keeps a relay's keyframe replay out.
//!
//! A raise takes the *lowest* reading across `AUDIO_HOLD_RAISE_WINDOW_MS`, so
//! a short burst of late video stays the video delay's to cover. A release
//! takes the *worst* reading across the longer `AUDIO_HOLD_RELAX_WINDOW_MS`,
//! so the hold does not pump up and down.

use crate::consts;
use crate::window::{Extreme, WindowExtreme};

/// How much longer audio must be held so that video `skew_ms` behind it is in
/// hand `margin_ms` before it is due: the part of the skew that `covered_ms`
/// does not, capped at `max_ms` and never negative.
///
/// `skew_ms` is audio PTS minus video PTS at the same point in the mux, so a
/// positive value is video trailing audio. `covered_ms` is how long audio
/// already waits between arriving and playing: Target Buffer plus the output
/// lead, or the output lead alone in low-latency mode.
pub fn hold_ms(skew_ms: i64, covered_ms: i32, margin_ms: i32, max_ms: i32) -> i32 {
    let need_ms = skew_ms + i64::from(margin_ms) - i64::from(covered_ms);
    need_ms.clamp(0, i64::from(max_ms)) as i32
}

const RAISE_WINDOW_NS: u64 = consts::AUDIO_HOLD_RAISE_WINDOW_MS * 1_000_000;
const RELAX_WINDOW_NS: u64 = consts::AUDIO_HOLD_RELAX_WINDOW_MS * 1_000_000;

/// A change of the hold, for the caller to apply and log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldChange {
    /// The hold before, in ms.
    pub from_ms: i32,
    /// The hold now, in ms.
    pub to_ms: i32,
    /// The skew it was sized from, in ms.
    pub skew_ms: i64,
    /// The skew called for more than the ceiling allows.
    pub capped: bool,
}

/// The skew readings of one connection and the decisions they support.
#[derive(Debug)]
pub struct AudioHold {
    /// When the first reading was taken; the windows only count once they
    /// are full.
    first_ns: Option<u64>,
    /// The lowest reading across the raise window.
    lowest: WindowExtreme,
    /// The highest reading across the relax window.
    highest: WindowExtreme,
}

impl AudioHold {
    /// Forget every reading: a new connection, or a timeline that broke so
    /// that the old readings no longer compare with the new ones.
    pub fn reset(&mut self) {
        self.first_ns = None;
        self.lowest.clear();
        self.highest.clear();
    }

    /// Whether anything has been measured since the last reset.
    pub fn measured(&self) -> bool {
        self.first_ns.is_some()
    }

    /// A video packet read at `now_ns`, `skew_ns` behind the newest audio
    /// decoded before it.
    pub fn observe(&mut self, now_ns: u64, skew_ns: i64) {
        self.first_ns.get_or_insert(now_ns);
        self.lowest.push(now_ns, skew_ns);
        self.highest.push(now_ns, skew_ns);
    }

    /// Before playback primes: the hold the worst reading so far needs, if
    /// that is more than `current_ms`. Nothing has been heard yet, so raising
    /// costs nothing but a later start, and the readings so far are all
    /// there is; it never lowers, since the next connection or a release
    /// window will.
    pub fn before_prime(&self, current_ms: i32, covered_ms: i32) -> Option<HoldChange> {
        let skew_ns = self.highest.value()?;
        let change = self.sized(skew_ns, current_ms, covered_ms);
        change.filter(|c| c.to_ms > current_ms)
    }

    /// Once playback runs and the buffer is regulated by playback speed: a
    /// raise when the skew sustained across the raise window needs at least
    /// `AUDIO_HOLD_RAISE_MIN_MS` more than `current_ms`, or a release when
    /// the worst reading across the relax window needs less than it by more
    /// than `AUDIO_HOLD_RELAX_MIN_MS`.
    pub fn regulate(&self, now_ns: u64, current_ms: i32, covered_ms: i32) -> Option<HoldChange> {
        let since_ns = now_ns.saturating_sub(self.first_ns?);
        if since_ns >= RAISE_WINDOW_NS
            && let Some(skew_ns) = self.lowest.value()
            && let Some(change) = self.sized(skew_ns, current_ms, covered_ms)
            && change.to_ms >= current_ms + consts::AUDIO_HOLD_RAISE_MIN_MS
        {
            return Some(change);
        }
        if since_ns >= RELAX_WINDOW_NS
            && let Some(skew_ns) = self.highest.value()
            && let Some(change) = self.sized(skew_ns, current_ms, covered_ms)
            && change.to_ms + consts::AUDIO_HOLD_RELAX_MIN_MS < current_ms
        {
            return Some(change);
        }
        None
    }

    fn sized(&self, skew_ns: i64, current_ms: i32, covered_ms: i32) -> Option<HoldChange> {
        let skew_ms = skew_ns / 1_000_000;
        let (margin_ms, max_ms) = (consts::AUDIO_HOLD_MARGIN_MS, consts::AUDIO_HOLD_MAX_MS);
        let to_ms = hold_ms(skew_ms, covered_ms, margin_ms, max_ms);
        let wanted_ms = skew_ms + i64::from(margin_ms) - i64::from(covered_ms);
        (to_ms != current_ms).then_some(HoldChange {
            from_ms: current_ms,
            to_ms,
            skew_ms,
            capped: wanted_ms > i64::from(max_ms),
        })
    }
}

impl Default for AudioHold {
    /// No readings yet.
    fn default() -> Self {
        Self {
            first_ns: None,
            lowest: WindowExtreme::new(RAISE_WINDOW_NS, Extreme::Lowest),
            highest: WindowExtreme::new(RELAX_WINDOW_NS, Extreme::Highest),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const FRAME: u64 = 33_333_333;
    /// Target Buffer 120 ms plus the 80 ms output lead.
    const COVERED: i32 = 200;

    fn hold() -> AudioHold {
        AudioHold::default()
    }

    /// One reading per frame from `from_ns` for `secs`, `skew_ms` behind.
    fn feed(h: &mut AudioHold, from_ns: u64, secs: f64, skew_ms: i64) -> u64 {
        let frames = (secs * 1e9 / FRAME as f64) as u64;
        let mut now = from_ns;
        for i in 0..frames {
            now = from_ns + i * FRAME;
            h.observe(now, skew_ms * MS as i64);
        }
        now
    }

    #[test]
    fn the_hold_is_the_uncovered_part_of_the_skew() {
        // Within what the target and lead cover, less the margin: nothing.
        assert_eq!(hold_ms(0, COVERED, 100, 5000), 0);
        assert_eq!(hold_ms(100, COVERED, 100, 5000), 0);
        // Video ahead of its audio is not a hold either.
        assert_eq!(hold_ms(-500, COVERED, 100, 5000), 0);
        // pocketSRT: audio about 350 ms ahead of its video.
        assert_eq!(hold_ms(350, COVERED, 100, 5000), 250);
        // The stabiliser stream from #33, and a deeper target covering more.
        assert_eq!(hold_ms(1650, COVERED, 100, 5000), 1550);
        assert_eq!(hold_ms(1650, 1080, 100, 5000), 670);
        assert_eq!(hold_ms(1650, 2080, 100, 5000), 0);
        // Capped.
        assert_eq!(hold_ms(9000, COVERED, 100, 5000), 5000);
        assert_eq!(hold_ms(i64::MAX / 2, COVERED, 100, 5000), 5000);
    }

    #[test]
    fn nothing_is_decided_without_a_reading() {
        let h = hold();
        assert!(!h.measured());
        assert_eq!(h.before_prime(0, COVERED), None);
        assert_eq!(h.regulate(60_000 * MS, 0, COVERED), None);
    }

    #[test]
    fn before_priming_the_worst_reading_sizes_the_hold() {
        let mut h = hold();
        h.observe(0, 300 * MS as i64);
        h.observe(FRAME, 350 * MS as i64);
        h.observe(2 * FRAME, 320 * MS as i64);
        let change = h.before_prime(0, COVERED).expect("raised");
        assert_eq!(change.from_ms, 0);
        assert_eq!(change.to_ms, 250);
        assert_eq!(change.skew_ms, 350);
        assert!(!change.capped);
        // Only ever up before priming.
        assert_eq!(h.before_prime(250, COVERED), None);
        assert_eq!(h.before_prime(400, COVERED), None);
    }

    #[test]
    fn a_sender_within_the_target_gets_no_hold() {
        let mut h = hold();
        let now = feed(&mut h, 0, 20.0, 50);
        assert_eq!(h.before_prime(0, COVERED), None);
        assert_eq!(h.regulate(now, 0, COVERED), None);
    }

    #[test]
    fn a_skew_that_lasts_the_raise_window_raises_the_hold() {
        let mut h = hold();
        let t = feed(&mut h, 0, 5.0, 50);
        assert_eq!(h.regulate(t, 0, COVERED), None);
        // The sender starts sending its video 400 ms behind its audio.
        let window = consts::AUDIO_HOLD_RAISE_WINDOW_MS * MS;
        let mut raised_at = None;
        for i in 0..200u64 {
            let now = t + FRAME + i * FRAME;
            h.observe(now, 400 * MS as i64);
            if raised_at.is_none()
                && let Some(change) = h.regulate(now, 0, COVERED)
            {
                assert_eq!(change.to_ms, 300);
                raised_at = Some(now - t);
            }
        }
        let after = raised_at.expect("raised");
        assert!(
            (window..window + 2 * FRAME).contains(&after),
            "raised {}ms after the skew began",
            after / MS
        );
    }

    #[test]
    fn a_burst_of_late_video_shorter_than_the_window_does_not_raise() {
        let mut h = hold();
        let mut t = feed(&mut h, 0, 5.0, 50);
        // Half a second of video 600 ms behind, every few seconds.
        for _ in 0..5 {
            t = feed(&mut h, t + FRAME, 0.5, 600);
            assert_eq!(h.regulate(t, 0, COVERED), None);
            t = feed(&mut h, t + FRAME, 3.0, 50);
            assert_eq!(h.regulate(t, 0, COVERED), None);
        }
    }

    #[test]
    fn a_raise_smaller_than_the_step_is_not_made() {
        let mut h = hold();
        let t = feed(&mut h, 0, 5.0, 350);
        assert_eq!(h.regulate(t, 250, COVERED), None);
        // 10 ms more is under the step.
        let t = feed(&mut h, t + FRAME, 5.0, 360);
        assert_eq!(h.regulate(t, 250, COVERED), None);
        // 30 ms more is not.
        let t = feed(&mut h, t + FRAME, 5.0, 380);
        assert_eq!(h.regulate(t, 250, COVERED).map(|c| c.to_ms), Some(280));
    }

    #[test]
    fn the_hold_is_released_only_after_a_whole_window_needed_less() {
        let mut h = hold();
        let mut t = feed(&mut h, 0, 12.0, 350);
        assert_eq!(h.regulate(t, 250, COVERED), None);
        // The sender recovers: video now only 150 ms behind.
        let recovered_at = t;
        let window = consts::AUDIO_HOLD_RELAX_WINDOW_MS * MS;
        let mut released = None;
        for _ in 0..400 {
            t += FRAME;
            h.observe(t, 150 * MS as i64);
            if let Some(change) = h.regulate(t, 250, COVERED) {
                released = Some((t - recovered_at, change));
                break;
            }
        }
        let (after, change) = released.expect("released");
        assert!(
            (window..window + 2 * FRAME).contains(&after),
            "released {}ms after the recovery",
            after / MS
        );
        assert_eq!(change.from_ms, 250);
        assert_eq!(change.to_ms, 50);
    }

    #[test]
    fn a_surplus_under_the_threshold_is_left_alone() {
        let mut h = hold();
        let t = feed(&mut h, 0, 12.0, 320);
        // 220 would do; 250 is within the threshold of it.
        assert_eq!(h.regulate(t, 250, COVERED), None);
    }

    #[test]
    fn jitter_settles_on_one_hold_rather_than_churning() {
        let mut h = hold();
        let mut current = 0;
        let mut changes_after_prime = 0;
        for i in 0..(60_000 * MS / FRAME) {
            let now = i * FRAME;
            // A frame and a chunk of granularity either side of 350 ms.
            let skew_ms = 350 + [-30i64, 0, 25, -10, 30][(i % 5) as usize];
            h.observe(now, skew_ms * MS as i64);
            // Primed a second in.
            let primed = i >= 30;
            let change = if primed {
                h.regulate(now, current, COVERED)
            } else {
                h.before_prime(current, COVERED)
            };
            if let Some(c) = change {
                current = c.to_ms;
                changes_after_prime += u32::from(primed);
            }
        }
        // Sized for the worst reading before priming, and left there: the
        // sustained reading is lower and the worst one is the same.
        assert_eq!(current, 280);
        assert_eq!(changes_after_prime, 0);
    }

    #[test]
    fn a_skew_past_the_ceiling_is_capped_and_says_so() {
        let mut h = hold();
        h.observe(0, 9_000 * MS as i64);
        let change = h.before_prime(0, COVERED).expect("raised");
        assert_eq!(change.to_ms, consts::AUDIO_HOLD_MAX_MS);
        assert!(change.capped);
    }

    #[test]
    fn reset_starts_over() {
        let mut h = hold();
        feed(&mut h, 0, 3.0, 800);
        assert!(h.measured());
        h.reset();
        assert!(!h.measured());
        assert_eq!(h.before_prime(0, COVERED), None);
    }
}
