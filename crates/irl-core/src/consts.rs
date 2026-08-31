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
/// Target buffer property floor.
pub const BUFFER_TARGET_MIN_MS: i32 = 20;
/// Target buffer property ceiling.
///
/// Not a limit of the controller — it is where holding the cushion stops
/// being free. Every millisecond of audio buffer is also a millisecond of
/// decoded video held in the pacing queue (see [`VIDEO_PACING_MAX_FRAMES`] /
/// [`VIDEO_PACING_MAX_BYTES`]), and the whole target is paid as startup delay
/// before playback primes. High-bitrate uplinks with deep sender-side
/// buffering do stall for several seconds, though, and 2s could not ride
/// those out, so the ceiling is set by what the video side can still pace
/// rather than by what the audio side needs.
pub const BUFFER_TARGET_MAX_MS: i32 = 8000;
/// Target buffer property step.
pub const BUFFER_TARGET_STEP_MS: i32 = 10;
/// Adaptive latency control default.
pub const DEFAULT_ADAPTIVE_SPEED: bool = true;

/// Catch-up (drain) speed authority, as a percentage above native rate.
///
/// The build direction stays fixed at an inaudible −2 %; this is the drain
/// direction, which is the audible one — 5 % is ~85 cents, obvious on music
/// and unremarkable on speech. Lower it to make a recovery slower but
/// inaudible, raise it to clear a backlog faster. Bounded below by the speed
/// trim's own ±1 % authority (a ceiling under that would leave the integral
/// term with nothing to work in) and above by where the pitch shift stops
/// sounding like anything but a fast-forward.
pub const DEFAULT_CATCHUP_PERCENT: i64 = 5;
/// Catch-up speed slider floor. See [`DEFAULT_CATCHUP_PERCENT`].
pub const CATCHUP_PERCENT_MIN: i32 = 2;
/// Catch-up speed slider ceiling. See [`DEFAULT_CATCHUP_PERCENT`].
pub const CATCHUP_PERCENT_MAX: i32 = 15;
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
///
/// The fast end is not a constant: it is the Catch-Up Speed setting, read per
/// use through [`crate::speed::catchup_speed_max`].
pub const AUDIO_SPEED_MIN: f32 = 0.98;
/// Fill deadband around target where the ramp is nearly flat.
pub const AUDIO_SPEED_DEADBAND_MS: i32 = 20;
/// EMA factor applied to the speed target per pump cycle.
pub const AUDIO_SPEED_SMOOTHING: f32 = 0.05;

/// Speed at the edge of the deadband.
///
/// The deadband used to be flat: dead-on 1.0 anywhere within 20 ms of target.
/// That is fine for a proportional-only loop, and fatal once the trim is added
/// — a region with zero proportional feedback leaves the integrator undamped,
/// and the pair limit-cycles through it forever (simulated: ±20 ms of fill on
/// a ~2 minute period, never settling). A shallow slope through the deadband
/// restores the damping. At 0.2 % it is 3.5 cents at the very edge, an order
/// of magnitude under anything audible, and it makes the ramp continuous where
/// it used to step.
pub const AUDIO_SPEED_DEADBAND_SLOPE: f32 = 0.002;

/// Integral gain of the speed trim, in 1/s² (error in seconds of buffer, dt
/// in seconds).
///
/// Deliberately far slower than the ramp. Their jobs are separated in time,
/// not in signal: the ramp owns transients (closed-loop time constant of a few
/// seconds), the trim owns the constant underneath them and converges over a
/// minute or two. Picked as the natural frequency of the level/trim loop,
/// ω = √gain ≈ 0.05 rad/s, which is ~20× slower than the ramp and so cannot
/// beat against it.
pub const AUDIO_SPEED_TRIM_GAIN: f64 = 0.0025;
/// Authority of the speed trim: enough for any real crystal (<0.01 %) or
/// frame-rate mismatch (~0.1 %), far below audibility (±1 % is 17 cents), and
/// small enough that the ramp keeps essentially all of its own authority.
pub const AUDIO_SPEED_TRIM_MAX: f32 = 0.01;
/// Only integrate while the level is within this much of target.
///
/// Further out the loop is working a transient — a backlog draining, a buffer
/// refilling — and the level is reporting that transient, not the sender's
/// rate. Three deadbands is comfortably wider than the standing error any rate
/// inside the trim's own authority can produce, so nothing the trim is meant
/// to correct falls outside the window.
pub const AUDIO_SPEED_TRIM_ERR_WINDOW_MS: i32 = 3 * AUDIO_SPEED_DEADBAND_MS;
/// A dt this long means the audio thread was not running (debugger, laptop
/// sleep, starvation). Integrating across it would credit the whole gap to the
/// sender's clock.
pub const AUDIO_SPEED_TRIM_MAX_DT_US: u64 = 1_000_000;
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
/// Sleep between audio-thread wakeups when the pump cannot say when it will
/// next have work (waiting for data rather than for the clock).
pub const AUDIO_PUMP_SLEEP_MS: u32 = 1;
/// Ceiling on the pump's own "next chunk is due in N ms" sleep.
///
/// Once primed the pump knows exactly when it next has to emit — the output
/// clock says so — and polling at 1 ms in the meantime is 1000 wakeups a second
/// taking two mutexes each, on a thread that has nothing to do. Sleeping to the
/// deadline instead costs nothing in responsiveness, because the deadline is
/// what gates emission. The cap keeps the per-cycle work that is not emission
/// (the concealment re-anchor, the fill peak the stats line reports) running at
/// a sane rate, and bounds how long a stop request waits for this thread.
pub const AUDIO_PUMP_MAX_SLEEP_MS: u32 = 20;
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
///
/// It has to carry the largest Target Buffer at the highest frame rate anyone
/// streams: the lead is the audio buffer, so 8 s at 120 fps is 960 frames. At
/// 512 the count bound, not the byte bound, was what decided when pacing gave
/// up — and it did so at a different latency for every frame rate. The byte
/// ceiling below is the one that should bind.
pub const VIDEO_PACING_MAX_FRAMES: usize = 1024;
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

/// Ring capacity when the format is degenerate and `4 × max_ms` works out to
/// nothing (`audio_buffer_init`'s `buf->capacity = 65536` fallback).
pub const AUDIO_BUFFER_FALLBACK_CAPACITY: usize = 65536;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constant, pinned against the value it has in the C plugin, so a
    /// typo in one of the tables above is caught once rather than diagnosed
    /// from a stream that sounds slightly wrong.
    ///
    /// The C source of each value is in the comment beside it: `irl-source.h`
    /// unless a file is named.
    #[test]
    fn consts_match_c_values() {
        // ── identity ──
        assert_eq!(SOURCE_ID, "irl_source"); // IRL_SOURCE_ID
        assert_eq!(VENDOR_NAME, "obs-irl-source"); // websocket-vendor.c
        assert_eq!(VENDOR_API_VERSION, 1); // websocket-vendor.c

        // ── settings defaults ──
        assert_eq!(DEFAULT_RECONNECT_DELAY_S, 2); // IRL_DEFAULT_RECONNECT_DELAY
        assert_eq!(RECONNECT_DELAY_MIN_S, 1); // settings.c
        assert_eq!(RECONNECT_DELAY_MAX_S, 60); // settings.c
        assert_eq!(NETWORK_BUFFER_MB, 2); // IRL_DEFAULT_NETWORK_BUFFER_MB
        assert_eq!(DEFAULT_BUFFER_TARGET_MS, 120); // IRL_DEFAULT_BUFFER_TARGET_MS
        assert_eq!(BUFFER_TARGET_MIN_MS, 20); // IRL_BUFFER_TARGET_MIN_MS
        assert_eq!(BUFFER_TARGET_MAX_MS, 8000); // IRL_BUFFER_TARGET_MAX_MS
        assert_eq!(BUFFER_TARGET_STEP_MS, 10); // settings.c
        const { assert!(DEFAULT_ADAPTIVE_SPEED) }; // IRL_DEFAULT_ADAPTIVE_SPEED
        assert_eq!(DEFAULT_CATCHUP_PERCENT, 5); // IRL_DEFAULT_CATCHUP_PERCENT
        assert_eq!(CATCHUP_PERCENT_MIN, 2); // IRL_CATCHUP_PERCENT_MIN
        assert_eq!(CATCHUP_PERCENT_MAX, 15); // IRL_CATCHUP_PERCENT_MAX
        const { assert!(DEFAULT_WAIT_FOR_KEYFRAME) }; // IRL_DEFAULT_WAIT_KEYFRAME
        const { assert!(!DEFAULT_LOW_LATENCY_AUDIO) }; // IRL_DEFAULT_LOW_LATENCY_AUDIO
        const { assert!(!DEFAULT_CLOSE_WHEN_INACTIVE) }; // IRL_DEFAULT_CLOSE_WHEN_INACTIVE
        const { assert!(DEFAULT_CLEAR_ON_DISCONNECT) }; // IRL_DEFAULT_CLEAR_ON_DISCONNECT

        // ── buffer watermarks ──
        assert_eq!(BUFFER_MIN_DIVISOR, 2); // IRL_BUFFER_MIN_DIVISOR
        assert_eq!(BUFFER_MIN_FLOOR_MS, 20); // IRL_BUFFER_MIN_FLOOR_MS
        assert_eq!(BUFFER_MAX_EXTRA_MS, 200); // IRL_BUFFER_MAX_EXTRA_MS
        assert_eq!(BUFFER_CAPACITY_MULTIPLIER, 4); // audio-buffer.c
        assert_eq!(AUDIO_BUFFER_FALLBACK_CAPACITY, 65536); // audio-buffer.c

        // ── PTS repair ──
        assert_eq!(SMALL_GAP_MS, 70); // IRL_SMALL_GAP_MS
        assert_eq!(LARGE_GAP_MS, 2000); // IRL_LARGE_GAP_MS
        assert_eq!(PTS_SMALL_GAP_RELOCK_COUNT, 8); // pts-repair.c
        assert_eq!(PTS_SMALL_GAP_TOLERANCE_MS, 2); // pts-repair.c
        assert_eq!(PTS_RELOCK_STEP_MS, 2); // pts-repair.c
        assert_eq!(AUDIO_PTS_MAX_CHUNKS, 256); // audio-buffer.h

        // ── audio output ──
        assert_eq!(FADE_DURATION_MS, 50); // IRL_FADE_DURATION_MS
        assert_eq!(STARTUP_AUDIO_WARMUP_MS, 150); // IRL_STARTUP_AUDIO_WARMUP_MS
        assert_eq!(BLEED_PACE_FILL_MS, 1000); // IRL_BLEED_PACE_FILL_MS
        assert_eq!(AUDIO_OFFSET_REANCHOR_MARGIN_MS, 400); // AUDIO_OFFSET_REANCHOR_MARGIN_MS
        assert_eq!(AUDIO_RECOVERY_HOLD_US, 1_500_000); // receiver-audio.c
        assert_eq!(AUDIO_TRIM_TRIGGER_MS, 90); // receiver-audio.c
        assert_eq!(AUDIO_CONCEAL_FADE_MS, 8); // receiver-audio.c
        assert_eq!(AUDIO_OUT_LEAD_MS, 80); // receiver-audio.c
        assert_eq!(AUDIO_OUT_MAX_LAG_MS, 150); // receiver-audio.c
        assert_eq!(AUDIO_SPEED_MIN, 0.98); // receiver-audio.c
        assert_eq!(AUDIO_SPEED_DEADBAND_MS, 20); // receiver-audio.c
        assert_eq!(AUDIO_SPEED_SMOOTHING, 0.05); // receiver-audio.c
        assert_eq!(AUDIO_SPEED_DEADBAND_SLOPE, 0.002); // receiver-audio.c
        assert_eq!(AUDIO_SPEED_TRIM_GAIN, 0.0025); // receiver-audio.c
        assert_eq!(AUDIO_SPEED_TRIM_MAX, 0.01); // receiver-audio.c
        assert_eq!(AUDIO_SPEED_TRIM_ERR_WINDOW_MS, 60); // receiver-audio.c (3 * deadband)
        assert_eq!(AUDIO_SPEED_TRIM_MAX_DT_US, 1_000_000); // receiver-audio.c
        assert_eq!(AUDIO_LL_MAX_FILL_MS, 100); // receiver-audio.c
        assert_eq!(AUDIO_DRAIN_STUCK_US, 20_000_000); // receiver-audio.c
        assert_eq!(AUDIO_DRAIN_STUCK_PROGRESS_MS, 100); // receiver-audio.c
        assert_eq!(AUDIO_SOFT_COMPENSATION_MAX_SAMPLES, 8); // receiver-audio.c
        assert_eq!(AUDIO_DEFAULT_FRAME_SAMPLES, 960); // receiver-audio.c
        assert_eq!(AUDIO_PUMP_BURST, 16); // receiver.c
        assert_eq!(AUDIO_PUMP_SLEEP_MS, 1); // receiver.c
        // No C ancestor: the C polled at AUDIO_PUMP_SLEEP_MS unconditionally.
        assert_eq!(AUDIO_PUMP_MAX_SLEEP_MS, 20);
        assert_eq!(AUDIO_MAX_CHANNELS, 8); // receiver-audio.c

        // ── decode ──
        assert_eq!(DECODER_FLUSH_COOLDOWN_US, 350_000); // receiver-decode.c
        assert_eq!(DECODER_WARNING_INTERVAL_US, 1_000_000); // receiver-decode.c
        assert_eq!(DECODER_ERROR_BURST, 3); // receiver-decode.c
        assert_eq!(VIDEO_DECODER_THREADS, 4); // receiver-stream.c
        assert_eq!(VIDEO_QUEUE_SIZE, 4); // IRL_VIDEO_QUEUE_SIZE
        assert_eq!(VIDEO_EXTRA_HW_FRAMES, 6); // receiver-stream.c

        // ── video timing / pacing ──
        assert_eq!(OBS_ASYNC_FRAME_BUDGET, 24); // IRL_OBS_ASYNC_FRAME_BUDGET
        assert_eq!(VIDEO_LEAD_WARN_INTERVAL_NS, 10_000_000_000); // IRL_VIDEO_LEAD_WARN_INTERVAL_NS
        assert_eq!(VIDEO_INTERVAL_MIN_NS, 4_000_000); // IRL_VIDEO_INTERVAL_MIN_NS
        assert_eq!(VIDEO_INTERVAL_MAX_NS, 100_000_000); // IRL_VIDEO_INTERVAL_MAX_NS
        assert_eq!(VIDEO_INTERVAL_DEFAULT_NS, 33_333_333); // IRL_VIDEO_INTERVAL_DEFAULT_NS
        assert_eq!(VIDEO_PACING_MAX_FRAMES, 1024); // IRL_VIDEO_PACING_MAX_FRAMES
        assert_eq!(VIDEO_PACING_MAX_BYTES, 1_073_741_824); // IRL_VIDEO_PACING_MAX_BYTES
        assert_eq!(VIDEO_PACING_SLACK_NS, 1_000_000); // IRL_VIDEO_PACING_SLACK_NS
        assert_eq!(VIDEO_PACING_MAX_WAIT_MS, 50); // IRL_VIDEO_PACING_MAX_WAIT_MS
        assert_eq!(VIDEO_OFFSET_HOLD_NS, 500_000_000); // IRL_VIDEO_OFFSET_HOLD_NS
        assert_eq!(VIDEO_TS_CLAMP_NS, 500_000_000); // video-handler.c
        assert_eq!(VIDEO_TS_CAP_NS, 200_000_000); // video-handler.c
        assert_eq!(XFER_PLANE_ALIGN, 64); // video-handler.c
        assert_eq!(XFER_DIM_ALIGN, 16); // video-handler.c (FFALIGN(w, 16))

        // ── stream / network ──
        assert_eq!(IO_STALL_TIMEOUT_US, 10_000_000); // IRL_IO_STALL_TIMEOUT_US
        assert_eq!(PROBE_FAST, 1_000_000); // receiver-stream.c
        assert_eq!(PROBE_FULL, 5_000_000); // receiver-stream.c
        assert_eq!(SRT_LATENCY_US, 200_000); // receiver-stream.c
        assert_eq!(RTMP_BUFFER_MS, 1000); // receiver-stream.c
        assert_eq!(UDP_FIFO_DEFAULT_PACKETS, 28_672); // receiver-stream.c (7 * 4096)
        assert_eq!(STATS_LOG_INTERVAL_NS, 30_000_000_000); // receiver-stream.c
    }
}
