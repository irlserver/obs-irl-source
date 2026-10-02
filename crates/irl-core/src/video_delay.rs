//! The floor under the video schedule.
//!
//! A frame handed to libobs after it is due is shown a tick late, and when two
//! late frames land inside one tick libobs drops the older one ("low fps").
//! The plugin cannot make a late frame early, so it keeps every due time at
//! or above a floor sized from how late frames reach it: `F`, the worst
//! `arrival - pts` seen plus an allowance, in stream-to-OBS-clock terms like
//! the audio playout offset `O` itself. A frame is due at
//! `pts + max(O, min(F, O + max))`, so the delay against the audio,
//! `clamp(F - O, 0, max)`, is derived and never stored. When the audio playout
//! grows (an audio hold building, concealment) the delay shrinks by the same
//! amount and nothing on screen moves; when it shrinks (a re-anchor, a hold
//! released) the floor keeps video where its frames can still be shown. The
//! delay is a lip-sync error traded for a smooth picture.
//!
//! The floor carries the stream's PTS epoch, so it is forgotten whenever the
//! timeline breaks.
//!
//! Before libobs's play head is anchored nothing has been shown, so a
//! shortfall raises the floor at once, measured only on video that is live
//! rather than a relay's catch-up ([`ArrivalFloor`]). Afterwards a raise moves
//! the picture, so it takes a shortfall that recurs across a window.
//!
//! The pre-anchor reading is one frame and can be a startup burst, so the
//! floor also comes back down: when a whole window of frames needed less by
//! more than a threshold, it slews to what the window needed plus a tick, at
//! the Catch-Up Speed the caller passes. Only the part of the floor above the
//! offset is on screen, so only that part slews; the rest goes at once. A
//! raise cancels a slew.
//!
//! The bar is "not late", not "a full delivery lead early": the lead covers
//! the pacing timer oversleeping, and a frame handed over on arrival never
//! sleeps.

use crate::arrival::ArrivalFloor;
use crate::consts;
use crate::window::{Extreme, WindowExtreme};

/// One raise of the floor, for the caller to log, as the delay against the
/// audio offset in force.
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

/// The floor turned out higher than a whole window of frames needed, and
/// starts slewing down. As delays against the audio offset in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelayRelax {
    /// The delay in force.
    pub from_ns: u64,
    /// Where the slew ends.
    pub to_ns: u64,
}

/// One step of the slew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RampStep {
    /// The delay now.
    pub delay_ns: u64,
    /// The slew reached its target.
    pub done: bool,
}

/// Late frames seen since the play head was anchored, awaiting a verdict.
#[derive(Debug, Clone, Copy)]
struct Window {
    since_ns: u64,
    last_ns: u64,
    frames: u32,
    /// The highest floor any frame in the window needed.
    worst_ns: i64,
}

/// The floor, the observation windows behind it and the liveness check that
/// gates the pre-anchor reading.
///
/// Every `offset_ns` parameter is the frame's undelayed due time minus its
/// PTS: the audio playout offset when the frame is mapped through it, or the
/// video-only fallback's equivalent.
#[derive(Debug)]
pub struct VideoDelay {
    floor_ns: Option<i64>,
    max_ns: u64,
    window_ns: u64,
    min_frames: u32,
    window: Option<Window>,
    /// Relaxing: the observation window, the threshold a surplus must exceed,
    /// the highest floor frames needed across the window, since when frames
    /// have arrived without a gap, and the slew in progress. A zero window
    /// turns relaxing off.
    relax_window_ns: u64,
    relax_min_ns: u64,
    needs: WindowExtreme,
    needs_since_ns: Option<u64>,
    last_arrival_ns: u64,
    ramp_to_ns: Option<i64>,
    ramp_at_ns: u64,
    live: ArrivalFloor,
}

impl VideoDelay {
    /// No floor yet; the delay it implies may grow to `max_ns`, and after the
    /// anchor only when at least `min_frames` frames fall short within
    /// `window_ns`, spread over at least half of it.
    pub fn new(max_ns: u64, window_ns: u64, min_frames: u32) -> Self {
        Self {
            floor_ns: None,
            max_ns,
            window_ns,
            min_frames,
            window: None,
            relax_window_ns: 0,
            relax_min_ns: 0,
            needs: WindowExtreme::new(0, Extreme::Highest),
            needs_since_ns: None,
            last_arrival_ns: 0,
            ramp_to_ns: None,
            ramp_at_ns: 0,
            live: ArrivalFloor::default(),
        }
    }

    /// Let the floor slew back down when every frame across `window_ns`
    /// needed less than it by more than `min_ns`.
    #[cfg(test)]
    #[must_use]
    fn with_relax(mut self, window_ns: u64, min_ns: u64) -> Self {
        self.relax_window_ns = window_ns;
        self.relax_min_ns = min_ns;
        self.needs = WindowExtreme::new(window_ns, Extreme::Highest);
        self
    }

    /// The evidence for a lower floor is void.
    fn forget_needs(&mut self) {
        self.needs.clear();
        self.needs_since_ns = None;
        self.ramp_to_ns = None;
    }

    /// The delay a frame scheduled at `offset_ns` carries.
    pub fn delay_ns(&self, offset_ns: i64) -> u64 {
        self.floor_ns.map_or(0, |floor_ns| {
            floor_ns
                .saturating_sub(offset_ns)
                .clamp(0, self.max_ns as i64) as u64
        })
    }

    /// The due time of a frame at `pts_ns` whose undelayed due time is
    /// `base_ns`.
    pub fn due_ns(&self, pts_ns: i64, base_ns: u64) -> u64 {
        base_ns.saturating_add(self.delay_ns(base_ns as i64 - pts_ns))
    }

    /// Whether the delay at `offset_ns` has reached its ceiling: a shortfall
    /// past this cannot be scheduled away, and the frames stay late.
    pub fn at_ceiling(&self, offset_ns: i64) -> bool {
        self.delay_ns(offset_ns) >= self.max_ns
    }

    /// Forget the floor and everything measured: a new connection, a cleared
    /// source, or a timeline that broke, since the floor carries the PTS
    /// epoch.
    pub fn reset(&mut self) {
        self.floor_ns = None;
        self.window = None;
        self.forget_needs();
        self.live.reset();
    }

    /// A video packet with `pts_ns` reached the video thread at
    /// `received_ns`, for the liveness check.
    pub fn note_packet(&mut self, received_ns: u64, pts_ns: i64) {
        self.live.note(received_ns, pts_ns);
    }

    /// Whether video arriving now is live rather than a relay's replay still
    /// catching up, which says nothing about the sender.
    pub fn live(&self, now_ns: u64) -> bool {
        self.live.live(now_ns)
    }

    /// Raise the floor so that a frame scheduled at `offset_ns` is due no
    /// earlier than `need_ns` past its PTS, in whole canvas ticks of delay,
    /// if that is more than the delay already is.
    fn raise_to(
        &mut self,
        need_ns: i64,
        offset_ns: i64,
        tick_ns: u64,
        frames: u32,
    ) -> Option<DelayRaise> {
        let shortfall_ns = need_ns.saturating_sub(offset_ns);
        if shortfall_ns <= 0 {
            return None;
        }
        let wanted_ns = round_up_to_tick(shortfall_ns as u64, tick_ns);
        let to_ns = wanted_ns.min(self.max_ns);
        let from_ns = self.delay_ns(offset_ns);
        if to_ns <= from_ns {
            return None;
        }
        self.floor_ns = Some(offset_ns + to_ns as i64);
        self.forget_needs();
        Some(DelayRaise {
            from_ns,
            to_ns,
            frames,
            capped: wanted_ns > self.max_ns,
        })
    }

    /// A frame considered for anchoring the play head: its packet reached
    /// the video thread at `received_ns`. Nothing has been shown yet, so a
    /// frame not in hand a canvas tick before it is due (the decode still to
    /// come) raises the floor at once.
    pub fn before_anchor(
        &mut self,
        received_ns: u64,
        pts_ns: i64,
        offset_ns: i64,
        tick_ns: u64,
    ) -> Option<DelayRaise> {
        self.window = None;
        let need_ns = received_ns as i64 - pts_ns + tick_ns as i64;
        self.raise_to(need_ns, offset_ns, tick_ns, 0)
    }

    /// A frame handed over at `now_ns`, after the anchor. A frame handed over
    /// past its due time is recorded; the floor is raised only once the
    /// window says that recurs.
    pub fn note(
        &mut self,
        now_ns: u64,
        pts_ns: i64,
        offset_ns: i64,
        tick_ns: u64,
    ) -> Option<DelayRaise> {
        let need_ns = now_ns as i64 - pts_ns;
        // An elapsed window is judged on what it holds, before this frame
        // starts the next one.
        let raise = self.expire(now_ns, offset_ns, tick_ns);
        if need_ns > offset_ns + self.delay_ns(offset_ns) as i64 {
            let window = self.window.get_or_insert(Window {
                since_ns: now_ns,
                last_ns: now_ns,
                frames: 0,
                worst_ns: i64::MIN,
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
    pub fn expire(&mut self, now_ns: u64, offset_ns: i64, tick_ns: u64) -> Option<DelayRaise> {
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
        self.raise_to(window.worst_ns, offset_ns, tick_ns, window.frames)
    }

    /// A frame handed over at `now_ns` after the anchor, whose packet reached
    /// the video thread at `received_ns`. Records the floor it needed (a
    /// canvas tick in hand); once every frame across a whole relax window,
    /// arriving without a gap, needed less than the floor by more than the
    /// threshold, starts a slew down to what they needed plus a tick.
    ///
    /// The window slides, so a sender that recovers is noticed one window
    /// after it did. The audio hold releases on the same kind of window, and
    /// the floor must have come down by then: a hold released over a stale
    /// floor would put the picture behind the sound for no reason.
    pub fn note_arrival(
        &mut self,
        now_ns: u64,
        received_ns: u64,
        pts_ns: i64,
        offset_ns: i64,
        tick_ns: u64,
    ) -> Option<DelayRelax> {
        if self.relax_window_ns == 0 {
            return None;
        }
        let tick = tick_ns as i64;
        let need_ns = received_ns as i64 - pts_ns + tick;
        // A slew must not run past what a frame in hand needs.
        if let (Some(to_ns), Some(floor_ns)) = (self.ramp_to_ns, self.floor_ns) {
            let keep_ns = need_ns + tick;
            if keep_ns >= floor_ns {
                self.ramp_to_ns = None;
            } else if keep_ns > to_ns {
                self.ramp_to_ns = Some(keep_ns);
            }
        }
        // Video that stopped for longer than a raise window is not a window
        // of frames that needed less.
        if now_ns.saturating_sub(self.last_arrival_ns) > self.window_ns {
            self.needs.clear();
            self.needs_since_ns = None;
        }
        self.last_arrival_ns = now_ns;
        let since_ns = *self.needs_since_ns.get_or_insert(now_ns);
        self.needs.push(now_ns, need_ns);
        if now_ns.saturating_sub(since_ns) < self.relax_window_ns || self.ramp_to_ns.is_some() {
            return None;
        }
        let floor_ns = self.floor_ns?;
        let to_ns = self.needs.value()? + tick;
        if floor_ns <= to_ns.saturating_add(self.relax_min_ns as i64) {
            return None;
        }
        self.ramp_to_ns = Some(to_ns);
        self.ramp_at_ns = now_ns;
        let to_delay_ns = to_ns.saturating_sub(offset_ns).clamp(0, self.max_ns as i64) as u64;
        Some(DelayRelax {
            from_ns: self.delay_ns(offset_ns),
            to_ns: to_delay_ns,
        })
    }

    /// Advance a slew in progress to `now_ns`, moving the delay a frame at
    /// `offset_ns` carries down by `rate` of the time since the last step
    /// (0.05 plays video 5% fast).
    pub fn ramp(&mut self, now_ns: u64, rate: f64, offset_ns: i64) -> Option<RampStep> {
        let to_ns = self.ramp_to_ns?;
        let Some(floor_ns) = self.floor_ns else {
            self.ramp_to_ns = None;
            return None;
        };
        let elapsed_ns = now_ns.saturating_sub(self.ramp_at_ns);
        self.ramp_at_ns = now_ns;
        let before_ns = self.delay_ns(offset_ns);
        // Above the ceiling and below the offset the floor is not on screen,
        // so only the part between them slews.
        let start_ns = floor_ns.min(offset_ns.saturating_add(self.max_ns as i64));
        let mut next_ns = if to_ns >= start_ns {
            to_ns
        } else {
            let step_ns = (elapsed_ns as f64 * rate.max(0.0)) as i64;
            (start_ns - step_ns).max(to_ns)
        };
        if next_ns <= offset_ns {
            next_ns = to_ns;
        }
        self.floor_ns = Some(next_ns);
        let done = next_ns <= to_ns;
        if done {
            self.ramp_to_ns = None;
        }
        let delay_ns = self.delay_ns(offset_ns);
        (delay_ns < before_ns || done).then_some(RampStep { delay_ns, done })
    }
}

impl Default for VideoDelay {
    /// The plugin's floor, from the `VIDEO_DELAY_*` constants, relaxing on.
    fn default() -> Self {
        let relax_window_ns = consts::VIDEO_DELAY_RELAX_WINDOW_MS * 1_000_000;
        Self {
            relax_window_ns,
            relax_min_ns: consts::VIDEO_DELAY_RELAX_MIN_MS * 1_000_000,
            needs: WindowExtreme::new(relax_window_ns, Extreme::Highest),
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
    /// A playout offset: stream PTS plus this is the OBS time the audio of it
    /// plays at.
    const O: i64 = -7_000_000_000;
    /// A frame's PTS, and the OBS time it is due at with no delay.
    const PTS: i64 = 10_000_000_000;
    const DUE: u64 = (PTS + O) as u64;

    fn delay() -> VideoDelay {
        VideoDelay::new(MAX, WINDOW, 3)
    }

    /// A frame at `PTS` reaching the thread `margin_ns` before its undelayed
    /// due time, considered for the anchor at offset `O`.
    fn before_anchor(d: &mut VideoDelay, margin_ns: i64) -> Option<DelayRaise> {
        d.before_anchor((DUE as i64 - margin_ns) as u64, PTS, O, TICK)
    }

    /// The frame `i` frames after `PTS`, handed over at `now_ns` against
    /// offset `O`.
    fn note(d: &mut VideoDelay, now_ns: u64, i: u64) -> Option<DelayRaise> {
        d.note(now_ns, PTS + (i * 33_333_333) as i64, O, TICK)
    }

    /// The frame `i` frames after `PTS`, handed over `late_ns` after it is
    /// due (negative: early). Returns the raise and the hand-over time.
    fn note_late(d: &mut VideoDelay, i: u64, late_ns: i64) -> (Option<DelayRaise>, u64) {
        let at = (due_of(d, i) as i64 + late_ns) as u64;
        (note(d, at, i), at)
    }

    /// When the frame `i` frames after `PTS` is due at offset `O`, delay
    /// included.
    fn due_of(d: &VideoDelay, i: u64) -> u64 {
        let pts = PTS + (i * 33_333_333) as i64;
        d.due_ns(pts, (pts + O) as u64)
    }

    #[test]
    fn a_frame_in_hand_early_enough_needs_no_delay() {
        let mut d = delay();
        assert_eq!(before_anchor(&mut d, TICK as i64), None);
        assert_eq!(before_anchor(&mut d, 500_000_000), None);
        assert_eq!(d.delay_ns(O), 0);
        assert_eq!(due_of(&d, 0), DUE);
    }

    #[test]
    fn a_frame_with_no_margin_gets_the_wanted_allowance_before_the_anchor() {
        let mut d = delay();
        let raise = before_anchor(&mut d, 0).expect("raised");
        assert_eq!(raise.from_ns, 0);
        assert_eq!(raise.to_ns, TICK);
        assert!(!raise.capped);
        assert_eq!(d.delay_ns(O), TICK);
        assert_eq!(due_of(&d, 0), DUE + TICK);
    }

    #[test]
    fn a_late_frame_gets_its_lateness_plus_the_allowance_in_whole_ticks() {
        let mut d = delay();
        // 150 ms late plus a 16.7 ms allowance is 166.7 ms: exactly 10 ticks.
        let raise = before_anchor(&mut d, -150_000_000).expect("raised");
        assert_eq!(raise.to_ns, 10 * TICK);
        // 151 ms late needs an eleventh.
        let mut d = delay();
        let raise = before_anchor(&mut d, -151_000_000).expect("raised");
        assert_eq!(raise.to_ns, 11 * TICK);
    }

    #[test]
    fn the_floor_only_ever_rises_before_the_anchor() {
        let mut d = delay();
        before_anchor(&mut d, -100_000_000).expect("raised");
        let first = d.delay_ns(O);
        assert_eq!(
            before_anchor(&mut d, 100_000_000),
            None,
            "a smaller shortfall must not lower the floor"
        );
        assert_eq!(d.delay_ns(O), first);
        let more = before_anchor(&mut d, -200_000_000).expect("raised");
        assert_eq!(more.from_ns, first);
        assert!(more.to_ns > first);
    }

    #[test]
    fn a_frame_the_floor_already_covers_raises_nothing() {
        let mut d = delay();
        before_anchor(&mut d, 0).expect("raised to a tick");
        // The same frame again is in hand a tick before its new due time.
        assert_eq!(before_anchor(&mut d, 0), None);
        assert_eq!(d.delay_ns(O), TICK);
    }

    #[test]
    fn the_ceiling_caps_the_delay_and_says_so() {
        let mut d = delay();
        assert!(!d.at_ceiling(O));
        let raise = before_anchor(&mut d, -2_000_000_000).expect("raised");
        assert_eq!(raise.to_ns, MAX);
        assert!(raise.capped);
        assert!(d.at_ceiling(O));
        // Still short at the ceiling: nothing more to do, nothing to report.
        assert_eq!(before_anchor(&mut d, -3_000_000_000), None);
        assert!(d.at_ceiling(O));
    }

    #[test]
    fn the_delay_is_derived_from_the_offset_in_force() {
        let mut d = delay();
        before_anchor(&mut d, -300_000_000).expect("raised");
        let set = d.delay_ns(O);
        assert_eq!(set, 19 * TICK);
        // The audio playout grows by 100 ms (the audio hold building): the
        // delay shrinks by as much, and the due time stays where it was.
        let grown = O + 100_000_000;
        assert_eq!(d.delay_ns(grown), set - 100_000_000);
        assert_eq!(
            d.due_ns(PTS, (PTS + grown) as u64),
            d.due_ns(PTS, (PTS + O) as u64)
        );
        // Grown past the floor: no delay, and video follows the audio.
        let past = O + set as i64 + 50_000_000;
        assert_eq!(d.delay_ns(past), 0);
        assert_eq!(d.due_ns(PTS, (PTS + past) as u64), (PTS + past) as u64);
        // And back below it (a re-anchor): the floor holds video back again.
        assert_eq!(d.delay_ns(O), set);
        // The ceiling applies to the derived delay, wherever the offset is.
        assert_eq!(d.delay_ns(O - 5_000_000_000), MAX);
        assert!(d.at_ceiling(O - 5_000_000_000));
        assert!(!d.at_ceiling(O));
    }

    #[test]
    fn a_raise_is_sized_against_the_offset_in_force() {
        let mut d = delay();
        before_anchor(&mut d, -300_000_000).expect("raised");
        // The playout has since grown by 250 ms, so a frame as late as the
        // first only needs what is left, and raises nothing.
        let grown = O + 250_000_000;
        let received = (DUE as i64 + 300_000_000) as u64;
        assert_eq!(d.before_anchor(received, PTS, grown, TICK), None);
        // One 100 ms later than that does, to a whole number of ticks
        // against the grown offset.
        let raise = d
            .before_anchor(received + 100_000_000, PTS, grown, TICK)
            .expect("raised");
        assert_eq!(raise.to_ns, (150_000_000 + TICK).div_ceil(TICK) * TICK);
        assert_eq!(d.delay_ns(grown), raise.to_ns);
    }

    #[test]
    fn reset_forgets_the_floor_and_the_liveness_history() {
        let mut d = delay();
        for i in 0..20u64 {
            d.note_packet(i * 33_333_333, (i * 33_333_333) as i64);
        }
        assert!(d.live(20 * 33_333_333));
        before_anchor(&mut d, -150_000_000).expect("raised");
        d.reset();
        assert_eq!(d.delay_ns(O), 0);
        assert_eq!(d.delay_ns(i64::MIN), 0);
        assert!(!d.at_ceiling(O));
        assert!(!d.live(20 * 33_333_333));
        assert_eq!(d.expire(u64::MAX, O, TICK), None);
    }

    #[test]
    fn after_the_anchor_a_frame_handed_over_before_it_is_due_is_not_short() {
        let mut d = delay();
        // In hand with anything from a nanosecond to a full lead to spare.
        for (i, early) in [1, 5_000_000, 20_000_000, 33_333_334]
            .into_iter()
            .enumerate()
        {
            let i = i as u64 * 9;
            assert_eq!(note_late(&mut d, i, -early).0, None);
        }
        assert_eq!(d.expire(u64::MAX, O, TICK), None);
        assert_eq!(d.delay_ns(O), 0);
    }

    #[test]
    fn after_the_anchor_one_late_frame_does_not_raise_the_floor() {
        let mut d = delay();
        let (raise, at) = note_late(&mut d, 0, 5_000_000);
        assert_eq!(raise, None);
        assert_eq!(d.expire(at + WINDOW, O, TICK), None);
        assert_eq!(d.delay_ns(O), 0);
    }

    #[test]
    fn after_the_anchor_a_burst_of_late_frames_does_not_raise_the_floor() {
        let mut d = delay();
        // Six frames late within 200 ms: one hiccup, not a standing shortfall.
        let mut at = 0;
        for i in 0..6 {
            let (raise, t) = note_late(&mut d, i, 5_000_000);
            assert_eq!(raise, None);
            at = t;
        }
        assert_eq!(d.expire(at + WINDOW, O, TICK), None);
        assert_eq!(d.delay_ns(O), 0);
    }

    #[test]
    fn after_the_anchor_a_recurring_shortfall_raises_the_floor_to_its_worst() {
        let mut d = delay();
        // Three frames spread across the window, 5, 20 and 1 ms late: the
        // worst needs 20 ms, which is two ticks.
        let (raise, t0) = note_late(&mut d, 0, 5_000_000);
        assert_eq!(raise, None);
        assert_eq!(note_late(&mut d, 18, 20_000_000).0, None);
        assert_eq!(
            note_late(&mut d, 27, 1_000_000).0,
            None,
            "the window has not run its course"
        );
        let raise = d
            .expire(t0 + WINDOW, O, TICK)
            .expect("raised at the window's end");
        assert_eq!(raise.to_ns, 2 * TICK);
        assert_eq!(raise.frames, 3);
        assert_eq!(d.delay_ns(O), 2 * TICK);
    }

    #[test]
    fn a_late_frame_after_the_window_judges_it_and_starts_the_next() {
        let mut d = delay();
        let t0 = due_of(&d, 0) + 5_000_000;
        for i in 0..3 {
            let k = i * 12;
            assert_eq!(note_late(&mut d, k, 5_000_000).0, None);
        }
        // The fourth frame is handed over after the window elapsed: the raise
        // comes out of this call, judged on the three before it.
        let t = t0 + WINDOW + 1;
        let k = 30;
        let late_by = t as i64 - due_of(&d, k) as i64;
        assert!((0..TICK as i64).contains(&late_by));
        let raise = note(&mut d, t, k).expect("raised");
        assert_eq!(raise.frames, 3);
        assert_eq!(raise.to_ns, TICK);
        // The fourth frame needed under a tick too, and the raise just
        // provided it: it does not start a new window.
        assert_eq!(d.expire(t + WINDOW, O, TICK), None);
    }

    #[test]
    fn frames_the_floor_covers_are_not_counted() {
        let mut d = delay();
        before_anchor(&mut d, 0).expect("raised to a tick");
        for i in 0..5 {
            let k = i * 9;
            // Handed over exactly at the delayed due time: on time.
            assert_eq!(note_late(&mut d, k, 0).0, None);
        }
        assert_eq!(d.expire(u64::MAX, O, TICK), None);
        assert_eq!(d.delay_ns(O), TICK);
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

    /// Feed `secs` of 30fps frames from `t0` that each reach the thread
    /// `margin_ns` before they would be due at offset `O`, with the playout
    /// offset at `offset(now)`, stepping the slew at 5% as the caller does.
    /// Returns the relax decisions and the delay after every frame.
    fn run_with(
        d: &mut VideoDelay,
        t0: u64,
        secs: u64,
        margin_ns: i64,
        offset: impl Fn(u64) -> i64,
    ) -> (Vec<DelayRelax>, Vec<u64>) {
        let mut relaxes = Vec::new();
        let mut delays = Vec::new();
        for i in 0..secs * 30 {
            let now = t0 + i * 33_333_333;
            let offset_ns = offset(now);
            d.ramp(now, 0.05, offset_ns);
            let pts = now as i64 + margin_ns - O;
            relaxes.extend(d.note_arrival(now, now, pts, offset_ns, TICK));
            delays.push(d.delay_ns(offset_ns));
        }
        (relaxes, delays)
    }

    fn run(d: &mut VideoDelay, t0: u64, secs: u64, margin_ns: i64) -> Vec<DelayRelax> {
        run_with(d, t0, secs, margin_ns, |_| O).0
    }

    #[test]
    fn a_floor_set_by_a_startup_transient_slews_back_down() {
        let mut d = relaxing();
        // One frame 1.3 s late at the anchor, on a stream whose frames then
        // arrive 100 ms early for good.
        before_anchor(&mut d, -1_300_000_000).expect("raised");
        let set = d.delay_ns(O);
        let (relaxes, delays) = run_with(&mut d, 1_000_000_000, 60, 100_000_000, |_| O);
        assert_eq!(relaxes.len(), 1, "one decision, not one per window");
        assert_eq!(relaxes[0].from_ns, set);
        assert_eq!(
            relaxes[0].to_ns, 0,
            "a tick of allowance and a tick of headroom fit in 100 ms"
        );
        assert_eq!(d.delay_ns(O), 0);
        assert!(delays.windows(2).all(|w| w[1] <= w[0]), "only ever down");
    }

    #[test]
    fn the_slew_is_gradual() {
        let mut d = relaxing();
        before_anchor(&mut d, -1_300_000_000).expect("raised");
        let set = d.delay_ns(O);
        // Eleven seconds: the window closes at ten, then one second of slew
        // at 5% takes back about 50 ms, not the whole delay.
        run(&mut d, 1_000_000_000, 11, 100_000_000);
        let taken = set - d.delay_ns(O);
        assert!((30_000_000..70_000_000).contains(&taken), "took {taken}ns");
    }

    #[test]
    fn a_floor_the_sender_really_needs_stays() {
        let mut d = relaxing();
        before_anchor(&mut d, -150_000_000).expect("raised");
        let set = d.delay_ns(O);
        // Every frame keeps arriving 150 ms late.
        assert!(run(&mut d, 1_000_000_000, 60, -150_000_000).is_empty());
        assert_eq!(d.delay_ns(O), set);
    }

    #[test]
    fn one_late_frame_in_the_window_keeps_the_floor_it_needed() {
        let mut d = relaxing();
        before_anchor(&mut d, -1_300_000_000).expect("raised");
        let t0 = 1_000_000_000;
        run(&mut d, t0, 5, 100_000_000);
        // Mid-window, one frame 400 ms late.
        let now = t0 + 5_000_000_000;
        let pts = now as i64 - 400_000_000 - O;
        d.note_arrival(now, now, pts, O, TICK);
        let relaxes = run(&mut d, t0 + 5_033_333_333, 60, 100_000_000);
        // 400 ms plus the tick allowance, plus a tick of headroom.
        assert_eq!(relaxes[0].to_ns, 400_000_000 + 2 * TICK);
    }

    #[test]
    fn a_raise_cancels_the_slew() {
        let mut d = relaxing();
        before_anchor(&mut d, -1_300_000_000).expect("raised");
        run(&mut d, 1_000_000_000, 12, 100_000_000);
        let raise = before_anchor(&mut d, -2_000_000_000 + MAX as i64 / 2);
        assert!(raise.is_some());
        let after = d.delay_ns(O);
        assert_eq!(d.ramp(20_000_000_000, 0.05, O), None);
        assert_eq!(d.delay_ns(O), after);
    }

    #[test]
    fn a_floor_below_the_offset_relaxes_at_once() {
        let mut d = relaxing();
        before_anchor(&mut d, -300_000_000).expect("raised");
        // The audio hold has grown the playout past the floor, so nothing
        // is delayed; the sender then recovers to 100 ms late against `O`.
        let grown = O + 500_000_000;
        assert_eq!(d.delay_ns(grown), 0);
        let (relaxes, delays) = run_with(&mut d, 1_000_000_000, 11, -100_000_000, |_| grown);
        assert_eq!(relaxes.len(), 1);
        assert_eq!(relaxes[0].from_ns, 0, "nothing was on screen");
        assert!(delays.iter().all(|&d| d == 0));
        // The floor went straight to what frames need: should the hold be
        // released, the delay is what these frames need, not what the first
        // one did.
        assert_eq!(d.delay_ns(O), 100_000_000 + 2 * TICK);
    }

    #[test]
    fn only_the_part_of_the_floor_above_the_offset_slews() {
        let mut d = relaxing();
        before_anchor(&mut d, -1_300_000_000).expect("raised");
        // Halfway through the slew the playout grows past what is left.
        let t0 = 1_000_000_000;
        let ramp_from = t0 + RELAX;
        let (_, delays) = run_with(&mut d, t0, 20, 100_000_000, |now| {
            if now < ramp_from + 2_000_000_000 {
                O
            } else {
                O + 1_400_000_000
            }
        });
        assert_eq!(*delays.last().unwrap(), 0);
        // Back at the old offset nothing reappears: the floor went to what
        // frames need as soon as it was out of sight.
        assert_eq!(d.delay_ns(O), 0);
    }

    #[test]
    fn without_with_relax_the_floor_never_drops() {
        let mut d = delay();
        before_anchor(&mut d, -1_300_000_000).expect("raised");
        let set = d.delay_ns(O);
        assert!(run(&mut d, 1_000_000_000, 60, 100_000_000).is_empty());
        assert_eq!(d.delay_ns(O), set);
    }
}
