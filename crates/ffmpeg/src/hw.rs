//! Hardware device contexts and the pooled GPU→CPU transfer.

use crate::frame::Frame;
use crate::{AVHWDeviceType, AVPixelFormat, Error, Result};

/// An `AVBufferRef` holding an `AVHWDeviceContext`.
pub struct HwDeviceContext {
    ptr: *mut ffmpeg_sys_next::AVBufferRef,
    kind: AVHWDeviceType,
}

unsafe impl Send for HwDeviceContext {}

impl HwDeviceContext {
    /// Try `av_hwdevice_ctx_create(type, NULL, NULL, 0)` for each type in
    /// order; the first success wins. `on_fail` is called for each failure so
    /// the caller can log it the way the C plugin does.
    pub fn probe(types: &[AVHWDeviceType], on_fail: &mut dyn FnMut(AVHWDeviceType, Error)) -> Option<Self> {
        let _ = (types, on_fail);
        todo!("W1-B")
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
        todo!("W1-B: av_buffer_unref")
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

unsafe impl Send for FramePool {}

impl FramePool {
    /// Returns the pool and the per-frame byte size (for the log line).
    pub fn new(fmt: AVPixelFormat, src_width: i32, src_height: i32) -> Result<(Self, usize)> {
        let _ = (fmt, src_width, src_height);
        todo!("W1-B")
    }

    pub fn matches(&self, fmt: AVPixelFormat, src_width: i32, src_height: i32) -> bool {
        let _ = (fmt, src_width, src_height, self.size);
        todo!("W1-B")
    }

    /// `av_buffer_pool_get` + `av_image_fill_arrays`; the buffer becomes
    /// `frame.buf[0]` so normal refcounting returns it to the pool.
    pub fn acquire(&self) -> Result<Frame> {
        let _ = (self.pool, self.fmt, self.width, self.height);
        todo!("W1-B")
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        todo!("W1-B: av_buffer_pool_uninit (pool lingers until the last buffer returns)")
    }
}

/// `av_hwframe_transfer_data(dst, src, 0)` into a caller-allocated `dst`.
pub fn hwframe_transfer_into(dst: &mut Frame, src: &Frame) -> Result<()> {
    let _ = (dst, src);
    todo!("W1-B")
}

/// `av_hwframe_transfer_data` into a fresh frame FFmpeg allocates (fallback
/// for backends that reject caller-allocated destinations).
pub fn hwframe_transfer_new(src: &Frame) -> Result<Frame> {
    let _ = src;
    todo!("W1-B")
}
