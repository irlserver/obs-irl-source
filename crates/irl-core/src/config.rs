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
        let _ = (value, nvdec_available);
        todo!("W1-C")
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
    pub fn derive(target_ms: i32) -> Self {
        let _ = (target_ms, consts::BUFFER_MIN_DIVISOR);
        todo!("W1-C")
    }
}
