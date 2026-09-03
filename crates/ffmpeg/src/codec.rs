//! Decoders.

use core::ffi::{CStr, c_void};
use core::mem::ManuallyDrop;

use crate::format::StreamRef;
use crate::frame::Frame;
use crate::hw::HwDeviceContext;
use crate::packet::Packet;
use crate::{AVCodecID, AVHWDeviceType, AVPixelFormat, Error, Rational, Result};

/// A borrowed `const AVCodec*` (static lifetime inside libavcodec).
#[derive(Debug, Clone, Copy)]
pub struct Codec(*const ffmpeg_sys_next::AVCodec);

// SAFETY: an AVCodec is an immutable descriptor libavcodec keeps for the whole
// process; sharing the pointer across threads only ever reads it.
unsafe impl Send for Codec {}
// SAFETY: as above.
unsafe impl Sync for Codec {}

/// One `AVCodecHWConfig` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwConfig {
    pub device_type: AVHWDeviceType,
    pub methods: i32,
    pub pix_fmt: AVPixelFormat,
}

impl Codec {
    /// `avcodec_find_decoder`.
    pub fn find_decoder(id: AVCodecID) -> Option<Self> {
        // SAFETY: accepts any AVCodecID; returns a static descriptor or null.
        let ptr = unsafe { ffmpeg_sys_next::avcodec_find_decoder(id) };
        (!ptr.is_null()).then_some(Self(ptr))
    }

    /// `avcodec_get_hw_config` loop.
    pub fn hw_configs(&self) -> Vec<HwConfig> {
        let mut out = Vec::new();
        let mut index = 0;
        loop {
            // SAFETY: `self.0` is a live static descriptor; avcodec_get_hw_config
            // returns null once `index` runs past the end.
            let cfg = unsafe { ffmpeg_sys_next::avcodec_get_hw_config(self.0, index) };
            if cfg.is_null() {
                return out;
            }
            // SAFETY: non-null means a static AVCodecHWConfig.
            unsafe {
                out.push(HwConfig {
                    device_type: (*cfg).device_type,
                    methods: (*cfg).methods,
                    pix_fmt: (*cfg).pix_fmt,
                });
            }
            index += 1;
        }
    }

    pub fn name(&self) -> &'static str {
        // SAFETY: `self.0` is a live static descriptor with a static `name`.
        let ptr = unsafe { (*self.0).name };
        if ptr.is_null() {
            return "unknown";
        }
        // SAFETY: `name` is a NUL-terminated string constant inside libavcodec.
        unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or("unknown")
    }

    #[doc(hidden)]
    pub fn as_ptr(&self) -> *const ffmpeg_sys_next::AVCodec {
        self.0
    }
}

/// The `get_format` callback type for forced-NVDEC negotiation. Receives the
/// offered formats (up to `AV_PIX_FMT_NONE`) and returns the chosen one or
/// `AV_PIX_FMT_NONE` to fail decoder open (no silent software fallback).
pub type GetFormatFn = fn(&Codec, &[AVPixelFormat]) -> AVPixelFormat;

/// `AVCodecContext::get_format`.
///
/// FFmpeg calls this from inside `avcodec_open2` (and again on a parameter
/// change), possibly on a decoder worker thread. Unwinding through the C frame
/// would abort the process, so the Rust callback runs under `catch_unwind` and
/// a panic degrades to `AV_PIX_FMT_NONE`, which is exactly the "no hardware
/// format on offer" answer the forced-NVDEC path already handles.
unsafe extern "C" fn get_format_shim(
    ctx: *mut ffmpeg_sys_next::AVCodecContext,
    fmts: *const AVPixelFormat,
) -> AVPixelFormat {
    if ctx.is_null() || fmts.is_null() {
        return AVPixelFormat::AV_PIX_FMT_NONE;
    }
    // SAFETY: `opaque` holds the `Box<CtxOpaque>` CodecBuilder::open installed,
    // owned by the `CodecContext` that owns this AVCodecContext, so it outlives
    // every call.
    let opaque = unsafe { (*ctx).opaque as *const CtxOpaque };
    if opaque.is_null() {
        return AVPixelFormat::AV_PIX_FMT_NONE;
    }
    // SAFETY: as above.
    let opaque = unsafe { &*opaque };

    // SAFETY: FFmpeg documents `fmt` as an array terminated by AV_PIX_FMT_NONE.
    let mut len = 0usize;
    while unsafe { *fmts.add(len) } != AVPixelFormat::AV_PIX_FMT_NONE {
        len += 1;
    }
    // SAFETY: `len` entries were just walked, all inside the array.
    let offered = unsafe { core::slice::from_raw_parts(fmts, len) };

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (opaque.get_format)(&opaque.codec, offered)
    }))
    .unwrap_or(AVPixelFormat::AV_PIX_FMT_NONE)
}

/// `AVCodecContext` before `avcodec_open2`.
pub struct CodecBuilder {
    ptr: *mut ffmpeg_sys_next::AVCodecContext,
    codec: Codec,
    get_format: Option<GetFormatFn>,
}

impl CodecBuilder {
    /// `avcodec_alloc_context3` + `avcodec_parameters_to_context`.
    pub fn from_stream(codec: Codec, stream: &StreamRef<'_>) -> Result<Self> {
        // SAFETY: `codec` is a live static descriptor.
        let ptr = unsafe { ffmpeg_sys_next::avcodec_alloc_context3(codec.0) };
        if ptr.is_null() {
            return Err(Error::nomem());
        }
        let this = Self {
            ptr,
            codec,
            get_format: None,
        };
        // SAFETY: `ptr` is a fresh context and `codecpar` a live parameter set
        // owned by the borrowed format context.
        let ret = unsafe {
            ffmpeg_sys_next::avcodec_parameters_to_context(ptr, (*stream.as_ptr()).codecpar)
        };
        // `this` frees the context in Drop if this fails.
        Error::check(ret)?;
        Ok(this)
    }

    pub fn pkt_timebase(self, tb: Rational) -> Self {
        // SAFETY: `self.ptr` is an unopened context we own.
        unsafe { (*self.ptr).pkt_timebase = tb.into() };
        self
    }

    /// `thread_count` / `thread_type`.
    pub fn threads(self, count: i32, kind: i32) -> Self {
        // SAFETY: as above; both are plain configuration fields read at open.
        unsafe {
            (*self.ptr).thread_count = count;
            (*self.ptr).thread_type = kind;
        }
        self
    }

    /// `flags |= AV_CODEC_FLAG_LOW_DELAY`.
    pub fn flag_low_delay(self) -> Self {
        // SAFETY: as above.
        unsafe { (*self.ptr).flags |= ffmpeg_sys_next::AV_CODEC_FLAG_LOW_DELAY as i32 };
        self
    }

    /// `flags2 |= AV_CODEC_FLAG2_FAST`.
    pub fn flag2_fast(self) -> Self {
        // SAFETY: as above.
        unsafe { (*self.ptr).flags2 |= ffmpeg_sys_next::AV_CODEC_FLAG2_FAST };
        self
    }

    /// `error_concealment |= mask` (e.g. `FF_EC_FAVOR_INTER`).
    pub fn error_concealment(self, mask: i32) -> Self {
        // SAFETY: as above.
        unsafe { (*self.ptr).error_concealment |= mask };
        self
    }

    pub fn extra_hw_frames(self, count: i32) -> Self {
        // SAFETY: as above.
        unsafe { (*self.ptr).extra_hw_frames = count };
        self
    }

    /// `hw_device_ctx = av_buffer_ref(device)`.
    pub fn hw_device(self, device: &HwDeviceContext) -> Self {
        // SAFETY: `device.as_ptr()` is a live AVBufferRef the caller owns;
        // av_buffer_ref takes an independent reference the codec context then
        // owns and releases in avcodec_free_context.
        unsafe { (*self.ptr).hw_device_ctx = ffmpeg_sys_next::av_buffer_ref(device.as_ptr()) };
        self
    }

    /// Install a `get_format` callback (stored in `ctx->opaque`, invoked
    /// through a `catch_unwind` shim that returns `AV_PIX_FMT_NONE` on panic).
    pub fn get_format(mut self, f: GetFormatFn) -> Self {
        self.get_format = Some(f);
        self
    }

    /// Whether `hw_device_ctx` is currently attached (the C plugin's
    /// "hardware was requested" check before the open attempt).
    pub fn has_hw_device(&self) -> bool {
        // SAFETY: `self.ptr` is an unopened context we own.
        !unsafe { (*self.ptr).hw_device_ctx }.is_null()
    }

    /// `avcodec_open2`. On failure the context is freed.
    pub fn open(self) -> Result<CodecContext> {
        // Take the fields apart without running Drop: `open` owns the context
        // from here on, and frees it itself if avcodec_open2 fails.
        let this = ManuallyDrop::new(self);
        let mut ptr = this.ptr;
        let codec = this.codec;

        let opaque = this.get_format.map(|f| {
            Box::new(CtxOpaque {
                get_format: f,
                codec,
            })
        });
        if let Some(boxed) = opaque.as_ref() {
            // SAFETY: `ptr` is an unopened context we own; the Box lives in the
            // returned CodecContext (or is dropped right after the context is
            // freed below), so the pointer stays valid for every callback.
            unsafe {
                (*ptr).opaque = (&raw const **boxed) as *mut c_void;
                (*ptr).get_format = Some(get_format_shim);
            }
        }

        // SAFETY: `ptr` is an unopened context built from `codec`; a null
        // options dictionary means "no extra options".
        let ret = unsafe { ffmpeg_sys_next::avcodec_open2(ptr, codec.0, core::ptr::null_mut()) };
        if ret < 0 {
            // SAFETY: `&mut ptr` is the sole owning pointer; freeing it also
            // releases the hw_device_ctx reference taken above.
            unsafe { ffmpeg_sys_next::avcodec_free_context(&mut ptr) };
            drop(opaque);
            return Err(Error(ret));
        }

        Ok(CodecContext { ptr, codec, opaque })
    }
}

impl Drop for CodecBuilder {
    fn drop(&mut self) {
        // Only reached when the builder is abandoned before `open`; `open`
        // itself defuses this with ManuallyDrop.
        // SAFETY: `&mut self.ptr` is our sole owning pointer.
        unsafe { ffmpeg_sys_next::avcodec_free_context(&mut self.ptr) };
    }
}

/// An open decoder.
pub struct CodecContext {
    ptr: *mut ffmpeg_sys_next::AVCodecContext,
    codec: Codec,
    // Box<CtxOpaque> behind ctx->opaque, kept so it outlives the context.
    opaque: Option<Box<CtxOpaque>>,
}

// SAFETY: an AVCodecContext has no thread affinity of its own (its internal
// worker threads are owned by libavcodec); the plugin gives each decoder to a
// single thread and never shares one.
unsafe impl Send for CodecContext {}

#[doc(hidden)]
pub struct CtxOpaque {
    pub get_format: GetFormatFn,
    pub codec: Codec,
}

impl CodecContext {
    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn codec_id(&self) -> AVCodecID {
        // SAFETY: `self.ptr` is an open context we own.
        unsafe { (*self.ptr).codec_id }
    }

    /// `hw_device_ctx != NULL` after open (FFmpeg may drop it on failure).
    pub fn has_hw_device(&self) -> bool {
        // SAFETY: as above.
        !unsafe { (*self.ptr).hw_device_ctx }.is_null()
    }

    /// `avcodec_send_packet`.
    pub fn send_packet(&mut self, pkt: &Packet) -> Result<()> {
        // SAFETY: `self.ptr` is an open decoder and `pkt` a live packet the
        // caller owns; avcodec_send_packet does not take ownership.
        Error::check(unsafe { ffmpeg_sys_next::avcodec_send_packet(self.ptr, pkt.as_ptr()) })
    }

    /// `avcodec_receive_frame` into `frame` (unrefs it first).
    pub fn receive_frame(&mut self, frame: &mut Frame) -> Result<()> {
        // avcodec_receive_frame unrefs internally, but the C plugin's callers
        // rely on the frame being blank on every error path too.
        frame.unref();
        // SAFETY: `self.ptr` is an open decoder and `frame` a live frame we own.
        Error::check(unsafe {
            ffmpeg_sys_next::avcodec_receive_frame(self.ptr, frame.as_mut_ptr())
        })
    }

    /// `avcodec_flush_buffers`.
    pub fn flush(&mut self) {
        // SAFETY: `self.ptr` is an open decoder.
        unsafe { ffmpeg_sys_next::avcodec_flush_buffers(self.ptr) };
    }

    #[doc(hidden)]
    pub fn as_mut_ptr(&mut self) -> *mut ffmpeg_sys_next::AVCodecContext {
        let _ = &self.opaque;
        self.ptr
    }
}

impl Drop for CodecContext {
    fn drop(&mut self) {
        // SAFETY: `&mut self.ptr` is our sole owning pointer. Rust runs this
        // body before dropping `self.opaque`, so the get_format opaque stays
        // alive until libavcodec can no longer call back into it.
        unsafe { ffmpeg_sys_next::avcodec_free_context(&mut self.ptr) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_built_in_decoder() {
        let codec = Codec::find_decoder(AVCodecID::AV_CODEC_ID_H264).expect("h264 decoder");
        assert_eq!(codec.name(), "h264");
        // The bundled build has hardware configs compiled in; the list must at
        // least be readable and terminate.
        let configs = codec.hw_configs();
        assert!(configs.len() < 64);
    }

    #[test]
    fn missing_decoder_is_none() {
        assert!(Codec::find_decoder(AVCodecID::AV_CODEC_ID_NONE).is_none());
    }
}
