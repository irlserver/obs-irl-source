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
//! a scheduling hiccup.
//!
//! The delay is also taken back, slowly. The measurement that sets it before
//! the anchor is one frame at the start of a connection, and on a poor link
//! that moment is a burst followed by catch-up (a relay replaying video from
//! an older keyframe next to live audio, a throttled uplink draining its
//! queue): the frame can look more than a second late on a stream that has no
//! skew at all once it settles, and a delay that never shrank then held
//! on-time video that far behind its audio for the whole connection. So every
//! frame handed over also reports its *arrival* margin, and when a whole
//! window of them needed less than the delay in force by more than a
//! threshold, the delay ramps down to what the window needed plus a tick. It
//! ramps rather than steps: due times move a little earlier each cycle, so
//! the picture plays a few percent fast (the caller passes the Catch-Up
//! Speed) and nothing jumps or is skipped. A raise cancels a ramp.
//!
//! The bar is deliberately "not late", not "a full delivery lead early". The
//! lead the pacing queue applies to a frame it holds is an allowance for its
//! own timer oversleeping on the way to the due time; a frame handed over on
//! arrival never sleeps, and libobs shows it on time as long as it is in the
//! queue before its due time passes. Asking every frame for the lead would
//! delay a healthy stream by the lead for nothing.

use crate::consts;

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

/// The delay turned out larger than a whole window of frames needed, and
/// starts ramping down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelayRelax {
    /// The delay in force.
    pub from_ns: u64,
    /// Where the ramp ends.
    pub to_ns: u64,
}

/// One step of the ramp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RampStep {
    /// The delay now.
    pub delay_ns: u64,
    /// How much earlier every due time moved this step.
    pub moved_ns: u64,
    /// The ramp reached its target.
    pub done: bool,
}

/// What the frames handed over since `since_ns` needed, at most.
#[derive(Debug, Clone, Copy)]
struct Surplus {
    since_ns: u64,
    frames: u32,
    worst_need_ns: u64,
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
    /// Relaxing: the observation window, the threshold a surplus must exceed,
    /// what has been observed, and the ramp in progress. A zero window turns
    /// relaxing off.
    relax_window_ns: u64,
    relax_min_ns: u64,
    surplus: Option<Surplus>,
    ramp_to_ns: Option<u64>,
    ramp_at_ns: u64,
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
            relax_window_ns: 0,
            relax_min_ns: 0,
            surplus: None,
            ramp_to_ns: None,
            ramp_at_ns: 0,
        }
    }

    /// Let the delay ramp back down when every frame across `window_ns`
    /// needed less than it by more than `min_ns`.
    #[cfg(test)]
    #[must_use]
    fn with_relax(mut self, window_ns: u64, min_ns: u64) -> Self {
        self.relax_window_ns = window_ns;
        self.relax_min_ns = min_ns;
        self
    }

    /// The delay every due time carries.
    pub fn delay_ns(&self) -> u64 {
        self.delay_ns
    }

    /// Whether the delay has reached its ceiling: a shortfall past this cannot
    /// be scheduled away, and the frames stay late.
    pub fn at_ceiling(&self) -> bool {
        self.delay_ns >= self.max_ns
    }

    /// Back to zero, for a new connection or a cleared source.
    pub fn reset(&mut self) {
        self.delay_ns = 0;
        self.window = None;
        self.surplus = None;
        self.ramp_to_ns = None;
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
        // The evidence for a smaller delay is void.
        self.surplus = None;
        self.ramp_to_ns = None;
        Some(raise)
    }

    /// A frame handed over after the anchor, `arrival_margin_ns` being its
    /// due time minus its packet's arrival, delay included. Records what it
    /// needed; once a whole relax window needed less than the delay in force
    /// by more than the threshold, starts a ramp down to that plus a tick.
    pub fn note_arrival(
        &mut self,
        now_ns: u64,
        arrival_margin_ns: i64,
        tick_ns: u64,
    ) -> Option<DelayRelax> {
        if self.relax_window_ns == 0 {
            return None;
        }
        let need_ns = self.required_ns(arrival_margin_ns, tick_ns, tick_ns);
        // A ramp must not run past what a frame in hand needs.
        if let Some(to_ns) = self.ramp_to_ns {
            let floor_ns = need_ns + tick_ns;
            if floor_ns >= self.delay_ns {
                self.ramp_to_ns = None;
            } else if floor_ns > to_ns {
                self.ramp_to_ns = Some(floor_ns);
            }
        }
        let surplus = self.surplus.get_or_insert(Surplus {
            since_ns: now_ns,
            frames: 0,
            worst_need_ns: 0,
        });
        surplus.frames += 1;
        surplus.worst_need_ns = surplus.worst_need_ns.max(need_ns);
        if now_ns.saturating_sub(surplus.since_ns) < self.relax_window_ns {
            return None;
        }
        let seen = *surplus;
        self.surplus = None;
        if seen.frames < self.min_frames || self.ramp_to_ns.is_some() {
            return None;
        }
        let to_ns = seen.worst_need_ns + tick_ns;
        if self.delay_ns <= to_ns + self.relax_min_ns {
            return None;
        }
        self.ramp_to_ns = Some(to_ns);
        self.ramp_at_ns = now_ns;
        Some(DelayRelax {
            from_ns: self.delay_ns,
            to_ns,
        })
    }

    /// Advance a ramp in progress to `now_ns`, moving the delay down by
    /// `rate` of the time since the last step (0.05 plays video 5% fast).
    pub fn ramp(&mut self, now_ns: u64, rate: f64) -> Option<RampStep> {
        let to_ns = self.ramp_to_ns?;
        let elapsed_ns = now_ns.saturating_sub(self.ramp_at_ns);
        self.ramp_at_ns = now_ns;
        let want_ns = (elapsed_ns as f64 * rate.max(0.0)) as u64;
        let moved_ns = want_ns.min(self.delay_ns.saturating_sub(to_ns));
        self.delay_ns -= moved_ns;
        let done = self.delay_ns <= to_ns;
        if done {
            self.ramp_to_ns = None;
        }
        (moved_ns > 0 || done).then_some(RampStep {
            delay_ns: self.delay_ns,
            moved_ns,
            done,
        })
    }

    /// The audio playout moved every due time `grown_ns` later because the
    /// audio hold ([`crate::audio_hold`]) is building its cushion. Take as
    /// much of that back out of the delay as there is, so due times stay
    /// where they were and the lip-sync error the delay stands for shrinks by
    /// the same amount; nothing on screen moves. Returns how much the delay
    /// came down.
    ///
    /// Readings already taken were measured against the playout as it was,
    /// so what they called for is `grown_ns` less now.
    pub fn absorb(&mut self, grown_ns: u64) -> u64 {
        let moved_ns = grown_ns.min(self.delay_ns);
        self.delay_ns -= moved_ns;
        if let Some(window) = self.window.as_mut() {
            window.worst_ns = window.worst_ns.saturating_sub(grown_ns);
        }
        if let Some(surplus) = self.surplus.as_mut() {
            surplus.worst_need_ns = surplus.worst_need_ns.saturating_sub(grown_ns);
        }
        if let Some(to_ns) = self.ramp_to_ns {
            let to_ns = to_ns.saturating_sub(grown_ns);
            self.ramp_to_ns = (to_ns < self.delay_ns).then_some(to_ns);
        }
        moved_ns
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

impl Default for VideoDelay {
    /// The plugin's delay, from the `VIDEO_DELAY_*` constants, relaxing on.
    fn default() -> Self {
        Self {
            relax_window_ns: consts::VIDEO_DELAY_RELAX_WINDOW_MS * 1_000_000,
            relax_min_ns: consts::VIDEO_DELAY_RELAX_MIN_MS * 1_000_000,
            ..Self::new(
                consts::VIDEO_DELAY_MAX_MS * 1_000_000,
                consts::VIDEO_DELAY_WINDOW_MS * 1_000_000,
                consts::VIDEO_DELAY_MIN_FRAMES,
            )
        }
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
        assert!(!d.at_ceiling());
        let raise = d.before_anchor(-2_000_000_000, TICK, TICK).expect("raised");
        assert_eq!(raise.to_ns, MAX);
        assert!(raise.capped);
        assert!(d.at_ceiling());
        // Still short at the ceiling: nothing more to do, nothing to report.
        assert_eq!(d.before_anchor(-3_000_000_000, TICK, TICK), None);
        assert!(d.at_ceiling());
    }

    #[test]
    fn a_delay_below_the_ceiling_is_not_at_it() {
        let mut d = delay();
        d.before_anchor(-150_000_000, TICK, TICK).expect("raised");
        assert!(d.delay_ns() > 0);
        assert!(!d.at_ceiling());
        d.reset();
        assert!(!d.at_ceiling());
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

    const RELAX: u64 = 10_000_000_000;

    fn relaxing() -> VideoDelay {
        delay().with_relax(RELAX, 50_000_000)
    }

    /// Feed `secs` of 30fps frames whose raw margin (delay excluded) is
    /// `raw_margin_ns`, stepping the ramp at 5% as the caller does.
    fn run(d: &mut VideoDelay, t0: u64, secs: u64, raw_margin_ns: i64) -> Vec<DelayRelax> {
        let mut relaxes = Vec::new();
        for i in 0..secs * 30 {
            let now = t0 + i * 33_333_333;
            d.ramp(now, 0.05);
            let margin = raw_margin_ns + d.delay_ns() as i64;
            relaxes.extend(d.note_arrival(now, margin, TICK));
        }
        relaxes
    }

    #[test]
    fn a_delay_set_by_a_startup_transient_ramps_back_down() {
        let mut d = relaxing();
        // One frame 1.3 s late at the anchor, on a stream whose frames then
        // arrive 100 ms early for good.
        d.before_anchor(-1_300_000_000, TICK, TICK).expect("raised");
        let set = d.delay_ns();
        let relaxes = run(&mut d, 1_000_000_000, 60, 100_000_000);
        assert_eq!(relaxes.len(), 1, "one decision, not one per window");
        assert_eq!(relaxes[0].from_ns, set);
        assert_eq!(
            relaxes[0].to_ns, TICK,
            "nothing needed, so one tick of headroom"
        );
        assert_eq!(d.delay_ns(), TICK);
    }

    #[test]
    fn the_ramp_is_gradual() {
        let mut d = relaxing();
        d.before_anchor(-1_300_000_000, TICK, TICK).expect("raised");
        let set = d.delay_ns();
        // Eleven seconds: the window closes at ten, then one second of ramp
        // at 5% takes back about 50 ms, not the whole delay.
        run(&mut d, 1_000_000_000, 11, 100_000_000);
        let taken = set - d.delay_ns();
        assert!((30_000_000..70_000_000).contains(&taken), "took {taken}ns");
    }

    #[test]
    fn a_delay_the_sender_really_needs_stays() {
        let mut d = relaxing();
        d.before_anchor(-150_000_000, TICK, TICK).expect("raised");
        let set = d.delay_ns();
        // Every frame keeps arriving 150 ms late.
        assert!(run(&mut d, 1_000_000_000, 60, -150_000_000).is_empty());
        assert_eq!(d.delay_ns(), set);
    }

    #[test]
    fn one_late_frame_in_the_window_keeps_the_delay_it_needed() {
        let mut d = relaxing();
        d.before_anchor(-1_300_000_000, TICK, TICK).expect("raised");
        let t0 = 1_000_000_000;
        run(&mut d, t0, 5, 100_000_000);
        // Mid-window, one frame 400 ms late.
        let margin = -400_000_000 + d.delay_ns() as i64;
        d.note_arrival(t0 + 5_000_000_000, margin, TICK);
        let relaxes = run(&mut d, t0 + 5_033_333_333, 60, 100_000_000);
        // 400 ms plus the tick allowance, in ticks, plus a tick of headroom.
        assert_eq!(relaxes[0].to_ns, 26 * TICK);
    }

    #[test]
    fn a_raise_cancels_the_ramp() {
        let mut d = relaxing();
        d.before_anchor(-1_300_000_000, TICK, TICK).expect("raised");
        run(&mut d, 1_000_000_000, 12, 100_000_000);
        let mid = d.delay_ns();
        let raise = d.before_anchor(-2_000_000_000 + mid as i64, TICK, TICK);
        assert!(raise.is_some());
        let after = d.delay_ns();
        assert_eq!(d.ramp(20_000_000_000, 0.05), None);
        assert_eq!(d.delay_ns(), after);
    }

    #[test]
    fn audio_hold_growth_is_taken_out_of_the_delay() {
        let mut d = delay();
        d.before_anchor(-300_000_000, TICK, TICK).expect("raised");
        let set = d.delay_ns();
        assert_eq!(d.absorb(100_000_000), 100_000_000);
        assert_eq!(d.delay_ns(), set - 100_000_000);
        // Never below zero, and only what there was is reported.
        assert_eq!(d.absorb(set), set - 100_000_000);
        assert_eq!(d.delay_ns(), 0);
        assert_eq!(d.absorb(TICK), 0);
    }

    #[test]
    fn late_frames_seen_before_the_growth_need_that_much_less() {
        let mut d = delay();
        // Three frames 400 ms late across the window...
        for i in 0..3u64 {
            d.note(i * 400_000_000, -400_000_000, 0, TICK);
        }
        // ...then the audio playout grows by 300 ms before the verdict.
        d.absorb(300_000_000);
        let raise = d.expire(WINDOW + 1).expect("still short");
        // What is left of the 400 ms the frames needed.
        assert_eq!(raise.to_ns, 24 * TICK - 300_000_000);
    }

    #[test]
    fn growth_moves_a_ramp_target_with_it() {
        let mut d = relaxing();
        d.before_anchor(-1_300_000_000, TICK, TICK).expect("raised");
        run(&mut d, 1_000_000_000, 11, 100_000_000);
        let ramping = d.delay_ns();
        assert!(ramping > TICK, "a ramp is under way");
        // Growth past the ramp's target ends it where the growth left it.
        d.absorb(ramping);
        assert_eq!(d.delay_ns(), 0);
        assert_eq!(d.ramp(30_000_000_000, 0.05), None);
    }

    #[test]
    fn without_with_relax_the_delay_never_shrinks() {
        let mut d = delay();
        d.before_anchor(-1_300_000_000, TICK, TICK).expect("raised");
        let set = d.delay_ns();
        assert!(run(&mut d, 1_000_000_000, 60, 100_000_000).is_empty());
        assert_eq!(d.delay_ns(), set);
    }
}
