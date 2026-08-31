//! `irl_pump_audio_once` and helpers (port of `src/receiver-audio.c`). W2-B.
//!
//! Design contract with libobs (verified against `obs-source.c` /
//! `obs-audio.c`), unchanged from the C:
//!
//!   1. OBS timestamps must be contiguous: `ts[n+1] = ts[n] + frames/rate`.
//!      Deviations under 70 ms are smoothed; 70 ms..2 s gaps are zero-filled
//!      by OBS (audible); >2 s flushes all queued audio. The timestamps here
//!      therefore come from a pure sample counter anchored once at prime time,
//!      and the clock never jumps outside declared restarts.
//!   2. `samples_per_sec` must be constant: any change makes OBS
//!      destroy/recreate its per-source resampler with no crossfade (a click
//!      per change). Playback speed is instead applied here with a persistent
//!      swresample compensation, ffplay-style.
//!   3. The OBS mixer consumes 21.3 ms ticks against wall clock; a source
//!      whose queued audio runs dry gets a tick of silence plus a time-shifted
//!      splice (crackle), and a source that falls behind the mix window makes
//!      OBS permanently add global audio buffering. So after priming the pump
//!      always emits — real audio or shaped concealment silence — and keeps a
//!      fixed lead ahead of wall clock.
//!
//! Buffer regulation is done by playback speed only, never by audible trims.
//! Backlog is trimmed only before playback primes; after that, content is
//! preserved: the read loop applies transport backpressure above a fill
//! ceiling and playback bleeds the excess at up to the configured Catch-Up
//! Speed.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use ffmpeg::Resampler;
use irl_core::{SpeedCarry, SpeedController, SpeedInputs, consts, dsp, timing};

use crate::audio::AudioSink;
use crate::shared::{AudioState, LifetimeStats, Shared};

/// Bytes one interleaved float sample occupies.
const SAMPLE_BYTES: usize = 4;

/// Audio-thread-owned state: the speed resampler, scratch buffers and the
/// speed controller.
pub struct AudioPump {
    shared: Arc<Shared>,
    sink: Box<dyn AudioSink>,
    speed_swr: Option<Resampler>,
    speed_scratch: Vec<u8>,
    pump_scratch: Vec<u8>,
    speed: SpeedController,
    /// Byte ⇄ float view for the rare in-place edits (see [`FloatEdit`]).
    float: FloatEdit,
    /// The OBS clock. Injectable so tests can drive the output clock without
    /// a running libobs; production always reads `os_gettime_ns`.
    now_ns: Box<dyn Fn() -> u64 + Send>,
    /// The FFmpeg-domain clock (`av_gettime`), which times the speed trim's
    /// integration and the underrun recovery hold. Injectable alongside
    /// [`Self::now_ns`] and for the same reason: a test that steps the OBS
    /// clock while this one runs at wall speed makes the trim integrate
    /// hundreds of times too slowly, so the loop it is meant to close never
    /// closes. The two are always the same real clock in production.
    now_us: Box<dyn Fn() -> u64 + Send>,
    /// How long the audio thread may sleep before this pump next has work.
    /// See [`Self::idle_sleep_ms`].
    idle_sleep_ms: u32,
}

impl AudioPump {
    /// Production pump emitting to the source.
    pub fn new(shared: Arc<Shared>) -> Self {
        Self::with_sink(shared.clone(), Box::new(shared.source))
    }

    /// Pump with an explicit sink (tests).
    pub fn with_sink(shared: Arc<Shared>, sink: Box<dyn AudioSink>) -> Self {
        Self {
            shared,
            sink,
            speed_swr: None,
            speed_scratch: Vec::new(),
            pump_scratch: Vec::new(),
            speed: SpeedController::new(),
            float: FloatEdit::default(),
            now_ns: Box::new(obs::time::gettime_ns),
            now_us: Box::new(|| ffmpeg::gettime_us() as u64),
            idle_sleep_ms: consts::AUDIO_PUMP_SLEEP_MS,
        }
    }

    /// Replace the OBS clock source (tests only: `os_gettime_ns` is libobs's
    /// monotonic clock and cannot be stepped from here).
    #[must_use]
    pub fn with_clock(mut self, now_ns: Box<dyn Fn() -> u64 + Send>) -> Self {
        self.now_ns = now_ns;
        self
    }

    /// Replace the FFmpeg-domain clock (tests only). A test that steps
    /// [`Self::with_clock`] should step this one with it, or the speed trim
    /// integrates against wall time while the buffer moves in virtual time.
    #[must_use]
    pub fn with_us_clock(mut self, now_us: Box<dyn Fn() -> u64 + Send>) -> Self {
        self.now_us = now_us;
        self
    }

    /// How long the caller may sleep after a `pump_once` that emitted nothing.
    ///
    /// Once primed the output clock says exactly when the next chunk is due, so
    /// the thread can sleep to that deadline instead of polling. When the pump
    /// is waiting on data rather than on the clock there is no deadline to
    /// compute, and this stays at [`consts::AUDIO_PUMP_SLEEP_MS`] — the C
    /// behaviour, and the right one, since the wake condition is a write from
    /// another thread.
    pub fn idle_sleep_ms(&self) -> u32 {
        self.idle_sleep_ms
    }

    /// One pump iteration. Takes `audio_state` exactly once for the whole
    /// call; returns whether audio (or concealment) was emitted.
    pub fn pump_once(&mut self) -> bool {
        let shared = Arc::clone(&self.shared);
        // The whole call runs under the audio state lock, exactly like the C
        // (`receiver.c: irl_audio_thread` takes it around
        // `irl_pump_audio_once`). Nothing below may take it again — the mutex
        // is not recursive. Buffer-mutex calls nest underneath it, which is
        // the documented order (audio state lock, then buffer).
        let mut state = shared.audio_state();
        self.pump_locked(&shared, &mut state)
    }

    fn pump_locked(&mut self, shared: &Shared, state: &mut AudioState) -> bool {
        // Every path but the "already queued far enough ahead" one below is
        // waiting on a write from another thread, which no deadline here can
        // predict; those keep the C's poll interval.
        self.idle_sleep_ms = consts::AUDIO_PUMP_SLEEP_MS;
        let low_latency = shared.cfg.low_latency_audio;

        let Some(fmt) = BufferFormat::of(shared) else {
            return false;
        };
        if fmt.rate <= 0 || fmt.channels <= 0 || fmt.bytes_per_sample <= 0 {
            return false;
        }
        let mut base_samples = state.decoded_frame_samples;
        if base_samples <= 0 {
            base_samples = consts::AUDIO_DEFAULT_FRAME_SAMPLES;
        }

        let chunk_ns = timing::frames_to_ns(base_samples as u64, fmt.rate as u32);
        let lead_ns = timing::output_lead_ns(base_samples, fmt.rate, low_latency);
        let now = (self.now_ns)();

        if !state.primed {
            // The C set `current_speed = 1.0f` at every connection prep and
            // runtime reset; the controller lives on this thread, and "not
            // yet primed" is exactly the window those resets cover (the speed
            // is only ever computed after priming).
            self.speed.reset();
            shared.conn.set_current_speed(1.0);
        }

        // Read before the primed block, which needs to know whether the input
        // is empty to tell a stalled output thread from a merely quiet source.
        // The hidden trim underneath only acts before priming, so running it
        // earlier in the cycle changes nothing about when it fires.
        let peek = shared.audio_buf().as_ref().and_then(|b| b.peek_state());
        let has_audio = peek.is_some();
        let mut fill_ms = peek.map_or(0, |s| s.fill_ms);
        // Only read before the low-latency trim below, which is the one path
        // that can move the fill between here and the read.
        let buffered_frames = peek.map_or(0, |s| s.fill_frames);
        let chunk_count = peek.map_or(0, |s| s.chunk_count);
        // The receiver thread reads this for the stats line; the audio state
        // lock is held for the whole pump, so the publish is covered.
        LifetimeStats::note_peak_i32(&shared.lifetime.audio_fill_peak_ms, fill_ms);

        if has_audio && maybe_trim_hidden_backlog(shared, state, fill_ms, chunk_count, low_latency)
        {
            return true;
        }

        if state.primed {
            // Cap runaway concealment latency before it desyncs A/V. Runs
            // even on a healthy-lead cycle: the offset inflates from past
            // outages, not from the current queue depth.
            if !low_latency {
                maybe_reanchor_offset(shared, state, now, chunk_ns, fmt.target_ms);
            }

            let next_ts = timing::output_next_ts(state.anchor_ns, state.samples, fmt.rate as u32);

            // Enough queued ahead of wall clock — nothing to do until the
            // lead runs down, and the output clock says exactly when that is.
            if next_ts >= now + lead_ns {
                let until_ns = next_ts - (now + lead_ns);
                self.idle_sleep_ms = (until_ns / 1_000_000).clamp(
                    consts::AUDIO_PUMP_SLEEP_MS as u64,
                    consts::AUDIO_PUMP_MAX_SLEEP_MS as u64,
                ) as u32;
                return false;
            }

            // Output clock fell far behind wall clock: the audio thread was
            // stalled. Restart the clock line once, declared and counted,
            // instead of letting OBS add permanent audio buffering for a late
            // source.
            if now > next_ts && now - next_ts > consts::AUDIO_OUT_MAX_LAG_MS as u64 * 1_000_000 {
                // An empty low-latency input is a quiet source, not a stalled
                // thread: that mode emits no concealment, so nothing there can
                // advance the sample counter. Stand the clock down instead.
                if low_latency && !has_audio {
                    suspend_low_latency_clock(shared, state, now - next_ts);
                    return false;
                }
                shared.conn.audio_output_restarts.fetch_add(1, Relaxed);
                shared.conn.audio_quality_events.fetch_add(1, Relaxed);
                irl_warn!(
                    "Audio output stalled {}ms; restarting output clock",
                    (now - next_ts) / 1_000_000
                );
                state.anchor_ns = now + chunk_ns;
                state.samples = 0;
                state.conceal_fade_pending = true;
            }
        }

        if !state.primed {
            let prime_ms = timing::prime_threshold_ms(fmt.target_ms, lead_ns, low_latency);
            if !has_audio || fill_ms < prime_ms {
                return false;
            }

            state.primed = true;
            state.anchor_ns = now + chunk_ns;
            state.samples = 0;
            // Reads and writes are both whole decoded chunks, so the residual
            // can only ever be a multiple of one: a 120ms target is not a
            // level the buffer can hold, and the loop straddles it at 106 or
            // 128ms. One differently-sized read moves that grid onto the
            // target, permanently, and priming is the moment to spend it —
            // nothing has been emitted yet, so there is no cadence to disturb.
            state.align_read_pending = true;
            irl_info!(
                "Audio output primed (fill={}ms lead={}ms rate={})",
                fill_ms,
                lead_ns / 1_000_000,
                fmt.rate
            );
        }

        if !has_audio {
            // Low-latency mode keeps the old behaviour: no concealment,
            // resume where the clock line left off (the stall restart above
            // covers long droughts).
            if low_latency {
                return false;
            }

            let now_us = (self.now_us)();
            if !super::audio_recovery_active(state, now_us) {
                irl_info!("Audio underrun: concealing with silence");
            }
            shared.conn.audio_underruns.fetch_add(1, Relaxed);
            shared.conn.audio_quality_events.fetch_add(1, Relaxed);
            super::mark_audio_recovery(state, now_us, consts::AUDIO_RECOVERY_HOLD_US);
            state.conceal_fade_pending = true;
            return self.emit_concealment_silence(shared, state, base_samples, &fmt);
        }

        // Low-latency mode has no speed control; cap runaway backlog by
        // skipping old chunks (latency wins over continuity here).
        if low_latency && fill_ms > consts::AUDIO_LL_MAX_FILL_MS {
            let chunk_ms = (chunk_ns / 1_000_000) as i32;
            let keep_ms = if chunk_ms * 2 > 0 { chunk_ms * 2 } else { 42 };
            let (trimmed, post) = {
                let mut guard = shared.audio_buf();
                match guard.as_mut() {
                    Some(buf) => buf.trim_to_keep_ms(keep_ms, 1),
                    None => (0, None),
                }
            };
            if trimmed > 0 {
                shared
                    .conn
                    .audio_resync_skipped_chunks
                    .fetch_add(trimmed as u64, Relaxed);
                shared.conn.audio_quality_events.fetch_add(1, Relaxed);
                fill_ms = post.map_or(0, |s| s.fill_ms);
            }
        }

        // Normally one decoded chunk; once, just after priming, whatever puts
        // the target on the buffer's residual grid. Bounded to 1.5 chunks by
        // `aligning_read_frames`, and priming guarantees at least target plus
        // the output lead is queued, so the long case is always satisfiable.
        let read_frames = if state.align_read_pending && !low_latency {
            state.align_read_pending = false;
            let target_frames = fmt.target_ms as i64 * fmt.rate as i64 / 1000;
            let aligned =
                timing::aligning_read_frames(buffered_frames, target_frames, base_samples);
            if aligned != base_samples {
                irl_debug!(
                    "Aligning audio read to the target: {aligned} frames instead of {base_samples}"
                );
            }
            aligned
        } else {
            base_samples
        };

        let frame_bytes = read_frames as usize * fmt.frame_size;
        ensure_scratch(&mut self.pump_scratch, frame_bytes);

        let (got, chunk_pts_ns) = {
            let mut guard = shared.audio_buf();
            match guard.as_mut() {
                Some(buf) => buf.read_pts(&mut self.pump_scratch[..frame_bytes]),
                None => (0, 0),
            }
        };
        if got == 0 {
            return false;
        }

        let in_frames = (got / (fmt.channels as usize * fmt.bytes_per_sample as usize)) as i32;
        let stream_duration_ns = timing::frames_to_ns(in_frames as u64, fmt.rate as u32);

        let watermarks = shared.hot.watermarks();
        let adaptive = shared.hot.adaptive_speed.load(Relaxed);
        // One reading of the catch-up ceiling and one of the clock for the
        // whole cycle: the ramp, the anti-windup, the actuator clamp and the
        // stuck-drain watch all have to agree on the same values, and the
        // slider applies live.
        let max_speed = shared.hot.max_speed();
        let now_us = (self.now_us)();
        let recovery_active = super::audio_recovery_active(state, now_us);
        let speed = self.speed.update(
            fill_ms,
            &mut state.speed_trim,
            SpeedInputs {
                wm: watermarks,
                adaptive,
                low_latency,
                max_speed,
                now_us,
                recovery_active,
            },
        );
        shared.conn.set_current_speed(speed);

        // Unwinnable-drain detection: once per emitted chunk, with the fill
        // and speed that produced it.
        if let Some(report) =
            state
                .drain
                .observe(fill_ms, speed, watermarks.target_ms, max_speed, now_us)
        {
            irl_warn!(
                "Audio buffer stuck at {}ms (target {}ms) with playback at +{:.0}% for {}s: \
the sender is delivering faster than real time, so the buffer cannot drain and latency stays here. \
Video stays in sync with it; check the sender's frame rate and clock",
                report.fill_ms,
                watermarks.target_ms,
                (speed - 1.0) * 100.0,
                report.stuck_s
            );
        }

        let mut frames_out = in_frames as u32;
        let mut use_speed_buf = false;
        if !low_latency && adaptive {
            let produced = Self::apply_output_speed(
                &mut self.speed_swr,
                &mut self.speed_scratch,
                &mut state.speed_carry,
                &self.pump_scratch[..got],
                in_frames,
                fmt.rate,
                fmt.channels,
                speed,
            );
            if let Some(speed_frames) = produced {
                frames_out = speed_frames as u32;
                use_speed_buf = true;
            }
        }

        if state.fade_in_pending {
            state.fade_in_frames_remaining = fmt.rate * consts::FADE_DURATION_MS / 1000;
            state.fade_in_pending = false;
            state.conceal_fade_pending = false;
        }

        let emit_bytes = frames_out as usize * fmt.frame_size;
        let channels = fmt.channels as usize;
        {
            let emit_buf: &mut [u8] = if use_speed_buf {
                &mut self.speed_scratch[..emit_bytes]
            } else {
                &mut self.pump_scratch[..emit_bytes]
            };
            if state.fade_in_frames_remaining > 0 {
                let total = fmt.rate * consts::FADE_DURATION_MS / 1000;
                let mut remaining = state.fade_in_frames_remaining;
                self.float.edit(emit_buf, |pcm| {
                    remaining = dsp::apply_fade_in_ramp(pcm, channels, remaining, total);
                });
                state.fade_in_frames_remaining = remaining;
            } else if state.conceal_fade_pending {
                self.float
                    .edit(emit_buf, |pcm| dsp::apply_fade_in(pcm, channels, fmt.rate));
                state.conceal_fade_pending = false;
            }
        }

        let timestamp = super::output_claim(state, frames_out, fmt.rate as u32);
        let emitted: &[u8] = if use_speed_buf {
            &self.speed_scratch[..emit_bytes]
        } else {
            &self.pump_scratch[..emit_bytes]
        };
        self.sink.output_audio(&obs::AudioFrame::interleaved(
            emitted,
            frames_out,
            obs::SpeakerLayout::from_channels(fmt.channels as u32),
            fmt.rate as u32,
            obs::AudioFormat::Float,
            timestamp,
        ));

        remember_last_sample(&mut state.out_last, emitted, channels);
        finalize_audio_output(
            shared,
            state,
            timestamp,
            frames_out,
            fmt.rate as u32,
            chunk_pts_ns,
            stream_duration_ns,
            (self.now_ns)(),
        );
        true
    }

    /// `emit_concealment_silence`: one chunk of silence that decays out of the
    /// last real sample, on the clock line the real audio left behind.
    fn emit_concealment_silence(
        &mut self,
        shared: &Shared,
        state: &mut AudioState,
        frames: i32,
        fmt: &BufferFormat,
    ) -> bool {
        let silence_bytes = frames as usize * fmt.frame_size;
        ensure_scratch(&mut self.pump_scratch, silence_bytes);

        let channels = fmt.channels as usize;
        let rate = fmt.rate;
        let last = state.out_last;
        self.float
            .edit(&mut self.pump_scratch[..silence_bytes], |pcm| {
                dsp::shape_silence_from_last(pcm, channels, rate, &last);
            });
        // Only the first concealment chunk decays from real audio;
        // subsequent ones are pure silence.
        state.out_last.forget();

        let timestamp = super::output_claim(state, frames as u32, fmt.rate as u32);
        self.sink.output_audio(&obs::AudioFrame::interleaved(
            &self.pump_scratch[..silence_bytes],
            frames as u32,
            obs::SpeakerLayout::from_channels(fmt.channels as u32),
            fmt.rate as u32,
            obs::AudioFormat::Float,
            timestamp,
        ));

        // Stream PTS does not advance during concealment: the video mapping
        // offset grows by the outage length, which matches the real playout
        // delay until the hidden trim pulls it back.
        finalize_audio_output(
            shared,
            state,
            timestamp,
            frames as u32,
            fmt.rate as u32,
            state.latest_buffered_end_pts_ns,
            0,
            (self.now_ns)(),
        );
        true
    }

    /// `apply_output_speed`: stretch/shrink one chunk by `speed` through the
    /// persistent output resampler. Returns the output frame count, or `None`
    /// to fall back to the unmodified input chunk.
    ///
    /// A free function rather than a method because it writes `speed_scratch`
    /// while reading `pump_scratch`, which a `&mut self` receiver would not
    /// allow.
    #[allow(clippy::too_many_arguments)]
    fn apply_output_speed(
        swr: &mut Option<Resampler>,
        scratch: &mut Vec<u8>,
        carry: &mut SpeedCarry,
        input: &[u8],
        in_frames: i32,
        rate: i32,
        channels: i32,
        speed: f32,
    ) -> Option<i32> {
        // `ensure_speed_swr`: rebuild when the format moved; a failed build
        // leaves `None`, so the next chunk retries (as the C zeroed the
        // cached rate/channels).
        if swr
            .as_ref()
            .is_none_or(|s| !s.matches_params(rate, channels))
        {
            *swr = Resampler::passthrough_f32(rate, channels).ok();
        }
        let swr = swr.as_mut()?;

        // Whole samples per chunk with the fractional remainder carried into
        // the next one: rounding each chunk independently quantises the applied
        // speed to multiples of 1/in_frames (~0.1 % at 1024 frames), which is
        // the entire range the deadband slope and most of the trim live in.
        let desired = carry.output_frames(in_frames, speed);
        if desired != in_frames && swr.set_compensation(desired - in_frames, desired).is_err() {
            return None;
        }

        let mut max_out = swr.out_samples(in_frames);
        if max_out < desired {
            max_out = desired;
        }
        max_out += 32;

        let need = max_out as usize * channels as usize * SAMPLE_BYTES;
        ensure_scratch(scratch, need);

        let got = swr
            .convert_interleaved(&mut scratch[..need], max_out, input, in_frames)
            .ok()?;
        if got <= 0 { None } else { Some(got) }
    }
}

/// The jitter buffer's format, read once per pump cycle.
struct BufferFormat {
    rate: i32,
    channels: i32,
    bytes_per_sample: i32,
    frame_size: usize,
    /// The buffer's own target, which `audio_buffer_resize` moves; the
    /// config's target (the hot watermarks) is a separate quantity and the C
    /// used each in specific places.
    target_ms: i32,
}

impl BufferFormat {
    fn of(shared: &Shared) -> Option<Self> {
        let guard = shared.audio_buf();
        let buf = guard.as_ref()?;
        Some(Self {
            rate: buf.sample_rate(),
            channels: buf.channels(),
            bytes_per_sample: buf.bytes_per_sample(),
            frame_size: buf.frame_size(),
            target_ms: buf.target_ms(),
        })
    }
}

/// Stand the low-latency output clock down until real audio returns.
///
/// Low-latency mode deliberately emits no concealment, so an empty input
/// cannot advance the sample counter. The output clock then sits still while
/// wall clock moves, and the stall check reads that as a stalled audio thread —
/// which it is not. Restarting it there re-anchors, waits one lead and trips
/// again, so a silent input produced a "restarting output clock" warning every
/// ~150ms for as long as it stayed silent, with `audio_output_restarts` and
/// `audio_quality_events` climbing on a source that was merely quiet.
///
/// Drop the stale mapping instead and let the normal prime path establish one
/// new clock when a real chunk arrives. Counted as an underrun, which is what
/// it is. Buffered mode is untouched: its concealment keeps the counter moving,
/// so a late clock there really is an output-side stall.
fn suspend_low_latency_clock(shared: &Shared, state: &mut AudioState, lag_ns: u64) {
    state.primed = false;
    state.anchor_ns = 0;
    state.samples = 0;
    state.latest_obs_end_ts_ns = 0;
    state.latest_buffered_end_pts_ns = 0;
    state.offset_baseline_set = false;
    state.conceal_fade_pending = true;
    shared.conn.last_obs_lead_ns.store(0, Relaxed);

    shared.conn.audio_underruns.fetch_add(1, Relaxed);
    shared.conn.audio_quality_events.fetch_add(1, Relaxed);
    super::mark_audio_recovery(
        state,
        ffmpeg::gettime_us() as u64,
        consts::AUDIO_RECOVERY_HOLD_US,
    );
    irl_warn!(
        "Low-latency audio input empty for {}ms; suspending output clock until audio resumes",
        lag_ns / 1_000_000
    );
}

/// `irl_audio_maybe_reanchor_offset`.
///
/// The audio→OBS playout offset is (obs clock end) − (stream PTS end) of the
/// latest chunk handed to OBS; the video path adds this same offset to every
/// frame PTS for lip sync. Concealment freezes the stream-PTS side while
/// advancing the OBS side, so a delivery stall inflates the offset by the
/// outage length. Once primed the only recovery is the speed-drain bleeding
/// buffer backlog, which does nothing when the concealed audio was dropped
/// rather than delayed: the latency then sticks and every later blip stacks
/// onto it.
///
/// When the offset drifts too far past its primed baseline AND the buffer has
/// already drained back to target, reclaim it with one declared re-anchor:
/// restart the output clock line and drop the stale mapping so the next chunk
/// rebuilds it fresh. That costs a single concealed splice but caps the
/// latency instead of letting it ratchet up without bound.
fn maybe_reanchor_offset(
    shared: &Shared,
    state: &mut AudioState,
    now: u64,
    chunk_ns: u64,
    buffer_target_ms: i32,
) {
    if state.latest_obs_end_ts_ns == 0 || state.latest_buffered_end_pts_ns <= 0 {
        return;
    }

    let offset_ns = state.latest_obs_end_ts_ns as i64 - state.latest_buffered_end_pts_ns;

    // The offset's absolute value is arbitrary (it carries the stream's PTS
    // epoch); only its drift from the primed baseline is meaningful, so
    // anchor the comparison the first time a valid offset is seen after
    // priming.
    if !state.offset_baseline_set {
        state.offset_baseline_ns = offset_ns;
        state.offset_baseline_set = true;
        return;
    }

    let margin_ns = consts::AUDIO_OFFSET_REANCHOR_MARGIN_MS * 1_000_000;
    let excess_ns = offset_ns - state.offset_baseline_ns;
    if excess_ns <= margin_ns {
        return;
    }

    // Only reclaim latency the speed-drain cannot. While backlog is queued
    // the inflation is real buffered audio, and draining it at up to the
    // catch-up speed preserves every sample, so leave it to the controller
    // (content is never skipped). We step in only once the buffer is back
    // at/below target, where the residual offset is phantom: concealment
    // silence with no backing audio (the concealed packets were dropped, not
    // merely late), which no speed-up can ever recover. Re-anchoring here
    // skips nothing.
    //
    // The audio state lock is already held (see `pump_once`); this fill query
    // takes and releases the buffer mutex underneath it, which is the
    // documented order.
    let fill_ms = shared.audio_buf().as_ref().map_or(0, |b| b.fill_ms());
    if fill_ms > buffer_target_ms {
        return;
    }

    state.anchor_ns = now + chunk_ns;
    state.samples = 0;
    state.latest_obs_end_ts_ns = 0;
    state.latest_buffered_end_pts_ns = 0;
    state.offset_baseline_set = false;
    state.conceal_fade_pending = true;

    shared.lifetime.audio_offset_reanchors.fetch_add(1, Relaxed);
    shared.conn.audio_quality_events.fetch_add(1, Relaxed);
    irl_warn!(
        "Audio latency drifted +{}ms past baseline (>{}ms) with buffer at/below target; re-anchoring output clock",
        excess_ns / 1_000_000,
        consts::AUDIO_OFFSET_REANCHOR_MARGIN_MS
    );
}

/// `maybe_trim_hidden_audio_backlog`.
///
/// Before playback primes, nothing has been audible yet, so excess startup
/// backlog can be dropped for free. This is the only trim path. Once audio is
/// live, content is never skipped: the read loop stops ingesting above a fill
/// ceiling (transport backpressure) and playback bleeds the backlog off at up
/// to the Catch-Up Speed.
fn maybe_trim_hidden_backlog(
    shared: &Shared,
    state: &AudioState,
    fill_ms: i32,
    chunk_count: usize,
    low_latency: bool,
) -> bool {
    if !shared.hot.adaptive_speed.load(Relaxed) {
        return false;
    }
    // `should_hide_audio_backlog`.
    if low_latency || state.primed {
        return false;
    }
    if chunk_count <= 1 {
        return false;
    }

    let out_rate = shared.audio_buf().as_ref().map_or(0, |b| b.sample_rate());
    let mut chunk_ms = 0;
    if out_rate > 0 && state.decoded_frame_samples > 0 {
        chunk_ms = (state.decoded_frame_samples as i64 * 1000 / out_rate as i64) as i32;
    }
    if chunk_ms <= 0 {
        chunk_ms = 21;
    }

    // Keep enough to satisfy the prime threshold, which includes the OBS-side
    // lead.
    let chunk_samples = if state.decoded_frame_samples > 0 {
        state.decoded_frame_samples
    } else {
        consts::AUDIO_DEFAULT_FRAME_SAMPLES
    };
    let target_ms = shared.hot.watermarks().target_ms;
    let mut keep_ms = target_ms + chunk_ms;
    if out_rate > 0 {
        keep_ms +=
            (timing::output_lead_ns(chunk_samples, out_rate, low_latency) / 1_000_000) as i32;
    }
    if fill_ms <= keep_ms + consts::AUDIO_TRIM_TRIGGER_MS {
        return false;
    }

    let (trimmed, post) = {
        let mut guard = shared.audio_buf();
        match guard.as_mut() {
            Some(buf) => buf.trim_to_keep_ms(keep_ms, 1),
            None => return false,
        }
    };
    if trimmed == 0 {
        return false;
    }

    shared
        .conn
        .audio_resync_skipped_chunks
        .fetch_add(trimmed as u64, Relaxed);
    shared
        .conn
        .audio_hidden_trimmed_chunks
        .fetch_add(trimmed as u64, Relaxed);
    irl_info!(
        "Audio trim: dropped {} hidden buffered chunk{} before playback (fill={}ms target={}ms)",
        trimmed,
        if trimmed == 1 { "" } else { "s" },
        post.map_or(0, |s| s.fill_ms),
        target_ms
    );
    true
}

/// `finalize_audio_output`: publish the playout mapping and the per-chunk
/// stats the receiver's stats line and `get_stats` read.
#[allow(clippy::too_many_arguments)]
fn finalize_audio_output(
    shared: &Shared,
    state: &mut AudioState,
    timestamp: u64,
    frames: u32,
    samples_per_sec: u32,
    chunk_pts_ns: i64,
    stream_duration_ns: u64,
    after_output: u64,
) {
    state.latest_buffered_end_pts_ns = chunk_pts_ns + stream_duration_ns as i64;

    if samples_per_sec > 0 {
        let audio_duration_ns = frames as u64 * 1_000_000_000 / samples_per_sec as u64;
        state.latest_obs_end_ts_ns = timestamp + audio_duration_ns;
        shared
            .conn
            .last_chunk_obs_ns
            .store(audio_duration_ns, Relaxed);
    } else {
        state.latest_obs_end_ts_ns = timestamp;
        shared.conn.last_chunk_obs_ns.store(0, Relaxed);
    }

    shared
        .conn
        .last_chunk_stream_ns
        .store(stream_duration_ns, Relaxed);
    shared.conn.last_frames_out.store(frames, Relaxed);
    shared
        .conn
        .last_samples_per_sec
        .store(samples_per_sec, Relaxed);

    let lead = if state.latest_obs_end_ts_ns > after_output {
        (state.latest_obs_end_ts_ns - after_output) as i64
    } else {
        0
    };
    shared.conn.last_obs_lead_ns.store(lead, Relaxed);

    shared.conn.total_audio_frames.fetch_add(1, Relaxed);
}

/// `remember_last_sample` over an interleaved-float byte buffer: only the last
/// frame is needed, so no whole-chunk decode happens here.
fn remember_last_sample(last: &mut irl_core::LastSample, samples: &[u8], channels: usize) {
    let mut values = [0.0f32; consts::AUDIO_MAX_CHANNELS];
    let take = channels.min(consts::AUDIO_MAX_CHANNELS);
    let frame_bytes = channels * SAMPLE_BYTES;
    if channels == 0 || frame_bytes == 0 || samples.len() < frame_bytes {
        // No frame to remember (or a nonsensical layout): clear, as C did.
        last.remember(&[], channels);
        return;
    }

    let at = samples.len() - samples.len() % frame_bytes;
    let start = at - frame_bytes;
    for (ch, value) in values[..take].iter_mut().enumerate() {
        let off = start + ch * SAMPLE_BYTES;
        *value = read_f32(&samples[off..off + SAMPLE_BYTES]);
    }
    // `channels > 8` reaches `remember` with a short slice, which is exactly
    // the case the C rejected (`*dst_valid = false`).
    last.remember(&values[..take], channels);
}

fn read_f32(bytes: &[u8]) -> f32 {
    let mut raw = [0u8; SAMPLE_BYTES];
    raw.copy_from_slice(bytes);
    f32::from_le_bytes(raw)
}

/// Grow a scratch buffer to at least `need` bytes (`ensure_scratch`).
fn ensure_scratch(buf: &mut Vec<u8>, need: usize) {
    if buf.len() < need {
        buf.resize(need, 0);
    }
}

/// `irl_core::dsp` edits interleaved float in place, while the audio path
/// carries bytes (what swresample and libobs both take) and this crate forbids
/// the unsafe cast between the two views. The rare in-place edits —
/// concealment shaping and the two splice fades — therefore decode into a
/// reusable `f32` scratch, run the ported helper, and write the result back. A
/// normal chunk is emitted untouched, so the steady-state copy count matches
/// the C.
#[derive(Default)]
struct FloatEdit {
    scratch: Vec<f32>,
}

impl FloatEdit {
    fn edit(&mut self, bytes: &mut [u8], edit: impl FnOnce(&mut [f32])) {
        self.scratch.clear();
        self.scratch
            .extend(bytes.chunks_exact(SAMPLE_BYTES).map(read_f32));
        edit(&mut self.scratch);
        for (dst, value) in bytes
            .chunks_exact_mut(SAMPLE_BYTES)
            .zip(self.scratch.iter())
        {
            dst.copy_from_slice(&value.to_le_bytes());
        }
    }
}
