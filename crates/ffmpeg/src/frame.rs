//! `AVFrame` with borrow-checked plane access.

use core::ffi::c_int;

use crate::{AVPixelFormat, AVSampleFormat, Error, Result};

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

// SAFETY: AVFrame buffers are refcounted through atomics, and a `Frame` is a
// unique owner (no `Clone`; `new_ref` allocates a second frame). Moving one to
// another thread hands over that sole ownership.
unsafe impl Send for Frame {}

impl Frame {
    /// `av_frame_alloc`.
    pub fn new() -> Result<Self> {
        // SAFETY: no arguments; returns a zeroed frame or null.
        let ptr = unsafe { ffmpeg_sys_next::av_frame_alloc() };
        if ptr.is_null() {
            return Err(Error::nomem());
        }
        Ok(Self(ptr))
    }

    /// A fresh frame with its own `av_frame_get_buffer` allocation. Only used
    /// by tests and callers that need a scratch destination; the decode path
    /// never allocates frame data itself.
    pub fn alloc_video(fmt: AVPixelFormat, width: i32, height: i32) -> Result<Self> {
        let frame = Self::new()?;
        // SAFETY: `frame.0` is a fresh frame with no buffers attached, so
        // writing its parameters and calling av_frame_get_buffer is the
        // documented allocation sequence.
        unsafe {
            (*frame.0).format = fmt as c_int;
            (*frame.0).width = width;
            (*frame.0).height = height;
        }
        // SAFETY: as above; align 0 lets FFmpeg pick its own alignment.
        Error::check(unsafe { ffmpeg_sys_next::av_frame_get_buffer(frame.0, 0) })?;
        Ok(frame)
    }

    /// New frame referencing the same buffers (`av_frame_ref`).
    pub fn new_ref(&self) -> Result<Self> {
        let dst = Self::new()?;
        // SAFETY: `dst.0` is a blank frame and `self.0` a live one; av_frame_ref
        // copies the properties and takes a reference on each buffer.
        Error::check(unsafe { ffmpeg_sys_next::av_frame_ref(dst.0, self.0) })?;
        Ok(dst)
    }

    /// `av_frame_unref`.
    pub fn unref(&mut self) {
        // SAFETY: `self.0` is live; av_frame_unref returns it to the blank state.
        unsafe { ffmpeg_sys_next::av_frame_unref(self.0) };
    }

    // ── video ──

    pub fn width(&self) -> i32 {
        // SAFETY: `self.0` is a live frame we own.
        unsafe { (*self.0).width }
    }

    pub fn height(&self) -> i32 {
        // SAFETY: as above.
        unsafe { (*self.0).height }
    }

    pub fn pix_fmt(&self) -> AVPixelFormat {
        // SAFETY: as above.
        crate::pix_fmt_from_raw(unsafe { (*self.0).format })
    }

    /// Restore display dimensions after a transfer into a 16-aligned pool frame.
    pub fn set_display_size(&mut self, width: i32, height: i32) {
        // SAFETY: as above; this only narrows the advertised size, exactly as
        // FFmpeg's own transfer_data_alloc() does after allocating padded.
        unsafe {
            (*self.0).width = width;
            (*self.0).height = height;
        }
    }

    /// `AV_FRAME_FLAG_KEY`.
    pub fn is_key(&self) -> bool {
        // SAFETY: as above.
        (unsafe { (*self.0).flags } & ffmpeg_sys_next::AV_FRAME_FLAG_KEY) != 0
    }

    /// `AV_FRAME_FLAG_CORRUPT`.
    pub fn is_corrupt(&self) -> bool {
        // SAFETY: as above.
        (unsafe { (*self.0).flags } & ffmpeg_sys_next::AV_FRAME_FLAG_CORRUPT) != 0
    }

    pub fn decode_error_flags(&self) -> i32 {
        // SAFETY: as above.
        unsafe { (*self.0).decode_error_flags }
    }

    /// `hw_frames_ctx != NULL`.
    pub fn is_hw(&self) -> bool {
        // SAFETY: as above.
        !unsafe { (*self.0).hw_frames_ctx }.is_null()
    }

    /// `((AVHWFramesContext*)hw_frames_ctx->data)->sw_format`.
    pub fn hw_sw_format(&self) -> Option<AVPixelFormat> {
        // SAFETY: `self.0` is live; `hw_frames_ctx` is either null or an
        // AVBufferRef whose `data` is an AVHWFramesContext, which is the only
        // thing libavutil ever puts there.
        unsafe {
            let ctx = (*self.0).hw_frames_ctx;
            if ctx.is_null() {
                return None;
            }
            let hwfc = (*ctx).data as *const ffmpeg_sys_next::AVHWFramesContext;
            if hwfc.is_null() {
                return None;
            }
            Some((*hwfc).sw_format)
        }
    }

    /// `av_image_get_buffer_size(format, width, height, 1)`.
    pub fn image_buffer_size(&self) -> Result<usize> {
        crate::image_buffer_size(self.pix_fmt(), self.width(), self.height(), 1)
    }

    /// Copy pts, colour properties and flags from `src` (used by the pooled
    /// transfer destination and the swscale destination wrapper).
    pub fn copy_video_props_from(&mut self, src: &Frame) {
        // SAFETY: both pointers are live frames we own; these are all plain
        // scalar/enum fields with no ownership attached.
        unsafe {
            (*self.0).pts = (*src.0).pts;
            (*self.0).colorspace = (*src.0).colorspace;
            (*self.0).color_range = (*src.0).color_range;
            (*self.0).color_primaries = (*src.0).color_primaries;
            (*self.0).color_trc = (*src.0).color_trc;
            (*self.0).chroma_location = (*src.0).chroma_location;
            (*self.0).flags = (*src.0).flags;
        }
    }

    /// Borrow plane `index`. `None` when the plane pointer is null **or the
    /// linesize is not positive** — the negative-stride case the C plugin
    /// routes to swscale — so the caller's format check lands in one place.
    /// Length is `linesize * plane_height(index)`, with the chroma height
    /// derived from `av_pix_fmt_desc_get(format)->log2_chroma_h`.
    pub fn plane(&self, index: usize) -> Option<&[u8]> {
        if index >= ffmpeg_sys_next::AV_NUM_DATA_POINTERS as usize {
            return None;
        }
        // SAFETY: `self.0` is live and `index` is inside the fixed-size arrays.
        let (data, linesize) = unsafe { ((*self.0).data[index], (*self.0).linesize[index]) };
        if data.is_null() || linesize <= 0 {
            return None;
        }
        let height = self.plane_height(index)?;
        if height <= 0 {
            return None;
        }
        let len = (linesize as usize).checked_mul(height as usize)?;
        // SAFETY: `data` is the start of a plane FFmpeg allocated with at least
        // `linesize * plane_height` addressable bytes (that is the layout
        // av_image_fill_arrays / av_frame_get_buffer produce, and what every
        // decoder writes), it stays alive and unaliased for the borrow of
        // `self`, and `u8` has no alignment or validity requirements.
        Some(unsafe { core::slice::from_raw_parts(data, len) })
    }

    /// Row count of plane `index`, or `None` when the format has no such plane.
    ///
    /// Vertical subsampling applies to the chroma components only, so the
    /// shift is taken from the components that actually live on this plane:
    /// luma (component 0) and alpha (component 3) are full height, chroma
    /// (components 1 and 2) are `ceil(height / 2^log2_chroma_h)`. That covers
    /// planar YUV (separate U and V planes), semi-planar NV12/P010 (both
    /// chroma components on plane 1) and packed/RGB (one plane) uniformly.
    fn plane_height(&self, index: usize) -> Option<i32> {
        let fmt = self.pix_fmt();
        // SAFETY: av_pix_fmt_count_planes accepts any AVPixelFormat.
        let planes = unsafe { ffmpeg_sys_next::av_pix_fmt_count_planes(fmt) };
        if planes <= 0 || index >= planes as usize {
            return None;
        }
        // SAFETY: av_pix_fmt_desc_get accepts any AVPixelFormat and returns a
        // pointer into libavutil's static descriptor table, or null.
        let desc = unsafe { ffmpeg_sys_next::av_pix_fmt_desc_get(fmt) };
        if desc.is_null() {
            return None;
        }
        // SAFETY: `desc` is a live static descriptor; `nb_components` is at
        // most 4, the length of `comp`.
        let (nb_components, log2_chroma_h, comp) =
            unsafe { ((*desc).nb_components as usize, (*desc).log2_chroma_h, (*desc).comp) };

        let mut full_height = index == 0;
        let mut found = index == 0;
        for (c, component) in comp.iter().enumerate().take(nb_components.min(4)) {
            if component.plane as usize != index {
                continue;
            }
            found = true;
            if c == 0 || c == 3 {
                full_height = true;
            }
        }
        if !found {
            return None;
        }

        let height = self.height();
        if full_height || log2_chroma_h == 0 {
            Some(height)
        } else {
            // Round up: an odd height still has a chroma row for the last line.
            Some(-((-height) >> log2_chroma_h))
        }
    }

    pub fn plane_linesize(&self, index: usize) -> i32 {
        if index >= ffmpeg_sys_next::AV_NUM_DATA_POINTERS as usize {
            return 0;
        }
        // SAFETY: `self.0` is live and `index` is inside the fixed-size array.
        unsafe { (*self.0).linesize[index] }
    }

    /// Number of planes with a non-null data pointer (max `AV_NUM_DATA_POINTERS`).
    pub fn plane_count(&self) -> usize {
        let fmt = self.pix_fmt();
        // SAFETY: accepts any AVPixelFormat.
        let planes = unsafe { ffmpeg_sys_next::av_pix_fmt_count_planes(fmt) };
        if planes <= 0 {
            return 0;
        }
        let planes = (planes as usize).min(ffmpeg_sys_next::AV_NUM_DATA_POINTERS as usize);
        // SAFETY: `self.0` is live and every index is inside `data`.
        (0..planes).take_while(|&i| !unsafe { (*self.0).data[i] }.is_null()).count()
    }

    pub fn colorimetry(&self) -> Colorimetry {
        // SAFETY: `self.0` is live; all five are plain enum fields.
        unsafe {
            Colorimetry {
                colorspace: (*self.0).colorspace,
                color_range: (*self.0).color_range,
                color_primaries: (*self.0).color_primaries,
                color_trc: (*self.0).color_trc,
                chroma_location: (*self.0).chroma_location,
            }
        }
    }

    // ── audio ──

    pub fn nb_samples(&self) -> i32 {
        // SAFETY: `self.0` is live.
        unsafe { (*self.0).nb_samples }
    }

    pub fn sample_rate(&self) -> i32 {
        // SAFETY: as above.
        unsafe { (*self.0).sample_rate }
    }

    /// `ch_layout.nb_channels`.
    pub fn channels(&self) -> i32 {
        // SAFETY: as above.
        unsafe { (*self.0).ch_layout.nb_channels }
    }

    pub fn sample_format(&self) -> AVSampleFormat {
        // SAFETY: as above.
        crate::sample_fmt_from_raw(unsafe { (*self.0).format })
    }

    pub fn duration(&self) -> i64 {
        // SAFETY: as above.
        unsafe { (*self.0).duration }
    }

    /// `best_effort_timestamp`, else `pts`; `None` for `AV_NOPTS_VALUE`.
    pub fn best_effort_pts(&self) -> Option<i64> {
        // SAFETY: as above.
        let (best, pts) = unsafe { ((*self.0).best_effort_timestamp, (*self.0).pts) };
        let value = if best != ffmpeg_sys_next::AV_NOPTS_VALUE { best } else { pts };
        (value != ffmpeg_sys_next::AV_NOPTS_VALUE).then_some(value)
    }

    /// `data[0]` as `nb_samples * channels * 4` bytes when the format is
    /// already interleaved float (`AV_SAMPLE_FMT_FLT`); `None` otherwise.
    pub fn interleaved_f32_bytes(&self) -> Option<&[u8]> {
        if self.sample_format() != AVSampleFormat::AV_SAMPLE_FMT_FLT {
            return None;
        }
        // SAFETY: `self.0` is live.
        let data = unsafe { (*self.0).data[0] };
        if data.is_null() {
            return None;
        }
        let samples = usize::try_from(self.nb_samples()).ok()?;
        let channels = usize::try_from(self.channels()).ok()?;
        let len = samples.checked_mul(channels)?.checked_mul(4)?;
        // SAFETY: a packed (non-planar) sample format puts every channel in
        // plane 0, so the decoder allocated at least nb_samples * channels
        // floats there; the slice borrows `self` and `u8` has no validity or
        // alignment requirements.
        Some(unsafe { core::slice::from_raw_parts(data, len) })
    }

    // ── common ──

    pub fn pts(&self) -> i64 {
        // SAFETY: `self.0` is live.
        unsafe { (*self.0).pts }
    }

    pub fn set_pts(&mut self, pts: i64) {
        // SAFETY: as above; `pts` is a plain scalar field.
        unsafe { (*self.0).pts = pts };
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
        // SAFETY: `&mut self.0` is our sole owning pointer; av_frame_free
        // unrefs the buffers first and nulls it.
        unsafe { ffmpeg_sys_next::av_frame_free(&mut self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuv420p_planes_have_luma_and_chroma_heights() {
        let frame = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_YUV420P, 64, 48).unwrap();
        assert_eq!(frame.plane_count(), 3);

        let y_stride = frame.plane_linesize(0);
        assert!(y_stride >= 64);
        assert_eq!(frame.plane(0).unwrap().len(), y_stride as usize * 48);

        for i in 1..3 {
            let stride = frame.plane_linesize(i);
            assert!(stride >= 32);
            assert_eq!(frame.plane(i).unwrap().len(), stride as usize * 24);
        }
        assert!(frame.plane(3).is_none());
        assert!(frame.plane(9).is_none());
    }

    #[test]
    fn nv12_chroma_plane_is_half_height() {
        let frame = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_NV12, 32, 32).unwrap();
        assert_eq!(frame.plane_count(), 2);
        let stride = frame.plane_linesize(1);
        assert_eq!(frame.plane(1).unwrap().len(), stride as usize * 16);
        assert!(frame.plane(2).is_none());
    }

    #[test]
    fn odd_height_rounds_chroma_up() {
        let frame = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_YUV420P, 16, 17).unwrap();
        let stride = frame.plane_linesize(1);
        // ceil(17 / 2) == 9
        assert_eq!(frame.plane(1).unwrap().len(), stride as usize * 9);
    }

    #[test]
    fn packed_rgb_has_one_full_height_plane() {
        let frame = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_BGRA, 20, 10).unwrap();
        assert_eq!(frame.plane_count(), 1);
        let stride = frame.plane_linesize(0);
        assert_eq!(frame.plane(0).unwrap().len(), stride as usize * 10);
        assert!(frame.plane(1).is_none());
    }

    #[test]
    fn yuv444p_planes_are_all_full_height() {
        let frame = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_YUV444P, 24, 18).unwrap();
        for i in 0..3 {
            let stride = frame.plane_linesize(i);
            assert_eq!(frame.plane(i).unwrap().len(), stride as usize * 18);
        }
    }

    #[test]
    fn blank_frame_lends_nothing() {
        let frame = Frame::new().unwrap();
        assert!(frame.plane(0).is_none());
        assert_eq!(frame.plane_count(), 0);
        assert_eq!(frame.plane_linesize(0), 0);
        assert!(!frame.is_hw());
        assert!(frame.hw_sw_format().is_none());
        assert!(frame.best_effort_pts().is_none());
        assert!(frame.interleaved_f32_bytes().is_none());
    }

    #[test]
    fn ref_shares_buffers_and_props() {
        let mut src = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_NV12, 16, 16).unwrap();
        src.set_pts(4242);
        let copy = src.new_ref().unwrap();
        assert_eq!(copy.pts(), 4242);
        assert_eq!(copy.plane(0).unwrap().as_ptr(), src.plane(0).unwrap().as_ptr());
        drop(src);
        // The reference keeps the buffer alive.
        assert_eq!(copy.plane(0).unwrap().len(), copy.plane_linesize(0) as usize * 16);
    }

    #[test]
    fn unref_clears_the_frame() {
        let mut frame = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_NV12, 16, 16).unwrap();
        assert!(frame.plane(0).is_some());
        frame.unref();
        assert!(frame.plane(0).is_none());
    }

    #[test]
    fn video_props_copy_across() {
        let mut src = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_NV12, 16, 16).unwrap();
        src.set_pts(99);
        // SAFETY: test-local frame we own; setting plain enum fields.
        unsafe {
            (*src.as_mut_ptr()).colorspace = ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT709;
            (*src.as_mut_ptr()).color_range = ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_JPEG;
        }
        let mut dst = Frame::new().unwrap();
        dst.copy_video_props_from(&src);
        assert_eq!(dst.pts(), 99);
        assert_eq!(dst.colorimetry(), src.colorimetry());
    }

    #[test]
    fn image_buffer_size_matches_dimensions() {
        let frame = Frame::alloc_video(AVPixelFormat::AV_PIX_FMT_YUV420P, 32, 32).unwrap();
        assert_eq!(frame.image_buffer_size().unwrap(), 32 * 32 * 3 / 2);
    }
}
