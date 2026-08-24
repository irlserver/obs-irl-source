//! Small sample-domain helpers: fades, silence shaping, last-sample memory.

use crate::consts::AUDIO_MAX_CHANNELS;

/// The last emitted sample per channel, for silence shaping.
#[derive(Debug, Clone, Copy, Default)]
pub struct LastSample {
    /// Per-channel value.
    pub values: [f32; AUDIO_MAX_CHANNELS],
    /// Channels valid in `values`.
    pub channels: usize,
    /// Whether anything has been remembered.
    pub valid: bool,
}

impl LastSample {
    /// Remember the last frame of `samples` (`channels` ≤ 8, else cleared).
    pub fn remember(&mut self, samples: &[f32], channels: usize) {
        let _ = (samples, channels);
        todo!("W1-C")
    }
}

/// Fade in over `min(AUDIO_CONCEAL_FADE_MS, whole chunk)`.
pub fn apply_fade_in(samples: &mut [f32], channels: usize, rate: i32) {
    let _ = (samples, channels, rate);
    todo!("W1-C")
}

/// Fill `samples` with silence that decays from `last` to zero.
pub fn shape_silence_from_last(samples: &mut [f32], channels: usize, rate: i32, last: &LastSample) {
    let _ = (samples, channels, rate, last);
    todo!("W1-C")
}

/// Linear 1→0 gain across the whole buffer (disconnect fade-out).
pub fn apply_linear_fade_out(samples: &mut [f32], channels: usize) {
    let _ = (samples, channels);
    todo!("W1-C")
}

/// Continue a fade-in ramp of `total` frames with `remaining` frames left;
/// returns the new `remaining`.
pub fn apply_fade_in_ramp(samples: &mut [f32], channels: usize, remaining: i32, total: i32) -> i32 {
    let _ = (samples, channels, remaining, total);
    todo!("W1-C")
}
