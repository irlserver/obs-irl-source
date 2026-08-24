//! swscale (FFmpeg ≥ 7.1 public `SwsContext`, dynamic mode): same-size
//! conversion of an unsupported pixel format to NV12.
//!
//! `sws_alloc_context()` without `sws_init_context()` — initialising would
//! latch the legacy backend for the context's lifetime — then
//! `sws_scale_frame(ctx, dst, src)` with a persistent, non-owning destination
//! wrapper whose planes point into the caller's buffer. The `flags` field of
//! the public struct is the one direct struct write in this crate; it is
//! isolated in `set_flags` with the `av_opt_set_int("sws_flags")` fallback
//! documented there.
//!
//! Why dynamic mode at all (ported from the C plugin's `video-handler.c`):
//! FFmpeg 9.0 landed the swscale rewrite, where conversions are decomposed
//! into elementary ops compiled into kernel chains. `sws_getContext()` sets
//! `is_legacy_init`, and swscale.h is blunt that "the stateful legacy API
//! always implies SWS_BACKEND_LEGACY" — `SWS_UNSTABLE` on such a context is
//! silently ignored. The new backends exist only behind `sws_alloc_context()`
//! with no `sws_init_context()`, driven by `sws_scale_frame()`.
//!
//! As of 9.0 the op chain refuses every conversion this plugin performs: it
//! only builds a pass when no chroma resampling is needed, so `yuv420p → nv12`
//! (and every other subsampled target) fails `ff_sws_op_list_generate()` with
//! `ENOTSUP` and falls back to the legacy pass, bit-identically. The flag is
//! wired up anyway because that is where upstream's work is going, and it
//! should be one env var away rather than a refactor away. Callers gate it on
//! `IRL_SWS_UNSTABLE`; the default is `flags = 0`, i.e. the legacy backend.

use core::ffi::{c_int, c_uint};

use crate::frame::Frame;
use crate::{AVPixelFormat, Error, Result};

pub struct Scaler {
    ptr: *mut ffmpeg_sys_next::SwsContext,
    dst: Frame,
}

// SAFETY: a SwsContext is a plain owned conversion state; the plugin's scaler
// is owned by the video thread alone and never shared.
unsafe impl Send for Scaler {}

impl Scaler {
    /// `unstable` sets `SWS_UNSTABLE` (the C plugin gates it on the
    /// `IRL_SWS_UNSTABLE` environment variable).
    pub fn new(unstable: bool) -> Result<Self> {
        // SAFETY: no arguments; returns an allocated, uninitialised context or
        // null. Deliberately no sws_init_context(): that is what would mark the
        // context legacy and lock it to the legacy backend.
        let ptr = unsafe { ffmpeg_sys_next::sws_alloc_context() };
        if ptr.is_null() {
            return Err(Error::nomem());
        }
        let dst = match Frame::new() {
            Ok(frame) => frame,
            Err(err) => {
                let mut ptr = ptr;
                // SAFETY: `ptr` is the context we just allocated and still own.
                unsafe { ffmpeg_sys_next::sws_free_context(&mut ptr) };
                return Err(err);
            }
        };
        let this = Self { ptr, dst };
        this.set_flags(if unstable {
            ffmpeg_sys_next::SwsFlags::SWS_UNSTABLE as c_uint
        } else {
            0
        });
        Ok(this)
    }

    /// The one direct write to a public FFmpeg struct in this crate.
    ///
    /// `SwsContext` became public in FFmpeg 7.1 and ffmpeg-sys-next's bindgen
    /// output exposes `flags` as a plain `c_uint` field, so this is a struct
    /// store. Were the struct ever opaque again, the equivalent is
    /// `av_opt_set_int(ctx as *mut c_void, c"sws_flags", flags as i64, 0)` —
    /// the same option, reached through the AVClass instead.
    fn set_flags(&self, flags: c_uint) {
        // SAFETY: `self.ptr` is an allocated, not-yet-used context we own, and
        // `flags` is a plain scalar field of the public struct.
        unsafe { (*self.ptr).flags = flags };
    }

    /// Convert `src` into NV12 planes `y` (stride `stride`) and `uv` (same
    /// stride, half height). Colour properties are copied from `src` so no
    /// colourspace conversion happens.
    pub fn scale_into_nv12(
        &mut self,
        src: &Frame,
        y: &mut [u8],
        uv: &mut [u8],
        stride: i32,
    ) -> Result<()> {
        let width = src.width();
        let height = src.height();
        if width <= 0 || height <= 0 || stride < width {
            return Err(Error::inval());
        }
        let chroma_rows = (height + 1) / 2;
        let y_need = (stride as usize).saturating_mul(height as usize);
        let uv_need = (stride as usize).saturating_mul(chroma_rows as usize);
        if y.len() < y_need || uv.len() < uv_need {
            return Err(Error::inval());
        }

        // In dynamic mode the context carries no dimensions or formats — every
        // property comes from the frames, and sws_scale_frame() reconfigures
        // itself when they change.
        //
        // The colour properties have to be copied across: dynamic mode reads
        // them from the frames, so leaving the destination unspecified would
        // invite a colourspace conversion the legacy path never did. This is a
        // pixel format change only.
        self.dst.copy_video_props_from(src);

        // SAFETY: `self.dst` is a frame we own with no buffers attached (its
        // `buf[]` stays null, so nothing is freed or refcounted here). Setting
        // `data[0]` puts sws_scale_frame() on its user-provided-buffer path, so
        // it neither allocates nor references anything. The pointers borrow `y`
        // and `uv` for the call only; they are cleared again below.
        unsafe {
            let raw = self.dst.as_mut_ptr();
            (*raw).width = width;
            (*raw).height = height;
            (*raw).format = AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*raw).data[0] = y.as_mut_ptr();
            (*raw).data[1] = uv.as_mut_ptr();
            (*raw).data[2] = core::ptr::null_mut();
            (*raw).data[3] = core::ptr::null_mut();
            (*raw).linesize[0] = stride;
            (*raw).linesize[1] = stride;
            (*raw).linesize[2] = 0;
            (*raw).linesize[3] = 0;
        }

        // SAFETY: `self.ptr` is an allocated context, `self.dst` a fully
        // described destination pointing at the caller's buffers, and `src` a
        // live source frame.
        let ret = unsafe {
            ffmpeg_sys_next::sws_scale_frame(self.ptr, self.dst.as_mut_ptr(), src.as_ptr())
        };

        // Never leave the caller's pointers behind in a frame that outlives
        // this call.
        // SAFETY: `self.dst` is ours and owns none of these pointers.
        unsafe {
            let raw = self.dst.as_mut_ptr();
            (*raw).data = [core::ptr::null_mut(); 8];
            (*raw).linesize = [0; 8];
        }

        Error::check(ret)
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        // SAFETY: `&mut self.ptr` is our sole owning pointer. `self.dst` is a
        // wrapper holding no buffers and is freed by `Frame::drop` afterwards.
        unsafe { ffmpeg_sys_next::sws_free_context(&mut self.ptr) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv12_buffers(width: usize, height: usize) -> (Vec<u8>, Vec<u8>) {
        (vec![0u8; width * height], vec![0u8; width * height.div_ceil(2)])
    }

    #[test]
    fn converts_yuv444p_to_nv12() {
        let mut scaler = Scaler::new(false).unwrap();
        let mut src = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_YUV444P, 64, 48).unwrap();
        // SAFETY: test-local frame we own; a mid-gray luma plane so the
        // conversion has something recognisable to carry across.
        unsafe {
            let raw = src.as_mut_ptr();
            for row in 0..48 {
                let line = (*raw).data[0].add(row * (*raw).linesize[0] as usize);
                core::ptr::write_bytes(line, 0x80, 64);
            }
            for plane in 1..3 {
                for row in 0..48 {
                    let line = (*raw).data[plane].add(row * (*raw).linesize[plane] as usize);
                    core::ptr::write_bytes(line, 0x40, 64);
                }
            }
        }

        let (mut y, mut uv) = nv12_buffers(64, 48);
        scaler.scale_into_nv12(&src, &mut y, &mut uv, 64).unwrap();
        assert!(y.iter().all(|&b| b == 0x80), "luma should pass through unchanged");
        assert!(uv.iter().any(|&b| b != 0), "chroma plane was not written");

        // Reusable: a second call on the same context must work too.
        scaler.scale_into_nv12(&src, &mut y, &mut uv, 64).unwrap();
    }

    #[test]
    fn unstable_backend_flag_builds() {
        let scaler = Scaler::new(true).unwrap();
        // SAFETY: reading back the field we set, on a context we own.
        assert_eq!(
            unsafe { (*scaler.ptr).flags },
            ffmpeg_sys_next::SwsFlags::SWS_UNSTABLE as c_uint
        );
    }

    #[test]
    fn rejects_undersized_destinations() {
        let mut scaler = Scaler::new(false).unwrap();
        let src = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_YUV444P, 64, 48).unwrap();
        let (mut y, mut uv) = nv12_buffers(64, 48);
        assert!(scaler.scale_into_nv12(&src, &mut y[..100], &mut uv, 64).is_err());
        assert!(scaler.scale_into_nv12(&src, &mut y, &mut uv[..10], 64).is_err());
        // A stride narrower than the picture is a caller bug, not a conversion.
        assert!(scaler.scale_into_nv12(&src, &mut y, &mut uv, 32).is_err());
    }
}
