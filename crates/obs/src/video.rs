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

impl VideoFormat {
    #[must_use]
    pub fn to_sys(self) -> obs_sys::video_format {
        use obs_sys::video_format as F;
        match self {
            Self::None => F::VIDEO_FORMAT_NONE,
            Self::I420 => F::VIDEO_FORMAT_I420,
            Self::Nv12 => F::VIDEO_FORMAT_NV12,
            Self::Yuy2 => F::VIDEO_FORMAT_YUY2,
            Self::Uyvy => F::VIDEO_FORMAT_UYVY,
            Self::Rgba => F::VIDEO_FORMAT_RGBA,
            Self::Bgra => F::VIDEO_FORMAT_BGRA,
            Self::I444 => F::VIDEO_FORMAT_I444,
            Self::I422 => F::VIDEO_FORMAT_I422,
            Self::I010 => F::VIDEO_FORMAT_I010,
            Self::P010 => F::VIDEO_FORMAT_P010,
        }
    }
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

impl ColorSpace {
    #[must_use]
    pub fn to_sys(self) -> obs_sys::video_colorspace {
        use obs_sys::video_colorspace as C;
        match self {
            Self::Default => C::VIDEO_CS_DEFAULT,
            Self::Bt601 => C::VIDEO_CS_601,
            Self::Bt709 => C::VIDEO_CS_709,
            Self::Srgb => C::VIDEO_CS_SRGB,
            Self::Pq2100 => C::VIDEO_CS_2100_PQ,
            Self::Hlg2100 => C::VIDEO_CS_2100_HLG,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRange {
    Default,
    Partial,
    Full,
}

impl ColorRange {
    #[must_use]
    pub fn to_sys(self) -> obs_sys::video_range_type {
        use obs_sys::video_range_type as R;
        match self {
            Self::Default => R::VIDEO_RANGE_DEFAULT,
            Self::Partial => R::VIDEO_RANGE_PARTIAL,
            Self::Full => R::VIDEO_RANGE_FULL,
        }
    }
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
    #[must_use]
    pub fn new(width: u32, height: u32, format: VideoFormat) -> Self {
        // `struct obs_source_frame` has no niche-carrying member (raw
        // pointers, integers, floats, bools and one `volatile long` libobs
        // resets itself), so an all-zero value is a valid one — the same
        // `memset(&frame, 0, sizeof frame)` the C plugin does.
        let mut inner: obs_sys::obs_source_frame = unsafe { core::mem::zeroed() };
        inner.width = width;
        inner.height = height;
        inner.format = format.to_sys();
        Self {
            inner,
            _planes: PhantomData,
        }
    }

    /// Attach plane `index` (0..8). `linesize` is the row stride in bytes.
    ///
    /// # Panics
    /// If `index` is not a valid plane index.
    #[must_use]
    pub fn plane(mut self, index: usize, data: &'a [u8], linesize: u32) -> Self {
        assert!(
            index < obs_sys::MAX_AV_PLANES,
            "plane index {index} out of range"
        );
        // libobs types the plane as `uint8_t *` but only ever reads it (it
        // copies the frame inside obs_source_output_video), so casting away
        // the shared reference's constness is sound; `'a` keeps the buffer
        // alive until the output call returns.
        self.inner.data[index] = data.as_ptr().cast_mut();
        self.inner.linesize[index] = linesize;
        self
    }

    #[must_use]
    pub fn timestamp(mut self, ns: u64) -> Self {
        self.inner.timestamp = ns;
        self
    }

    /// Fill `color_matrix`, `color_range_min/max` and `full_range` through
    /// `video_format_get_parameters_for_format` (the C `setup_color_params`).
    /// Falls back to BT.709 partial range when libobs rejects the combination.
    #[must_use]
    pub fn colorimetry(mut self, cs: ColorSpace, range: ColorRange) -> Self {
        self.inner.full_range = range == ColorRange::Full;

        let format = self.inner.format;
        // SAFETY: the three out-parameters are the frame's own fixed-size
        // arrays (16 / 3 / 3 floats), exactly the widths libobs writes.
        let ok = unsafe {
            obs_sys::video_format_get_parameters_for_format(
                cs.to_sys(),
                range.to_sys(),
                format,
                self.inner.color_matrix.as_mut_ptr(),
                self.inner.color_range_min.as_mut_ptr(),
                self.inner.color_range_max.as_mut_ptr(),
            )
        };

        if !ok {
            // libobs leaves the arrays untouched when it does not know the
            // combination; BT.709 limited range is what every OBS source
            // falls back to, and is what an unknown stream most likely is.
            // SAFETY: as above.
            unsafe {
                obs_sys::video_format_get_parameters_for_format(
                    obs_sys::video_colorspace::VIDEO_CS_709,
                    obs_sys::video_range_type::VIDEO_RANGE_PARTIAL,
                    format,
                    self.inner.color_matrix.as_mut_ptr(),
                    self.inner.color_range_min.as_mut_ptr(),
                    self.inner.color_range_max.as_mut_ptr(),
                )
            };
        }

        self
    }

    #[doc(hidden)]
    pub fn as_sys(&self) -> &obs_sys::obs_source_frame {
        &self.inner
    }
}
