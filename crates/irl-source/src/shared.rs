//! State shared between the OBS thread and the three worker threads for one
//! run of the receiver (`start_receiver` … `stop_receiver`).
//!
//! This file is the decomposition of the C `struct irl_source` into owners.
//! It is written by the orchestrator and frozen: agents that need a change
//! request it rather than editing here.
//!
//! Ownership map (C field → Rust home):
//! - OBS thread only (`source.rs`): `fit_pending`, `media_stopped`,
//!   `close_when_inactive`, the authoritative `Config`, the thread handles.
//! - [`Shared`]: built fresh at every `start_receiver` (this replaces the C
//!   `reset_runtime_state()`: everything it zeroed is a field here and starts
//!   zeroed; everything it deliberately kept lives in [`LifetimeStats`]).
//! - [`AudioState`] under `Shared::audio_state` (the C `audio_state_lock`).
//!   The audio pump locks it **once** per `pump_once` and passes `&mut` down;
//!   nothing below may lock it again.
//! - `Shared::audio_buf`: the jitter buffer under its own lock. Lock order:
//!   `audio_state` → `audio_buf`. `video.q` is never held together with
//!   either.
//! - [`ConnStats`] / [`LifetimeStats`]: counters as relaxed atomics, readable
//!   from any thread without a lock (the C read them unsynchronised).
//! - Receiver-thread-owned, video-thread-owned and audio-thread-owned state
//!   are plain structs inside those threads (`receiver/mod.rs`,
//!   `video/thread.rs`, `audio/pump.rs`); they are not in this file.

use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed,
};
use std::time::Duration;

use parking_lot::{Condvar, Mutex, MutexGuard};

use irl_core::{AudioBuffer, DrainWatch, HwDecode, LastSample, SpeedCarry, SpeedTrim, Watermarks};

/// Settings latched when the stream opens; changing any of them forces a
/// restart (`config_requires_restart`).
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub url: CString,
    pub ffmpeg_options: Option<String>,
    pub hw_decode: HwDecode,
    pub low_latency_audio: bool,
    pub small_gap_ms: i32,
    pub large_gap_ms: i32,
}

/// Settings swapped in place while the workers run (`config_apply_hot`).
pub struct HotConfig {
    pub reconnect_delay_s: AtomicI32,
    pub adaptive_speed: AtomicBool,
    /// Percent above native rate the drain may reach (Catch-Up Speed).
    pub catchup_percent: AtomicI32,
    pub wait_for_keyframe: AtomicBool,
    pub clear_on_disconnect: AtomicBool,
    /// The three watermarks publish together, after `AudioBuffer::resize`
    /// succeeded; three separate atomics could be read torn mid-resize.
    pub watermarks: Mutex<Watermarks>,
}

impl HotConfig {
    pub fn watermarks(&self) -> Watermarks {
        *self.watermarks.lock()
    }

    /// The drain ceiling this cycle. Derived per read rather than cached: the
    /// slider applies live, and every consumer of it inside one controller
    /// cycle has to see the same value.
    pub fn max_speed(&self) -> f32 {
        irl_core::catchup_speed_max(self.catchup_percent.load(Relaxed))
    }
}

/// Run-level flags.
pub struct RunFlags {
    /// `thread_active`. An `Arc` because the FFmpeg interrupt watch shares it,
    /// so a stop request reaches a receiver blocked inside `av_read_frame`.
    pub thread_active: Arc<AtomicBool>,
    /// `reconnecting`.
    pub reconnecting: AtomicBool,
    /// `audio_stream_present`: "this connection carries audio", mirrored for
    /// the video thread, which must not touch `audio_stream_idx`.
    pub audio_present: AtomicBool,
}

/// Everything the C guarded with `audio_state_lock` that is not a counter.
#[derive(Debug)]
pub struct AudioState {
    // Output clock: `ts = anchor + samples / rate`, anchored once at prime.
    pub primed: bool,
    pub anchor_ns: u64,
    pub samples: u64,

    // Playout mapping (audio → OBS clock), the lip-sync source for video.
    pub latest_obs_end_ts_ns: u64,
    pub latest_buffered_end_pts_ns: i64,
    pub offset_baseline_ns: i64,
    pub offset_baseline_set: bool,

    // Concealment.
    pub out_last: LastSample,
    pub conceal_fade_pending: bool,

    // Fade-in / warm-up.
    pub fade_in_pending: bool,
    pub fade_in_frames_remaining: i32,
    pub startup_warmup_remaining_ms: i32,

    /// Decoded samples per frame (AAC 1024, Opus 960): the output chunk size.
    pub decoded_frame_samples: i32,

    // Stream PTS mirrors (ns).
    pub latest_audio_stream_pts_ns: i64,
    pub latest_video_stream_pts_ns: i64,

    /// Underrun recovery hold (FFmpeg µs domain).
    pub recovery_until_us: u64,
    pub drain: DrainWatch,

    /// The speed controller's integral term. Written only by the audio
    /// thread, but it lives here because its lifetime is not the pump's: it
    /// survives a decoder flush (`reset_audio_timing_state`) and is cleared by
    /// a stream reset (`reset_stream_timing_state`), and both of those are
    /// called from the receiver thread.
    pub speed_trim: SpeedTrim,
    /// Fractional output-sample debt carried between chunks, cleared by
    /// `reset_audio_timing_state` for the same reason.
    pub speed_carry: SpeedCarry,
}

impl AudioState {
    pub fn new(startup_warmup_ms: i32) -> Self {
        Self {
            primed: false,
            anchor_ns: 0,
            samples: 0,
            latest_obs_end_ts_ns: 0,
            latest_buffered_end_pts_ns: 0,
            offset_baseline_ns: 0,
            offset_baseline_set: false,
            out_last: LastSample::default(),
            conceal_fade_pending: false,
            fade_in_pending: false,
            fade_in_frames_remaining: 0,
            startup_warmup_remaining_ms: startup_warmup_ms,
            decoded_frame_samples: 0,
            latest_audio_stream_pts_ns: 0,
            latest_video_stream_pts_ns: 0,
            recovery_until_us: 0,
            drain: DrainWatch::default(),
            speed_trim: SpeedTrim::new(),
            speed_carry: SpeedCarry::new(),
        }
    }
}

/// Per-connection counters: zeroed with every new `Shared`.
#[derive(Default)]
pub struct ConnStats {
    pub total_audio_frames: AtomicU64,
    pub total_video_frames: AtomicU64,
    pub pts_repairs: AtomicU64,
    pub pts_normalizations: AtomicU64,
    pub pts_interpolations: AtomicU64,
    pub pts_resets: AtomicU64,
    pub pts_last_gap_ms: AtomicI32,
    pub pts_max_gap_ms: AtomicI32,
    pub silence_insertions: AtomicU64,
    pub audio_underruns: AtomicU64,
    pub audio_resync_skipped_chunks: AtomicU64,
    pub audio_hidden_trimmed_chunks: AtomicU64,
    pub audio_quality_events: AtomicU64,
    pub audio_output_restarts: AtomicU64,
    pub audio_decoder_flushes: AtomicU64,
    pub video_corrupt_frames: AtomicU64,
    pub video_corrupt_held: AtomicU64,
    /// `f32::to_bits` of the smoothed playback speed.
    pub current_speed_bits: AtomicU32,
    pub last_obs_lead_ns: AtomicI64,
    pub last_chunk_stream_ns: AtomicU64,
    pub last_chunk_obs_ns: AtomicU64,
    pub last_frames_out: AtomicU32,
    pub last_samples_per_sec: AtomicU32,
    pub video_lead_ns: AtomicI64,
    /// EMA of decoded PTS deltas; written by the receiver, read by video.
    pub video_frame_interval_ns: AtomicI64,
    // Mirrors of the video thread's anchors for stats / media_get_state.
    pub video_ts_init: AtomicBool,
    pub video_sys_base: AtomicU64,
    pub video_pts_base: AtomicI64,
    pub last_video_width: AtomicI32,
    pub last_video_height: AtomicI32,
}

impl ConnStats {
    pub fn current_speed(&self) -> f32 {
        let bits = self.current_speed_bits.load(Relaxed);
        if bits == 0 { 1.0 } else { f32::from_bits(bits) }
    }

    pub fn set_current_speed(&self, speed: f32) {
        self.current_speed_bits.store(speed.to_bits(), Relaxed);
    }
}

/// Counters cumulative for the source's life: exactly the set the C
/// `reset_runtime_state()` left alone.
#[derive(Default)]
pub struct LifetimeStats {
    pub reconnect_count: AtomicU64,
    pub video_queue_drops: AtomicU64,
    pub video_pinned_peak: AtomicI32,
    pub pacing_now: AtomicI32,
    pub pacing_peak: AtomicI32,
    pub pacing_bytes: AtomicUsize,
    pub pacing_overflows: AtomicU64,
    pub audio_fill_peak_ms: AtomicI32,
    pub video_lead_peak_ns: AtomicI64,
    pub video_lead_excess: AtomicU64,
    pub audio_offset_reanchors: AtomicU64,
    pub video_pkt_eagain: AtomicU64,
    pub audio_pkt_eagain: AtomicU64,
    pub video_pkt_dropped: AtomicU64,
    pub audio_pkt_dropped: AtomicU64,
}

impl LifetimeStats {
    /// `field = max(field, value)`.
    pub fn note_peak_i32(field: &AtomicI32, value: i32) {
        field.fetch_max(value, Relaxed);
    }

    pub fn note_peak_i64(field: &AtomicI64, value: i64) {
        field.fetch_max(value, Relaxed);
    }
}

/// The receiver → video frame queue (C `video_queue` + `video_queue_lock` +
/// `video_queue_cond`). Depth is small because queued hardware frames pin
/// decoder surfaces; overflow drops the oldest, never the newest, so audio is
/// never stalled. Frames carry their PTS already rescaled to nanoseconds: the
/// receiver may close the format context while frames are still queued, and
/// a queued frame must not borrow it (enforced by `ffmpeg::Frame` having no
/// such lifetime).
pub struct VideoChannel {
    q: Mutex<VideoQueue>,
    cv: Condvar,
}

pub struct VideoQueue {
    frames: std::collections::VecDeque<ffmpeg::Frame>,
    /// 1 while the video thread holds a popped frame through the HW transfer.
    in_flight: usize,
    /// Set by the receiver on disconnect; the *video* thread performs the
    /// actual `obs_source_output_video(NULL)` so a frame already mid-conversion
    /// cannot repaint after the clear.
    clear_pending: bool,
}

/// A frame popped from the queue while the video thread transfers it.
/// Dropping it clears `in_flight`, so a panic mid-transfer cannot leave the
/// pinned-surface accounting wrong.
pub struct InFlight<'a> {
    channel: &'a VideoChannel,
    frame: Option<ffmpeg::Frame>,
}

impl InFlight<'_> {
    /// Take the frame out (the guard still clears `in_flight` on drop).
    pub fn take(&mut self) -> Option<ffmpeg::Frame> {
        self.frame.take()
    }

    pub fn frame(&self) -> Option<&ffmpeg::Frame> {
        self.frame.as_ref()
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut q = self.channel.q.lock();
        q.in_flight = q.in_flight.saturating_sub(1);
    }
}

impl VideoChannel {
    pub fn new() -> Self {
        Self {
            q: Mutex::new(VideoQueue {
                frames: std::collections::VecDeque::with_capacity(
                    irl_core::consts::VIDEO_QUEUE_SIZE,
                ),
                in_flight: 0,
                clear_pending: false,
            }),
            cv: Condvar::new(),
        }
    }

    /// Receiver thread: enqueue, dropping the oldest when full.
    pub fn push(&self, frame: ffmpeg::Frame, lifetime: &LifetimeStats) {
        let mut q = self.q.lock();
        if q.frames.len() >= irl_core::consts::VIDEO_QUEUE_SIZE {
            q.frames.pop_front(); // Drop → av_frame_free → surface released
            lifetime.video_queue_drops.fetch_add(1, Relaxed);
        }
        q.frames.push_back(frame);
        let pinned = (q.frames.len() + q.in_flight) as i32;
        LifetimeStats::note_peak_i32(&lifetime.video_pinned_peak, pinned);
        drop(q);
        self.cv.notify_one();
    }

    /// Receiver thread, on disconnect: drop everything queued and ask the
    /// video thread to clear the OBS frame.
    pub fn request_clear(&self) {
        let mut q = self.q.lock();
        q.frames.clear();
        q.clear_pending = true;
        drop(q);
        self.cv.notify_one();
    }

    /// Video thread: consume the clear request.
    pub fn take_clear(&self) -> bool {
        std::mem::take(&mut self.q.lock().clear_pending)
    }

    /// Video thread: pop the oldest frame, marking it in flight.
    pub fn pop_in_flight(&self, lifetime: &LifetimeStats) -> Option<InFlight<'_>> {
        let mut q = self.q.lock();
        let frame = q.frames.pop_front()?;
        q.in_flight += 1;
        let pinned = (q.frames.len() + q.in_flight) as i32;
        LifetimeStats::note_peak_i32(&lifetime.video_pinned_peak, pinned);
        drop(q);
        Some(InFlight {
            channel: self,
            frame: Some(frame),
        })
    }

    pub fn len(&self) -> usize {
        self.q.lock().frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Video thread pacing sleep: returns as soon as a frame or a clear
    /// arrives, or the run is stopping, or `timeout` elapses. The predicate is
    /// re-checked under the lock, as in the C. Spurious wakeups are allowed;
    /// callers re-derive due times from the OBS clock every cycle.
    pub fn wait(&self, timeout: Duration, active: &AtomicBool) {
        let mut q = self.q.lock();
        if !q.clear_pending && q.frames.is_empty() && active.load(Relaxed) {
            self.cv.wait_for(&mut q, timeout);
        }
    }

    /// Stop path: wake the sleeper.
    pub fn wake_all(&self) {
        self.cv.notify_all();
    }

    /// Receiver stop: drain everything (frames are freed).
    pub fn drain(&self) {
        self.q.lock().frames.clear();
    }
}

impl Default for VideoChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared state for one receiver run.
pub struct Shared {
    pub source: obs::SourceHandle,
    pub cfg: StreamConfig,
    pub hot: HotConfig,
    pub flags: RunFlags,
    pub audio_state: Mutex<AudioState>,
    /// The jitter buffer. `None` until the first audio frame configures it.
    /// Lock order: `audio_state` before this.
    pub audio_buf: Mutex<Option<AudioBuffer>>,
    pub video: VideoChannel,
    pub conn: ConnStats,
    pub lifetime: Arc<LifetimeStats>,
    pub interrupt: Arc<ffmpeg::InterruptWatch>,
}

impl Shared {
    /// Build the state for a fresh run. `hot` carries the current hot values
    /// (the OBS thread's authoritative config); `lifetime` survives runs.
    pub fn new(
        source: obs::SourceHandle,
        cfg: StreamConfig,
        hot: HotValues,
        lifetime: Arc<LifetimeStats>,
    ) -> Arc<Self> {
        let thread_active = Arc::new(AtomicBool::new(false));
        let interrupt = ffmpeg::InterruptWatch::new(
            thread_active.clone(),
            irl_core::consts::IO_STALL_TIMEOUT_US,
        );
        Arc::new(Self {
            source,
            audio_state: Mutex::new(AudioState::new(irl_core::consts::STARTUP_AUDIO_WARMUP_MS)),
            audio_buf: Mutex::new(None),
            video: VideoChannel::new(),
            conn: ConnStats::default(),
            lifetime,
            interrupt,
            hot: HotConfig {
                reconnect_delay_s: AtomicI32::new(hot.reconnect_delay_s),
                adaptive_speed: AtomicBool::new(hot.adaptive_speed),
                catchup_percent: AtomicI32::new(hot.catchup_percent),
                wait_for_keyframe: AtomicBool::new(hot.wait_for_keyframe),
                clear_on_disconnect: AtomicBool::new(hot.clear_on_disconnect),
                watermarks: Mutex::new(hot.watermarks),
            },
            flags: RunFlags {
                thread_active,
                reconnecting: AtomicBool::new(false),
                audio_present: AtomicBool::new(false),
            },
            cfg,
        })
    }

    pub fn is_active(&self) -> bool {
        self.flags.thread_active.load(Relaxed)
    }

    /// Lock the audio state. The pump takes this exactly once per iteration.
    pub fn audio_state(&self) -> MutexGuard<'_, AudioState> {
        self.audio_state.lock()
    }

    /// Lock the jitter buffer (only while holding, or never needing,
    /// `audio_state`).
    pub fn audio_buf(&self) -> MutexGuard<'_, Option<AudioBuffer>> {
        self.audio_buf.lock()
    }
}

/// Plain copy of the hot settings, used to seed [`HotConfig`].
#[derive(Debug, Clone, Copy)]
pub struct HotValues {
    pub reconnect_delay_s: i32,
    pub adaptive_speed: bool,
    pub catchup_percent: i32,
    pub wait_for_keyframe: bool,
    pub clear_on_disconnect: bool,
    pub watermarks: Watermarks,
}

/// Spawn a worker thread whose panic is contained: it is logged, the run is
/// flagged inactive (which also trips the FFmpeg interrupt watch so a receiver
/// blocked in `av_read_frame` unblocks) and the video sleeper is woken, after
/// which the normal stop/reconnect path takes over.
pub fn spawn_worker(
    name: &'static str,
    shared: Arc<Shared>,
    body: fn(Arc<Shared>),
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(shared.clone())));
            if let Err(payload) = result {
                let msg = obs::panic::payload_message(payload.as_ref());
                irl_error!("{name} thread panicked: {msg}; stopping the stream");
                shared.flags.thread_active.store(false, Relaxed);
                shared.video.wake_all();
            }
        })
}
