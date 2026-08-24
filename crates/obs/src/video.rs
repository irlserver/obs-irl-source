//! Async video output.

use core::marker::PhantomData;

/// The subset of `enum video_format` an FFmpeg-fed source produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFormat {
    None,
    I420,
    Nv12,
    Yuy2,
    Uyvy,
    Rgba,
    Bgra,
    I444,
    I422,
    I010,
    P010,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    Default,
    Bt601,
    Bt709,
    Srgb,
    Pq2100,
    Hlg2100,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRange {
    Default,
    Partial,
    Full,
}

/// An `obs_source_frame` under construction, borrowing its planes.
///
/// The borrow is what makes the zero-copy path sound: libobs copies inside
/// `obs_source_output_video`, and `'a` ends at that call, so the decoded frame
/// cannot be released while OBS still reads it.
pub struct VideoFrame<'a> {
    inner: obs_sys::obs_source_frame,
    _planes: PhantomData<&'a [u8]>,
}

impl<'a> VideoFrame<'a> {
    pub fn new(width: u32, height: u32, format: VideoFormat) -> Self {
        let _ = (width, height, format);
        todo!("W1-A: zeroed obs_source_frame with width/height/format")
    }

    /// Attach plane `index` (0..8). `linesize` is the row stride in bytes.
    pub fn plane(self, index: usize, data: &'a [u8], linesize: u32) -> Self {
        let _ = (index, data, linesize);
        todo!("W1-A")
    }

    pub fn timestamp(self, ns: u64) -> Self {
        let _ = ns;
        todo!("W1-A")
    }

    /// Fill `color_matrix`, `color_range_min/max` and `full_range` through
    /// `video_format_get_parameters_for_format` (the C `setup_color_params`).
    /// Falls back to BT.709 partial range when libobs rejects the combination.
    pub fn colorimetry(self, cs: ColorSpace, range: ColorRange) -> Self {
        let _ = (cs, range);
        todo!("W1-A")
    }

    #[doc(hidden)]
    pub fn as_sys(&self) -> &obs_sys::obs_source_frame {
        &self.inner
    }
}
