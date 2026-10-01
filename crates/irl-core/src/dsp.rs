//! Small sample-domain helpers: fades, silence shaping, last-sample memory.
//!
//! Ports `remember_last_sample`, `audio_apply_fade_in`,
//! `shape_silence_from_last` (`receiver-audio.c:210-265`), the fade-in ramp in
//! `irl_pump_audio_once` (`receiver-audio.c:861-883`) and the fade of
//! `audio_buffer_read_with_fade_out`. Everything works on interleaved `f32`,
//! which is the only format the plugin ever hands to OBS; [`FloatEdit`] and
//! [`LastSample::remember_bytes`] bridge to the byte buffers it travels in.

use crate::consts::{self, AUDIO_MAX_CHANNELS};

/// Bytes one interleaved float sample occupies (`sizeof(float)`).
pub const SAMPLE_BYTES: usize = 4;

fn read_f32(bytes: &[u8]) -> f32 {
    let mut raw = [0u8; SAMPLE_BYTES];
    raw.copy_from_slice(bytes);
    f32::from_le_bytes(raw)
}

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
        let frames = samples.len().checked_div(channels).unwrap_or(0);
        if frames == 0 || channels == 0 || channels > AUDIO_MAX_CHANNELS {
            self.valid = false;
            self.channels = 0;
            return;
        }

        let last = (frames - 1) * channels;
        self.values[..channels].copy_from_slice(&samples[last..last + channels]);
        self.channels = channels;
        self.valid = true;
    }

    /// [`Self::remember`] over an interleaved-float byte buffer: only the last
    /// frame is needed, so no whole-chunk decode happens here.
    pub fn remember_bytes(&mut self, samples: &[u8], channels: usize) {
        let mut values = [0.0f32; AUDIO_MAX_CHANNELS];
        let take = channels.min(AUDIO_MAX_CHANNELS);
        let frame_bytes = channels * SAMPLE_BYTES;
        if channels == 0 || samples.len() < frame_bytes {
            self.remember(&[], channels);
            return;
        }

        let start = samples.len() - samples.len() % frame_bytes - frame_bytes;
        for (ch, value) in values[..take].iter_mut().enumerate() {
            let off = start + ch * SAMPLE_BYTES;
            *value = read_f32(&samples[off..off + SAMPLE_BYTES]);
        }
        // `channels > 8` reaches `remember` with a short slice, which is
        // exactly the case the C rejected (`*dst_valid = false`).
        self.remember(&values[..take], channels);
    }

    /// Forget the remembered frame; the next concealment chunk is pure
    /// silence (`ctx->audio_out_last_valid = false` after the first one).
    pub fn forget(&mut self) {
        self.valid = false;
    }
}

/// The helpers here edit interleaved float in place, while the audio path
/// carries bytes (what swresample and libobs both take) and the plugin crates
/// forbid the unsafe cast between the two views. The rare in-place edits
/// (concealment shaping and the splice fades) therefore decode into a reusable
/// `f32` scratch, run the helper, and write the result back. A normal chunk is
/// emitted untouched, so the steady-state copy count matches the C.
#[derive(Debug, Default)]
pub struct FloatEdit {
    scratch: Vec<f32>,
}

impl FloatEdit {
    /// Run `edit` over `bytes` viewed as interleaved `f32`.
    pub fn edit(&mut self, bytes: &mut [u8], edit: impl FnOnce(&mut [f32])) {
        self.scratch.clear();
        self.scratch
            .extend(bytes.chunks_exact(SAMPLE_BYTES).map(read_f32));
        edit(&mut self.scratch);
        for (dst, value) in bytes
            .chunks_exact_mut(SAMPLE_BYTES)
            .zip(self.scratch.iter())
        {
            dst.copy_from_slice(&value.to_le_bytes());
        }
    }
}

/// Frames the concealment fade covers: `AUDIO_CONCEAL_FADE_MS` worth, never
/// more than the chunk (`audio_conceal_fade_frames`).
fn conceal_fade_frames(rate: i32, max_frames: usize) -> usize {
    if rate <= 0 || max_frames == 0 {
        return 0;
    }
    let frames = rate as i64 * consts::AUDIO_CONCEAL_FADE_MS as i64 / 1000;
    let frames = if frames <= 0 { 1 } else { frames as usize };
    frames.min(max_frames)
}

/// Fade in over `min(AUDIO_CONCEAL_FADE_MS, whole chunk)`.
///
/// Applied to the first real chunk after concealment or a clock restart, so
/// the splice back to live audio does not click.
pub fn apply_fade_in(samples: &mut [f32], channels: usize, rate: i32) {
    if channels == 0 {
        return;
    }
    let frames = samples.len() / channels;
    let fade_frames = conceal_fade_frames(rate, frames);
    if fade_frames == 0 {
        return;
    }

    for f in 0..fade_frames {
        let gain = (f + 1) as f32 / fade_frames as f32;
        for ch in 0..channels {
            samples[f * channels + ch] *= gain;
        }
    }
}

/// Fill `samples` with silence that decays from `last` to zero.
///
/// The C caller memsets the scratch buffer before calling
/// `shape_silence_from_last`; both steps live here, so the whole buffer is
/// silence and only its head carries the decay.
pub fn shape_silence_from_last(samples: &mut [f32], channels: usize, rate: i32, last: &LastSample) {
    samples.fill(0.0);
    if channels == 0 {
        return;
    }
    let frames = samples.len() / channels;
    let fade_frames = conceal_fade_frames(rate, frames);
    if !last.valid || last.channels != channels || fade_frames == 0 {
        return;
    }

    for f in 0..fade_frames {
        let gain = 1.0 - (f + 1) as f32 / fade_frames as f32;
        for ch in 0..channels {
            samples[f * channels + ch] = last.values[ch] * gain;
        }
    }
}

/// Continue a fade-in ramp of `total` frames with `remaining` frames left;
/// returns the new `remaining`.
///
/// The ramp spans several chunks (50 ms at the reconnect fade), so it is
/// driven by a residual frame count rather than by the chunk it lands in.
pub fn apply_fade_in_ramp(samples: &mut [f32], channels: usize, remaining: i32, total: i32) -> i32 {
    if channels == 0 || total <= 0 || remaining <= 0 {
        return remaining;
    }
    let frames = samples.len() / channels;
    let mut remaining = remaining;

    for f in 0..frames {
        if remaining <= 0 {
            break;
        }
        let into = total - remaining;
        let gain = into as f32 / total as f32;
        for ch in 0..channels {
            samples[f * channels + ch] *= gain;
        }
        remaining -= 1;
    }
    remaining
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: i32 = 48_000;
    /// 8 ms at 48 kHz.
    const FADE: usize = 384;

    #[test]
    fn remember_keeps_the_last_frame() {
        let mut last = LastSample::default();
        let samples = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        last.remember(&samples, 2);
        assert!(last.valid);
        assert_eq!(last.channels, 2);
        assert_eq!(&last.values[..2], &[0.5, 0.6]);
    }

    #[test]
    fn remember_bytes_keeps_the_last_whole_frame() {
        let bytes: Vec<u8> = [0.1f32, 0.2, 0.3, 0.4, 0.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut last = LastSample::default();
        last.remember_bytes(&bytes, 2);
        assert!(last.valid);
        assert_eq!(&last.values[..2], &[0.3, 0.4]);

        last.remember_bytes(&bytes[..4], 2);
        assert!(!last.valid);
    }

    #[test]
    fn float_edit_round_trips_through_bytes() {
        let mut bytes: Vec<u8> = [1.0f32, -2.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        FloatEdit::default().edit(&mut bytes, |pcm| pcm.iter_mut().for_each(|v| *v *= 0.5));
        assert_eq!(&bytes[..4], &0.5f32.to_le_bytes());
        assert_eq!(&bytes[4..], &(-1.0f32).to_le_bytes());
    }

    #[test]
    fn remember_rejects_more_than_eight_channels() {
        let mut last = LastSample::default();
        last.remember(&[1.0; 18], 9);
        assert!(!last.valid);
        assert_eq!(last.channels, 0);

        // Exactly eight is fine.
        last.remember(&[1.0; 16], 8);
        assert!(last.valid);
        assert_eq!(last.channels, 8);
    }

    #[test]
    fn remember_rejects_empty_input() {
        let mut last = LastSample::default();
        last.remember(&[1.0, 1.0], 2);
        assert!(last.valid);
        last.remember(&[], 2);
        assert!(!last.valid);
        last.remember(&[1.0, 1.0], 0);
        assert!(!last.valid);
    }

    #[test]
    fn fade_in_covers_eight_ms() {
        let mut samples = vec![1.0f32; 960 * 2];
        apply_fade_in(&mut samples, 2, RATE);

        assert!((samples[0] - 1.0 / FADE as f32).abs() < 1e-6);
        assert_eq!(samples[1], samples[0], "both channels get the same gain");
        // The last faded frame reaches unity ...
        assert!((samples[(FADE - 1) * 2] - 1.0).abs() < 1e-6);
        // ... and everything past the fade is untouched.
        assert_eq!(samples[FADE * 2], 1.0);
        assert_eq!(*samples.last().unwrap(), 1.0);
    }

    #[test]
    fn fade_in_of_a_short_chunk_covers_the_whole_chunk() {
        let mut samples = vec![1.0f32; 4 * 2];
        apply_fade_in(&mut samples, 2, RATE);
        assert_eq!(samples, vec![0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0]);
    }

    #[test]
    fn fade_in_ignores_degenerate_input() {
        let mut samples = vec![1.0f32; 8];
        apply_fade_in(&mut samples, 0, RATE);
        assert_eq!(samples, vec![1.0; 8]);
        apply_fade_in(&mut samples, 2, 0);
        assert_eq!(samples, vec![1.0; 8]);
        apply_fade_in(&mut [], 2, RATE);
    }

    #[test]
    fn silence_decays_from_the_last_sample() {
        let mut last = LastSample::default();
        last.remember(&[0.0, 0.0, 0.8, -0.8], 2);

        let mut samples = vec![7.0f32; 960 * 2];
        shape_silence_from_last(&mut samples, 2, RATE, &last);

        // First frame steps one gain unit down from the remembered value.
        let step = 1.0 - 1.0 / FADE as f32;
        assert!((samples[0] - 0.8 * step).abs() < 1e-6);
        assert!((samples[1] + 0.8 * step).abs() < 1e-6);
        // The ramp lands exactly on zero ...
        assert_eq!(samples[(FADE - 1) * 2], 0.0);
        // ... and the rest of the buffer is silence.
        assert!(samples[FADE * 2..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn silence_is_flat_without_a_remembered_sample() {
        let mut samples = vec![7.0f32; 64];
        let last = LastSample::default();
        shape_silence_from_last(&mut samples, 2, RATE, &last);
        assert!(samples.iter().all(|&s| s == 0.0));

        // A channel-count mismatch is treated the same way.
        let mut last = LastSample::default();
        last.remember(&[0.5], 1);
        let mut samples = vec![7.0f32; 64];
        shape_silence_from_last(&mut samples, 2, RATE, &last);
        assert!(samples.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn fade_in_ramp_spans_several_chunks() {
        let total = RATE * consts::FADE_DURATION_MS / 1000; // 2400 frames
        let mut remaining = total;

        // First chunk: 960 frames of the 2400-frame ramp.
        let mut chunk = vec![1.0f32; 960 * 2];
        remaining = apply_fade_in_ramp(&mut chunk, 2, remaining, total);
        assert_eq!(remaining, total - 960);
        assert_eq!(chunk[0], 0.0, "the ramp starts at silence");
        assert!((chunk[959 * 2] - 959.0 / total as f32).abs() < 1e-6);

        // Skip ahead to the last chunk, which finishes the ramp and leaves
        // the rest of the samples at unity.
        remaining = 100;
        let mut chunk = vec![1.0f32; 960 * 2];
        remaining = apply_fade_in_ramp(&mut chunk, 2, remaining, total);
        assert_eq!(remaining, 0);
        assert!((chunk[0] - (total - 100) as f32 / total as f32).abs() < 1e-6);
        assert_eq!(chunk[100 * 2], 1.0);
        assert_eq!(*chunk.last().unwrap(), 1.0);
    }

    #[test]
    fn fade_in_ramp_is_a_no_op_when_finished() {
        let mut chunk = vec![1.0f32; 8];
        assert_eq!(apply_fade_in_ramp(&mut chunk, 2, 0, 2400), 0);
        assert_eq!(chunk, vec![1.0; 8]);
        assert_eq!(apply_fade_in_ramp(&mut chunk, 2, 10, 0), 10);
        assert_eq!(chunk, vec![1.0; 8]);
    }
}
