//! swresample: format conversion to interleaved float, and compensation-based
//! playback speed control.

use core::ffi::c_void;
use core::mem::MaybeUninit;

use crate::frame::Frame;
use crate::{AVSampleFormat, Error, Result};

/// Bytes per output sample: every output of this module is `AV_SAMPLE_FMT_FLT`.
const OUT_BYTES_PER_SAMPLE: usize = 4;

pub struct Resampler {
    ptr: *mut ffmpeg_sys_next::SwrContext,
    in_rate: i32,
    in_channels: i32,
    in_format: AVSampleFormat,
    out_rate: i32,
    out_channels: i32,
}

// SAFETY: a SwrContext is a plain owned conversion state with no thread
// affinity; each one here belongs to exactly one thread (the receiver thread's
// intake resampler, the audio thread's speed resampler).
unsafe impl Send for Resampler {}

/// `av_channel_layout_default` into a stack layout, released on drop.
struct DefaultLayout(ffmpeg_sys_next::AVChannelLayout);

impl DefaultLayout {
    fn new(channels: i32) -> Self {
        let mut layout = MaybeUninit::<ffmpeg_sys_next::AVChannelLayout>::zeroed();
        // SAFETY: av_channel_layout_default fully initialises the layout it is
        // handed (it writes order, nb_channels and the mask union).
        unsafe { ffmpeg_sys_next::av_channel_layout_default(layout.as_mut_ptr(), channels) };
        // SAFETY: initialised by the call above.
        Self(unsafe { layout.assume_init() })
    }

    fn as_ptr(&self) -> *const ffmpeg_sys_next::AVChannelLayout {
        &self.0
    }
}

impl Drop for DefaultLayout {
    fn drop(&mut self) {
        // SAFETY: `self.0` is an initialised layout; uninit releases any
        // custom-order allocation (default layouts have none, but the pairing
        // is what the API documents).
        unsafe { ffmpeg_sys_next::av_channel_layout_uninit(&mut self.0) };
    }
}

impl Resampler {
    /// `swr_alloc_set_opts2(out = FLT/out_channels/out_rate, in = frame's
    /// layout/format/rate)` + `swr_init`.
    pub fn to_interleaved_f32(input: &Frame, out_channels: i32, out_rate: i32) -> Result<Self> {
        let out_layout = DefaultLayout::new(out_channels);
        let in_format = input.sample_format();
        let in_rate = input.sample_rate();
        let in_channels = input.channels();

        let mut ptr = core::ptr::null_mut();
        // SAFETY: `&mut ptr` is a valid out-parameter, both layouts are live
        // (the input one is borrowed from `input` for the duration of the call),
        // and a null log context is allowed.
        let ret = unsafe {
            ffmpeg_sys_next::swr_alloc_set_opts2(
                &mut ptr,
                out_layout.as_ptr(),
                AVSampleFormat::AV_SAMPLE_FMT_FLT,
                out_rate,
                &raw const (*input.as_ptr()).ch_layout,
                in_format,
                in_rate,
                0,
                core::ptr::null_mut(),
            )
        };
        let this = Self::adopt(
            ptr,
            ret,
            in_rate,
            in_channels,
            in_format,
            out_rate,
            out_channels,
        )?;
        this.init()
    }

    /// Identity FLT→FLT at `rate`/`channels`, forced active with
    /// `av_opt_set_int("flags", SWR_FLAG_RESAMPLE)` **before** `swr_init`, so
    /// the first `set_compensation` does not re-init mid-stream.
    pub fn passthrough_f32(rate: i32, channels: i32) -> Result<Self> {
        let layout = DefaultLayout::new(channels);
        let mut ptr = core::ptr::null_mut();
        // SAFETY: as above; both sides share one live layout.
        let ret = unsafe {
            ffmpeg_sys_next::swr_alloc_set_opts2(
                &mut ptr,
                layout.as_ptr(),
                AVSampleFormat::AV_SAMPLE_FMT_FLT,
                rate,
                layout.as_ptr(),
                AVSampleFormat::AV_SAMPLE_FMT_FLT,
                rate,
                0,
                core::ptr::null_mut(),
            )
        };
        let this = Self::adopt(
            ptr,
            ret,
            rate,
            channels,
            AVSampleFormat::AV_SAMPLE_FMT_FLT,
            rate,
            channels,
        )?;

        // Force the resampler active from the start; otherwise the first
        // swr_set_compensation() call reinitialises the context mid-stream.
        // SAFETY: `this.ptr` is an allocated, not-yet-initialised context and
        // "flags" is an AVOption of the SwrContext class.
        unsafe {
            ffmpeg_sys_next::av_opt_set_int(
                this.ptr as *mut c_void,
                c"flags".as_ptr(),
                ffmpeg_sys_next::SWR_FLAG_RESAMPLE as i64,
                0,
            );
        }
        this.init()
    }

    /// Wrap the result of `swr_alloc_set_opts2`, freeing on failure.
    #[allow(clippy::too_many_arguments)]
    fn adopt(
        ptr: *mut ffmpeg_sys_next::SwrContext,
        ret: core::ffi::c_int,
        in_rate: i32,
        in_channels: i32,
        in_format: AVSampleFormat,
        out_rate: i32,
        out_channels: i32,
    ) -> Result<Self> {
        if ptr.is_null() {
            return Err(if ret < 0 { Error(ret) } else { Error::nomem() });
        }
        let this = Self {
            ptr,
            in_rate,
            in_channels,
            in_format,
            out_rate,
            out_channels,
        };
        // `this` frees the context in Drop if the option set failed.
        Error::check(ret)?;
        Ok(this)
    }

    fn init(self) -> Result<Self> {
        // SAFETY: `self.ptr` is an allocated context with its options set.
        Error::check(unsafe { ffmpeg_sys_next::swr_init(self.ptr) })?;
        Ok(self)
    }

    /// Whether the input side still matches `frame` (rate/channels/format).
    pub fn matches(&self, frame: &Frame) -> bool {
        self.in_rate == frame.sample_rate()
            && self.in_channels == frame.channels()
            && self.in_format == frame.sample_format()
    }

    pub fn matches_params(&self, rate: i32, channels: i32) -> bool {
        self.in_rate == rate && self.in_channels == channels
    }

    /// Channels on the output side (always interleaved float).
    pub fn out_channels(&self) -> i32 {
        self.out_channels
    }

    /// Sample rate on the output side.
    pub fn out_rate(&self) -> i32 {
        self.out_rate
    }

    /// `swr_set_compensation(delta, distance)`.
    pub fn set_compensation(
        &mut self,
        sample_delta: i32,
        compensation_distance: i32,
    ) -> Result<()> {
        // SAFETY: `self.ptr` is an initialised context.
        Error::check(unsafe {
            ffmpeg_sys_next::swr_set_compensation(self.ptr, sample_delta, compensation_distance)
        })
    }

    /// `swr_get_out_samples`.
    pub fn out_samples(&self, in_samples: i32) -> i32 {
        // SAFETY: `self.ptr` is an initialised context.
        unsafe { ffmpeg_sys_next::swr_get_out_samples(self.ptr, in_samples) }
    }

    /// Bytes one output frame occupies (`channels * 4`).
    fn out_frame_bytes(&self) -> usize {
        self.out_channels.max(0) as usize * OUT_BYTES_PER_SAMPLE
    }

    fn check_out_capacity(&self, out: &[u8], max_out_frames: i32) -> Result<()> {
        if max_out_frames < 0 {
            return Err(Error::inval());
        }
        let need = (max_out_frames as usize).saturating_mul(self.out_frame_bytes());
        if out.len() < need {
            Err(Error::inval())
        } else {
            Ok(())
        }
    }

    /// `swr_convert(out, max_out_frames, frame.extended_data, nb_samples)`.
    /// Returns frames written.
    pub fn convert_from_frame(
        &mut self,
        out: &mut [u8],
        max_out_frames: i32,
        input: &Frame,
    ) -> Result<i32> {
        self.check_out_capacity(out, max_out_frames)?;
        let out_ptr = out.as_mut_ptr();
        // SAFETY: `out` has room for `max_out_frames` interleaved output frames
        // (checked above), and `extended_data` is the decoder-owned array of
        // per-plane input pointers with `nb_samples` samples each.
        let got = unsafe {
            ffmpeg_sys_next::swr_convert(
                self.ptr,
                &out_ptr,
                max_out_frames,
                (*input.as_ptr()).extended_data as *const *const u8,
                input.nb_samples(),
            )
        };
        if got < 0 { Err(Error(got)) } else { Ok(got) }
    }

    /// `swr_convert` from an interleaved float buffer. Returns frames written.
    pub fn convert_interleaved(
        &mut self,
        out: &mut [u8],
        max_out_frames: i32,
        input: &[u8],
        in_frames: i32,
    ) -> Result<i32> {
        self.check_out_capacity(out, max_out_frames)?;
        if in_frames < 0 {
            return Err(Error::inval());
        }
        let in_need = (in_frames as usize)
            .saturating_mul(self.in_channels.max(0) as usize)
            .saturating_mul(OUT_BYTES_PER_SAMPLE);
        if input.len() < in_need {
            return Err(Error::inval());
        }

        let out_ptr = out.as_mut_ptr();
        let in_ptr = input.as_ptr();
        // SAFETY: both buffers are large enough for the frame counts passed
        // (checked above) and the input is packed float, so a single plane
        // pointer is the whole input.
        let got = unsafe {
            ffmpeg_sys_next::swr_convert(self.ptr, &out_ptr, max_out_frames, &in_ptr, in_frames)
        };
        if got < 0 { Err(Error(got)) } else { Ok(got) }
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        // SAFETY: `&mut self.ptr` is our sole owning pointer; swr_free tolerates
        // an uninitialised-but-allocated context and nulls the pointer.
        unsafe { ffmpeg_sys_next::swr_free(&mut self.ptr) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AVPixelFormat;

    fn silent_stereo(frames: usize) -> Vec<u8> {
        vec![0u8; frames * 2 * OUT_BYTES_PER_SAMPLE]
    }

    #[test]
    fn passthrough_preserves_frame_count() {
        let mut swr = Resampler::passthrough_f32(48_000, 2).unwrap();
        assert!(swr.matches_params(48_000, 2));
        assert!(!swr.matches_params(44_100, 2));
        assert_eq!(swr.out_channels(), 2);
        assert_eq!(swr.out_rate(), 48_000);

        let input = silent_stereo(1024);
        let max_out = swr.out_samples(1024).max(1024) + 32;
        let mut out = vec![0u8; max_out as usize * 2 * OUT_BYTES_PER_SAMPLE];
        let got = swr
            .convert_interleaved(&mut out, max_out, &input, 1024)
            .unwrap();
        // A forced-active identity resampler keeps a small internal delay, so
        // the first call can come back a hair short; it must not exceed input.
        assert!(got > 0 && got <= 1024, "got {got}");
    }

    #[test]
    fn compensation_shifts_the_output_frame_count() {
        let mut swr = Resampler::passthrough_f32(48_000, 2).unwrap();
        let input = silent_stereo(1024);
        let max_out = 2048;
        let mut out = vec![0u8; max_out as usize * 2 * OUT_BYTES_PER_SAMPLE];

        // Prime the delay line so the steady-state count is stable.
        swr.convert_interleaved(&mut out, max_out, &input, 1024)
            .unwrap();
        let baseline = swr
            .convert_interleaved(&mut out, max_out, &input, 1024)
            .unwrap();
        assert_eq!(baseline, 1024);

        // Ask for 5% more output over the next chunk: playback slows down.
        let desired = 1075;
        swr.set_compensation(desired - 1024, desired).unwrap();
        let stretched = swr
            .convert_interleaved(&mut out, max_out, &input, 1024)
            .unwrap();
        assert!(stretched > baseline, "{stretched} should exceed {baseline}");
    }

    #[test]
    fn undersized_output_is_rejected_before_ffmpeg_sees_it() {
        let mut swr = Resampler::passthrough_f32(48_000, 2).unwrap();
        let input = silent_stereo(64);
        let mut out = vec![0u8; 16];
        assert!(swr.convert_interleaved(&mut out, 64, &input, 64).is_err());

        let mut big = vec![0u8; 64 * 2 * OUT_BYTES_PER_SAMPLE];
        // Input too small for the frame count claimed.
        assert!(
            swr.convert_interleaved(&mut big, 64, &input[..8], 64)
                .is_err()
        );
    }

    #[test]
    fn planar_input_converts_to_interleaved_float() {
        // A planar-float frame is the common decoder output (AAC, Opus).
        let mut frame = crate::Frame::new().unwrap();
        // SAFETY: test-local blank frame; setting audio parameters before
        // av_frame_get_buffer is the documented allocation sequence.
        unsafe {
            let raw = frame.as_mut_ptr();
            (*raw).format = AVSampleFormat::AV_SAMPLE_FMT_FLTP as core::ffi::c_int;
            (*raw).nb_samples = 512;
            (*raw).sample_rate = 48_000;
            ffmpeg_sys_next::av_channel_layout_default(&raw mut (*raw).ch_layout, 2);
            assert_eq!(ffmpeg_sys_next::av_frame_get_buffer(raw, 0), 0);
        }
        assert_eq!(frame.channels(), 2);
        assert!(
            frame.interleaved_f32_bytes().is_none(),
            "planar is not interleaved"
        );

        let mut swr = Resampler::to_interleaved_f32(&frame, 2, 48_000).unwrap();
        assert!(swr.matches(&frame));

        let max_out = swr.out_samples(512) + 32;
        let mut out = vec![0u8; max_out as usize * 2 * OUT_BYTES_PER_SAMPLE];
        let got = swr.convert_from_frame(&mut out, max_out, &frame).unwrap();
        assert_eq!(got, 512);

        // A video frame obviously does not match the audio input side.
        let video = crate::Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_NV12, 16, 16).unwrap();
        assert!(!swr.matches(&video));
    }
}
