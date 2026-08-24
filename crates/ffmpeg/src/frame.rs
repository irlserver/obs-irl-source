//! `AVFrame` with borrow-checked plane access.

use crate::{AVPixelFormat, AVSampleFormat, Result};

/// Colour metadata copied off a decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colorimetry {
    pub colorspace: ffmpeg_sys_next::AVColorSpace,
    pub color_range: ffmpeg_sys_next::AVColorRange,
    pub color_primaries: ffmpeg_sys_next::AVColorPrimaries,
    pub color_trc: ffmpeg_sys_next::AVColorTransferCharacteristic,
    pub chroma_location: ffmpeg_sys_next::AVChromaLocation,
}

/// An owned `AVFrame*`. Refcounted buffers make it safe to move between
/// threads; it carries no borrow of any format/codec context by design (the
/// receiver thread may close those while frames are still queued).
pub struct Frame(*mut ffmpeg_sys_next::AVFrame);

unsafe impl Send for Frame {}

impl Frame {
    /// `av_frame_alloc`.
    pub fn new() -> Result<Self> {
        todo!("W1-B")
    }

    /// New frame referencing the same buffers (`av_frame_ref`).
    pub fn new_ref(&self) -> Result<Self> {
        todo!("W1-B")
    }

    /// `av_frame_unref`.
    pub fn unref(&mut self) {
        todo!("W1-B")
    }

    // ── video ──

    pub fn width(&self) -> i32 {
        todo!("W1-B")
    }

    pub fn height(&self) -> i32 {
        todo!("W1-B")
    }

    pub fn pix_fmt(&self) -> AVPixelFormat {
        todo!("W1-B")
    }

    /// Restore display dimensions after a transfer into a 16-aligned pool frame.
    pub fn set_display_size(&mut self, width: i32, height: i32) {
        let _ = (width, height);
        todo!("W1-B")
    }

    /// `AV_FRAME_FLAG_KEY`.
    pub fn is_key(&self) -> bool {
        todo!("W1-B")
    }

    /// `AV_FRAME_FLAG_CORRUPT`.
    pub fn is_corrupt(&self) -> bool {
        todo!("W1-B")
    }

    pub fn decode_error_flags(&self) -> i32 {
        todo!("W1-B")
    }

    /// `hw_frames_ctx != NULL`.
    pub fn is_hw(&self) -> bool {
        todo!("W1-B")
    }

    /// `((AVHWFramesContext*)hw_frames_ctx->data)->sw_format`.
    pub fn hw_sw_format(&self) -> Option<AVPixelFormat> {
        todo!("W1-B")
    }

    /// `av_image_get_buffer_size(format, width, height, 1)`.
    pub fn image_buffer_size(&self) -> Result<usize> {
        todo!("W1-B")
    }

    /// Copy pts, colour properties and flags from `src` (used by the pooled
    /// transfer destination and the swscale destination wrapper).
    pub fn copy_video_props_from(&mut self, src: &Frame) {
        let _ = src;
        todo!("W1-B")
    }

    /// Borrow plane `index`. `None` when the plane pointer is null **or the
    /// linesize is not positive** — the negative-stride case the C plugin
    /// routes to swscale — so the caller's format check lands in one place.
    /// Length is `linesize * plane_height(index)`, with the chroma height
    /// derived from `av_pix_fmt_desc_get(format)->log2_chroma_h`.
    pub fn plane(&self, index: usize) -> Option<&[u8]> {
        let _ = index;
        todo!("W1-B")
    }

    pub fn plane_linesize(&self, index: usize) -> i32 {
        let _ = index;
        todo!("W1-B")
    }

    /// Number of planes with a non-null data pointer (max `AV_NUM_DATA_POINTERS`).
    pub fn plane_count(&self) -> usize {
        todo!("W1-B")
    }

    pub fn colorimetry(&self) -> Colorimetry {
        todo!("W1-B")
    }

    // ── audio ──

    pub fn nb_samples(&self) -> i32 {
        todo!("W1-B")
    }

    pub fn sample_rate(&self) -> i32 {
        todo!("W1-B")
    }

    /// `ch_layout.nb_channels`.
    pub fn channels(&self) -> i32 {
        todo!("W1-B")
    }

    pub fn sample_format(&self) -> AVSampleFormat {
        todo!("W1-B")
    }

    pub fn duration(&self) -> i64 {
        todo!("W1-B")
    }

    /// `best_effort_timestamp`, else `pts`; `None` for `AV_NOPTS_VALUE`.
    pub fn best_effort_pts(&self) -> Option<i64> {
        todo!("W1-B")
    }

    /// `data[0]` as `nb_samples * channels * 4` bytes when the format is
    /// already interleaved float (`AV_SAMPLE_FMT_FLT`); `None` otherwise.
    pub fn interleaved_f32_bytes(&self) -> Option<&[u8]> {
        todo!("W1-B")
    }

    // ── common ──

    pub fn pts(&self) -> i64 {
        todo!("W1-B")
    }

    pub fn set_pts(&mut self, pts: i64) {
        let _ = pts;
        todo!("W1-B")
    }

    #[doc(hidden)]
    pub fn as_ptr(&self) -> *const ffmpeg_sys_next::AVFrame {
        self.0
    }

    #[doc(hidden)]
    pub fn as_mut_ptr(&mut self) -> *mut ffmpeg_sys_next::AVFrame {
        self.0
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        todo!("W1-B: av_frame_free")
    }
}
