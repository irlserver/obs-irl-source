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

use crate::frame::Frame;
use crate::Result;

pub struct Scaler {
    ptr: *mut ffmpeg_sys_next::SwsContext,
    dst: Frame,
}

unsafe impl Send for Scaler {}

impl Scaler {
    /// `unstable` sets `SWS_UNSTABLE` (the C plugin gates it on the
    /// `IRL_SWS_UNSTABLE` environment variable).
    pub fn new(unstable: bool) -> Result<Self> {
        let _ = unstable;
        todo!("W1-B")
    }

    /// Convert `src` into NV12 planes `y` (stride `stride`) and `uv` (same
    /// stride, half height). Colour properties are copied from `src` so no
    /// colourspace conversion happens.
    pub fn scale_into_nv12(&mut self, src: &Frame, y: &mut [u8], uv: &mut [u8], stride: i32) -> Result<()> {
        let _ = (src, y, uv, stride, &mut self.dst, self.ptr);
        todo!("W1-B")
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        todo!("W1-B: sws_free_context; dst is a wrapper and is freed by Frame::drop")
    }
}
