//! Every tuning constant of the plugin, in one place. The `consts_are_pinned`
//! test pins every value, so a typo is caught once.

/// The source id registered with OBS; also what the websocket vendor matches.
pub const SOURCE_ID: &str = "irl_source";
/// obs-websocket vendor name.
pub const VENDOR_NAME: &str = "obs-irl-source";
/// Version of the vendor request API this plugin serves.
///
/// Bumped when a request is added or a response field changes meaning, so a
/// client can feature-detect instead of probing. 2: the timecode sync
/// requests (`GetSyncStatus`, `SetSyncConfig`) and the `sync_*` stat fields.
pub const VENDOR_API_VERSION: i64 = 2;

// ── Settings defaults ──

/// Seconds between reconnect attempts.
pub const DEFAULT_RECONNECT_DELAY_S: i64 = 2;
/// Reconnect delay property bounds.
pub const RECONNECT_DELAY_MIN_S: i32 = 1;
/// Reconnect delay property bounds.
pub const RECONNECT_DELAY_MAX_S: i32 = 60;
/// Transport receive buffer handed to FFmpeg (`buffer_size` / `recv_buffer_size`).
pub const NETWORK_BUFFER_MB: i64 = 2;
/// Target jitter buffer fill.
pub const DEFAULT_BUFFER_TARGET_MS: i64 = 120;
/// Target buffer property floor.
pub const BUFFER_TARGET_MIN_MS: i32 = 20;
/// Target buffer property ceiling.
///
/// Not a limit of the controller: it is where holding the cushion stops
/// being free. Every millisecond of audio buffer is also a millisecond of
/// compressed video held in the packet queue, and the whole target is paid as
/// startup delay before playback primes. High-bitrate uplinks with deep
/// sender-side buffering do stall for several seconds, though, and 2s could
/// not ride those out.
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
/// Ring capacity is this many times `buffer_max_ms`.
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
/// Recovery hold after the audio decoder was flushed or the stream timeline
/// was reset (microseconds).
pub const AUDIO_RESET_RECOVERY_HOLD_US: u64 = 2_500_000;
/// How long the packet-level keyframe gate waits before giving up and feeding
/// the decoder whatever arrives (microseconds).
pub const VIDEO_PKT_GATE_TIMEOUT_US: u64 = 5_000_000;
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
/// EMA factor applied to the *level* the controller reads, per pump cycle.
///
/// Not everything upstream hands over a smooth stream. A remux hop (MediaMTX,
/// an RTMP relay) delivers in batches, and the buffer level then sawtooths
/// across the whole ramp at the batch period. A controller reading the
/// instantaneous level chases that and modulates playback speed at the same
/// period — measured in the network simulation at 1.2 % peak-to-peak on a
/// 500 ms batch and 3.3 % on a 1 s batch, where 1 % is 17 cents. That is
/// audible as pitch wobble, and since video due times are the frame PTS plus
/// the audio playout offset, it is visible as judder at the same time.
///
/// ~2.5 s at a 20 ms chunk: an order of magnitude longer than any batch period
/// worth smoothing, and an order of magnitude shorter than the drain after a
/// stall, which lasts tens of seconds and must not be damped away. Measured, it
/// makes that drain slightly *faster*, because the smoothed level holds the
/// ramp at full authority instead of relaxing on every dip.
pub const AUDIO_SPEED_LEVEL_SMOOTHING: f32 = 0.008;

/// Speed at the edge of the deadband.
///
/// The deadband is sloped rather than flat because a region with zero
/// proportional feedback leaves the trim's integrator undamped, and the pair
/// limit-cycles through it (simulated: ±20 ms of fill on a ~2 minute period,
/// never settling). At 0.2 % the slope is 3.5 cents at the very edge, an order
/// of magnitude under anything audible.
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
/// `extra_hw_frames`: headroom above what the decoder itself needs. Decoded
/// frames are transferred to system memory on the decoding thread and never
/// queued as hardware surfaces, so this is slack, not a queue depth.
pub const VIDEO_EXTRA_HW_FRAMES: i32 = 6;

/// How much decoded video the video thread keeps ahead of the display point.
///
/// This — not the Target Buffer — is what bounds decoded-frame memory. The
/// stream's whole latency is held as compressed packets in the receiver → video
/// queue, and decode happens just before display, so a 4K60 stream at an 8s
/// target costs ~20MB of packets instead of ~6GB of decoded frames.
///
/// It has to cover the decoder's own pipeline (reordering, hardware surfaces in
/// flight) plus jitter in the video thread's wakeups, and nothing more: every
/// millisecond here is decoded frames resident in RAM.
pub const VIDEO_DECODE_LEAD_MS: i64 = 250;

/// Compressed packets the receiver → video queue may hold, in milliseconds of
/// media. The queue carries the whole configured latency, so it must clear the
/// largest Target Buffer with room for the burst that arrives when a stalled
/// sender catches up.
pub const VIDEO_PACKET_QUEUE_MAX_MS: i64 = BUFFER_TARGET_MAX_MS as i64 + 4000;
/// Byte ceiling on the same queue: 12s of a 40Mbps feed, and a hard bound on
/// what a sender that lies about its timestamps can make the plugin allocate.
pub const VIDEO_PACKET_QUEUE_MAX_BYTES: usize = 64 * 1024 * 1024;

// ── Video timing / pacing ──

/// Bounds on the measured frame interval (250fps..10fps).
pub const VIDEO_INTERVAL_MIN_NS: i64 = 4_000_000;
/// Bounds on the measured frame interval (250fps..10fps).
pub const VIDEO_INTERVAL_MAX_NS: i64 = 100_000_000;
/// Pacing queue frame ceiling.
///
/// It has to carry the largest Target Buffer at the highest frame rate anyone
/// streams: the lead is the audio buffer, so 8 s at 120 fps is 960 frames. The
/// byte ceiling below is the one meant to bind; a count bound that binds first
/// gives up at a different latency for every frame rate.
pub const VIDEO_PACING_MAX_FRAMES: usize = 1024;
/// Pacing queue byte ceiling (1 GiB).
pub const VIDEO_PACING_MAX_BYTES: usize = 1024 * 1024 * 1024;
/// Emit rather than sleep again when this close to due.
pub const VIDEO_PACING_SLACK_NS: i64 = 1_000_000;

/// How far ahead of its due time a frame is handed to libobs, in OBS canvas
/// ticks.
///
/// The frame still carries its due time as its timestamp, so libobs shows it
/// at the same moment either way; the lead makes sure it is already queued
/// when its render tick runs. `ready_async_frame()` takes the frame whose
/// timestamp it has just passed, so a frame handed over *at* its due time
/// slips to the next tick whenever the video thread wakes late, which on a
/// 30fps source and a 60fps canvas is visible judder.
///
/// Two ticks covers that wakeup jitter (coarse on Windows) and keeps the queue
/// at one to four source frames, far under the 30 at which `cache_video()`
/// discards the whole async queue.
pub const VIDEO_PACING_LEAD_TICKS: u64 = 2;
/// Ceiling on that lead, for a canvas running at an unusually low frame rate.
pub const VIDEO_PACING_MAX_LEAD_NS: u64 = 50_000_000;
/// Canvas tick assumed when libobs has not reported one yet (60fps).
pub const VIDEO_CANVAS_TICK_DEFAULT_NS: u64 = 16_666_667;
/// Ceiling on a single pacing sleep.
pub const VIDEO_PACING_MAX_WAIT_MS: u64 = 50;

/// Margin past the audio prime estimate that video waits for the audio playout
/// mapping before anchoring libobs's play head on its own clock.
///
/// libobs anchors its play head to the first frame's arrival and never moves
/// it, and the video-only fallback schedule disagrees with the audio mapping
/// by ~100 ms, so video holds until audio primes. The prime is expected within
/// `STARTUP_AUDIO_WARMUP_MS + target + AUDIO_OUT_LEAD_MS`; this is the slack
/// past that before a stream whose audio never arrives is let through on the
/// fallback anyway.
pub const VIDEO_ANCHOR_WAIT_MARGIN_MS: i64 = 1000;

/// Ceiling on the video delay (`irl_core::video_delay`): the video floor less
/// the audio playout offset.
///
/// A late frame is shown on arrival with or without the delay, which only
/// lets it be paced, so the ceiling is not a latency bound but a stop on a
/// decoder or host whose lateness grows without end. It has to clear every
/// skew a phone produces: a stabiliser has sent video 1.6 s behind its audio
/// (#33). Past the ceiling the stream plays unpaced.
pub const VIDEO_DELAY_MAX_MS: u64 = 5000;
/// After the play head is anchored, a raise of the video delay moves the
/// picture, so late frames must recur across this window before one is made.
pub const VIDEO_DELAY_WINDOW_MS: u64 = 1000;
/// Late frames within that window, spread over at least half of it, that count
/// as recurring. A single scheduling hiccup on the host makes a couple of
/// consecutive frames late; a sender skew makes them late all window long.
pub const VIDEO_DELAY_MIN_FRAMES: u32 = 3;
/// Before the anchor, video sizes the delay only once it is live rather than
/// still catching up (`irl_core::arrival`): the floor of arrival minus PTS
/// must not have fallen by more than the tolerance across the lookback. The
/// lookback is what a genuinely late sender pays before its first picture;
/// the tolerance is half a 30fps frame, above ordinary arrival jitter and
/// well below the frame-per-frame fall of a catch-up.
pub const VIDEO_LIVE_LOOKBACK_MS: u64 = 250;
/// How far the arrival floor may fall across the lookback and still count as
/// settled; see [`VIDEO_LIVE_LOOKBACK_MS`].
pub const VIDEO_LIVE_TOLERANCE_MS: u64 = 16;
/// A stream that never stops catching up is measured anyway after this.
pub const VIDEO_LIVE_MAX_WAIT_MS: u64 = 2000;
/// How long every frame handed over must have needed less than the video
/// floor before it slews back down; the window slides. Long enough that one
/// quiet stretch on a link that really is late does not start a slew a raise
/// then has to undo.
pub const VIDEO_DELAY_RELAX_WINDOW_MS: u64 = 10_000;
/// A floor higher than frames need by less than this is left alone: a slew
/// is not worth starting for a couple of canvas ticks.
pub const VIDEO_DELAY_RELAX_MIN_MS: u64 = 50;
/// How early, against its due time, video stamped exactly the measured skew
/// behind its audio should be in hand once the audio hold
/// (`irl_core::audio_hold`) is in force. Covers the chunk the buffer level
/// dithers by, the frame-and-chunk granularity of the mux-order skew reading,
/// frame reordering, and a canvas tick to decode in.
pub const AUDIO_HOLD_MARGIN_MS: i32 = 100;
/// Ceiling on the audio hold. The same as the video delay ceiling: past it
/// the video delay paces the rest, as a lip-sync error, rather than the
/// buffer chasing a sender whose skew has no bound.
pub const AUDIO_HOLD_MAX_MS: i32 = 5000;
/// Once audio plays, how long a skew must last, unbroken, before the hold is
/// raised for it. Shorter bursts of late video are the video delay's to
/// cover: paying for a hiccup in latency for the rest of the connection is
/// the wrong trade.
pub const AUDIO_HOLD_RAISE_WINDOW_MS: u64 = 2000;
/// Raises smaller than this are not made once audio plays. The margin already
/// covers a skew that creeps by a few milliseconds.
pub const AUDIO_HOLD_RAISE_MIN_MS: i32 = 20;
/// How long every skew reading must have needed less than the hold in force
/// before the hold is released to what they needed. The same as the video
/// delay's relax window, for the same reason.
pub const AUDIO_HOLD_RELAX_WINDOW_MS: u64 = 10_000;
/// Surplus hold below this is left alone.
pub const AUDIO_HOLD_RELAX_MIN_MS: i32 = 50;
/// How long audio that is ready to prime waits for the first skew reading,
/// when the connection carries video, so the hold is in force from the first
/// sample instead of being built at -2 % afterwards. Video counts as live
/// within `VIDEO_LIVE_MAX_WAIT_MS` of its first packet, so this only runs out
/// for a video stream that delivers nothing.
pub const AUDIO_HOLD_PRIME_WAIT_MS: u64 = 2000;
/// How long the last audio playout offset is reused after it goes away.
pub const VIDEO_OFFSET_HOLD_NS: u64 = 500_000_000;
/// Video-only fallback: clamp on drift between stream and system clock.
pub const VIDEO_TS_CLAMP_NS: i64 = 500_000_000;
/// Video-only fallback: forward cap.
pub const VIDEO_TS_CAP_NS: u64 = 200_000_000;

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
/// Packets the reader thread may read ahead of the receiver. A handoff, not a
/// buffer: past it the reader stops reading and the transport holds the rest.
pub const RECEIVER_READ_AHEAD_PACKETS: usize = 16;
/// Longest the receiver waits for a packet before it looks at the run state
/// again. Held packets are released on time regardless.
pub const RECEIVER_IDLE_WAIT_MS: u64 = 100;

/// Ring capacity when the format is degenerate and `4 × max_ms` works out to
/// nothing.
pub const AUDIO_BUFFER_FALLBACK_CAPACITY: usize = 65536;

// ── Timecode sync ──

/// Per-source opt-in default.
pub const DEFAULT_SYNC_ENABLED: bool = false;
/// Shared presentation offset default.
pub const SYNC_DEFAULT_OFFSET_MS: i32 = 5000;
/// Offset floor.
pub const SYNC_MIN_OFFSET_MS: i32 = 0;
/// Offset ceiling. Held as compressed packets, so the cost is bitrate × offset
/// rather than decoded frames: 30s of a 6Mbit/s feed is about 22MB. The
/// ceiling is here to bound a typo, not because the buffering is expensive.
pub const SYNC_MAX_OFFSET_MS: i32 = 30000;
/// The offset is set, and reported, in whole seconds: it is a number
/// co-streamers read to each other, and "six" is a thing two people can agree
/// on over a call in a way that 6123ms is not. Stored in milliseconds because
/// everything downstream computes in time.
pub const SYNC_OFFSET_STEP_MS: i32 = 1000;
/// A server run for this plugin, so a fresh install lands on the burst polling
/// policy and the drift correction that comes with it. Only a default: any
/// host can be typed in, and one not on the burst list is polled at the
/// public-pool cadence.
pub const SYNC_DEFAULT_NTP_SERVER: &str = "ntp.kringkast.com";
/// Rolling window over which arrival latency is peak-held, in buckets of
/// [`SYNC_PEAK_BUCKET_NS`].
pub const SYNC_PEAK_BUCKETS: usize = 12;
/// Bucket width of the latency peak: 12 × 5s is the 60s window.
pub const SYNC_PEAK_BUCKET_NS: u64 = 5_000_000_000;
/// How long the presentation error is averaged over. A duration, not a sample
/// count, because sources run at anything from 25 to 240fps.
pub const SYNC_ERROR_WINDOW_NS: u64 = 1_000_000_000;
/// Capacity of the error window: what a second costs at the fastest rate the
/// interval bounds accept (250fps).
pub const SYNC_ERROR_SAMPLES: usize = 256;
/// Completed timecode seconds the frame rate is decided from. Enough for two
/// of them to agree while a damaged one is outvoted, and small enough that a
/// genuine rate change lands within this many seconds.
pub const SYNC_FPS_SECONDS: usize = 4;
/// Packet ceiling of the delay line: 30s of 60fps video interleaved with audio
/// several times over.
pub const SYNC_DELAY_MAX_PACKETS: usize = 16384;
/// Byte ceiling of the delay line (256 MiB). Hitting either ceiling means
/// something pathological; the read loop applies backpressure before that.
pub const SYNC_DELAY_MAX_BYTES: usize = 256 * 1024 * 1024;
/// Sources the registry (and the dock) can hold.
pub const SYNC_MAX_SOURCES: usize = 64;
/// Presentation error we call aligned: just above two frames at 60fps.
pub const SYNC_LOCK_TOLERANCE_NS: i64 = 40_000_000;
/// The wider band to leave before admitting we are not; stops a row
/// flickering between Locked and Acquiring on ordinary jitter.
pub const SYNC_UNLOCK_TOLERANCE_NS: i64 = 120_000_000;
/// Fraction of real time the hold may move by while locked, deliberately
/// below the speed controller's -2%/+5% authority so drift correction stays
/// inaudible.
pub const SYNC_SLEW_RATE: f64 = 0.015;
/// Rate at which an unwanted hold is given back. Zeroing it would make every
/// queued packet due at once; this sits inside the +5% catch-up authority, so
/// the cost is mild chipmunk and unwinding a five-second hold takes ~2 min.
pub const SYNC_RELEASE_RATE: f64 = 0.04;
/// Slack added to the hold when working out when a correction will show up:
/// decode, the jitter cushion and the audio output lead.
pub const SYNC_SETTLE_MARGIN_NS: i64 = 750_000_000;
/// No timecode for this long and the source has stopped being alignable.
pub const SYNC_TC_STALE_NS: u64 = 3_000_000_000;
/// Deliberate over-hold at engage: bounds the anchor's pre-roll wait rather
/// than making the seed accurate. Early is the harmless direction.
pub const SYNC_SEED_BIAS_NS: i64 = 250_000_000;
/// Longest pre-roll the playout anchor will ask the audio output to wait.
pub const SYNC_ANCHOR_MAX_WAIT_NS: i64 = 1_500_000_000;
/// Longest the audio output is kept from priming while sync works out its
/// hold. A backstop for a sender that never engages.
pub const SYNC_PRIME_WAIT_NS: u64 = 3_000_000_000;
/// Hysteresis on the "cannot reach the offset" alarm: enter after this long.
pub const SYNC_ALARM_ENTER_NS: u64 = 4_000_000_000;
/// Hysteresis on the "cannot reach the offset" alarm: leave after this long.
pub const SYNC_ALARM_LEAVE_NS: u64 = 8_000_000_000;
/// ~5Hz snapshot publication to the registry.
pub const SYNC_PUBLISH_INTERVAL_NS: u64 = 200_000_000;
/// Published status older than this is reported as stale rather than as
/// whatever it last said. Comfortably longer than the publish interval.
pub const SYNC_PUBLISH_STALE_NS: u64 = 3_000_000_000;
/// Anchor smoothing: eight samples is a third of a second at 30fps and
/// averages the timecode's one-frame grid away without lagging an offset
/// change. See `present_bias` in the controller.
pub const SYNC_BIAS_SMOOTHING_DIV: i64 = 8;

// ── NTP client ──

/// Seconds between 1900-01-01 (NTP epoch) and 1970-01-01 (Unix epoch).
pub const NTP_UNIX_EPOCH_DELTA: u64 = 2_208_988_800;
/// UDP port.
pub const NTP_PORT: u16 = 123;
/// SNTP packet size.
pub const NTP_PACKET_BYTES: usize = 48;
/// Receive timeout per query.
pub const NTP_RECV_TIMEOUT_MS: u64 = 1200;
/// Samples in an acquisition burst; the lowest-RTT one wins.
pub const NTP_BURST: usize = 4;
/// Bounds of the randomised burst-policy poll interval.
pub const NTP_BURST_MIN_INTERVAL_MS: u32 = 1000;
/// Bounds of the randomised burst-policy poll interval.
pub const NTP_BURST_MAX_INTERVAL_MS: u32 = 30000;
/// Retry gap after a failed burst-policy poll.
pub const NTP_BURST_RETRY_MS: u32 = 2000;
/// Standard policy: RFC 4330's recommended minimum.
pub const NTP_POLL_INTERVAL_MS: u32 = 64000;
/// Ceiling the kiss-o'-death backoff doubles toward.
pub const NTP_POLL_MAX_INTERVAL_MS: u32 = 1_024_000;
/// Retry faster until a first sync exists.
pub const NTP_RETRY_INTERVAL_MS: u32 = 8000;
/// Backstop for the idle thread while polling is switched off.
pub const NTP_IDLE_INTERVAL_MS: u32 = 3_600_000;
/// Failover pool.
pub const NTP_FALLBACK_SERVER: &str = "pool.ntp.org";
/// Consecutive failures before a burst server hands over to the pool.
pub const NTP_FALLBACK_AFTER_FAILURES: u32 = 3;
/// Bounds of the randomised probe interval for the failed primary.
pub const NTP_FALLBACK_PROBE_MIN_MS: u32 = 30000;
/// Bounds of the randomised probe interval for the failed primary.
pub const NTP_FALLBACK_PROBE_MAX_MS: u32 = 60000;
/// A round trip worse than this says nothing useful about the offset.
pub const NTP_MAX_USABLE_RTT_NS: i64 = 1_000_000_000;
/// Weight applied to each accepted sample once a sync exists, where the drift
/// fit is not driving the offset.
pub const NTP_OFFSET_SMOOTHING: f64 = 0.25;
/// Past this the estimate is stale enough to re-seat rather than ease.
pub const NTP_RESEAT_THRESHOLD_NS: i64 = 2_000_000_000;
/// Drift window size: ~16 minutes at the burst policy's mean interval.
pub const NTP_DRIFT_CAPACITY: usize = 64;
/// Samples older than this are left out of the fit.
pub const NTP_DRIFT_MAX_AGE_NS: i64 = 15 * 60 * 1_000_000_000;
/// Slack on the round-trip filter, so a sub-millisecond server does not
/// reject almost everything on jitter alone.
pub const NTP_DRIFT_RTT_SLACK_NS: i64 = 200_000;
/// Minimum samples for a fit.
pub const NTP_DRIFT_MIN_SAMPLES: usize = 8;
/// Minimum baseline for a fit: at 20 ppm, three minutes is 3.6ms of signal.
pub const NTP_DRIFT_MIN_SPAN_NS: i64 = 180 * 1_000_000_000;
/// Pairs closer than this are noise divided by a small number.
pub const NTP_DRIFT_MIN_PAIR_NS: i64 = 30 * 1_000_000_000;
/// Minimum pair count for a slope.
pub const NTP_DRIFT_MIN_PAIRS: usize = 4;
/// A fit past this is a broken measurement, not a broken clock.
pub const NTP_DRIFT_MAX_PPM: f64 = 500.0;
/// Ceiling on the extrapolation itself.
pub const NTP_DRIFT_MAX_CORRECTION_NS: i64 = 50_000_000;
/// Servers run for this plugin, where the burst policy is both wanted and
/// welcome. Matched case-insensitively, exactly or as a parent domain.
pub const NTP_BURST_HOSTS: &[&str] = &["ntp.kringkast.com"];
/// Several missed polls; past this the offset is old enough that everything
/// derived from it is suspect (the dock's "fault" threshold).
pub const NTP_STALE_AGE_NS: u64 = 300 * 1_000_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constant, pinned to its value, so a typo in one of the tables
    /// above is caught once rather than diagnosed from a stream that sounds
    /// slightly wrong. A deliberate retune changes the value here too.
    #[test]
    fn consts_are_pinned() {
        // ── identity ──
        assert_eq!(SOURCE_ID, "irl_source");
        assert_eq!(VENDOR_NAME, "obs-irl-source");
        assert_eq!(VENDOR_API_VERSION, 2);

        // ── settings defaults ──
        assert_eq!(DEFAULT_RECONNECT_DELAY_S, 2);
        assert_eq!(RECONNECT_DELAY_MIN_S, 1);
        assert_eq!(RECONNECT_DELAY_MAX_S, 60);
        assert_eq!(NETWORK_BUFFER_MB, 2);
        assert_eq!(DEFAULT_BUFFER_TARGET_MS, 120);
        assert_eq!(BUFFER_TARGET_MIN_MS, 20);
        assert_eq!(BUFFER_TARGET_MAX_MS, 8000);
        assert_eq!(BUFFER_TARGET_STEP_MS, 10);
        const { assert!(DEFAULT_ADAPTIVE_SPEED) };
        assert_eq!(DEFAULT_CATCHUP_PERCENT, 5);
        assert_eq!(CATCHUP_PERCENT_MIN, 2);
        assert_eq!(CATCHUP_PERCENT_MAX, 15);
        const { assert!(DEFAULT_WAIT_FOR_KEYFRAME) };
        const { assert!(!DEFAULT_LOW_LATENCY_AUDIO) };
        const { assert!(!DEFAULT_CLOSE_WHEN_INACTIVE) };
        const { assert!(DEFAULT_CLEAR_ON_DISCONNECT) };

        // ── buffer watermarks ──
        assert_eq!(BUFFER_MIN_DIVISOR, 2);
        assert_eq!(BUFFER_MIN_FLOOR_MS, 20);
        assert_eq!(BUFFER_MAX_EXTRA_MS, 200);
        assert_eq!(BUFFER_CAPACITY_MULTIPLIER, 4);
        assert_eq!(AUDIO_BUFFER_FALLBACK_CAPACITY, 65536);

        // ── PTS repair ──
        assert_eq!(SMALL_GAP_MS, 70);
        assert_eq!(LARGE_GAP_MS, 2000);
        assert_eq!(PTS_SMALL_GAP_RELOCK_COUNT, 8);
        assert_eq!(PTS_SMALL_GAP_TOLERANCE_MS, 2);
        assert_eq!(PTS_RELOCK_STEP_MS, 2);
        assert_eq!(AUDIO_PTS_MAX_CHUNKS, 256);

        // ── audio output ──
        assert_eq!(FADE_DURATION_MS, 50);
        assert_eq!(STARTUP_AUDIO_WARMUP_MS, 150);
        assert_eq!(BLEED_PACE_FILL_MS, 1000);
        assert_eq!(AUDIO_OFFSET_REANCHOR_MARGIN_MS, 400);
        assert_eq!(AUDIO_RECOVERY_HOLD_US, 1_500_000);
        assert_eq!(AUDIO_RESET_RECOVERY_HOLD_US, 2_500_000);
        assert_eq!(VIDEO_PKT_GATE_TIMEOUT_US, 5_000_000);
        assert_eq!(AUDIO_TRIM_TRIGGER_MS, 90);
        assert_eq!(AUDIO_CONCEAL_FADE_MS, 8);
        assert_eq!(AUDIO_OUT_LEAD_MS, 80);
        assert_eq!(AUDIO_OUT_MAX_LAG_MS, 150);
        assert_eq!(AUDIO_SPEED_MIN, 0.98);
        assert_eq!(AUDIO_SPEED_DEADBAND_MS, 20);
        assert_eq!(AUDIO_SPEED_SMOOTHING, 0.05);
        assert_eq!(AUDIO_SPEED_LEVEL_SMOOTHING, 0.008);
        assert_eq!(AUDIO_SPEED_DEADBAND_SLOPE, 0.002);
        assert_eq!(AUDIO_SPEED_TRIM_GAIN, 0.0025);
        assert_eq!(AUDIO_SPEED_TRIM_MAX, 0.01);
        assert_eq!(AUDIO_SPEED_TRIM_ERR_WINDOW_MS, 60); // 3 * deadband
        assert_eq!(AUDIO_SPEED_TRIM_MAX_DT_US, 1_000_000);
        assert_eq!(AUDIO_LL_MAX_FILL_MS, 100);
        assert_eq!(AUDIO_DRAIN_STUCK_US, 20_000_000);
        assert_eq!(AUDIO_DRAIN_STUCK_PROGRESS_MS, 100);
        assert_eq!(AUDIO_SOFT_COMPENSATION_MAX_SAMPLES, 8);
        assert_eq!(AUDIO_DEFAULT_FRAME_SAMPLES, 960);
        assert_eq!(AUDIO_PUMP_BURST, 16);
        assert_eq!(AUDIO_PUMP_SLEEP_MS, 1);
        assert_eq!(AUDIO_PUMP_MAX_SLEEP_MS, 20);
        assert_eq!(AUDIO_MAX_CHANNELS, 8);

        // ── decode ──
        assert_eq!(DECODER_FLUSH_COOLDOWN_US, 350_000);
        assert_eq!(DECODER_WARNING_INTERVAL_US, 1_000_000);
        assert_eq!(DECODER_ERROR_BURST, 3);
        assert_eq!(VIDEO_DECODER_THREADS, 4);
        assert_eq!(VIDEO_EXTRA_HW_FRAMES, 6);
        assert_eq!(VIDEO_DECODE_LEAD_MS, 250);
        assert_eq!(VIDEO_PACKET_QUEUE_MAX_MS, 12_000);
        assert_eq!(VIDEO_PACKET_QUEUE_MAX_BYTES, 67_108_864);

        // ── video timing / pacing ──
        assert_eq!(VIDEO_INTERVAL_MIN_NS, 4_000_000);
        assert_eq!(VIDEO_INTERVAL_MAX_NS, 100_000_000);
        assert_eq!(VIDEO_PACING_MAX_FRAMES, 1024);
        assert_eq!(VIDEO_PACING_MAX_BYTES, 1_073_741_824);
        assert_eq!(VIDEO_PACING_SLACK_NS, 1_000_000);
        assert_eq!(VIDEO_PACING_LEAD_TICKS, 2);
        assert_eq!(VIDEO_ANCHOR_WAIT_MARGIN_MS, 1000);
        assert_eq!(VIDEO_DELAY_MAX_MS, 5000);
        assert_eq!(VIDEO_DELAY_WINDOW_MS, 1000);
        assert_eq!(VIDEO_DELAY_MIN_FRAMES, 3);
        assert_eq!(VIDEO_LIVE_LOOKBACK_MS, 250);
        assert_eq!(VIDEO_LIVE_TOLERANCE_MS, 16);
        assert_eq!(VIDEO_LIVE_MAX_WAIT_MS, 2000);
        assert_eq!(VIDEO_DELAY_RELAX_WINDOW_MS, 10_000);
        assert_eq!(VIDEO_DELAY_RELAX_MIN_MS, 50);
        assert_eq!(AUDIO_HOLD_MARGIN_MS, 100);
        assert_eq!(AUDIO_HOLD_MAX_MS, 5000);
        assert_eq!(AUDIO_HOLD_RAISE_WINDOW_MS, 2000);
        assert_eq!(AUDIO_HOLD_RAISE_MIN_MS, 20);
        assert_eq!(AUDIO_HOLD_RELAX_WINDOW_MS, 10_000);
        assert_eq!(AUDIO_HOLD_RELAX_MIN_MS, 50);
        assert_eq!(AUDIO_HOLD_PRIME_WAIT_MS, 2000);
        assert_eq!(VIDEO_PACING_MAX_LEAD_NS, 50_000_000);
        assert_eq!(VIDEO_CANVAS_TICK_DEFAULT_NS, 16_666_667);
        assert_eq!(VIDEO_PACING_MAX_WAIT_MS, 50);
        assert_eq!(VIDEO_OFFSET_HOLD_NS, 500_000_000);
        assert_eq!(VIDEO_TS_CLAMP_NS, 500_000_000);
        assert_eq!(VIDEO_TS_CAP_NS, 200_000_000);

        // ── stream / network ──
        assert_eq!(IO_STALL_TIMEOUT_US, 10_000_000);
        assert_eq!(PROBE_FAST, 1_000_000);
        assert_eq!(PROBE_FULL, 5_000_000);
        assert_eq!(SRT_LATENCY_US, 200_000);
        assert_eq!(RTMP_BUFFER_MS, 1000);
        assert_eq!(UDP_FIFO_DEFAULT_PACKETS, 28_672); // 7 * 4096
        assert_eq!(STATS_LOG_INTERVAL_NS, 30_000_000_000);
        assert_eq!(RECEIVER_READ_AHEAD_PACKETS, 16);
        assert_eq!(RECEIVER_IDLE_WAIT_MS, 100);

        // ── timecode sync ──
        const { assert!(!DEFAULT_SYNC_ENABLED) };
        assert_eq!(SYNC_DEFAULT_OFFSET_MS, 5000);
        assert_eq!(SYNC_MIN_OFFSET_MS, 0);
        assert_eq!(SYNC_MAX_OFFSET_MS, 30000);
        assert_eq!(SYNC_OFFSET_STEP_MS, 1000);
        assert_eq!(SYNC_DEFAULT_NTP_SERVER, "ntp.kringkast.com");
        assert_eq!(SYNC_PEAK_BUCKETS, 12);
        assert_eq!(SYNC_PEAK_BUCKET_NS, 5_000_000_000);
        assert_eq!(SYNC_ERROR_WINDOW_NS, 1_000_000_000);
        assert_eq!(SYNC_ERROR_SAMPLES, 256);
        assert_eq!(SYNC_FPS_SECONDS, 4);
        assert_eq!(SYNC_DELAY_MAX_PACKETS, 16384);
        assert_eq!(SYNC_DELAY_MAX_BYTES, 268_435_456);
        assert_eq!(SYNC_MAX_SOURCES, 64);
        assert_eq!(SYNC_LOCK_TOLERANCE_NS, 40_000_000);
        assert_eq!(SYNC_UNLOCK_TOLERANCE_NS, 120_000_000);
        assert_eq!(SYNC_SLEW_RATE, 0.015);
        assert_eq!(SYNC_RELEASE_RATE, 0.04);
        assert_eq!(SYNC_SETTLE_MARGIN_NS, 750_000_000);
        assert_eq!(SYNC_TC_STALE_NS, 3_000_000_000);
        assert_eq!(SYNC_SEED_BIAS_NS, 250_000_000);
        assert_eq!(SYNC_ANCHOR_MAX_WAIT_NS, 1_500_000_000);
        assert_eq!(SYNC_PRIME_WAIT_NS, 3_000_000_000);
        assert_eq!(SYNC_ALARM_ENTER_NS, 4_000_000_000);
        assert_eq!(SYNC_ALARM_LEAVE_NS, 8_000_000_000);
        assert_eq!(SYNC_PUBLISH_INTERVAL_NS, 200_000_000);
        assert_eq!(SYNC_PUBLISH_STALE_NS, 3_000_000_000);
        assert_eq!(SYNC_BIAS_SMOOTHING_DIV, 8);

        // ── NTP client ──
        assert_eq!(NTP_UNIX_EPOCH_DELTA, 2_208_988_800);
        assert_eq!(NTP_PORT, 123);
        assert_eq!(NTP_PACKET_BYTES, 48);
        assert_eq!(NTP_RECV_TIMEOUT_MS, 1200);
        assert_eq!(NTP_BURST, 4);
        assert_eq!(NTP_BURST_MIN_INTERVAL_MS, 1000);
        assert_eq!(NTP_BURST_MAX_INTERVAL_MS, 30000);
        assert_eq!(NTP_BURST_RETRY_MS, 2000);
        assert_eq!(NTP_POLL_INTERVAL_MS, 64000);
        assert_eq!(NTP_POLL_MAX_INTERVAL_MS, 1_024_000);
        assert_eq!(NTP_RETRY_INTERVAL_MS, 8000);
        assert_eq!(NTP_IDLE_INTERVAL_MS, 3_600_000);
        assert_eq!(NTP_FALLBACK_SERVER, "pool.ntp.org");
        assert_eq!(NTP_FALLBACK_AFTER_FAILURES, 3);
        assert_eq!(NTP_FALLBACK_PROBE_MIN_MS, 30000);
        assert_eq!(NTP_FALLBACK_PROBE_MAX_MS, 60000);
        assert_eq!(NTP_MAX_USABLE_RTT_NS, 1_000_000_000);
        assert_eq!(NTP_OFFSET_SMOOTHING, 0.25);
        assert_eq!(NTP_RESEAT_THRESHOLD_NS, 2_000_000_000);
        assert_eq!(NTP_DRIFT_CAPACITY, 64);
        assert_eq!(NTP_DRIFT_MAX_AGE_NS, 900_000_000_000);
        assert_eq!(NTP_DRIFT_RTT_SLACK_NS, 200_000);
        assert_eq!(NTP_DRIFT_MIN_SAMPLES, 8);
        assert_eq!(NTP_DRIFT_MIN_SPAN_NS, 180_000_000_000);
        assert_eq!(NTP_DRIFT_MIN_PAIR_NS, 30_000_000_000);
        assert_eq!(NTP_DRIFT_MIN_PAIRS, 4);
        assert_eq!(NTP_DRIFT_MAX_PPM, 500.0);
        assert_eq!(NTP_DRIFT_MAX_CORRECTION_NS, 50_000_000);
        assert_eq!(NTP_BURST_HOSTS, &["ntp.kringkast.com"]);
        assert_eq!(NTP_STALE_AGE_NS, 300_000_000_000);
    }
}
