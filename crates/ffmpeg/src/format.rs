//! Demuxing with an interrupt callback.

use core::ffi::CStr;
use core::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use crate::dict::Dictionary;
use crate::packet::Packet;
use crate::{AVCodecID, Rational, Result};

/// Port of the C `interrupt_cb`: abort the blocking FFmpeg call when the
/// stream was told to stop, or when the current I/O call has been blocked
/// longer than `timeout_us` (a dead-but-open connection otherwise hangs
/// `av_read_frame` forever).
///
/// `active` is the *same* atomic as the stream's `thread_active` flag, so a
/// stop request reaches a receiver blocked inside FFmpeg. `io_start_us` is
/// armed by the calling thread before each blocking call (0 = disarmed).
pub struct InterruptWatch {
    active: Arc<AtomicBool>,
    io_start_us: AtomicU64,
    timeout_us: u64,
}

impl InterruptWatch {
    pub fn new(active: Arc<AtomicBool>, timeout_us: u64) -> Arc<Self> {
        Arc::new(Self { active, io_start_us: AtomicU64::new(0), timeout_us })
    }

    /// Record the start of a blocking call (`av_gettime()`).
    pub fn arm(&self) {
        todo!("W1-B")
    }

    pub fn disarm(&self) {
        self.io_start_us.store(0, core::sync::atomic::Ordering::Relaxed);
    }

    /// The interrupt decision. Touches only atomics and `av_gettime`, so it is
    /// safe to run inside FFmpeg's callback without `catch_unwind`.
    pub fn should_abort(&self) -> bool {
        let _ = self.timeout_us;
        todo!("W1-B")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Video,
    Audio,
    Other,
}

/// A borrowed `AVStream`.
#[derive(Clone, Copy)]
pub struct StreamRef<'a> {
    ptr: *const ffmpeg_sys_next::AVStream,
    _fmt: core::marker::PhantomData<&'a FormatContext>,
}

impl StreamRef<'_> {
    pub fn index(&self) -> usize {
        todo!("W1-B")
    }

    pub fn media_type(&self) -> MediaType {
        todo!("W1-B: codecpar->codec_type")
    }

    pub fn codec_id(&self) -> AVCodecID {
        todo!("W1-B")
    }

    pub fn time_base(&self) -> Rational {
        todo!("W1-B")
    }

    /// `codecpar->width/height` (video).
    pub fn dimensions(&self) -> (i32, i32) {
        todo!("W1-B")
    }

    /// `codecpar->sample_rate`, `codecpar->ch_layout.nb_channels` (audio).
    pub fn audio_params(&self) -> (i32, i32) {
        todo!("W1-B")
    }

    #[doc(hidden)]
    pub fn as_ptr(&self) -> *const ffmpeg_sys_next::AVStream {
        self.ptr
    }
}

/// An open `AVFormatContext`.
pub struct FormatContext {
    ptr: *mut ffmpeg_sys_next::AVFormatContext,
    watch: Arc<InterruptWatch>,
}

// Only ever used from the receiver thread; Send lets the thread own it.
unsafe impl Send for FormatContext {}

impl FormatContext {
    /// `avformat_alloc_context` + interrupt callback + `avformat_open_input`.
    /// `opts` is consumed; unrecognised entries are returned for logging.
    pub fn open(url: &CStr, opts: Dictionary, watch: Arc<InterruptWatch>) -> Result<(Self, Vec<String>)> {
        let _ = (url, opts, watch);
        todo!("W1-B")
    }

    /// `avformat_find_stream_info` (arms the watch first).
    pub fn find_stream_info(&mut self) -> Result<()> {
        todo!("W1-B")
    }

    pub fn streams(&self) -> impl Iterator<Item = StreamRef<'_>> {
        let _ = self.ptr;
        core::iter::empty()
    }

    pub fn stream(&self, index: usize) -> Option<StreamRef<'_>> {
        let _ = index;
        todo!("W1-B")
    }

    /// `av_read_frame` (arms the watch first). `Err(EAGAIN)`/`EOF` map to the
    /// C plugin's read-error handling.
    pub fn read_frame(&mut self, pkt: &mut Packet) -> Result<()> {
        let _ = pkt;
        todo!("W1-B")
    }

    pub fn watch(&self) -> &Arc<InterruptWatch> {
        &self.watch
    }
}

impl Drop for FormatContext {
    fn drop(&mut self) {
        todo!("W1-B: avformat_close_input")
    }
}
