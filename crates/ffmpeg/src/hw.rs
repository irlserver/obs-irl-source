//! Hardware device contexts and the pooled GPU→CPU transfer.

use crate::frame::Frame;
use crate::{AVHWDeviceType, AVPixelFormat, Error, Result, ffalign};

/// Plane alignment for pooled transfer destinations; what
/// `av_frame_get_buffer()` would pick on a modern x86 (AVX-512 stores), and
/// what lets FFmpeg's uncached-copy fast path engage on D3D11VA.
pub const XFER_PLANE_ALIGN: i32 = 64;

/// Surface dimensions are padded to this before allocating a transfer
/// destination: hardware backends copy in aligned blocks, and every one of
/// them clips the copy to the source size.
const XFER_DIM_ALIGN: i32 = 16;

/// An `AVBufferRef` holding an `AVHWDeviceContext`.
pub struct HwDeviceContext {
    ptr: *mut ffmpeg_sys_next::AVBufferRef,
    kind: AVHWDeviceType,
}

// SAFETY: an AVHWDeviceContext buffer is refcounted with atomics and has no
// thread affinity; the receiver thread is its sole owner.
unsafe impl Send for HwDeviceContext {}

impl HwDeviceContext {
    /// Try `av_hwdevice_ctx_create(type, NULL, NULL, 0)` for each type in
    /// order; the first success wins. `on_fail` is called for each failure so
    /// the caller can log it the way the C plugin does.
    pub fn probe(
        types: &[AVHWDeviceType],
        on_fail: &mut dyn FnMut(AVHWDeviceType, Error),
    ) -> Option<Self> {
        for &kind in types {
            let mut ptr = core::ptr::null_mut();
            // SAFETY: `&mut ptr` is a valid out-parameter; a null device name
            // and null options mean "the default device, no options", and flags
            // must be 0.
            let ret = unsafe {
                ffmpeg_sys_next::av_hwdevice_ctx_create(
                    &mut ptr,
                    kind,
                    core::ptr::null(),
                    core::ptr::null_mut(),
                    0,
                )
            };
            if ret == 0 && !ptr.is_null() {
                return Some(Self { ptr, kind });
            }
            if !ptr.is_null() {
                // SAFETY: the call left a reference behind despite failing.
                unsafe { ffmpeg_sys_next::av_buffer_unref(&mut ptr) };
            }
            on_fail(kind, Error(if ret < 0 { ret } else { Error::nomem().0 }));
        }
        None
    }

    pub fn kind(&self) -> AVHWDeviceType {
        self.kind
    }

    #[doc(hidden)]
    pub fn as_ptr(&self) -> *mut ffmpeg_sys_next::AVBufferRef {
        self.ptr
    }
}

impl Drop for HwDeviceContext {
    fn drop(&mut self) {
        // SAFETY: `&mut self.ptr` is our reference to the device context;
        // av_buffer_unref drops it and nulls the pointer.
        unsafe { ffmpeg_sys_next::av_buffer_unref(&mut self.ptr) };
    }
}

/// Recycled destination buffers for `av_hwframe_transfer_data`, so a 4K frame
/// does not pay a fresh 12–24 MB allocation (and its page faults) 60 times a
/// second. Buffers are sized `av_image_get_buffer_size(fmt, FFALIGN(w,16),
/// FFALIGN(h,16), 64)`; the 64-byte plane alignment is what lets FFmpeg's
/// uncached-copy fast path engage on D3D11VA.
pub struct FramePool {
    pool: *mut ffmpeg_sys_next::AVBufferPool,
    fmt: AVPixelFormat,
    width: i32,
    height: i32,
    size: usize,
}

// SAFETY: an AVBufferPool is internally synchronised (it is designed to be
// shared by decoder worker threads); the plugin's pool is owned by the video
// thread alone.
unsafe impl Send for FramePool {}

impl FramePool {
    /// Returns the pool and the per-frame byte size (for the log line).
    pub fn new(fmt: AVPixelFormat, src_width: i32, src_height: i32) -> Result<(Self, usize)> {
        let width = ffalign(src_width, XFER_DIM_ALIGN);
        let height = ffalign(src_height, XFER_DIM_ALIGN);
        let size = crate::image_buffer_size(fmt, width, height, XFER_PLANE_ALIGN)?;

        // SAFETY: `size` is a positive byte count; a null allocator means the
        // default av_buffer_alloc.
        let pool = unsafe { ffmpeg_sys_next::av_buffer_pool_init(size, None) };
        if pool.is_null() {
            return Err(Error::nomem());
        }
        Ok((Self { pool, fmt, width, height, size }, size))
    }

    /// The padded dimensions the pool's buffers are laid out for.
    pub fn dimensions(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    pub fn matches(&self, fmt: AVPixelFormat, src_width: i32, src_height: i32) -> bool {
        self.fmt == fmt
            && self.width == ffalign(src_width, XFER_DIM_ALIGN)
            && self.height == ffalign(src_height, XFER_DIM_ALIGN)
    }

    /// `av_buffer_pool_get` + `av_image_fill_arrays`; the buffer becomes
    /// `frame.buf[0]` so normal refcounting returns it to the pool.
    ///
    /// The frame comes back carrying the *padded* dimensions, exactly as
    /// FFmpeg's own `transfer_data_alloc()` leaves them; the caller restores
    /// the display size with [`Frame::set_display_size`] after the transfer.
    pub fn acquire(&self) -> Result<Frame> {
        // SAFETY: `self.pool` is a live pool we own.
        let mut buf = unsafe { ffmpeg_sys_next::av_buffer_pool_get(self.pool) };
        if buf.is_null() {
            return Err(Error::nomem());
        }

        let mut frame = match Frame::new() {
            Ok(f) => f,
            Err(err) => {
                // SAFETY: `buf` is the reference we just took from the pool.
                unsafe { ffmpeg_sys_next::av_buffer_unref(&mut buf) };
                return Err(err);
            }
        };

        // SAFETY: `frame` is a blank frame we own; `buf->data` points at
        // `self.size` bytes, which is exactly what av_image_get_buffer_size
        // reported for these format/dimensions/alignment in `new`.
        let ret = unsafe {
            let raw = frame.as_mut_ptr();
            (*raw).format = self.fmt as core::ffi::c_int;
            (*raw).width = self.width;
            (*raw).height = self.height;
            ffmpeg_sys_next::av_image_fill_arrays(
                (*raw).data.as_mut_ptr(),
                (*raw).linesize.as_mut_ptr(),
                (*buf).data,
                self.fmt,
                self.width,
                self.height,
                XFER_PLANE_ALIGN,
            )
        };
        if ret < 0 {
            // SAFETY: `buf` has not been handed to the frame yet.
            unsafe { ffmpeg_sys_next::av_buffer_unref(&mut buf) };
            return Err(Error(ret));
        }

        // SAFETY: the frame is blank, so `buf[0]` is null and takes ownership
        // of the pool reference here; av_frame_unref returns it to the pool.
        unsafe { (*frame.as_mut_ptr()).buf[0] = buf };
        Ok(frame)
    }

    /// Bytes per pooled buffer.
    pub fn buffer_size(&self) -> usize {
        self.size
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        // SAFETY: `&mut self.pool` is our owning pointer. This is safe with
        // pooled buffers still alive in the pacing queue: the pool lingers
        // internally until its last buffer is returned.
        unsafe { ffmpeg_sys_next::av_buffer_pool_uninit(&mut self.pool) };
    }
}

/// `av_hwframe_transfer_data(dst, src, 0)` into a caller-allocated `dst`.
pub fn hwframe_transfer_into(dst: &mut Frame, src: &Frame) -> Result<()> {
    // SAFETY: both frames are live and owned by the caller; `dst` carries the
    // destination buffers (from FramePool::acquire) and `src` a hardware frame.
    Error::check(unsafe {
        ffmpeg_sys_next::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0)
    })
}

/// `av_hwframe_transfer_data` into a fresh frame FFmpeg allocates (fallback
/// for backends that reject caller-allocated destinations).
pub fn hwframe_transfer_new(src: &Frame) -> Result<Frame> {
    let mut dst = Frame::new()?;
    // SAFETY: `dst` is a blank frame, which is what tells
    // av_hwframe_transfer_data to allocate the destination itself.
    Error::check(unsafe {
        ffmpeg_sys_next::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0)
    })?;
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_never_panics() {
        let mut failures = Vec::new();
        let device = HwDeviceContext::probe(
            &[AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI],
            &mut |kind, err| failures.push((kind, err)),
        );
        match device {
            Some(dev) => assert_eq!(dev.kind(), AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI),
            None => assert_eq!(failures.len(), 1),
        }
    }

    #[test]
    fn probe_of_nothing_is_none() {
        let mut called = 0;
        assert!(HwDeviceContext::probe(&[], &mut |_, _| called += 1).is_none());
        assert_eq!(called, 0);
    }

    #[test]
    fn pool_pads_dimensions_and_recycles() {
        let (pool, size) = FramePool::new(AVPixelFormat::AV_PIX_FMT_NV12, 1920, 1080).unwrap();
        assert_eq!(pool.dimensions(), (1920, 1088));
        assert!(pool.matches(AVPixelFormat::AV_PIX_FMT_NV12, 1920, 1080));
        assert!(pool.matches(AVPixelFormat::AV_PIX_FMT_NV12, 1920, 1081), "same padded height");
        assert!(!pool.matches(AVPixelFormat::AV_PIX_FMT_YUV420P, 1920, 1080));
        assert!(!pool.matches(AVPixelFormat::AV_PIX_FMT_NV12, 1280, 720));
        assert_eq!(size, pool.buffer_size());
        assert!(size >= 1920 * 1088 * 3 / 2);

        let first = pool.acquire().unwrap();
        assert_eq!((first.width(), first.height()), (1920, 1088));
        let base = first.plane(0).unwrap().as_ptr();
        assert_eq!(first.plane(0).unwrap().len(), first.plane_linesize(0) as usize * 1088);
        drop(first);

        // The buffer is back in the pool, so the next acquire reuses it.
        let second = pool.acquire().unwrap();
        assert_eq!(second.plane(0).unwrap().as_ptr(), base);
    }

    #[test]
    fn display_size_can_be_restored_after_a_transfer() {
        let (pool, _) = FramePool::new(AVPixelFormat::AV_PIX_FMT_NV12, 1920, 1080).unwrap();
        let mut frame = pool.acquire().unwrap();
        frame.set_display_size(1920, 1080);
        assert_eq!((frame.width(), frame.height()), (1920, 1080));
    }

    #[test]
    fn transfer_from_a_software_frame_fails_cleanly() {
        let src = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_NV12, 16, 16).unwrap();
        assert!(hwframe_transfer_new(&src).is_err());
        let mut dst = Frame::new().unwrap();
        assert!(hwframe_transfer_into(&mut dst, &src).is_err());
    }
}
