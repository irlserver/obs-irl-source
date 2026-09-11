//! The standing delay on the video schedule.
//!
//! Pacing hands each frame to libobs before it is due, which is what keeps
//! libobs's async queue about one frame deep and the cadence smooth. A frame
//! handed over *after* it is due is shown a canvas tick late, and when two
//! late frames reach libobs inside one tick it discards the older one: that is
//! the "low fps" a stream shows when its video reaches the plugin later than
//! the audio of the same instant by more than Target Buffer covers. How early
//! a frame is in hand is the sender's to set, so the plugin cannot make a late
//! frame early; what it can do is move the whole video schedule later by a
//! fixed amount, so that the frames stop being late.
//!
//! That amount is the delay here. It is added to every due time and sized from
//! the worst shortfall seen. It trades a fixed, known lip-sync error for a
//! smooth picture; the caller's log line says how much more Target Buffer
//! would take the error back to zero. Before libobs's play head is anchored
//! nothing has been shown yet, so a shortfall is acted on at once. Afterwards a
//! raise moves the picture (one frame holds for the size of the raise), so it
//! takes a shortfall that recurs across a window, not a single late frame from
//! a scheduling hiccup. The delay never shrinks within a connection: lowering
//! it would be a second visible step, for a margin that might well tighten
//! again.
//!
//! The bar is deliberately "not late", not "a full delivery lead early". The
//! lead the pacing queue applies to a frame it holds is an allowance for its
//! own timer oversleeping on the way to the due time; a frame handed over on
//! arrival never sleeps, and libobs shows it on time as long as it is in the
//! queue before its due time passes. Asking every frame for the lead would
//! delay a healthy stream by the lead for nothing.

/// One change of the delay, for the caller to log and mirror into the stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelayRaise {
    /// The delay before.
    pub from_ns: u64,
    /// The delay now.
    pub to_ns: u64,
    /// Frames that fell short in the window that caused the raise; zero for a
    /// raise before the anchor, which one frame is enough for.
    pub frames: u32,
    /// The shortfall called for more than the ceiling allows.
    pub capped: bool,
}

/// Late frames seen since the play head was anchored, awaiting a verdict.
#[derive(Debug, Clone, Copy)]
struct Window {
    since_ns: u64,
    last_ns: u64,
    frames: u32,
    /// The largest delay any frame in the window needed.
    worst_ns: u64,
}

/// The standing delay and the observation window behind it.
#[derive(Debug)]
pub struct VideoDelay {
    delay_ns: u64,
    max_ns: u64,
    window_ns: u64,
    min_frames: u32,
    window: Option<Window>,
}

impl VideoDelay {
    /// A zero delay that may grow to `max_ns`, and after the anchor only when
    /// at least `min_frames` frames fall short within `window_ns`, spread over
    /// at least half of it.
    pub fn new(max_ns: u64, window_ns: u64, min_frames: u32) -> Self {
        Self {
            delay_ns: 0,
            max_ns,
            window_ns,
            min_frames,
            window: None,
        }
    }

    /// The delay every due time carries.
    pub fn delay_ns(&self) -> u64 {
        self.delay_ns
    }

    /// Back to zero, for a new connection or a cleared source.
    pub fn reset(&mut self) {
        self.delay_ns = 0;
        self.window = None;
    }

    /// The whole delay a frame needs so that it is in hand `want_ns` before it
    /// is due, in whole canvas ticks. `margin_ns` is how early the frame was in
    /// hand against its due time *as the pacing queue holds it*, delay
    /// included, so the delay is taken back out before the shortfall is
    /// measured.
    fn required_ns(&self, margin_ns: i64, want_ns: u64, tick_ns: u64) -> u64 {
        let raw_margin_ns = margin_ns - self.delay_ns as i64;
        let shortfall_ns = want_ns as i64 - raw_margin_ns;
        if shortfall_ns <= 0 {
            return 0;
        }
        round_up_to_tick(shortfall_ns as u64, tick_ns)
    }

    /// Raise to `need_ns` if that is more than the delay already is.
    fn raise_to(&mut self, need_ns: u64, frames: u32) -> Option<DelayRaise> {
        let to_ns = need_ns.min(self.max_ns);
        if to_ns <= self.delay_ns {
            return None;
        }
        let raise = DelayRaise {
            from_ns: self.delay_ns,
            to_ns,
            frames,
            capped: need_ns > self.max_ns,
        };
        self.delay_ns = to_ns;
        Some(raise)
    }

    /// A frame considered for anchoring the play head, `margin_ns` being its
    /// due time minus its packet's arrival. Nothing has been shown yet, so a
    /// shortfall against `want_ns` raises the delay at once.
    pub fn before_anchor(
        &mut self,
        margin_ns: i64,
        want_ns: u64,
        tick_ns: u64,
    ) -> Option<DelayRaise> {
        self.window = None;
        let need_ns = self.required_ns(margin_ns, want_ns, tick_ns);
        self.raise_to(need_ns, 0)
    }

    /// A frame handed over after the anchor, `margin_ns` being its due time
    /// minus the hand-over time. A shortfall against `want_ns` is recorded;
    /// the delay is raised only once the window says it recurs.
    pub fn note(
        &mut self,
        now_ns: u64,
        margin_ns: i64,
        want_ns: u64,
        tick_ns: u64,
    ) -> Option<DelayRaise> {
        // Against the delay its due time carries: a raise below must not make
        // this frame look short by the amount just added.
        let need_ns = self.required_ns(margin_ns, want_ns, tick_ns);
        // An elapsed window is judged on what it holds, before this frame
        // starts the next one.
        let raise = self.expire(now_ns);
        if need_ns > self.delay_ns {
            let window = self.window.get_or_insert(Window {
                since_ns: now_ns,
                last_ns: now_ns,
                frames: 0,
                worst_ns: 0,
            });
            window.last_ns = now_ns;
            window.frames += 1;
            window.worst_ns = window.worst_ns.max(need_ns);
        }
        raise
    }

    /// Judge a window that has run its course. Called once per pacing cycle as
    /// well as from [`Self::note`], so a window whose late frames simply
    /// stopped is closed rather than kept open for the next one.
    pub fn expire(&mut self, now_ns: u64) -> Option<DelayRaise> {
        let window = self.window?;
        if now_ns.saturating_sub(window.since_ns) < self.window_ns {
            return None;
        }
        self.window = None;
        let recurring = window.frames >= self.min_frames
            && window.last_ns.saturating_sub(window.since_ns) >= self.window_ns / 2;
        if !recurring {
            return None;
        }
        self.raise_to(window.worst_ns, window.frames)
    }
}

/// `ns` rounded up to a whole number of canvas ticks. libobs displays on its
/// ticks, so a fraction of one buys nothing.
fn round_up_to_tick(ns: u64, tick_ns: u64) -> u64 {
    if tick_ns == 0 {
        return ns;
    }
    ns.div_ceil(tick_ns) * tick_ns
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: u64 = 16_666_667;
    const WINDOW: u64 = 1_000_000_000;
    const MAX: u64 = 1_000_000_000;

    fn delay() -> VideoDelay {
        VideoDelay::new(MAX, WINDOW, 3)
    }

    #[test]
    fn a_frame_in_hand_early_enough_needs_no_delay() {
        let mut d = delay();
        assert_eq!(d.before_anchor(TICK as i64, TICK, TICK), None);
        assert_eq!(d.before_anchor(500_000_000, TICK, TICK), None);
        assert_eq!(d.delay_ns(), 0);
    }

    #[test]
    fn a_frame_with_no_margin_gets_the_wanted_allowance_before_the_anchor() {
        let mut d = delay();
        let raise = d.before_anchor(0, TICK, TICK).expect("raised");
        assert_eq!(raise.from_ns, 0);
        assert_eq!(raise.to_ns, TICK);
        assert!(!raise.capped);
        assert_eq!(d.delay_ns(), TICK);
    }

    #[test]
    fn a_late_frame_gets_its_lateness_plus_the_allowance_in_whole_ticks() {
        let mut d = delay();
        // 150 ms late plus a 16.7 ms allowance is 166.7 ms: exactly 10 ticks.
        let raise = d.before_anchor(-150_000_000, TICK, TICK).expect("raised");
        assert_eq!(raise.to_ns, 10 * TICK);
        // 151 ms late needs an eleventh.
        let mut d = delay();
        let raise = d.before_anchor(-151_000_000, TICK, TICK).expect("raised");
        assert_eq!(raise.to_ns, 11 * TICK);
    }

    #[test]
    fn the_delay_only_ever_grows_before_the_anchor() {
        let mut d = delay();
        d.before_anchor(-100_000_000, TICK, TICK).expect("raised");
        let first = d.delay_ns();
        // A frame with more margin (its due already carries the delay).
        assert_eq!(
            d.before_anchor(first as i64 + 100_000_000, TICK, TICK),
            None,
            "a smaller shortfall must not lower the delay"
        );
        assert_eq!(d.delay_ns(), first);
        // A frame with less raises it further.
        let more = d.before_anchor(-200_000_000, TICK, TICK).expect("raised");
        assert_eq!(more.from_ns, first);
        assert!(more.to_ns > first);
    }

    #[test]
    fn the_existing_delay_counts_toward_the_margin() {
        let mut d = delay();
        d.before_anchor(0, TICK, TICK).expect("raised to a tick");
        // Due times now carry the delay: a frame due a tick after it arrived
        // has, net of the delay, no margin — exactly what the delay covers.
        assert_eq!(d.before_anchor(TICK as i64, TICK, TICK), None);
    }

    #[test]
    fn the_ceiling_caps_the_delay_and_says_so() {
        let mut d = delay();
        let raise = d.before_anchor(-2_000_000_000, TICK, TICK).expect("raised");
        assert_eq!(raise.to_ns, MAX);
        assert!(raise.capped);
        // Still short at the ceiling: nothing more to do, nothing to report.
        assert_eq!(d.before_anchor(-3_000_000_000, TICK, TICK), None);
    }

    #[test]
    fn after_the_anchor_a_frame_handed_over_before_it_is_due_is_not_short() {
        let mut d = delay();
        let t0 = 10_000_000_000;
        // In hand with anything from a nanosecond to a full lead to spare.
        for (i, margin) in [1, 5_000_000, 20_000_000, 33_333_334]
            .into_iter()
            .enumerate()
        {
            assert_eq!(d.note(t0 + i as u64 * 300_000_000, margin, 0, TICK), None);
        }
        assert_eq!(d.expire(t0 + 2 * WINDOW), None);
        assert_eq!(d.delay_ns(), 0);
    }

    #[test]
    fn after_the_anchor_one_late_frame_does_not_raise_the_delay() {
        let mut d = delay();
        let t0 = 10_000_000_000;
        assert_eq!(d.note(t0, -5_000_000, 0, TICK), None);
        assert_eq!(d.expire(t0 + WINDOW), None);
        assert_eq!(d.delay_ns(), 0);
    }

    #[test]
    fn after_the_anchor_a_burst_of_late_frames_does_not_raise_the_delay() {
        let mut d = delay();
        let t0 = 10_000_000_000;
        // Six frames late within 100 ms: one hiccup, not a standing shortfall.
        for i in 0..6 {
            assert_eq!(d.note(t0 + i * TICK, -5_000_000, 0, TICK), None);
        }
        assert_eq!(d.expire(t0 + WINDOW), None);
        assert_eq!(d.delay_ns(), 0);
    }

    #[test]
    fn after_the_anchor_a_recurring_shortfall_raises_the_delay_to_its_worst() {
        let mut d = delay();
        let t0 = 10_000_000_000;
        // Three frames spread across the window, 5, 20 and 1 ms late: the
        // worst needs 20 ms, which is two ticks.
        assert_eq!(d.note(t0, -5_000_000, 0, TICK), None);
        assert_eq!(d.note(t0 + 600_000_000, -20_000_000, 0, TICK), None);
        assert_eq!(
            d.note(t0 + 900_000_000, -1_000_000, 0, TICK),
            None,
            "the window has not run its course"
        );
        let raise = d.expire(t0 + WINDOW).expect("raised at the window's end");
        assert_eq!(raise.to_ns, 2 * TICK);
        assert_eq!(raise.frames, 3);
        assert_eq!(d.delay_ns(), 2 * TICK);
    }

    #[test]
    fn a_late_frame_after_the_window_judges_it_and_starts_the_next() {
        let mut d = delay();
        let t0 = 10_000_000_000;
        for i in 0..3 {
            assert_eq!(d.note(t0 + i * 400_000_000, -5_000_000, 0, TICK), None);
        }
        // The fourth frame arrives after the window elapsed: the raise comes
        // out of this call, judged on the three before it.
        let t = t0 + WINDOW + 1;
        let raise = d.note(t, -5_000_000, 0, TICK).expect("raised");
        assert_eq!(raise.frames, 3);
        assert_eq!(raise.to_ns, TICK);
        // The fourth frame needed a tick too, and the raise just provided it:
        // it does not start a new window.
        assert_eq!(d.expire(t + WINDOW), None);
    }

    #[test]
    fn frames_covered_by_the_delay_are_not_counted() {
        let mut d = delay();
        d.before_anchor(0, TICK, TICK).expect("raised to a tick");
        let t0 = 10_000_000_000;
        for i in 0..5 {
            // Due carries the delay; net of it the frame is exactly on time.
            assert_eq!(d.note(t0 + i * 300_000_000, TICK as i64, 0, TICK), None);
        }
        assert_eq!(d.expire(t0 + 2 * WINDOW), None);
        assert_eq!(d.delay_ns(), TICK);
    }

    #[test]
    fn reset_returns_to_zero() {
        let mut d = delay();
        d.before_anchor(0, TICK, TICK).expect("raised");
        d.reset();
        assert_eq!(d.delay_ns(), 0);
        assert_eq!(d.expire(u64::MAX), None);
    }

    #[test]
    fn rounding_goes_up_to_whole_ticks() {
        assert_eq!(round_up_to_tick(1, TICK), TICK);
        assert_eq!(round_up_to_tick(TICK, TICK), TICK);
        assert_eq!(round_up_to_tick(TICK + 1, TICK), 2 * TICK);
        assert_eq!(round_up_to_tick(12_345, 0), 12_345);
    }
}
