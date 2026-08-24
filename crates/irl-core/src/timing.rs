//! Output-clock arithmetic (port of `receiver-audio.c:267-329`).

use crate::consts;
use crate::rescale;

/// `anchor + samples / rate` in nanoseconds: the pure sample-counter clock.
///
/// Ports `audio_output_next_ts`. The whole audio contract rests on this being
/// a counter and not a clock read: OBS wants `ts[n+1] = ts[n] + frames/rate`
/// exactly, so the timestamp is always re-derived from the anchor and the
/// running sample count rather than accumulated.
pub fn output_next_ts(anchor_ns: u64, samples: u64, rate: u32) -> u64 {
    if rate == 0 {
        return anchor_ns;
    }
    let offset = rescale::rescale_near(samples as i64, 1_000_000_000, rate as i64);
    anchor_ns.wrapping_add(offset as u64)
}

/// Lead kept ahead of wall clock: `max(AUDIO_OUT_LEAD_MS, 3 chunks)`, or one
/// chunk in low-latency mode.
///
/// Ports `audio_output_lead_ns`. The 80 ms floor has to cover the plugin's own
/// delivery jitter (1 ms pump sleep plus scheduling) and one OBS mix tick
/// (21.3 ms), with margin.
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
///
/// Ports `audio_expected_samples`.
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
/// Ports `audio_soft_compensation_samples`. Real discontinuities are PTS
/// repair's job; this only takes out tiny per-frame drift, in the spirit of a
/// bounded `aresample` async correction.
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
///
/// Ports the prime gate in `irl_pump_audio_once` (`receiver-audio.c:776-783`).
pub fn prime_threshold_ms(target_ms: i32, lead_ns: u64, low_latency: bool) -> i32 {
    if low_latency {
        return 0;
    }
    target_ms + (lead_ns / 1_000_000) as i32
}

/// Nanoseconds for `frames` at `rate`, truncating (the C `chunk_ns` /
/// `stream_duration_ns` form: plain integer division, not `av_rescale`).
pub fn frames_to_ns(frames: u64, rate: u32) -> u64 {
    if rate == 0 {
        return 0;
    }
    (frames as u128 * 1_000_000_000 / rate as u128) as u64
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
}
