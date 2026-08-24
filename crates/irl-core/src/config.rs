//! Settings-derived values.

use crate::consts;

/// The `hw_decode` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwDecode {
    /// Probe the platform's device types in order, fall back to software.
    #[default]
    Auto,
    /// Software decode only.
    Off,
    /// CUDA/NVDEC only; fail the decoder open rather than fall back.
    Nvdec,
}

impl HwDecode {
    /// Stored setting value (`0` Auto, `1` Off, `2` NVDEC).
    pub fn as_i64(self) -> i64 {
        match self {
            Self::Auto => 0,
            Self::Off => 1,
            Self::Nvdec => 2,
        }
    }

    /// Parse a stored value; out of range degrades to `Auto`. `nvdec_available`
    /// is false on platforms without a CUDA build (macOS), where a saved NVDEC
    /// setting also degrades to `Auto`. Returns the value and whether it was
    /// degraded (the C plugin logs a warning in that case).
    pub fn from_i64(value: i64, nvdec_available: bool) -> (Self, bool) {
        match value {
            0 => (Self::Auto, false),
            1 => (Self::Off, false),
            // A scene collection saved on Windows or Linux can carry NVDEC to
            // a platform where CUDA cannot exist; degrade rather than force a
            // device that would leave the source videoless.
            2 if nvdec_available => (Self::Nvdec, false),
            2 => (Self::Auto, true),
            _ => (Self::Auto, true),
        }
    }
}

/// The three buffer watermarks, always published together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watermarks {
    /// Target fill.
    pub target_ms: i32,
    /// Speed controller low watermark.
    pub min_ms: i32,
    /// Fill at which drain speed peaks; also sizes the ring.
    pub max_ms: i32,
}

impl Watermarks {
    /// `min = max(target / 2, 20)`, `max = target + 200`.
    ///
    /// A non-positive target falls back to the default, as `config_load` does.
    pub fn derive(target_ms: i32) -> Self {
        let target_ms = if target_ms <= 0 {
            consts::DEFAULT_BUFFER_TARGET_MS as i32
        } else {
            target_ms
        };
        let mut min_ms = target_ms / consts::BUFFER_MIN_DIVISOR as i32;
        if min_ms < consts::BUFFER_MIN_FLOOR_MS as i32 {
            min_ms = consts::BUFFER_MIN_FLOOR_MS as i32;
        }
        Self {
            target_ms,
            min_ms,
            max_ms: target_ms + consts::BUFFER_MAX_EXTRA_MS as i32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermarks_at_the_default_target() {
        let wm = Watermarks::derive(consts::DEFAULT_BUFFER_TARGET_MS as i32);
        assert_eq!(
            wm,
            Watermarks {
                target_ms: 120,
                min_ms: 60,
                max_ms: 320,
            }
        );
    }

    #[test]
    fn minimum_watermark_has_a_twenty_ms_floor() {
        assert_eq!(Watermarks::derive(20).min_ms, 20);
        assert_eq!(Watermarks::derive(30).min_ms, 20);
        // 40/2 = 20 is the first target where the divisor takes over.
        assert_eq!(Watermarks::derive(40).min_ms, 20);
        assert_eq!(Watermarks::derive(42).min_ms, 21);
        assert_eq!(Watermarks::derive(2000).min_ms, 1000);
    }

    #[test]
    fn maximum_watermark_is_target_plus_two_hundred() {
        assert_eq!(Watermarks::derive(20).max_ms, 220);
        assert_eq!(Watermarks::derive(2000).max_ms, 2200);
    }

    #[test]
    fn a_missing_target_falls_back_to_the_default() {
        let wm = Watermarks::derive(0);
        assert_eq!(wm.target_ms, 120);
        assert_eq!(Watermarks::derive(-1).target_ms, 120);
    }

    #[test]
    fn hw_decode_round_trips_its_stored_value() {
        for mode in [HwDecode::Auto, HwDecode::Off, HwDecode::Nvdec] {
            assert_eq!(HwDecode::from_i64(mode.as_i64(), true), (mode, false));
        }
        assert_eq!(HwDecode::default(), HwDecode::Auto);
    }

    #[test]
    fn unknown_hw_decode_degrades_to_auto() {
        assert_eq!(HwDecode::from_i64(-1, true), (HwDecode::Auto, true));
        assert_eq!(HwDecode::from_i64(3, true), (HwDecode::Auto, true));
        assert_eq!(HwDecode::from_i64(i64::MAX, true), (HwDecode::Auto, true));
    }

    #[test]
    fn nvdec_degrades_where_cuda_cannot_exist() {
        assert_eq!(HwDecode::from_i64(2, false), (HwDecode::Auto, true));
        // The other modes are unaffected.
        assert_eq!(HwDecode::from_i64(0, false), (HwDecode::Auto, false));
        assert_eq!(HwDecode::from_i64(1, false), (HwDecode::Off, false));
    }
}
