//! Every tuning constant of the plugin, in one place. Values are the C
//! plugin's (`include/irl-source.h` and the file-local `#define`s); the
//! `consts_match_c_values` test pins them so a typo is caught once.

/// The source id registered with OBS; also what the websocket vendor matches.
pub const SOURCE_ID: &str = "irl_source";
/// obs-websocket vendor name.
pub const VENDOR_NAME: &str = "obs-irl-source";
/// Version of the vendor request API this plugin serves.
pub const VENDOR_API_VERSION: i64 = 1;

// ── Settings defaults ──

/// Seconds between reconnect attempts.
pub const DEFAULT_RECONNECT_DELAY_S: i64 = 2;
/// Reconnect delay property bounds.
pub const RECONNECT_DELAY_MIN_S: i32 = 1;
/// Reconnect delay property bounds.
pub const RECONNECT_DELAY_MAX_S: i32 = 60;
/// Transport receive buffer handed to FFmpeg (`buffer_size` / `recv_buffer_size`).
/// Formerly the dead `network_buffer_mb` setting; now a constant.
pub const NETWORK_BUFFER_MB: i64 = 2;
/// Target jitter buffer fill.
pub const DEFAULT_BUFFER_TARGET_MS: i64 = 120;
/// Target buffer property bounds and step.
pub const BUFFER_TARGET_MIN_MS: i32 = 20;
/// Target buffer property bounds and step.
pub const BUFFER_TARGET_MAX_MS: i32 = 2000;
/// Target buffer property bounds and step.
pub const BUFFER_TARGET_STEP_MS: i32 = 10;
/// Adaptive latency control default.
pub const DEFAULT_ADAPTIVE_SPEED: bool = true;
/// Wait for the first keyframe before showing video.
pub const DEFAULT_WAIT_FOR_KEYFRAME: bool = true;
/// Low-latency (unbuffered) audio mode default.
pub const DEFAULT_LOW_LATENCY_AUDIO: bool = false;
/// Close the stream when the source is hidden/inactive.
pub const DEFAULT_CLOSE_WHEN_INACTIVE: bool = false;
/// Show nothing when the stream ends.
pub const DEFAULT_CLEAR_ON_DISCONNECT: bool = true;

// ── Buffer watermark derivation ──

/// `min = max(target / MIN_DIVISOR, MIN_FLOOR)`.
pub const BUFFER_MIN_DIVISOR: i64 = 2;
/// Floor for the derived minimum watermark.
pub const BUFFER_MIN_FLOOR_MS: i64 = 20;
/// `max = target + MAX_EXTRA`.
pub const BUFFER_MAX_EXTRA_MS: i64 = 200;
/// Ring capacity is this many times `buffer_max_ms` (see `audio-buffer.c`).
pub const BUFFER_CAPACITY_MULTIPLIER: i64 = 4;

// ── PTS repair ──

/// Below this gap the PTS is interpolated (decoder wobble).
pub const SMALL_GAP_MS: i32 = 70;
/// At or above this gap the timeline is reset.
pub const LARGE_GAP_MS: i32 = 2000;
/// Consecutive identical small repairs before entering relock.
pub const PTS_SMALL_GAP_RELOCK_COUNT: i32 = 8;
/// Tolerance for treating two small gaps as identical, and for exiting relock.
pub const PTS_SMALL_GAP_TOLERANCE_MS: i32 = 2;
/// Slew per frame while relocking.
pub const PTS_RELOCK_STEP_MS: i32 = 2;
/// Parallel PTS chunk queue depth of the ring buffer.
pub const AUDIO_PTS_MAX_CHUNKS: usize = 256;

// ── Audio output ──

/// Fade on disconnect/reconnect (avoids clicks).
pub const FADE_DURATION_MS: i32 = 50;
/// Decoded audio discarded at startup to skip decoder warm-up artifacts.
pub const STARTUP_AUDIO_WARMUP_MS: i32 = 150;
/// Above this fill the receiver stops reading and lets the transport buffer.
pub const BLEED_PACE_FILL_MS: i32 = 1000;
/// Playout offset drift past the primed baseline that triggers a re-anchor.
pub const AUDIO_OFFSET_REANCHOR_MARGIN_MS: i64 = 400;
/// Recovery hold after an underrun (microseconds).
pub const AUDIO_RECOVERY_HOLD_US: u64 = 1_500_000;
/// Hidden backlog trim trigger above target.
pub const AUDIO_TRIM_TRIGGER_MS: i32 = 90;
/// Fade applied when resuming from concealment.
pub const AUDIO_CONCEAL_FADE_MS: i32 = 8;
/// Minimum lead of queued audio ahead of wall clock.
pub const AUDIO_OUT_LEAD_MS: i32 = 80;
/// Output clock lag past which the clock line is restarted.
pub const AUDIO_OUT_MAX_LAG_MS: i64 = 150;
/// Slowest playback speed (buffer building).
pub const AUDIO_SPEED_MIN: f32 = 0.98;
/// Fastest playback speed (backlog drain).
pub const AUDIO_SPEED_MAX: f32 = 1.05;
/// Fill deadband around target where speed is 1.0.
pub const AUDIO_SPEED_DEADBAND_MS: i32 = 20;
/// EMA factor applied to the speed target per pump cycle.
pub const AUDIO_SPEED_SMOOTHING: f32 = 0.05;
/// Low-latency mode: skip old chunks above this fill.
pub const AUDIO_LL_MAX_FILL_MS: i32 = 100;
/// A drain at full authority that has not progressed for this long is stuck.
pub const AUDIO_DRAIN_STUCK_US: u64 = 20_000_000;
/// Progress that resets the stuck-drain watch.
pub const AUDIO_DRAIN_STUCK_PROGRESS_MS: i32 = 100;
/// Soft compensation is applied only for deltas within ±this many samples.
pub const AUDIO_SOFT_COMPENSATION_MAX_SAMPLES: i32 = 8;
/// Default chunk size before the decoder reports one (Opus frame).
pub const AUDIO_DEFAULT_FRAME_SAMPLES: i32 = 960;
/// Pump iterations per audio-thread wakeup.
pub const AUDIO_PUMP_BURST: u32 = 16;
/// Sleep between audio-thread wakeups.
pub const AUDIO_PUMP_SLEEP_MS: u32 = 1;
/// Maximum channels remembered for silence shaping.
pub const AUDIO_MAX_CHANNELS: usize = 8;

// ── Decode ──

/// Cooldown between audio decoder flushes on corruption bursts.
pub const DECODER_FLUSH_COOLDOWN_US: u64 = 350_000;
/// Throttle for decoder warning log lines.
pub const DECODER_WARNING_INTERVAL_US: u64 = 1_000_000;
/// Consecutive decode errors before the (audio) decoder is flushed.
pub const DECODER_ERROR_BURST: i32 = 3;
/// Video thread count handed to the decoder.
pub const VIDEO_DECODER_THREADS: i32 = 4;
/// Receiver → video queue depth.
pub const VIDEO_QUEUE_SIZE: usize = 4;
/// `extra_hw_frames`: the queue plus the two frames in flight around it.
pub const VIDEO_EXTRA_HW_FRAMES: i32 = VIDEO_QUEUE_SIZE as i32 + 2;

// ── Video timing / pacing ──

/// Reporting budget for frames parked in libobs's async queue.
pub const OBS_ASYNC_FRAME_BUDGET: i64 = 24;
/// Throttle for the video lead warning.
pub const VIDEO_LEAD_WARN_INTERVAL_NS: u64 = 10_000_000_000;
/// Bounds on the measured frame interval (250fps..10fps).
pub const VIDEO_INTERVAL_MIN_NS: i64 = 4_000_000;
/// Bounds on the measured frame interval (250fps..10fps).
pub const VIDEO_INTERVAL_MAX_NS: i64 = 100_000_000;
/// Interval estimate before enough frames have arrived.
pub const VIDEO_INTERVAL_DEFAULT_NS: i64 = 33_333_333;
/// Pacing queue frame ceiling.
pub const VIDEO_PACING_MAX_FRAMES: usize = 512;
/// Pacing queue byte ceiling (1 GiB).
pub const VIDEO_PACING_MAX_BYTES: usize = 1024 * 1024 * 1024;
/// Emit rather than sleep again when this close to due.
pub const VIDEO_PACING_SLACK_NS: i64 = 1_000_000;
/// Ceiling on a single pacing sleep.
pub const VIDEO_PACING_MAX_WAIT_MS: u64 = 50;
/// How long the last audio playout offset is reused after it goes away.
pub const VIDEO_OFFSET_HOLD_NS: u64 = 500_000_000;
/// Video-only fallback: clamp on drift between stream and system clock.
pub const VIDEO_TS_CLAMP_NS: i64 = 500_000_000;
/// Video-only fallback: forward cap.
pub const VIDEO_TS_CAP_NS: u64 = 200_000_000;
/// Plane alignment of the transfer pool (FFmpeg's uncached-copy fast path).
pub const XFER_PLANE_ALIGN: i32 = 64;
/// Transfer pool dimension alignment.
pub const XFER_DIM_ALIGN: i32 = 16;

// ── Stream / network ──

/// Abort a blocking FFmpeg I/O call after this long without progress.
pub const IO_STALL_TIMEOUT_US: u64 = 10_000_000;
/// `probesize` / `analyzeduration` for the fast reconnect probe.
pub const PROBE_FAST: i64 = 1_000_000;
/// `probesize` / `analyzeduration` for the full probe.
pub const PROBE_FULL: i64 = 5_000_000;
/// `latency` passed to libsrt (microseconds).
pub const SRT_LATENCY_US: i64 = 200_000;
/// `rtmp_buffer` in milliseconds.
pub const RTMP_BUFFER_MS: i64 = 1000;
/// UDP `fifo_size` is only set when it exceeds FFmpeg's default of 7×4096 packets.
pub const UDP_FIFO_DEFAULT_PACKETS: i64 = 7 * 4096;
/// Interval of the periodic receiver stats log line.
pub const STATS_LOG_INTERVAL_NS: u64 = 30_000_000_000;
