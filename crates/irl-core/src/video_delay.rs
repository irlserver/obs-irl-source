//! The standing delay on the video schedule.
//!
//! Pacing hands each frame to libobs a delivery lead before it is due, which
//! is what keeps libobs's async queue about one frame deep and the cadence
//! smooth. That only works for a frame that is in hand at least a lead before
//! its due time. How early it is in hand — its *arrival margin*, the due time
//! minus the moment its packet reached the video thread — is set by the
//! sender: video that leaves the encoder later than the audio of the same
//! instant has that much less margin, and once that skew exceeds what Target
//! Buffer covers the margin is negative and every frame is late. A late frame
//! goes out on arrival, unpaced, and bursty arrival then shows as dropped
//! frames, because libobs discards a frame it finds behind its play head
//! whenever a newer one is already queued. That is the "low fps" a stream with
//! trailing video shows, and it is what the delay here prevents.
//!
//! The delay is added to every due time and sized from the worst margin seen,
//! so that frames are in hand a full lead early again. It trades a fixed,
//! known lip-sync error for a smooth picture; the caller's log line says how
//! much more Target Buffer would take the error back to zero. Before libobs's
//! play head is anchored nothing has been shown yet, so a shortfall is acted
//! on at once. Afterwards a raise moves the picture (one frame holds for the
//! size of the raise), so it takes a shortfall that recurs across a window,
//! not a single late frame from a scheduling hiccup. The delay never shrinks
//! within a connection: lowering it would be a second visible step, for a
//! margin that might well tighten again.

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
    /// The margin called for more than the ceiling allows.
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

    /// The whole delay a frame needs so that it is in hand `lead_ns` before it
    /// is due, in whole canvas ticks. `due_ns` is the frame's scheduled time
    /// *including* the current delay, as the pacing queue holds it, and
    /// `received_ns` when its packet arrived.
    fn required_ns(&self, due_ns: u64, received_ns: u64, lead_ns: u64, tick_ns: u64) -> u64 {
        let margin_ns = due_ns as i64 - self.delay_ns as i64 - received_ns as i64;
        let shortfall_ns = lead_ns as i64 - margin_ns;
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

    /// A frame considered for anchoring the play head. Nothing has been shown
    /// yet, so a shortfall raises the delay at once.
    pub fn before_anchor(
        &mut self,
        due_ns: u64,
        received_ns: u64,
        lead_ns: u64,
        tick_ns: u64,
    ) -> Option<DelayRaise> {
        self.window = None;
        let need_ns = self.required_ns(due_ns, received_ns, lead_ns, tick_ns);
        self.raise_to(need_ns, 0)
    }

    /// A frame handed over after the anchor. A shortfall is recorded; the
    /// delay is raised only once the window says it recurs.
    pub fn note(
        &mut self,
        now_ns: u64,
        due_ns: u64,
        received_ns: u64,
        lead_ns: u64,
        tick_ns: u64,
    ) -> Option<DelayRaise> {
        // Against the delay its due time carries: a raise below must not make
        // this frame look short by the amount just added.
        let need_ns = self.required_ns(due_ns, received_ns, lead_ns, tick_ns);
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
    const LEAD: u64 = 2 * TICK;
    const WINDOW: u64 = 1_000_000_000;
    const MAX: u64 = 1_000_000_000;

    fn delay() -> VideoDelay {
        VideoDelay::new(MAX, WINDOW, 3)
    }

    #[test]
    fn a_frame_with_a_full_lead_in_hand_needs_no_delay() {
        let mut d = delay();
        assert_eq!(d.before_anchor(1_000 + LEAD, 1_000, LEAD, TICK), None);
        assert_eq!(
            d.before_anchor(1_000 + 500_000_000, 1_000, LEAD, TICK),
            None
        );
        assert_eq!(d.delay_ns(), 0);
    }

    #[test]
    fn a_frame_with_no_margin_gets_the_lead_before_the_anchor() {
        let mut d = delay();
        let raise = d.before_anchor(5_000, 5_000, LEAD, TICK).expect("raised");
        assert_eq!(raise.from_ns, 0);
        assert_eq!(raise.to_ns, LEAD);
        assert!(!raise.capped);
        assert_eq!(d.delay_ns(), LEAD);
    }

    #[test]
    fn a_late_frame_gets_its_lateness_plus_the_lead_in_whole_ticks() {
        let mut d = delay();
        // 150 ms late: 150 + 33.3 = 183.3 ms, which is 11 ticks (183.33 ms).
        let received = 1_000_000_000;
        let due = received - 150_000_000;
        let raise = d.before_anchor(due, received, LEAD, TICK).expect("raised");
        assert_eq!(raise.to_ns, 11 * TICK);
    }

    #[test]
    fn the_delay_only_ever_grows_before_the_anchor() {
        let mut d = delay();
        d.before_anchor(0, 100_000_000, LEAD, TICK).expect("raised");
        let first = d.delay_ns();
        // A frame with more margin (its due already carries the delay).
        assert_eq!(
            d.before_anchor(first + 100_000_000, 0, LEAD, TICK),
            None,
            "a smaller shortfall must not lower the delay"
        );
        assert_eq!(d.delay_ns(), first);
        // A frame with less raises it further.
        let more = d
            .before_anchor(first, 200_000_000, LEAD, TICK)
            .expect("raised");
        assert_eq!(more.from_ns, first);
        assert!(more.to_ns > first);
    }

    #[test]
    fn the_existing_delay_counts_toward_the_margin() {
        let mut d = delay();
        d.before_anchor(0, 0, LEAD, TICK)
            .expect("raised to the lead");
        // Due times now carry the delay: a frame due `LEAD` after it arrived
        // has, net of the delay, no margin — exactly what the delay covers.
        assert_eq!(d.before_anchor(10_000 + LEAD, 10_000, LEAD, TICK), None);
    }

    #[test]
    fn the_ceiling_caps_the_delay_and_says_so() {
        let mut d = delay();
        let received = 5_000_000_000;
        let raise = d
            .before_anchor(received - 2_000_000_000, received, LEAD, TICK)
            .expect("raised");
        assert_eq!(raise.to_ns, MAX);
        assert!(raise.capped);
        // Still short at the ceiling: nothing more to do, nothing to report.
        assert_eq!(
            d.before_anchor(received - 3_000_000_000, received, LEAD, TICK),
            None
        );
    }

    #[test]
    fn after_the_anchor_one_late_frame_does_not_raise_the_delay() {
        let mut d = delay();
        let t0 = 10_000_000_000;
        assert_eq!(d.note(t0, t0, t0, LEAD, TICK), None);
        assert_eq!(d.expire(t0 + WINDOW), None);
        assert_eq!(d.delay_ns(), 0);
    }

    #[test]
    fn after_the_anchor_a_burst_of_late_frames_does_not_raise_the_delay() {
        let mut d = delay();
        let t0 = 10_000_000_000;
        // Six frames late within 100 ms: one hiccup, not a standing shortfall.
        for i in 0..6 {
            let t = t0 + i * TICK;
            assert_eq!(d.note(t, t, t, LEAD, TICK), None);
        }
        assert_eq!(d.expire(t0 + WINDOW), None);
        assert_eq!(d.delay_ns(), 0);
    }

    #[test]
    fn after_the_anchor_a_recurring_shortfall_raises_the_delay_to_its_worst() {
        let mut d = delay();
        let t0 = 10_000_000_000;
        // Three frames spread across the window, 10 ms, 0 ms and 5 ms of
        // margin against a 33.3 ms lead: the worst needs 33.3 ms, two ticks.
        assert_eq!(d.note(t0, t0 + 10_000_000, t0, LEAD, TICK), None);
        assert_eq!(
            d.note(
                t0 + 600_000_000,
                t0 + 600_000_000,
                t0 + 600_000_000,
                LEAD,
                TICK
            ),
            None
        );
        assert_eq!(
            d.note(
                t0 + 900_000_000,
                t0 + 905_000_000,
                t0 + 900_000_000,
                LEAD,
                TICK
            ),
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
            let t = t0 + i * 400_000_000;
            assert_eq!(d.note(t, t, t, LEAD, TICK), None);
        }
        // The fourth frame arrives after the window elapsed: the raise comes
        // out of this call, judged on the three before it.
        let t = t0 + WINDOW + 1;
        let raise = d.note(t, t, t, LEAD, TICK).expect("raised");
        assert_eq!(raise.frames, 3);
        assert_eq!(raise.to_ns, LEAD);
        // The fourth frame needed the lead too, and the raise just provided
        // it: it does not start a new window.
        assert_eq!(d.expire(t + WINDOW), None);
    }

    #[test]
    fn frames_covered_by_the_delay_are_not_counted() {
        let mut d = delay();
        d.before_anchor(0, 0, LEAD, TICK)
            .expect("raised to the lead");
        let t0 = 10_000_000_000;
        for i in 0..5 {
            let t = t0 + i * 300_000_000;
            // Due carries the delay; net margin is zero, which the delay covers.
            assert_eq!(d.note(t, t + LEAD, t, LEAD, TICK), None);
        }
        assert_eq!(d.expire(t0 + 2 * WINDOW), None);
        assert_eq!(d.delay_ns(), LEAD);
    }

    #[test]
    fn reset_returns_to_zero() {
        let mut d = delay();
        d.before_anchor(0, 0, LEAD, TICK).expect("raised");
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
