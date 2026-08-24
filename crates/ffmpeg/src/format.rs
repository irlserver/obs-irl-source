//! Demuxing with an interrupt callback.

use core::ffi::{CStr, c_int, c_void};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::dict::Dictionary;
use crate::packet::Packet;
use crate::{AVCodecID, Error, Rational, Result};

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
        self.io_start_us.store(crate::gettime_us() as u64, Ordering::Relaxed);
    }

    pub fn disarm(&self) {
        self.io_start_us.store(0, core::sync::atomic::Ordering::Relaxed);
    }

    /// The interrupt decision. Touches only atomics and `av_gettime`, so it is
    /// safe to run inside FFmpeg's callback without `catch_unwind`.
    pub fn should_abort(&self) -> bool {
        if !self.active.load(Ordering::Relaxed) {
            return true;
        }
        let start = self.io_start_us.load(Ordering::Relaxed);
        start != 0 && (crate::gettime_us() as u64).wrapping_sub(start) > self.timeout_us
    }
}

/// `AVIOInterruptCB::callback`.
///
/// Deliberately allocation-free and panic-free: it reads two atomics and calls
/// `av_gettime`, none of which can unwind, so no `catch_unwind` guard is
/// needed (and none could be installed on a callback FFmpeg invokes from
/// inside a blocking read anyway).
unsafe extern "C" fn interrupt_shim(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    // SAFETY: `opaque` is the `Arc<InterruptWatch>` pointer FormatContext::open
    // installed. The `FormatContext` owns that Arc and frees the format context
    // (and with it the callback) in its own Drop, so the target outlives every
    // invocation.
    let watch = unsafe { &*(opaque as *const InterruptWatch) };
    if watch.should_abort() { 1 } else { 0 }
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
    /// # Safety
    /// `ptr` must be a live `AVStream` owned by a `FormatContext` that outlives
    /// `'a`.
    unsafe fn new(ptr: *const ffmpeg_sys_next::AVStream) -> Self {
        Self { ptr, _fmt: core::marker::PhantomData }
    }

    pub fn index(&self) -> usize {
        // SAFETY: `self.ptr` is a live stream owned by the borrowed context.
        unsafe { (*self.ptr).index.max(0) as usize }
    }

    pub fn media_type(&self) -> MediaType {
        // SAFETY: as above; `codecpar` is always set on a demuxed stream.
        match unsafe { (*(*self.ptr).codecpar).codec_type } {
            ffmpeg_sys_next::AVMediaType::AVMEDIA_TYPE_VIDEO => MediaType::Video,
            ffmpeg_sys_next::AVMediaType::AVMEDIA_TYPE_AUDIO => MediaType::Audio,
            _ => MediaType::Other,
        }
    }

    pub fn codec_id(&self) -> AVCodecID {
        // SAFETY: as above.
        unsafe { (*(*self.ptr).codecpar).codec_id }
    }

    pub fn time_base(&self) -> Rational {
        // SAFETY: as above.
        unsafe { (*self.ptr).time_base }.into()
    }

    /// `codecpar->width/height` (video).
    pub fn dimensions(&self) -> (i32, i32) {
        // SAFETY: as above.
        unsafe { ((*(*self.ptr).codecpar).width, (*(*self.ptr).codecpar).height) }
    }

    /// `codecpar->sample_rate`, `codecpar->ch_layout.nb_channels` (audio).
    pub fn audio_params(&self) -> (i32, i32) {
        // SAFETY: as above.
        unsafe {
            ((*(*self.ptr).codecpar).sample_rate, (*(*self.ptr).codecpar).ch_layout.nb_channels)
        }
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
    pub fn open(
        url: &CStr,
        mut opts: Dictionary,
        watch: Arc<InterruptWatch>,
    ) -> Result<(Self, Vec<String>)> {
        // SAFETY: no arguments; returns a fresh context or null.
        let mut ptr = unsafe { ffmpeg_sys_next::avformat_alloc_context() };
        if ptr.is_null() {
            return Err(Error::nomem());
        }

        // SAFETY: `ptr` is a fresh context we own. The opaque is the address of
        // the Arc's payload, which stays valid because `self.watch` below holds
        // a strong reference for as long as the context exists.
        unsafe {
            (*ptr).interrupt_callback.callback = Some(interrupt_shim);
            (*ptr).interrupt_callback.opaque = Arc::as_ptr(&watch) as *mut c_void;
        }

        watch.arm();
        // SAFETY: `&mut ptr` is a valid `AVFormatContext**` holding our
        // allocated context, `url` is NUL-terminated, and `opts` is our owned
        // dictionary which the call consumes entries from in place. On failure
        // avformat_open_input frees the context and stores null in `ptr`.
        let ret = unsafe {
            ffmpeg_sys_next::avformat_open_input(
                &mut ptr,
                url.as_ptr(),
                core::ptr::null(),
                opts.as_mut_ptr(),
            )
        };
        if ret < 0 {
            if !ptr.is_null() {
                // SAFETY: the context survived the failure; close it ourselves.
                unsafe { ffmpeg_sys_next::avformat_close_input(&mut ptr) };
            }
            return Err(Error(ret));
        }

        let unrecognised = opts.remaining_keys();
        Ok((Self { ptr, watch }, unrecognised))
    }

    /// `avformat_find_stream_info` (arms the watch first).
    pub fn find_stream_info(&mut self) -> Result<()> {
        self.watch.arm();
        // SAFETY: `self.ptr` is an open context; a null options array is valid.
        Error::check(unsafe {
            ffmpeg_sys_next::avformat_find_stream_info(self.ptr, core::ptr::null_mut())
        })
    }

    pub fn streams(&self) -> impl Iterator<Item = StreamRef<'_>> {
        // SAFETY: `self.ptr` is an open context.
        let count = unsafe { (*self.ptr).nb_streams } as usize;
        (0..count).filter_map(move |i| self.stream(i))
    }

    pub fn stream(&self, index: usize) -> Option<StreamRef<'_>> {
        // SAFETY: `self.ptr` is an open context; `streams` is an array of
        // `nb_streams` non-null pointers.
        unsafe {
            if index >= (*self.ptr).nb_streams as usize {
                return None;
            }
            let s = *(*self.ptr).streams.add(index);
            if s.is_null() {
                return None;
            }
            Some(StreamRef::new(s))
        }
    }

    /// `av_read_frame` (arms the watch first). `Err(EAGAIN)`/`EOF` map to the
    /// C plugin's read-error handling.
    pub fn read_frame(&mut self, pkt: &mut Packet) -> Result<()> {
        self.watch.arm();
        // SAFETY: `self.ptr` is an open context and `pkt` a live packet we own;
        // av_read_frame unrefs it before filling it in.
        Error::check(unsafe { ffmpeg_sys_next::av_read_frame(self.ptr, pkt.as_mut_ptr()) })
    }

    pub fn watch(&self) -> &Arc<InterruptWatch> {
        &self.watch
    }

    #[doc(hidden)]
    pub fn as_mut_ptr(&mut self) -> *mut ffmpeg_sys_next::AVFormatContext {
        self.ptr
    }
}

impl Drop for FormatContext {
    fn drop(&mut self) {
        // SAFETY: `&mut self.ptr` is our sole owning pointer. This runs before
        // `self.watch` is dropped (Rust drops the body first, then the fields),
        // so the interrupt callback's opaque stays valid until the context is
        // gone.
        unsafe { ffmpeg_sys_next::avformat_close_input(&mut self.ptr) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_aborts_when_inactive() {
        let active = Arc::new(AtomicBool::new(true));
        let watch = InterruptWatch::new(active.clone(), 5_000_000);
        assert!(!watch.should_abort());
        active.store(false, Ordering::Relaxed);
        assert!(watch.should_abort());
    }

    #[test]
    fn watch_aborts_on_io_stall() {
        let active = Arc::new(AtomicBool::new(true));
        // Zero timeout: any armed elapsed time is a stall.
        let watch = InterruptWatch::new(active, 0);
        assert!(!watch.should_abort(), "disarmed watch must not abort");
        watch.arm();
        // av_gettime has microsecond resolution; give it something to measure.
        crate::usleep(2_000);
        assert!(watch.should_abort());
        watch.disarm();
        assert!(!watch.should_abort());
    }

    #[test]
    fn open_fails_cleanly_on_a_bad_url() {
        let active = Arc::new(AtomicBool::new(true));
        let watch = InterruptWatch::new(active, 1_000_000);
        let mut opts = Dictionary::new();
        opts.set(c"probesize", c"1000000").unwrap();
        let err = FormatContext::open(c"irl-nonexistent://nowhere", opts, watch)
            .err()
            .expect("opening a bogus protocol must fail");
        assert!(err.code() < 0);
    }
}
