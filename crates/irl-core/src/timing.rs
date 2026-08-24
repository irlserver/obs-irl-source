//! Output-clock arithmetic (port of `receiver-audio.c:267-329`).

/// `anchor + samples / rate` in nanoseconds: the pure sample-counter clock.
pub fn output_next_ts(anchor_ns: u64, samples: u64, rate: u32) -> u64 {
    let _ = (anchor_ns, samples, rate);
    todo!("W1-C")
}

/// Lead kept ahead of wall clock: `max(AUDIO_OUT_LEAD_MS, 3 chunks)`, or one
/// chunk in low-latency mode.
pub fn output_lead_ns(chunk_samples: i32, rate: i32, low_latency: bool) -> u64 {
    let _ = (chunk_samples, rate, low_latency);
    todo!("W1-C")
}

/// Samples a packet of `duration` (stream time base) should contain at
/// `rate`; `fallback` when the duration is unusable.
pub fn expected_samples(duration: i64, tb_num: i32, tb_den: i32, rate: i32, fallback: i32) -> i32 {
    let _ = (duration, tb_num, tb_den, rate, fallback);
    todo!("W1-C")
}

/// `expected − actual` clamped to ±`AUDIO_SOFT_COMPENSATION_MAX_SAMPLES`,
/// zero outside that window.
pub fn soft_compensation_samples(expected: i32, actual: i32) -> i32 {
    let _ = (expected, actual);
    todo!("W1-C")
}

/// Fill required before priming: `target + lead`, or 0 in low-latency mode.
pub fn prime_threshold_ms(target_ms: i32, lead_ns: u64, low_latency: bool) -> i32 {
    let _ = (target_ms, lead_ns, low_latency);
    todo!("W1-C")
}

/// Nanoseconds for `frames` at `rate`.
pub fn frames_to_ns(frames: u64, rate: u32) -> u64 {
    let _ = (frames, rate);
    todo!("W1-C")
}
