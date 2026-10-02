//! State shared between the OBS thread and the three worker threads for one
//! run of the receiver (`start_receiver` … `stop_receiver`).
//!
//! Ownership map:
//! - OBS thread only (`source.rs`): `fit_pending`, `media_stopped`,
//!   `close_when_inactive`, the authoritative `Config`, the thread handles.
//! - [`Shared`]: built fresh at every `start_receiver`, so per-run state
//!   starts zeroed; what has to survive a restart lives in [`LifetimeStats`].
//! - [`AudioState`] under `Shared::audio_state`. The audio pump locks it
//!   **once** per `pump_once` and passes `&mut` down; nothing below may lock
//!   it again.
//! - `Shared::audio_buf`: the jitter buffer under its own lock. Lock order:
//!   `audio_state` → `audio_buf` → `hot.watermarks`. `video.q` is never held
//!   together with any of them.
//! - [`ConnStats`] / [`LifetimeStats`]: counters as relaxed atomics, readable
//!   from any thread without a lock.
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

use irl_core::arrival::ArrivalFloor;
use irl_core::{
    AudioBuffer, AudioHold, DrainWatch, HwDecode, LastSample, OutputClock, SpeedCarry, SpeedTrim,
    Watermarks,
};

/// Settings latched when the stream opens; changing any of them forces a
/// restart (`Config::requires_restart`).
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub url: CString,
    pub ffmpeg_options: Option<String>,
    pub hw_decode: HwDecode,
    pub low_latency_audio: bool,
}

/// Settings swapped in place while the workers run (`Config::apply_hot`).
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
    /// Swap in the atomic hot values. The watermarks are not among them: they
    /// publish only once the ring has grown (`config::publish_watermarks`).
    pub fn store(&self, hot: &HotValues) {
        self.reconnect_delay_s.store(hot.reconnect_delay_s, Relaxed);
        self.adaptive_speed.store(hot.adaptive_speed, Relaxed);
        self.catchup_percent.store(hot.catchup_percent, Relaxed);
        self.wait_for_keyframe.store(hot.wait_for_keyframe, Relaxed);
        self.clear_on_disconnect
            .store(hot.clear_on_disconnect, Relaxed);
    }

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

/// The video-decode flags the audio path also touches. `first_keyframe` gates
/// audio intake as well as video output, and the receiver clears `corrupted`
/// when PTS repair resets the timeline. The rest of video decode state is the
/// video thread's own.
#[derive(Default)]
pub struct VideoFlags {
    pub first_keyframe: AtomicBool,
    pub corrupted: AtomicBool,
    /// An audio PTS reset broke the timeline, so the video thread's
    /// frame-interval estimate and decoder-error bookkeeping no longer describe
    /// this stream. Set by the receiver, consumed by the video thread.
    pub timeline_reset: AtomicBool,
}

/// Run-level flags.
pub struct RunFlags {
    /// An `Arc` because the FFmpeg interrupt watch shares it, so a stop
    /// request reaches a receiver blocked inside `av_read_frame`.
    pub thread_active: Arc<AtomicBool>,
    pub reconnecting: AtomicBool,
    /// "This connection carries audio", mirrored for the video thread, which
    /// must not touch the receiver's `audio_stream_idx`.
    pub audio_present: AtomicBool,
    /// "This connection carries video", for the audio pump, which waits for a
    /// skew reading before it primes (`audio::hold::prime_may_wait`).
    pub video_present: AtomicBool,
}

/// Audio timing state that is not a counter, under `Shared::audio_state`.
#[derive(Debug, Default)]
pub struct AudioState {
    pub clock: OutputClock,

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

    /// Stream PTS of the newest decoded audio (ns).
    pub latest_audio_stream_pts_ns: i64,

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
    /// Set at priming: the next read is sized to put the target on the
    /// buffer's residual grid instead of leaving the loop to straddle it. See
    /// [`irl_core::timing::aligning_read_frames`].
    pub align_read_pending: bool,

    /// The audio hold (`irl_core::audio_hold`, wired in by `audio::hold`):
    /// the skew readings, taken by the receiver as it reads each video packet.
    pub hold: AudioHold,
    /// Whether video read now is live rather than a relay's replay catching
    /// up, which would read as skew.
    pub hold_live: ArrivalFloor,
    /// The hold in force, in ms. Outside low-latency mode it is folded into
    /// the published watermarks: their target is Target Buffer plus this.
    pub hold_ms: i32,
    /// How much of the raises made since priming the buffer has yet to build.
    pub hold_unbuilt_ns: u64,
    /// Running total of what it has built, which is how far the playout
    /// offset grew for the hold. The video thread takes the growth out of its
    /// standing delay.
    pub hold_built_ns: u64,
    /// The hold when `offset_baseline_ns` was taken, so a re-anchor does not
    /// read the hold building as concealment drift.
    pub offset_baseline_hold_ms: i32,
    /// When the pump first waited on a skew reading to prime; zero while it
    /// has not.
    pub hold_wait_since_ns: u64,
    /// The pump may prime as far as the hold is concerned: a reading was
    /// taken, or the wait for one ran out.
    pub hold_wait_done: bool,
}

impl AudioState {
    /// The audio → OBS playout mapping, `(latest_obs_end_ts_ns,
    /// latest_buffered_end_pts_ns)`, once the pump has published one.
    pub fn playout_mapping(&self) -> Option<(u64, i64)> {
        (self.latest_obs_end_ts_ns != 0 && self.latest_buffered_end_pts_ns > 0)
            .then_some((self.latest_obs_end_ts_ns, self.latest_buffered_end_pts_ns))
    }
}

/// Per-connection counters: zeroed with every new `Shared`.
#[derive(Default)]
pub struct ConnStats {
    pub total_audio_frames: AtomicU64,
    pub total_video_frames: AtomicU64,
    pub pts_max_gap_ms: AtomicI32,
    pub audio_underruns: AtomicU64,
    pub audio_output_restarts: AtomicU64,
    /// Chunks dropped from the jitter buffer after playback primed, which only
    /// the low-latency backlog cap does. Not a stat: it is how the network
    /// simulation holds buffered mode to "audible audio is never skipped".
    pub audible_skipped_chunks: AtomicU64,
    /// HEVC frames held back for a missing reference, for the line that
    /// reports the hold ending.
    pub video_corrupt_held: AtomicU64,
    /// `f32::to_bits` of the smoothed playback speed.
    pub current_speed_bits: AtomicU32,
    /// Mirror of the video thread's standing delay, for the stats.
    pub video_delay_ns: AtomicU64,
    /// Mirror of `AudioState::hold_ms`, for the stats and the video thread.
    pub audio_hold_ms: AtomicI32,
    /// PTS of the newest video packet the receiver pushed, for `av_skew_ms`.
    /// Taken at arrival: the decoded-frame PTS trails it by however long the
    /// packet waited for its due time, which is not the sender's doing.
    pub video_arrival_pts_ns: AtomicI64,
    /// EMA of decoded PTS deltas, for the frame rate the stats line reports.
    pub video_frame_interval_ns: AtomicI64,
    /// Mirror of the video thread's fallback anchor, for `media_get_state`
    /// and for the receiver to request a re-anchor by clearing it.
    pub video_ts_init: AtomicBool,
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

/// Counters cumulative for the source's life, carried across runs.
#[derive(Default)]
pub struct LifetimeStats {
    pub video_queue_drops: AtomicU64,
    /// Peak packets queued for decode.
    pub video_queue_peak: AtomicI32,
    pub pacing_now: AtomicI32,
    pub pacing_peak: AtomicI32,
    pub pacing_bytes: AtomicUsize,
    pub audio_fill_peak_ms: AtomicI32,
    /// Declared output-clock re-anchors. Not a stat: the network simulation
    /// counts them, with the output restarts, as the only places the OBS
    /// clock may jump.
    pub audio_offset_reanchors: AtomicU64,
    pub video_pkt_eagain: AtomicU64,
    pub audio_pkt_eagain: AtomicU64,
    pub video_pkt_dropped: AtomicU64,
    pub audio_pkt_dropped: AtomicU64,
}

/// The receiver → video queue: **compressed packets**, not decoded frames.
///
/// This is where video's share of the configured latency is held. Held as
/// decoded frames it would not scale (8s of 4K60 is ~6GB; the same 8s of
/// packets is ~20MB), so the video thread decodes each packet just before it
/// is due and keeps only [`irl_core::consts::VIDEO_DECODE_LEAD_MS`] of
/// decoded frames alive. Decode cannot live on the receiver thread either:
/// during a network stall it is blocked in `av_read_frame`, which is exactly
/// when video must keep draining the buffer it has.
///
/// Bounded by media duration and bytes. Overflow drops from the front, which
/// costs artifacts until the next keyframe; the receiver's read backpressure
/// stops ingest long before this fills.
#[derive(Default)]
pub struct VideoChannel {
    q: Mutex<VideoQueue>,
    cv: Condvar,
}

/// A packet waiting to be decoded, with everything the queue needs to bound
/// itself. `pts_ns` is approximate: it only feeds the span bound, and output
/// timing comes from the decoded frame's own PTS.
pub struct TimedPacket {
    pub packet: ffmpeg::Packet,
    pub pts_ns: i64,
    pub bytes: usize,
    /// OBS clock when the receiver queued it. The frames it decodes into can
    /// be in hand no earlier, so this is where their arrival margin is
    /// measured from (`irl_core::video_delay`).
    pub received_ns: u64,
}

/// What the receiver sends the video thread, in order.
pub enum VideoMsg {
    /// A new connection's decoder. Ordering matters: packets after this one
    /// belong to it, and packets before it to the decoder it replaces.
    Decoder(Box<VideoDecoder>),
    Packet(TimedPacket),
}

/// A video decoder handed from the receiver (which opens the stream) to the
/// video thread (which owns it from then on).
pub struct VideoDecoder {
    pub ctx: ffmpeg::CodecContext,
    pub time_base: ffmpeg::Rational,
    pub codec_id: ffmpeg::AVCodecID,
}

#[derive(Default)]
struct VideoQueue {
    msgs: std::collections::VecDeque<VideoMsg>,
    packets: usize,
    bytes: usize,
    /// Set by the receiver on disconnect; the *video* thread performs the
    /// actual `obs_source_output_video(NULL)` so a frame already mid-conversion
    /// cannot repaint after the clear.
    clear_pending: bool,
}

impl VideoQueue {
    fn clear(&mut self) {
        self.msgs.clear();
        self.packets = 0;
        self.bytes = 0;
    }

    fn has_work(&self, has_room: bool) -> bool {
        self.clear_pending || (has_room && !self.msgs.is_empty())
    }

    /// Media time between the oldest and newest queued packet.
    fn span_ns(&self) -> i64 {
        let mut first = None;
        let mut last = None;
        for msg in &self.msgs {
            if let VideoMsg::Packet(p) = msg {
                first.get_or_insert(p.pts_ns);
                last = Some(p.pts_ns);
            }
        }
        match (first, last) {
            (Some(a), Some(b)) => (b - a).max(0),
            _ => 0,
        }
    }
}

impl VideoChannel {
    /// Receiver thread: hand the video thread the decoder for a new
    /// connection.
    pub fn install_decoder(&self, decoder: VideoDecoder) {
        let mut q = self.q.lock();
        q.msgs.push_back(VideoMsg::Decoder(Box::new(decoder)));
        drop(q);
        self.cv.notify_one();
    }

    /// Receiver thread: enqueue a packet, dropping the oldest if a bound is
    /// exceeded.
    pub fn push_packet(&self, packet: TimedPacket, lifetime: &LifetimeStats) {
        let mut q = self.q.lock();
        q.bytes += packet.bytes;
        q.packets += 1;
        q.msgs.push_back(VideoMsg::Packet(packet));

        while q.bytes > irl_core::consts::VIDEO_PACKET_QUEUE_MAX_BYTES
            || q.span_ns() > irl_core::consts::VIDEO_PACKET_QUEUE_MAX_MS * 1_000_000
        {
            // Dropping a packet costs artifacts until the next keyframe, so
            // this is a last resort the read backpressure should prevent.
            let Some(dropped) = q.msgs.pop_front() else {
                break;
            };
            if let VideoMsg::Packet(p) = dropped {
                q.bytes -= p.bytes;
                q.packets -= 1;
                lifetime.video_queue_drops.fetch_add(1, Relaxed);
            }
        }

        lifetime
            .video_queue_peak
            .fetch_max(q.packets as i32, Relaxed);
        drop(q);
        self.cv.notify_one();
    }

    /// Receiver thread, on disconnect: drop everything queued and ask the
    /// video thread to clear the OBS frame.
    pub fn request_clear(&self) {
        let mut q = self.q.lock();
        q.clear();
        q.clear_pending = true;
        drop(q);
        self.cv.notify_one();
    }

    /// Video thread: consume the clear request.
    pub fn take_clear(&self) -> bool {
        std::mem::take(&mut self.q.lock().clear_pending)
    }

    /// Whether the next message is a decoder handover, which the video thread
    /// takes regardless of pacing room.
    pub fn next_is_decoder(&self) -> bool {
        matches!(self.q.lock().msgs.front(), Some(VideoMsg::Decoder(_)))
    }

    /// Video thread: take the next message.
    pub fn pop(&self) -> Option<VideoMsg> {
        let mut q = self.q.lock();
        let msg = q.msgs.pop_front()?;
        if let VideoMsg::Packet(p) = &msg {
            q.bytes -= p.bytes;
            q.packets -= 1;
        }
        Some(msg)
    }

    /// Packets queued for decode.
    pub fn len(&self) -> usize {
        self.q.lock().packets
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of compressed video queued.
    pub fn bytes(&self) -> usize {
        self.q.lock().bytes
    }

    /// Media duration queued for decode: the latency actually being held.
    pub fn span_ns(&self) -> i64 {
        self.q.lock().span_ns()
    }

    /// Whether the video thread has something to do right now.
    ///
    /// A queued packet is only work if there is somewhere to put what it
    /// decodes (`has_room` is the pacing queue's). Counting any queued message
    /// as work would spin the thread at full CPU whenever the pacing queue
    /// sits at its decode lead, which is the normal steady state.
    pub fn has_work(&self, has_room: bool) -> bool {
        self.q.lock().has_work(has_room)
    }

    /// Video thread pacing sleep: returns as soon as there is work, or the run
    /// is stopping, or `timeout` elapses. The predicate is re-checked under the
    /// lock. Spurious wakeups are allowed; callers re-derive due times from the
    /// OBS clock every cycle.
    pub fn wait(&self, timeout: Duration, has_room: bool, active: &AtomicBool) {
        let mut q = self.q.lock();
        if !q.has_work(has_room) && active.load(Relaxed) {
            self.cv.wait_for(&mut q, timeout);
        }
    }

    /// Stop path: wake the sleeper.
    pub fn wake_all(&self) {
        self.cv.notify_all();
    }

    /// Receiver stop: drain everything.
    pub fn drain(&self) {
        self.q.lock().clear();
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
    /// Video-decode state the audio path also reads or clears.
    pub video_flags: VideoFlags,
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
            audio_state: Mutex::new(AudioState {
                startup_warmup_remaining_ms: irl_core::consts::STARTUP_AUDIO_WARMUP_MS,
                ..AudioState::default()
            }),
            audio_buf: Mutex::new(None),
            video: VideoChannel::default(),
            video_flags: VideoFlags::default(),
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
                video_present: AtomicBool::new(false),
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
