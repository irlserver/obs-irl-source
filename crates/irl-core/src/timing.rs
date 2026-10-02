//! Output-clock arithmetic.

use crate::consts;
use crate::rescale;

/// `anchor + samples / rate` in nanoseconds: the pure sample-counter clock.
///
/// The whole audio contract rests on this being a counter and not a clock
/// read: OBS wants `ts[n+1] = ts[n] + frames/rate`
/// exactly, so the timestamp is always re-derived from the anchor and the
/// running sample count rather than accumulated.
pub fn output_next_ts(anchor_ns: u64, samples: u64, rate: u32) -> u64 {
    if rate == 0 {
        return anchor_ns;
    }
    let offset = rescale::rescale_near(samples as i64, 1_000_000_000, rate as i64);
    anchor_ns.wrapping_add(offset as u64)
}

/// The audio output clock: [`output_next_ts`] over an anchor set when playback
/// primes and a count of the samples claimed since.
///
/// The anchor only moves through [`Self::restart`] and [`Self::stand_down`],
/// which are the declared restarts; every other timestamp is a claim on the
/// counter, so consecutive submissions stay contiguous.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OutputClock {
    primed: bool,
    anchor_ns: u64,
    samples: u64,
}

impl OutputClock {
    /// Whether a clock line is running.
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Samples claimed since the clock line started.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Start a new clock line whose first sample plays at `at_ns`.
    pub fn restart(&mut self, at_ns: u64) {
        self.primed = true;
        self.anchor_ns = at_ns;
        self.samples = 0;
    }

    /// Stop the clock line until the next [`Self::restart`].
    pub fn stand_down(&mut self) {
        *self = Self::default();
    }

    /// The timestamp the next claimed sample will carry.
    pub fn next_ts(&self, rate: u32) -> u64 {
        output_next_ts(self.anchor_ns, self.samples, rate)
    }

    /// Reserve `frames` and return the timestamp of the first of them.
    pub fn claim(&mut self, frames: u32, rate: u32) -> u64 {
        let ts = self.next_ts(rate);
        self.samples += u64::from(frames);
        ts
    }
}

/// Lead kept ahead of wall clock: `max(AUDIO_OUT_LEAD_MS, 3 chunks)`, or one
/// chunk in low-latency mode.
///
/// The 80 ms floor has to cover the plugin's own delivery jitter (1 ms pump
/// sleep plus scheduling) and one OBS mix tick (21.3 ms), with margin.
pub fn output_lead_ns(chunk_samples: i32, rate: i32, low_latency: bool) -> u64 {
    if rate <= 0 || chunk_samples <= 0 {
        return if low_latency {
            0
        } else {
            consts::AUDIO_OUT_LEAD_MS as u64 * 1_000_000
        };
    }
    let chunk_ns = chunk_samples as u64 * 1_000_000_000 / rate as u64;
    if low_latency {
        return chunk_ns;
    }
    let lead = consts::AUDIO_OUT_LEAD_MS as u64 * 1_000_000;
    if lead < chunk_ns * 3 {
        chunk_ns * 3
    } else {
        lead
    }
}

/// Samples a packet of `duration` (stream time base) should contain at
/// `rate`; `fallback` when the duration is unusable.
pub fn expected_samples(duration: i64, tb_num: i32, tb_den: i32, rate: i32, fallback: i32) -> i32 {
    if duration <= 0 || rate <= 0 || tb_den <= 0 {
        return fallback;
    }
    let expected = rescale::rescale_q_near(duration, tb_num as i64, tb_den as i64, 1, rate as i64);
    if expected <= 0 || expected > i32::MAX as i64 {
        return fallback;
    }
    expected as i32
}

/// `expected − actual` clamped to ±`AUDIO_SOFT_COMPENSATION_MAX_SAMPLES`,
/// zero outside that window.
///
/// Real discontinuities are PTS repair's job; this only takes out tiny
/// per-frame drift, in the spirit of a bounded `aresample` async correction.
pub fn soft_compensation_samples(expected: i32, actual: i32) -> i32 {
    let delta = expected - actual;
    let window =
        -consts::AUDIO_SOFT_COMPENSATION_MAX_SAMPLES..=consts::AUDIO_SOFT_COMPENSATION_MAX_SAMPLES;
    if !window.contains(&delta) {
        return 0;
    }
    delta
}

/// Fill required before priming: `target + lead`, or 0 in low-latency mode.
pub fn prime_threshold_ms(target_ms: i32, lead_ns: u64, low_latency: bool) -> i32 {
    if low_latency {
        return 0;
    }
    target_ms + (lead_ns / 1_000_000) as i32
}

/// Frames to read this cycle so the buffer's reachable levels straddle the
/// target evenly instead of landing wherever priming happened to leave them.
///
/// Reads and writes are both whole decoded chunks, so the level can only be
/// `phase + k · chunk` (21.3 ms steps for 1024-sample AAC) and dithers between
/// the two grid points either side of the target, with `phase` set by accident
/// at priming. The average cushion can then be up to a chunk below the target
/// (see `docs/audio-timing-pitfalls.md`).
///
/// One read of a different size moves `phase` for good. Centring the pair on
/// the target (levels at `target ± chunk/2`) makes the average cushion the
/// configured one. Putting a grid point *on* the target would be worse: the
/// pair becomes `target` and `target − chunk`, half a chunk low on average.
///
/// Nothing is skipped or duplicated, and it is a phase correction, not a rate
/// one, so it cannot fight the speed controller.
///
/// Returns `base_frames` when the grid is already centred, otherwise a size
/// within half a chunk of a normal read. The caller must have that many frames
/// buffered, which after priming it does by construction.
pub fn aligning_read_frames(fill_frames: i64, target_frames: i64, base_frames: i32) -> i32 {
    if base_frames <= 0 || fill_frames <= 0 {
        return base_frames;
    }
    let base = base_frames as i64;
    // After a read of `r` the level is `fill - r` and the grid it then walks is
    // everything congruent to that mod `base`. We want that grid to pass half a
    // chunk above the target, so the pair straddles it: `fill - r == target +
    // base/2 (mod base)`, hence `r == fill - target - base/2 (mod base)`.
    let delta = (fill_frames - target_frames - base / 2).rem_euclid(base);
    if delta == 0 {
        return base_frames;
    }
    // `delta` and `delta + base` both satisfy that. Take whichever is nearer a
    // normal chunk, so the one odd read stays within half a chunk of the usual
    // size rather than being a stub.
    let frames = if delta * 2 >= base {
        delta
    } else {
        delta + base
    };
    frames as i32
}

/// Nanoseconds for `frames` at `rate`, truncating (plain integer division,
/// not `av_rescale`).
pub fn frames_to_ns(frames: u64, rate: u32) -> u64 {
    if rate == 0 {
        return 0;
    }
    (frames as u128 * 1_000_000_000 / rate as u128) as u64
}

/// Rate limit: true, and `last` moves to `now`, unless the previous pass was
/// less than `interval` ago. A zero `last` has never passed, so the first call
/// always does. Units are the caller's, as long as all three agree.
///
/// The difference wraps: `av_gettime` is wall clock, and a step backwards must
/// not suppress a decoder flush until the clock has caught up again.
pub fn throttle(last: &mut u64, now: u64, interval: u64) -> bool {
    if *last != 0 && now.wrapping_sub(*last) < interval {
        return false;
    }
    *last = now;
    true
}

#[cfg(test)]
mod alignment_tests {
    use super::*;

    /// 1024-sample AAC frames at 48 kHz: the case the quantisation note in
    /// `docs/audio-timing-pitfalls.md` is written about.
    const BASE: i32 = 1024;

    fn ms(v: f64) -> i64 {
        (v * 48.0) as i64
    }

    /// The property the whole function exists for: after one aligning read the
    /// reachable levels sit half a chunk either side of the target, so the
    /// dither averages to the cushion the user configured.
    fn straddles_target(fill: i64, target: i64, base: i32) -> bool {
        let read = aligning_read_frames(fill, target, base) as i64;
        (fill - read - target).rem_euclid(base as i64) == base as i64 / 2
    }

    #[test]
    fn one_read_centres_the_dither_on_the_target() {
        // Primed at 213ms against a 120ms target, the level dithers between
        // 106ms and 128ms, so the average cushion runs short.
        assert!(straddles_target(ms(213.0), ms(120.0), BASE));
        // And from wherever else priming happens to land.
        for fill_ms in [100, 121, 150, 200, 213, 400, 874, 8000] {
            assert!(
                straddles_target(ms(fill_ms as f64), ms(120.0), BASE),
                "fill {fill_ms}ms"
            );
        }
    }

    #[test]
    fn putting_a_grid_point_on_the_target_would_be_worse() {
        // Pins the reasoning, because "align it exactly" is the obvious wrong
        // answer: the pair becomes target and target - chunk, so the average
        // cushion sits half a chunk low instead of on the target.
        let (target, base) = (ms(120.0), BASE as i64);
        let read = aligning_read_frames(ms(213.0), target, BASE) as i64;
        let after = ms(213.0) - read;
        assert_ne!(
            (after - target).rem_euclid(base),
            0,
            "grid landed on target"
        );
        // The two reachable levels either side of the target are equidistant.
        let above = (after - target).rem_euclid(base);
        assert_eq!(above, base - above);
    }

    #[test]
    fn it_holds_for_every_target_and_both_common_frame_sizes() {
        // AAC 1024 and Opus 960, across the whole Target Buffer slider.
        for base in [960, 1024] {
            for target_ms in [20, 40, 120, 500, 2000, 8000] {
                let target = ms(target_ms as f64);
                for extra in 0..base as i64 {
                    let fill = target + 4 * base as i64 + extra;
                    assert!(
                        straddles_target(fill, target, base),
                        "base {base} target {target_ms}ms extra {extra}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_odd_read_stays_within_half_a_chunk_of_a_normal_one() {
        // A stub of a chunk would be emitted to OBS as a 3-sample buffer; the
        // clock stays contiguous either way, but there is no reason to.
        for extra in 0..BASE as i64 {
            let read = aligning_read_frames(ms(120.0) + 4 * BASE as i64 + extra, ms(120.0), BASE);
            assert!(
                (BASE / 2..=BASE * 3 / 2).contains(&read),
                "extra {extra} gave a {read}-frame read"
            );
        }
    }

    #[test]
    fn an_already_centred_grid_is_left_alone() {
        // No correction means no odd chunk, which is the steady state: this
        // runs once per priming, not once per cycle.
        let target = ms(120.0);
        let centred = target + BASE as i64 / 2 + 4 * BASE as i64;
        assert_eq!(aligning_read_frames(centred, target, BASE), BASE);
    }

    #[test]
    fn degenerate_inputs_fall_back_to_a_normal_read() {
        assert_eq!(aligning_read_frames(0, 5760, BASE), BASE);
        assert_eq!(aligning_read_frames(-1, 5760, BASE), BASE);
        assert_eq!(aligning_read_frames(10_000, 5760, 0), 0);
        assert_eq!(aligning_read_frames(10_000, 5760, -1), -1);
    }

    #[test]
    fn a_target_above_the_fill_still_centres() {
        // Priming can only happen above the target, but a live retune can drop
        // the level below it, and the modulo has to stay well behaved there.
        assert!(straddles_target(ms(50.0), ms(120.0), BASE));
        assert!(straddles_target(ms(1.0), ms(8000.0), BASE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: i32 = 48_000;
    /// One AAC frame.
    const AAC: i32 = 1024;
    /// One Opus frame.
    const OPUS: i32 = 960;

    #[test]
    fn lead_is_eighty_ms_or_three_chunks() {
        // 3 x 20 ms = 60 ms, so the 80 ms floor wins.
        assert_eq!(output_lead_ns(OPUS, RATE, false), 80_000_000);
        // 3 x 21.3 ms = 64 ms: still the floor.
        assert_eq!(output_lead_ns(AAC, RATE, false), 80_000_000);
        // A 60 ms chunk needs 180 ms of lead.
        assert_eq!(output_lead_ns(RATE * 60 / 1000, RATE, false), 180_000_000);
    }

    #[test]
    fn low_latency_lead_is_one_chunk() {
        assert_eq!(output_lead_ns(OPUS, RATE, true), 20_000_000);
        assert_eq!(output_lead_ns(AAC, RATE, true), 21_333_333);
    }

    #[test]
    fn prime_threshold_is_target_plus_lead() {
        let lead = output_lead_ns(OPUS, RATE, false);
        assert_eq!(prime_threshold_ms(120, lead, false), 200);
        assert_eq!(prime_threshold_ms(120, lead, true), 0);
    }

    #[test]
    fn compensation_is_bounded_at_eight_samples() {
        assert_eq!(soft_compensation_samples(1024, 1024), 0);
        assert_eq!(soft_compensation_samples(1028, 1024), 4);
        assert_eq!(soft_compensation_samples(1020, 1024), -4);
        assert_eq!(soft_compensation_samples(1032, 1024), 8);
        assert_eq!(soft_compensation_samples(1016, 1024), -8);
        // Beyond the window it is a real discontinuity: leave it alone.
        assert_eq!(soft_compensation_samples(1033, 1024), 0);
        assert_eq!(soft_compensation_samples(1015, 1024), 0);
        assert_eq!(soft_compensation_samples(20_000, 1024), 0);
    }

    #[test]
    fn expected_samples_rescales_the_packet_duration() {
        // A 1024-sample packet in a 48 kHz time base.
        assert_eq!(expected_samples(1024, 1, 48_000, 48_000, 960), 1024);
        // The same packet in a 90 kHz time base.
        assert_eq!(expected_samples(1920, 1, 90_000, 48_000, 960), 1024);
        // Unusable inputs fall back.
        assert_eq!(expected_samples(0, 1, 48_000, 48_000, 960), 960);
        assert_eq!(expected_samples(-5, 1, 48_000, 48_000, 960), 960);
        assert_eq!(expected_samples(1024, 1, 0, 48_000, 960), 960);
        assert_eq!(expected_samples(1024, 1, 48_000, 0, 960), 960);
        // A duration that rescales to nothing falls back too.
        assert_eq!(expected_samples(1, 1, 90_000_000, 48_000, 960), 960);
    }

    #[test]
    fn output_clock_is_contiguous_over_ten_thousand_chunks() {
        // The contract: ts[n+1] - ts[n] must equal the duration of chunk n,
        // with no accumulated error, for as long as the connection lives.
        let anchor = 123_456_789_000u64;
        let mut samples = 0u64;
        let mut prev = output_next_ts(anchor, samples, RATE as u32);
        assert_eq!(prev, anchor);

        for i in 0..10_000u64 {
            // Alternate chunk sizes: adaptive speed makes the emitted frame
            // count vary chunk to chunk.
            let frames = if i % 3 == 0 { 1024 } else { 1000 };
            samples += frames;
            let ts = output_next_ts(anchor, samples, RATE as u32);
            assert!(ts > prev);
            // Every timestamp is exactly the anchor plus the running count.
            assert_eq!(ts, anchor + (samples * 1_000_000_000 + 24_000) / 48_000);
            prev = ts;
        }

        // 10 000 chunks in, the clock is still anchored, not drifting.
        let total_ns = (samples * 1_000_000_000 + 24_000) / 48_000;
        assert_eq!(prev, anchor + total_ns);
    }

    #[test]
    fn a_clock_starts_unprimed_and_a_restart_primes_it() {
        let mut clock = OutputClock::default();
        assert!(!clock.is_primed());
        clock.restart(1_000);
        assert!(clock.is_primed());
        assert_eq!(clock.samples(), 0);
        assert_eq!(clock.next_ts(RATE as u32), 1_000);
    }

    #[test]
    fn claims_are_contiguous_on_the_counter() {
        let anchor = 5_000_000_000;
        let mut clock = OutputClock::default();
        clock.restart(anchor);
        let mut samples = 0u64;
        for frames in [1024u32, 1000, 1031, 1024] {
            let ts = clock.claim(frames, RATE as u32);
            assert_eq!(ts, output_next_ts(anchor, samples, RATE as u32));
            samples += u64::from(frames);
        }
        assert_eq!(clock.samples(), samples);
        assert_eq!(
            clock.next_ts(RATE as u32),
            output_next_ts(anchor, samples, RATE as u32)
        );
    }

    #[test]
    fn a_restart_drops_the_claimed_samples_and_moves_the_anchor() {
        let mut clock = OutputClock::default();
        clock.restart(1_000_000_000);
        clock.claim(4800, RATE as u32);
        clock.restart(3_000_000_000);
        assert!(clock.is_primed());
        assert_eq!(clock.samples(), 0);
        assert_eq!(clock.claim(1024, RATE as u32), 3_000_000_000);
    }

    #[test]
    fn standing_down_unprimes_and_forgets_the_line() {
        let mut clock = OutputClock::default();
        clock.restart(1_000_000_000);
        clock.claim(1024, RATE as u32);
        clock.stand_down();
        assert_eq!(clock, OutputClock::default());
        assert!(!clock.is_primed());
    }

    #[test]
    fn frames_to_ns_truncates() {
        assert_eq!(frames_to_ns(48_000, 48_000), 1_000_000_000);
        assert_eq!(frames_to_ns(960, 48_000), 20_000_000);
        assert_eq!(frames_to_ns(1024, 48_000), 21_333_333);
        assert_eq!(frames_to_ns(1024, 0), 0);
    }

    #[test]
    fn degenerate_rates_do_not_divide_by_zero() {
        assert_eq!(output_next_ts(500, 100, 0), 500);
        assert_eq!(output_lead_ns(960, 0, false), 80_000_000);
        assert_eq!(output_lead_ns(960, 0, true), 0);
        assert_eq!(output_lead_ns(0, 48_000, false), 80_000_000);
    }

    #[test]
    fn throttle_passes_first_then_once_per_interval() {
        let mut last = 0;
        assert!(throttle(&mut last, 5, 10));
        assert!(!throttle(&mut last, 14, 10));
        assert_eq!(last, 5);
        assert!(throttle(&mut last, 15, 10));
        assert_eq!(last, 15);
        // A clock stepped back behind the last pass does not hold it shut.
        assert!(throttle(&mut last, 3, 10));
    }
}
