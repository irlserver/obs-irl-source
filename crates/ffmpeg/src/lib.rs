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

use core::ffi::{CStr, c_char, c_int};
use core::fmt;

/// An FFmpeg error code (negative `AVERROR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub c_int);

pub type Result<T> = core::result::Result<T, Error>;

impl Error {
    pub fn is_eagain(&self) -> bool {
        self.0 == sys::AVERROR(sys::EAGAIN)
    }

    pub fn is_eof(&self) -> bool {
        self.0 == sys::AVERROR_EOF
    }

    /// `AVERROR(ERANGE)` and friends, for option-setting diagnostics.
    pub fn code(&self) -> c_int {
        self.0
    }

    /// `AVERROR(ENOMEM)` — every allocation failure in this crate.
    pub(crate) const fn nomem() -> Self {
        Error(-(sys::ENOMEM as c_int))
    }

    /// `AVERROR(EINVAL)` — a caller-side contract violation this crate catches
    /// before it can reach FFmpeg (a scratch buffer that is too small, say).
    pub(crate) const fn inval() -> Self {
        Error(-(sys::EINVAL as c_int))
    }

    /// Wrap a raw return code: `Ok(())` for `>= 0`, `Err` otherwise.
    pub(crate) fn check(ret: c_int) -> Result<()> {
        if ret < 0 { Err(Error(ret)) } else { Ok(()) }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0 as c_char; sys::AV_ERROR_MAX_STRING_SIZE];
        // SAFETY: `buf` is a valid writable buffer of exactly the length we
        // pass; av_strerror NUL-terminates within it (falling back to a
        // generic message for unknown codes) and never reads past it.
        let ok = unsafe { sys::av_strerror(self.0, buf.as_mut_ptr(), buf.len()) } == 0;
        if ok {
            // SAFETY: av_strerror returned 0, so `buf` holds a NUL-terminated
            // string entirely inside the buffer.
            let msg = unsafe { CStr::from_ptr(buf.as_ptr()) };
            f.write_str(&msg.to_string_lossy())
        } else {
            write!(f, "error {}", self.0)
        }
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

impl From<Rational> for sys::AVRational {
    fn from(r: Rational) -> Self {
        sys::AVRational {
            num: r.num,
            den: r.den,
        }
    }
}

impl From<sys::AVRational> for Rational {
    fn from(r: sys::AVRational) -> Self {
        Rational {
            num: r.num,
            den: r.den,
        }
    }
}

/// `av_rescale_q`.
pub fn rescale_q(value: i64, from: Rational, to: Rational) -> i64 {
    // SAFETY: av_rescale_q is pure arithmetic over by-value plain-old-data.
    unsafe { sys::av_rescale_q(value, from.into(), to.into()) }
}

/// `av_rescale_q_rnd(..., AV_ROUND_UP)`.
pub fn rescale_q_round_up(value: i64, from: Rational, to: Rational) -> i64 {
    // SAFETY: as above; AV_ROUND_UP is a valid AVRounding value.
    unsafe { sys::av_rescale_q_rnd(value, from.into(), to.into(), sys::AVRounding::AV_ROUND_UP) }
}

/// `av_rescale`.
pub fn rescale(value: i64, mul: i64, div: i64) -> i64 {
    // SAFETY: pure arithmetic over scalars.
    unsafe { sys::av_rescale(value, mul, div) }
}

/// The endpoint identity of a URL, split by `av_url_split`. Userinfo, path,
/// query and fragment are never copied out: they can all carry credentials,
/// which is the point of using this for log lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    /// Scheme, empty when the URL has none.
    pub protocol: String,
    /// Hostname, empty when absent.
    pub hostname: String,
    /// Port, or -1 when absent.
    pub port: i32,
}

/// `av_url_split`, keeping only protocol, hostname and port.
pub fn url_split(url: &core::ffi::CStr) -> UrlParts {
    let mut proto = [0u8; 32];
    let mut host = [0u8; 256];
    let mut port: c_int = -1;
    // SAFETY: the buffers outlive the call and their sizes are passed; the
    // components we do not want are requested with (NULL, 0), which
    // av_url_split documents as "do not fill"; `url` is NUL-terminated.
    unsafe {
        ffmpeg_sys_next::av_url_split(
            proto.as_mut_ptr().cast(),
            proto.len() as c_int,
            core::ptr::null_mut(),
            0,
            host.as_mut_ptr().cast(),
            host.len() as c_int,
            &mut port,
            core::ptr::null_mut(),
            0,
            url.as_ptr(),
        );
    }
    let str_of = |buf: &[u8]| {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    };
    UrlParts {
        protocol: str_of(&proto),
        hostname: str_of(&host),
        port,
    }
}

/// `av_gettime()` in microseconds. FFmpeg-side timers only.
pub fn gettime_us() -> i64 {
    // SAFETY: no arguments, no shared state.
    unsafe { sys::av_gettime() }
}

/// `av_usleep`.
pub fn usleep(us: u32) {
    // SAFETY: no arguments beyond a scalar duration.
    unsafe {
        sys::av_usleep(us);
    }
}

/// Borrow a `const char *` FFmpeg owns for the life of the process, falling
/// back to `fallback` for NULL or non-UTF-8.
fn static_str(ptr: *const c_char, fallback: &'static str) -> &'static str {
    if ptr.is_null() {
        return fallback;
    }
    // SAFETY: the caller passes a pointer to a NUL-terminated string constant
    // inside libav*, which lives for the whole process.
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or(fallback)
}

/// `avcodec_get_name`.
pub fn codec_name(id: AVCodecID) -> &'static str {
    // SAFETY: avcodec_get_name accepts any AVCodecID and returns a static string.
    static_str(unsafe { sys::avcodec_get_name(id) }, "unknown")
}

/// `av_hwdevice_get_type_name`.
pub fn hwdevice_type_name(kind: AVHWDeviceType) -> &'static str {
    // SAFETY: accepts any AVHWDeviceType; returns a static string or NULL.
    static_str(unsafe { sys::av_hwdevice_get_type_name(kind) }, "none")
}

/// `av_get_pix_fmt_name` (for log lines).
pub fn pix_fmt_name(fmt: AVPixelFormat) -> &'static str {
    // SAFETY: accepts any AVPixelFormat; returns a static string or NULL.
    static_str(unsafe { sys::av_get_pix_fmt_name(fmt) }, "unknown")
}

/// `av_image_get_buffer_size(fmt, w, h, align)`.
pub fn image_buffer_size(fmt: AVPixelFormat, width: i32, height: i32, align: i32) -> Result<usize> {
    // SAFETY: pure computation over scalars and the static pixel format table.
    let size = unsafe { sys::av_image_get_buffer_size(fmt, width, height, align) };
    if size <= 0 {
        return Err(if size < 0 {
            Error(size)
        } else {
            Error::inval()
        });
    }
    Ok(size as usize)
}

/// Decompose an `LIBAV*_VERSION_INT`-shaped version number.
fn version_triple(v: u32) -> (u32, u32, u32) {
    (v >> 16, (v >> 8) & 0xff, v & 0xff)
}

/// The library versions the crate was built against (`LIBAVCODEC_VERSION_*`),
/// for the connection log line.
pub fn version_string() -> String {
    // SAFETY: these take no arguments and only read compiled-in constants.
    let (ac_maj, ac_min, ac_mic) = version_triple(unsafe { sys::avcodec_version() });
    // SAFETY: as above.
    let (af_maj, af_min, af_mic) = version_triple(unsafe { sys::avformat_version() });
    format!("libavcodec {ac_maj}.{ac_min}.{ac_mic}, libavformat {af_maj}.{af_min}.{af_mic}")
}

/// `FFALIGN(x, a)` for power-of-two `a`.
pub(crate) const fn ffalign(x: i32, a: i32) -> i32 {
    (x + a - 1) & !(a - 1)
}

/// Turn a raw `AVFrame::format` / `AVCodecParameters::format` into the enum.
///
/// The bindgen enum is `#[repr(i32)]` with contiguous discriminants from
/// `AV_PIX_FMT_NONE` (-1) to `AV_PIX_FMT_NB`, so a range check is enough to
/// make the transmute sound; anything outside becomes `AV_PIX_FMT_NONE`
/// (which is how the C plugin's `default:` arms treat unknown formats too).
pub(crate) fn pix_fmt_from_raw(raw: c_int) -> AVPixelFormat {
    if raw < AVPixelFormat::AV_PIX_FMT_NONE as c_int || raw > AVPixelFormat::AV_PIX_FMT_NB as c_int
    {
        return AVPixelFormat::AV_PIX_FMT_NONE;
    }
    // SAFETY: `raw` is inside the enum's contiguous discriminant range,
    // checked immediately above.
    unsafe { core::mem::transmute::<c_int, AVPixelFormat>(raw) }
}

/// Same for `AVFrame::format` on audio frames.
pub(crate) fn sample_fmt_from_raw(raw: c_int) -> AVSampleFormat {
    if raw < AVSampleFormat::AV_SAMPLE_FMT_NONE as c_int
        || raw > AVSampleFormat::AV_SAMPLE_FMT_NB as c_int
    {
        return AVSampleFormat::AV_SAMPLE_FMT_NONE;
    }
    // SAFETY: `raw` is inside the enum's contiguous discriminant range.
    unsafe { core::mem::transmute::<c_int, AVSampleFormat>(raw) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eagain_is_recognised_and_printable() {
        let err = Error(sys::AVERROR(sys::EAGAIN));
        assert!(err.is_eagain());
        assert!(!err.is_eof());
        let text = err.to_string();
        assert!(!text.is_empty(), "av_strerror produced nothing");
        assert!(!text.contains("error -"), "unexpected fallback: {text}");
    }

    #[test]
    fn eof_is_recognised() {
        let err = Error(sys::AVERROR_EOF);
        assert!(err.is_eof());
        assert!(!err.is_eagain());
    }

    #[test]
    fn rescale_ms_to_ns() {
        assert_eq!(
            rescale_q(1, Rational::new(1, 1000), Rational::new(1, 1_000_000_000)),
            1_000_000
        );
        assert_eq!(rescale(3, 1_000_000_000, 1000), 3_000_000);
    }

    #[test]
    fn rescale_rounds_up() {
        // 1 tick of 1/3 s into milliseconds: 333.33… rounds up to 334.
        let up = rescale_q_round_up(1, Rational::new(1, 3), Rational::new(1, 1000));
        assert_eq!(up, 334);
        assert_eq!(
            rescale_q(1, Rational::new(1, 3), Rational::new(1, 1000)),
            333
        );
    }

    #[test]
    fn names_and_versions() {
        assert_eq!(codec_name(AVCodecID::AV_CODEC_ID_H264), "h264");
        assert_eq!(pix_fmt_name(AVPixelFormat::AV_PIX_FMT_NV12), "nv12");
        assert_eq!(
            hwdevice_type_name(AVHWDeviceType::AV_HWDEVICE_TYPE_NONE),
            "none"
        );
        assert!(version_string().starts_with("libavcodec "));
    }

    #[test]
    fn buffer_size_matches_planar_layout() {
        let size = image_buffer_size(AVPixelFormat::AV_PIX_FMT_YUV420P, 64, 64, 1).unwrap();
        assert_eq!(size, 64 * 64 * 3 / 2);
        assert!(image_buffer_size(AVPixelFormat::AV_PIX_FMT_YUV420P, 0, 0, 1).is_err());
    }

    #[test]
    fn ffalign_rounds_to_16() {
        assert_eq!(ffalign(1080, 16), 1088);
        assert_eq!(ffalign(1920, 16), 1920);
    }

    #[test]
    fn raw_format_conversion_is_bounded() {
        assert_eq!(pix_fmt_from_raw(0), AVPixelFormat::AV_PIX_FMT_YUV420P);
        assert_eq!(pix_fmt_from_raw(-7), AVPixelFormat::AV_PIX_FMT_NONE);
        assert_eq!(pix_fmt_from_raw(1_000_000), AVPixelFormat::AV_PIX_FMT_NONE);
        assert_eq!(sample_fmt_from_raw(3), AVSampleFormat::AV_SAMPLE_FMT_FLT);
        assert_eq!(sample_fmt_from_raw(99), AVSampleFormat::AV_SAMPLE_FMT_NONE);
    }
}

#[cfg(test)]
mod url_split_tests {
    #[test]
    fn splits_endpoint_and_drops_credentials() {
        let p = super::url_split(c"srt://host.example:9000?passphrase=secret");
        assert_eq!(
            (p.protocol.as_str(), p.hostname.as_str(), p.port),
            ("srt", "host.example", 9000)
        );
        let p = super::url_split(c"rtmp://user:pw@live.example/app/streamkey");
        assert_eq!(
            (p.protocol.as_str(), p.hostname.as_str(), p.port),
            ("rtmp", "live.example", -1)
        );
        let p = super::url_split(c"srt://[2001:db8::1]:9000");
        assert_eq!(p.hostname, "2001:db8::1");
        assert_eq!(p.port, 9000);
        let p = super::url_split(c"not a url");
        assert_eq!(p.protocol, "");
    }
}
