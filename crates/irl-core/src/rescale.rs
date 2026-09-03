//! FFmpeg's integer rescaling, reimplemented so `irl-core` stays FFI-free.
//!
//! Ports `av_rescale_rnd` / `av_rescale_q` (`libavutil/mathematics.c`) for the
//! two rounding modes the plugin uses. The arithmetic is done in `i128`, which
//! is what FFmpeg's overflow-avoiding split does with 64-bit halves; the
//! results agree for every value the plugin can produce.

/// `av_rescale_rnd(a, b, c, AV_ROUND_NEAR_INF)`: `a * b / c`, rounding halves
/// away from zero. Returns `i64::MIN` on the invalid inputs FFmpeg rejects.
pub(crate) fn rescale_near(a: i64, b: i64, c: i64) -> i64 {
    if c <= 0 || b < 0 {
        return i64::MIN;
    }
    let (sign, a) = if a < 0 {
        (-1i128, -(a as i128))
    } else {
        (1i128, a as i128)
    };
    let r = (c as i128) / 2;
    let v = sign * ((a * b as i128 + r) / c as i128);
    clamp_i64(v)
}

/// `av_rescale_rnd(a, b, c, AV_ROUND_UP)`: `a * b / c` rounded away from zero
/// for positive `a` (and towards zero for negative `a`, which is how FFmpeg
/// mirrors the mode across the sign).
pub(crate) fn rescale_up(a: i64, b: i64, c: i64) -> i64 {
    if c <= 0 || b < 0 {
        return i64::MIN;
    }
    if a < 0 {
        // FFmpeg flips UP to DOWN for a negative operand (rnd ^ ((rnd >> 1) & 1)).
        return -clamp_i64(((-(a as i128)) * b as i128) / c as i128);
    }
    let v = (a as i128 * b as i128 + (c as i128 - 1)) / c as i128;
    clamp_i64(v)
}

/// `av_rescale_q(a, bq, cq)`: rescale from time base `bn/bd` to `cn/cd`,
/// rounding halves away from zero.
pub(crate) fn rescale_q_near(a: i64, bn: i64, bd: i64, cn: i64, cd: i64) -> i64 {
    rescale_near(a, bn.saturating_mul(cd), cn.saturating_mul(bd))
}

/// `av_rescale_q_rnd(a, bq, cq, AV_ROUND_UP)`.
pub(crate) fn rescale_q_up(a: i64, bn: i64, bd: i64, cn: i64, cd: i64) -> i64 {
    rescale_up(a, bn.saturating_mul(cd), cn.saturating_mul(bd))
}

fn clamp_i64(v: i128) -> i64 {
    if v > i64::MAX as i128 {
        i64::MAX
    } else if v < i64::MIN as i128 {
        i64::MIN
    } else {
        v as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_rounds_halves_away_from_zero() {
        assert_eq!(rescale_near(1, 1, 2), 1);
        assert_eq!(rescale_near(-1, 1, 2), -1);
        assert_eq!(rescale_near(3, 1, 2), 2);
        assert_eq!(rescale_near(1, 1, 3), 0);
    }

    #[test]
    fn up_rounds_away_from_zero() {
        assert_eq!(rescale_up(1, 1, 3), 1);
        assert_eq!(rescale_up(3, 1, 3), 1);
        assert_eq!(rescale_up(4, 1, 3), 2);
        assert_eq!(rescale_up(-4, 1, 3), -1);
    }

    #[test]
    fn q_matches_the_time_base_form() {
        // 90 kHz ticks to milliseconds.
        assert_eq!(rescale_q_near(90_000, 1, 90_000, 1, 1000), 1000);
        // Milliseconds to a 48 kHz time base, rounding up.
        assert_eq!(rescale_q_up(1, 1, 1000, 1, 48_000), 48);
    }

    #[test]
    fn invalid_denominators_return_min() {
        assert_eq!(rescale_near(1, 1, 0), i64::MIN);
        assert_eq!(rescale_up(1, -1, 1), i64::MIN);
    }
}
