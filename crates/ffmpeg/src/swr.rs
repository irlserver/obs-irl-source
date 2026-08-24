//! swresample: format conversion to interleaved float, and compensation-based
//! playback speed control.

use crate::frame::Frame;
use crate::{AVSampleFormat, Result};

pub struct Resampler {
    ptr: *mut ffmpeg_sys_next::SwrContext,
    in_rate: i32,
    in_channels: i32,
    in_format: AVSampleFormat,
}

unsafe impl Send for Resampler {}

impl Resampler {
    /// `swr_alloc_set_opts2(out = FLT/out_channels/out_rate, in = frame's
    /// layout/format/rate)` + `swr_init`.
    pub fn to_interleaved_f32(input: &Frame, out_channels: i32, out_rate: i32) -> Result<Self> {
        let _ = (input, out_channels, out_rate);
        todo!("W1-B")
    }

    /// Identity FLT→FLT at `rate`/`channels`, forced active with
    /// `av_opt_set_int("flags", SWR_FLAG_RESAMPLE)` **before** `swr_init`, so
    /// the first `set_compensation` does not re-init mid-stream.
    pub fn passthrough_f32(rate: i32, channels: i32) -> Result<Self> {
        let _ = (rate, channels);
        todo!("W1-B")
    }

    /// Whether the input side still matches `frame` (rate/channels/format).
    pub fn matches(&self, frame: &Frame) -> bool {
        let _ = (frame, self.in_rate, self.in_channels, self.in_format);
        todo!("W1-B")
    }

    pub fn matches_params(&self, rate: i32, channels: i32) -> bool {
        self.in_rate == rate && self.in_channels == channels
    }

    /// `swr_set_compensation(delta, distance)`.
    pub fn set_compensation(&mut self, sample_delta: i32, compensation_distance: i32) -> Result<()> {
        let _ = (sample_delta, compensation_distance);
        todo!("W1-B")
    }

    /// `swr_get_out_samples`.
    pub fn out_samples(&self, in_samples: i32) -> i32 {
        let _ = in_samples;
        todo!("W1-B")
    }

    /// `swr_convert(out, max_out_frames, frame.extended_data, nb_samples)`.
    /// Returns frames written.
    pub fn convert_from_frame(&mut self, out: &mut [u8], max_out_frames: i32, input: &Frame) -> Result<i32> {
        let _ = (out, max_out_frames, input);
        todo!("W1-B")
    }

    /// `swr_convert` from an interleaved float buffer. Returns frames written.
    pub fn convert_interleaved(&mut self, out: &mut [u8], max_out_frames: i32, input: &[u8], in_frames: i32) -> Result<i32> {
        let _ = (out, max_out_frames, input, in_frames);
        todo!("W1-B")
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        todo!("W1-B: swr_free")
    }
}
