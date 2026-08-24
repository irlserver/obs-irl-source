//! RAII wrappers over `ffmpeg-sys-next` for exactly what an OBS live-ingest
//! source needs: demux with an interrupt callback, decode (software or through
//! a hardware device context), hardware→system frame transfer into a pooled
//! frame, swresample (including compensation-based speed control) and
//! swscale (`sws_scale_frame` on the public `SwsContext`).
//!
//! Every `unsafe` FFmpeg call in the plugin lives in this crate. The API is
//! deliberately close to the C surface (the C plugin is the behavioral spec),
//! but ownership is explicit: `Frame`, `Packet`, `CodecContext`,
//! `FormatContext`, `HwDeviceContext`, `FramePool`, `Resampler` and `Scaler`
//! free their FFmpeg objects in `Drop`.
//!
//! Time domains: [`gettime_us`] is `av_gettime()` (microseconds) and is used
//! only for FFmpeg-side timers (interrupt watch, decoder cooldowns). OBS
//! timestamps come from the `obs` crate's clock; never mix the two.

pub mod codec;
pub mod dict;
pub mod format;
pub mod frame;
pub mod hw;
pub mod packet;
pub mod swr;
pub mod sws;

pub use codec::{Codec, CodecBuilder, CodecContext, GetFormatFn, HwConfig};
pub use dict::Dictionary;
pub use format::{FormatContext, InterruptWatch, MediaType, StreamRef};
pub use frame::{Colorimetry, Frame};
pub use hw::{FramePool, HwDeviceContext, hwframe_transfer_into, hwframe_transfer_new};
pub use packet::Packet;
pub use swr::Resampler;
pub use sws::Scaler;

/// Raw bindings, re-exported for enum/constant names (`AVPixelFormat`,
/// `AVCodecID`, `AVHWDeviceType`, ...). Plugin crates must not call functions
/// through this; they carry `#![forbid(unsafe_code)]`.
pub use ffmpeg_sys_next as sys;
pub use ffmpeg_sys_next::{AVCodecID, AVHWDeviceType, AVPixelFormat, AVSampleFormat};

use core::ffi::c_int;
use core::fmt;

/// An FFmpeg error code (negative `AVERROR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub c_int);

pub type Result<T> = core::result::Result<T, Error>;

impl Error {
    pub fn is_eagain(&self) -> bool {
        todo!("W1-B: self.0 == AVERROR(EAGAIN)")
    }

    pub fn is_eof(&self) -> bool {
        todo!("W1-B: self.0 == AVERROR_EOF")
    }

    /// `AVERROR(ERANGE)` and friends, for option-setting diagnostics.
    pub fn code(&self) -> c_int {
        self.0
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = f;
        todo!("W1-B: av_strerror into a stack buffer")
    }
}

impl std::error::Error for Error {}

/// `AVRational`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rational {
    pub num: i32,
    pub den: i32,
}

impl Rational {
    pub const fn new(num: i32, den: i32) -> Self {
        Self { num, den }
    }
}

/// `av_rescale_q`.
pub fn rescale_q(value: i64, from: Rational, to: Rational) -> i64 {
    let _ = (value, from, to);
    todo!("W1-B")
}

/// `av_rescale_q_rnd(..., AV_ROUND_UP)`.
pub fn rescale_q_round_up(value: i64, from: Rational, to: Rational) -> i64 {
    let _ = (value, from, to);
    todo!("W1-B")
}

/// `av_rescale`.
pub fn rescale(value: i64, mul: i64, div: i64) -> i64 {
    let _ = (value, mul, div);
    todo!("W1-B")
}

/// `av_gettime()` in microseconds. FFmpeg-side timers only.
pub fn gettime_us() -> i64 {
    todo!("W1-B")
}

/// `av_usleep`.
pub fn usleep(us: u32) {
    let _ = us;
    todo!("W1-B")
}

/// `avcodec_get_name`.
pub fn codec_name(id: AVCodecID) -> &'static str {
    let _ = id;
    todo!("W1-B")
}

/// `av_hwdevice_get_type_name`.
pub fn hwdevice_type_name(kind: AVHWDeviceType) -> &'static str {
    let _ = kind;
    todo!("W1-B")
}

/// `av_get_pix_fmt_name` (for log lines).
pub fn pix_fmt_name(fmt: AVPixelFormat) -> &'static str {
    let _ = fmt;
    todo!("W1-B")
}

/// `av_image_get_buffer_size(fmt, w, h, align)`.
pub fn image_buffer_size(fmt: AVPixelFormat, width: i32, height: i32, align: i32) -> Result<usize> {
    let _ = (fmt, width, height, align);
    todo!("W1-B")
}

/// The library versions the crate was built against (`LIBAVCODEC_VERSION_*`),
/// for the connection log line.
pub fn version_string() -> String {
    todo!("W1-B")
}
