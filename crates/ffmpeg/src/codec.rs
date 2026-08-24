//! Decoders.

use crate::format::StreamRef;
use crate::frame::Frame;
use crate::hw::HwDeviceContext;
use crate::packet::Packet;
use crate::{AVCodecID, AVHWDeviceType, AVPixelFormat, Rational, Result};

/// A borrowed `const AVCodec*` (static lifetime inside libavcodec).
#[derive(Debug, Clone, Copy)]
pub struct Codec(*const ffmpeg_sys_next::AVCodec);

unsafe impl Send for Codec {}
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
        let _ = id;
        todo!("W1-B")
    }

    /// `avcodec_get_hw_config` loop.
    pub fn hw_configs(&self) -> Vec<HwConfig> {
        todo!("W1-B")
    }

    pub fn name(&self) -> &'static str {
        todo!("W1-B")
    }
}

/// The `get_format` callback type for forced-NVDEC negotiation. Receives the
/// offered formats (up to `AV_PIX_FMT_NONE`) and returns the chosen one or
/// `AV_PIX_FMT_NONE` to fail decoder open (no silent software fallback).
pub type GetFormatFn = fn(&Codec, &[AVPixelFormat]) -> AVPixelFormat;

/// `AVCodecContext` before `avcodec_open2`.
pub struct CodecBuilder {
    ptr: *mut ffmpeg_sys_next::AVCodecContext,
    codec: Codec,
    get_format: Option<GetFormatFn>,
}

impl CodecBuilder {
    /// `avcodec_alloc_context3` + `avcodec_parameters_to_context`.
    pub fn from_stream(codec: Codec, stream: &StreamRef<'_>) -> Result<Self> {
        let _ = (codec, stream);
        todo!("W1-B")
    }

    pub fn pkt_timebase(self, tb: Rational) -> Self {
        let _ = tb;
        todo!("W1-B")
    }

    /// `thread_count` / `thread_type`.
    pub fn threads(self, count: i32, kind: i32) -> Self {
        let _ = (count, kind);
        todo!("W1-B")
    }

    /// `flags |= AV_CODEC_FLAG_LOW_DELAY`.
    pub fn flag_low_delay(self) -> Self {
        todo!("W1-B")
    }

    /// `flags2 |= AV_CODEC_FLAG2_FAST`.
    pub fn flag2_fast(self) -> Self {
        todo!("W1-B")
    }

    /// `error_concealment |= mask` (e.g. `FF_EC_FAVOR_INTER`).
    pub fn error_concealment(self, mask: i32) -> Self {
        let _ = mask;
        todo!("W1-B")
    }

    pub fn extra_hw_frames(self, count: i32) -> Self {
        let _ = count;
        todo!("W1-B")
    }

    /// `hw_device_ctx = av_buffer_ref(device)`.
    pub fn hw_device(self, device: &HwDeviceContext) -> Self {
        let _ = device;
        todo!("W1-B")
    }

    /// Install a `get_format` callback (stored in `ctx->opaque`, invoked
    /// through a `catch_unwind` shim that returns `AV_PIX_FMT_NONE` on panic).
    pub fn get_format(mut self, f: GetFormatFn) -> Self {
        self.get_format = Some(f);
        self
    }

    /// `avcodec_open2`. On failure the context is freed.
    pub fn open(self) -> Result<CodecContext> {
        let _ = (self.ptr, self.codec, self.get_format);
        todo!("W1-B")
    }
}

/// An open decoder.
pub struct CodecContext {
    ptr: *mut ffmpeg_sys_next::AVCodecContext,
    codec: Codec,
    // Box<CtxOpaque> behind ctx->opaque, kept so it outlives the context.
    opaque: Option<Box<CtxOpaque>>,
}

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
        todo!("W1-B")
    }

    /// `hw_device_ctx != NULL` after open (FFmpeg may drop it on failure).
    pub fn has_hw_device(&self) -> bool {
        todo!("W1-B")
    }

    /// `avcodec_send_packet`.
    pub fn send_packet(&mut self, pkt: &Packet) -> Result<()> {
        let _ = pkt;
        todo!("W1-B")
    }

    /// `avcodec_receive_frame` into `frame` (unrefs it first).
    pub fn receive_frame(&mut self, frame: &mut Frame) -> Result<()> {
        let _ = frame;
        todo!("W1-B")
    }

    /// `avcodec_flush_buffers`.
    pub fn flush(&mut self) {
        todo!("W1-B")
    }

    #[doc(hidden)]
    pub fn as_mut_ptr(&mut self) -> *mut ffmpeg_sys_next::AVCodecContext {
        let _ = &self.opaque;
        self.ptr
    }
}

impl Drop for CodecContext {
    fn drop(&mut self) {
        todo!("W1-B: avcodec_free_context")
    }
}
