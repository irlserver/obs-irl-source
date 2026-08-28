/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * receiver-audio.c — audio output core
 *
 * Design contract with libobs (verified against obs-source.c /
 * obs-audio.c):
 *
 *   1. OBS timestamps must be contiguous: ts[n+1] = ts[n] +
 *      frames/rate.  Deviations under 70ms are smoothed; 70ms..2s
 *      gaps are zero-filled by OBS (audible); >2s flushes all
 *      queued audio.  We therefore derive timestamps from a pure
 *      sample counter anchored once at prime time, and never jump
 *      the clock outside declared restarts.
 *
 *   2. samples_per_sec must be constant: any change makes OBS
 *      destroy/recreate its per-source resampler with no crossfade
 *      (a click per change).  Playback speed is instead applied
 *      here with a persistent swresample compensation, ffplay-style.
 *
 *   3. The OBS mixer consumes 21.3ms ticks against wall clock; a
 *      source whose queued audio runs dry gets a tick of silence
 *      plus a time-shifted splice (crackle), and a source that
 *      falls behind the mix window causes OBS to permanently add
 *      global audio buffering.  So after priming we always emit —
 *      real audio or shaped concealment silence — and keep a fixed
 *      lead ahead of wall clock.
 *
 * Buffer regulation is done by playback speed only, never by audible
 * trims. Backlog is trimmed only before playback primes; after that,
 * content is preserved: the read loop applies transport backpressure
 * above a fill ceiling and playback bleeds the excess at up to the
 * configured catch-up speed.
 */

#include <limits.h>
#include <stdlib.h>
#include <string.h>

#include <libavutil/opt.h>

#include "receiver-internal.h"

#define AUDIO_RECOVERY_HOLD_US 1500000ULL
#define AUDIO_TRIM_TRIGGER_MS 90
#define AUDIO_CONCEAL_FADE_MS 8

/* Lead of submitted-audio end over wall clock. Must cover our
 * delivery jitter (1ms pump sleep + scheduling) plus one OBS mix
 * tick (21.3ms), with margin. Latency cost is paid once. */
#define AUDIO_OUT_LEAD_MS 80

/* If the output clock falls this far behind wall clock the audio
 * thread was stalled (debugger, laptop sleep, starvation); restart
 * the clock line instead of letting OBS add permanent buffering. */
#define AUDIO_OUT_MAX_LAG_MS 150

/* AUDIO_OFFSET_REANCHOR_MARGIN_MS lives in irl-source.h, where the video
 * side also uses it to decide when a lead is worth reporting. */

/* Playback speed authority for buffer regulation. Asymmetric,
 * IRLToolkit-style: draining a post-stall backlog runs fast (every sample
 * is preserved instead of skipped) while the build direction stays at an
 * inaudible -2%. The drain ceiling is the Catch-Up Speed setting rather
 * than a constant, because it is the one bound here that is audible and
 * the right trade differs by content: at the +5% default, draining 1s of
 * backlog takes ~20s and music is noticeably sharp where speech is not. */
#define AUDIO_SPEED_MIN 0.98f
#define AUDIO_SPEED_DEADBAND_MS 20
#define AUDIO_SPEED_SMOOTHING 0.05f

/* Speed at the edge of the deadband.
 *
 * The deadband used to be flat: dead-on 1.0 anywhere within 20ms of
 * target. That is fine for a proportional-only loop, and fatal once the
 * trim below is added — a region with zero proportional feedback leaves
 * the integrator undamped, and the pair limit-cycles through it forever
 * (simulated: +-20ms of fill on a ~2 minute period, never settling).
 * A shallow slope through the deadband restores the damping. At 0.2% it
 * is 3.5 cents at the very edge, an order of magnitude under anything
 * audible, and it makes the ramp continuous where it used to step. */
#define AUDIO_SPEED_DEADBAND_SLOPE 0.002f

/* Upper speed limit, from the hot Catch-Up Speed setting. Read per use
 * rather than cached: the slider applies live, and every consumer here
 * (ramp, anti-windup, clamp, stuck-drain detection) has to agree on the
 * same value within a cycle for the anti-windup comparisons to hold. */
static inline float audio_speed_max(const struct irl_source *ctx)
{
	long pct = os_atomic_load_long(&ctx->config.catchup_percent);
	if (pct < IRL_CATCHUP_PERCENT_MIN)
		pct = IRL_CATCHUP_PERCENT_MIN;
	else if (pct > IRL_CATCHUP_PERCENT_MAX)
		pct = IRL_CATCHUP_PERCENT_MAX;
	return 1.0f + (float)pct / 100.0f;
}

/* Speed trim: the integral term that holds the buffer at target when
 * the sender's media clock is not wall clock. See the field comment on
 * audio_speed_trim in irl-source.h for why the ramp alone cannot.
 *
 * The gain is deliberately far slower than the ramp. Their jobs are
 * separated in time, not in signal: the ramp owns transients (closed-loop
 * time constant of a few seconds), the trim owns the constant underneath
 * them and converges over a minute or two. Picked as the natural frequency
 * of the level/trim loop, w = sqrt(gain) ~= 0.05 rad/s, which is ~20x
 * slower than the ramp and so cannot beat against it.
 *
 * Error is in seconds of buffer and dt in seconds, so the gain is 1/s^2. */
#define AUDIO_SPEED_TRIM_GAIN 0.0025
#define AUDIO_SPEED_TRIM_MAX 0.01f

/* Only integrate while the level is near target. Further out the loop is
 * working a transient — a backlog draining, a buffer refilling — and the
 * level is reporting that transient, not the sender's rate. Three
 * deadbands is comfortably wider than the standing error any rate inside
 * the trim's own authority can produce, so nothing the trim is meant to
 * correct falls outside the window. */
#define AUDIO_SPEED_TRIM_ERR_WINDOW_MS (3 * AUDIO_SPEED_DEADBAND_MS)

/* A dt this long means the audio thread was not running (debugger, laptop
 * sleep, starvation). Integrating across it would credit the whole gap to
 * the sender's clock. */
#define AUDIO_SPEED_TRIM_MAX_DT_US 1000000ULL

/* Low-latency mode has no speed control, so cap stale backlog by
 * skipping chunks when fill runs away. */
#define AUDIO_LL_MAX_FILL_MS 100

/* The drain is bounded by the catch-up speed, so a sender whose media clock
 * runs faster than that can never be caught up with: the buffer rises to
 * the read loop's bleed ceiling and parks there, and latency parks with it.
 * Nothing the plugin may do fixes that — draining harder would mean
 * skipping audio, which this design does not do once primed — but it
 * should not look like normal operation either, so detect it and say so.
 *
 * Twenty seconds is chosen to sit clear of a legitimate burst: even a
 * backlog filling the ceiling drains back under buffer_max in about 13s at
 * the default target and catch-up speed, after which the speed ramp backs
 * off on its own. A lower catch-up setting drains slower, but "slower" is
 * still progress, which is what the check below actually tests for. */
#define AUDIO_DRAIN_STUCK_US 20000000ULL
/* Treat the drain as making progress if fill has come down by this much
 * since the window opened, so a slow but real recovery is not reported. */
#define AUDIO_DRAIN_STUCK_PROGRESS_MS 100

/* Grow a per-thread scratch buffer to at least `need` bytes. Returns
 * the buffer or NULL on OOM. The buffer is owned by the caller's
 * thread; no synchronisation here. */
static uint8_t *ensure_scratch(uint8_t **buf, size_t *cap, size_t need)
{
	if (need == 0)
		return *buf;
	if (need > *cap) {
		size_t new_cap = *cap ? *cap : 4096;
		while (new_cap < need)
			new_cap *= 2;
		uint8_t *next = realloc(*buf, new_cap);
		if (!next)
			return NULL;
		*buf = next;
		*cap = new_cap;
	}
	return *buf;
}

void irl_reset_audio_timing_state(struct irl_source *ctx)
{
	ctx->audio_out_primed = false;
	ctx->audio_out_anchor_ns = 0;
	ctx->audio_out_samples = 0;
	ctx->audio_conceal_fade_pending = false;
	ctx->audio_out_last_valid = false;
	ctx->audio_out_last_channels = 0;
	ctx->audio_speed_frac = 0.0;
	ctx->audio_playout_offset_baseline_ns = 0;
	ctx->audio_playout_offset_baseline_set = false;
	ctx->audio_last_obs_lead_ns = 0;
	ctx->audio_last_chunk_stream_duration_ns = 0;
	ctx->audio_last_chunk_obs_duration_ns = 0;
	ctx->audio_last_frames_out = 0;
	ctx->audio_last_samples_per_sec = 0;
	ctx->audio_recovery_until_us = 0;
	ctx->latest_audio_stream_pts_ns = 0;
	ctx->latest_audio_buffered_end_pts_ns = 0;
	ctx->latest_audio_obs_end_ts_ns = 0;
	ctx->decoded_frame_samples = 0;
	ctx->startup_audio_warmup_remaining_ms = 0;
	ctx->audio_last_sample_channels = 0;
	ctx->audio_last_sample_valid = false;
	ctx->audio_decode_errors = 0;
	ctx->audio_last_decoder_flush_time_us = 0;
	ctx->audio_last_decoder_warning_time_us = 0;
	ctx->audio_drain_stuck_since_us = 0;
	ctx->audio_drain_stuck_fill_ms = 0;
	ctx->audio_drain_warn_time_us = 0;
}

void irl_reset_stream_timing_state(struct irl_source *ctx)
{
	irl_reset_audio_timing_state(ctx);

	/* The trim is a property of the sender, so it deliberately survives
	 * the audio-only resets above (a throttled decoder flush must not
	 * cost two minutes of relearning). It does not survive this one: a
	 * PTS-repair reset means the timeline broke badly enough that the
	 * level no longer maps to the sender's clock, and a reconnect may not
	 * even be the same encoder. Relearning costs nothing worse than the
	 * behaviour before the trim existed. */
	ctx->audio_speed_trim = 0.0f;
	ctx->audio_speed_trim_last_us = 0;

	ctx->video_ts_init = false;
	ctx->latest_video_stream_pts_ns = 0;
	/* State, not counters: the interval has to be re-measured for the
	 * new stream, and a stale lead would be reported until the first
	 * frame arrives. video_lead_excess is cumulative for the source,
	 * like the other quality counters. */
	ctx->video_prev_pts_ns = 0;
	ctx->video_frame_interval_ns = 0;
	ctx->video_lead_ns = 0;
	ctx->video_decode_errors = 0;
	ctx->video_last_decoder_warning_time_us = 0;
	ctx->video_corrupted = false;
	ctx->video_skip_logged = false;
	ctx->video_hold_logged = false;
}

void irl_mark_audio_recovery(struct irl_source *ctx, uint64_t duration_us)
{
	uint64_t now_us = (uint64_t)av_gettime();
	uint64_t until_us = now_us + duration_us;

	if (until_us > ctx->audio_recovery_until_us)
		ctx->audio_recovery_until_us = until_us;
}

bool irl_audio_recovery_active(const struct irl_source *ctx)
{
	uint64_t now_us = (uint64_t)av_gettime();
	return ctx->audio_recovery_until_us != 0 &&
	       now_us < ctx->audio_recovery_until_us;
}

static int64_t audio_frame_pts(const AVFrame *frame)
{
	if (frame->best_effort_timestamp != AV_NOPTS_VALUE)
		return frame->best_effort_timestamp;
	if (frame->pts != AV_NOPTS_VALUE)
		return frame->pts;
	return AV_NOPTS_VALUE;
}

static int audio_frame_duration_ms(int samples, int sample_rate)
{
	if (samples <= 0 || sample_rate <= 0)
		return 0;

	int64_t ms = (int64_t)samples * 1000LL / sample_rate;
	if (ms <= 0)
		ms = 1;
	return (int)ms;
}

static int audio_conceal_fade_frames(int sample_rate, int max_frames)
{
	if (sample_rate <= 0 || max_frames <= 0)
		return 0;

	int frames = sample_rate * AUDIO_CONCEAL_FADE_MS / 1000;
	if (frames <= 0)
		frames = 1;
	return frames < max_frames ? frames : max_frames;
}

static void remember_last_sample(const uint8_t *samples, int frames,
				 int channels, float *dst, int *dst_channels,
				 bool *dst_valid)
{
	if (!samples || frames <= 0 || channels <= 0 || channels > 8) {
		*dst_valid = false;
		*dst_channels = 0;
		return;
	}

	const float *pcm = (const float *)samples;
	const float *last = pcm + (size_t)(frames - 1) * channels;
	for (int ch = 0; ch < channels; ch++)
		dst[ch] = last[ch];
	*dst_channels = channels;
	*dst_valid = true;
}

static void audio_apply_fade_in(uint8_t *samples, int frames, int channels,
				int sample_rate)
{
	int fade_frames = audio_conceal_fade_frames(sample_rate, frames);
	if (!samples || fade_frames <= 0 || channels <= 0)
		return;

	float *pcm = (float *)samples;
	for (int f = 0; f < fade_frames; f++) {
		float gain = (float)(f + 1) / (float)fade_frames;
		for (int ch = 0; ch < channels; ch++)
			pcm[(size_t)f * channels + ch] *= gain;
	}
}

/* Ramp the head of a silence buffer down from the last played
 * sample values so a dropout decays instead of clicking. */
static void shape_silence_from_last(uint8_t *silence, size_t silence_bytes,
				    int channels, int frame_size,
				    int sample_rate, const float *last_sample,
				    int last_channels, bool last_valid)
{
	if (!silence || silence_bytes == 0 || channels <= 0 ||
	    frame_size <= 0 || sample_rate <= 0)
		return;

	int frames = (int)(silence_bytes / (size_t)frame_size);
	int fade_frames = audio_conceal_fade_frames(sample_rate, frames);
	if (!last_valid || last_channels != channels || fade_frames <= 0)
		return;

	float *pcm = (float *)silence;
	for (int f = 0; f < fade_frames; f++) {
		float gain = 1.0f - (float)(f + 1) / (float)fade_frames;
		for (int ch = 0; ch < channels; ch++)
			pcm[(size_t)f * channels + ch] = last_sample[ch] * gain;
	}
}

static int audio_expected_samples(const struct irl_source *ctx,
				  int64_t duration, int out_rate,
				  int fallback_samples)
{
	if (duration <= 0 || out_rate <= 0 || ctx->pts_state.tb_den <= 0)
		return fallback_samples;

	int64_t expected = av_rescale_q(duration,
					(AVRational){ctx->pts_state.tb_num,
						     ctx->pts_state.tb_den},
					(AVRational){1, out_rate});
	if (expected <= 0 || expected > INT_MAX)
		return fallback_samples;
	return (int)expected;
}

static int audio_soft_compensation_samples(const struct irl_source *ctx,
					   int64_t duration, int out_rate,
					   int actual_samples)
{
	int expected = audio_expected_samples(ctx, duration, out_rate,
					     actual_samples);
	int delta = expected - actual_samples;

	/* Let PTS repair handle real discontinuities. This is only for
	 * tiny per-frame drift, similar in spirit to a bounded aresample
	 * async correction. */
	if (delta < -8 || delta > 8)
		return 0;
	return delta;
}

/* ── Output clock ─────────────────────────────────────────── */

static uint64_t audio_output_next_ts(const struct irl_source *ctx,
				     int out_rate)
{
	return ctx->audio_out_anchor_ns +
	       (uint64_t)av_rescale((int64_t)ctx->audio_out_samples,
				    1000000000LL, out_rate);
}

uint64_t irl_audio_output_claim(struct irl_source *ctx, int frames,
				int out_rate)
{
	uint64_t ts = audio_output_next_ts(ctx, out_rate);
	ctx->audio_out_samples += (uint64_t)frames;
	return ts;
}

static uint64_t audio_output_lead_ns(const struct irl_source *ctx,
				     int chunk_samples, int out_rate)
{
	uint64_t chunk_ns =
		(uint64_t)chunk_samples * 1000000000ULL / (uint64_t)out_rate;
	if (ctx->config.low_latency_audio)
		return chunk_ns;

	uint64_t lead = AUDIO_OUT_LEAD_MS * 1000000ULL;
	if (lead < chunk_ns * 3)
		lead = chunk_ns * 3;
	return lead;
}

/* ── Speed control ────────────────────────────────────────── */

/* Integrate the level error into the speed trim.
 *
 * `ramp` is the proportional term this cycle, passed in so the trim can
 * tell whether the loop is still in control. Caller holds
 * audio_state_lock. */
static void audio_update_speed_trim(struct irl_source *ctx, int fill_ms,
				    int target_ms, float ramp)
{
	uint64_t now_us = (uint64_t)av_gettime();
	uint64_t last_us = ctx->audio_speed_trim_last_us;

	ctx->audio_speed_trim_last_us = now_us;

	/* First cycle after a start or a stall: re-seed dt, integrate
	 * nothing. */
	if (last_us == 0 || now_us <= last_us ||
	    now_us - last_us > AUDIO_SPEED_TRIM_MAX_DT_US)
		return;

	/* Concealment and post-reset recovery move the fill for reasons
	 * that are not the sender's clock. Integrating there would learn
	 * the outage. */
	if (irl_audio_recovery_active(ctx))
		return;

	int err_ms = fill_ms - target_ms;
	if (err_ms > AUDIO_SPEED_TRIM_ERR_WINDOW_MS ||
	    err_ms < -AUDIO_SPEED_TRIM_ERR_WINDOW_MS)
		return;

	double dt = (double)(now_us - last_us) / 1000000.0;
	double err_s = (double)err_ms / 1000.0;
	double step = AUDIO_SPEED_TRIM_GAIN * err_s * dt;

	/* Anti-windup, the second half of the error-window gate above.
	 *
	 * While the loop is saturated the level has stopped reporting the
	 * sender's rate — it reports that the controller ran out of
	 * authority. At the default target the window gate already covers
	 * this, but a small target puts min_ms within 60ms of target and the
	 * command can pin while the error is still inside the window, so the
	 * check earns its place there.
	 *
	 * Test the command actually issued, not the ramp alone: the actuator
	 * clamps ramp + trim, so with the trim near its own limit the sum
	 * saturates while the ramp is still short of it. */
	float command = ramp + ctx->audio_speed_trim;
	bool pinned_high = command >= audio_speed_max(ctx) - 0.0005f;
	bool pinned_low = command <= AUDIO_SPEED_MIN + 0.0005f;
	if ((pinned_high && step > 0.0) || (pinned_low && step < 0.0))
		return;

	double next = (double)ctx->audio_speed_trim + step;
	if (next > AUDIO_SPEED_TRIM_MAX)
		next = AUDIO_SPEED_TRIM_MAX;
	else if (next < -(double)AUDIO_SPEED_TRIM_MAX)
		next = -(double)AUDIO_SPEED_TRIM_MAX;
	ctx->audio_speed_trim = (float)next;
}

static float compute_buffered_output_speed(struct irl_source *ctx, int fill_ms)
{
	if (!os_atomic_load_bool(&ctx->config.adaptive_speed) ||
	    ctx->config.low_latency_audio) {
		ctx->current_speed = 1.0f;
		/* Nothing is regulating the buffer, so a trim learned before
		 * the setting changed describes a loop that no longer runs. */
		ctx->audio_speed_trim = 0.0f;
		ctx->audio_speed_trim_last_us = 0;
		return 1.0f;
	}

	int target_ms = (int)os_atomic_load_long(&ctx->config.buffer_target_ms);
	int min_ms = (int)os_atomic_load_long(&ctx->config.buffer_min_ms);
	int max_ms = (int)os_atomic_load_long(&ctx->config.buffer_max_ms);
	int low_edge = target_ms - AUDIO_SPEED_DEADBAND_MS;
	int high_edge = target_ms + AUDIO_SPEED_DEADBAND_MS;
	float ramp;

	if (fill_ms < low_edge) {
		int span = low_edge - min_ms;
		float t = span > 0 ? (float)(low_edge - fill_ms) / (float)span
				   : 1.0f;
		if (t > 1.0f)
			t = 1.0f;
		float edge = 1.0f - AUDIO_SPEED_DEADBAND_SLOPE;
		ramp = edge - (edge - AUDIO_SPEED_MIN) * t;
	} else if (fill_ms > high_edge) {
		int span = max_ms - high_edge;
		float t = span > 0 ? (float)(fill_ms - high_edge) / (float)span
				   : 1.0f;
		if (t > 1.0f)
			t = 1.0f;
		float edge = 1.0f + AUDIO_SPEED_DEADBAND_SLOPE;
		ramp = edge + (audio_speed_max(ctx) - edge) * t;
	} else {
		/* Shallow slope rather than a flat 1.0: see
		 * AUDIO_SPEED_DEADBAND_SLOPE. This is what damps the trim. */
		ramp = 1.0f + AUDIO_SPEED_DEADBAND_SLOPE *
				      (float)(fill_ms - target_ms) /
				      (float)AUDIO_SPEED_DEADBAND_MS;
	}

	audio_update_speed_trim(ctx, fill_ms, target_ms, ramp);

	/* The trim shifts the operating point the ramp swings around; the
	 * hard clamp below is unchanged, because the min and the catch-up
	 * ceiling are the audibility limits and hold absolutely. That the slow-down
	 * authority shrinks to -1% once the trim has learned +1% is
	 * correct, not a loss: having established that the sender runs
	 * fast, dropping to 0.98 absolute would be over-correcting. */
	float target_speed = ramp + ctx->audio_speed_trim;

	if (ctx->current_speed <= 0.0f)
		ctx->current_speed = 1.0f;
	ctx->current_speed +=
		(target_speed - ctx->current_speed) * AUDIO_SPEED_SMOOTHING;
	if (ctx->current_speed < AUDIO_SPEED_MIN)
		ctx->current_speed = AUDIO_SPEED_MIN;
	float speed_max = audio_speed_max(ctx);
	if (ctx->current_speed > speed_max)
		ctx->current_speed = speed_max;
	return ctx->current_speed;
}

static bool ensure_speed_swr(struct irl_source *ctx, int rate, int channels)
{
	if (ctx->speed_swr && ctx->speed_swr_rate == rate &&
	    ctx->speed_swr_channels == channels)
		return true;

	if (ctx->speed_swr)
		swr_free(&ctx->speed_swr);
	ctx->speed_swr_rate = 0;
	ctx->speed_swr_channels = 0;

	AVChannelLayout layout;
	av_channel_layout_default(&layout, channels);
	if (swr_alloc_set_opts2(&ctx->speed_swr, &layout, AV_SAMPLE_FMT_FLT,
				rate, &layout, AV_SAMPLE_FMT_FLT, rate, 0,
				NULL) < 0 ||
	    !ctx->speed_swr) {
		av_channel_layout_uninit(&layout);
		return false;
	}
	av_channel_layout_uninit(&layout);

	/* Force the resampler active from the start; otherwise the first
	 * swr_set_compensation() call reinitialises the context mid-stream. */
	av_opt_set_int(ctx->speed_swr, "flags", SWR_FLAG_RESAMPLE, 0);
	if (swr_init(ctx->speed_swr) < 0) {
		swr_free(&ctx->speed_swr);
		return false;
	}

	ctx->speed_swr_rate = rate;
	ctx->speed_swr_channels = channels;
	return true;
}

/* Stretch/shrink one chunk by `speed` through the persistent output
 * resampler. Returns output frame count, or -1 to fall back to the
 * unmodified input chunk. */
static int apply_output_speed(struct irl_source *ctx, const uint8_t *in,
			      int in_frames, int rate, int channels,
			      float speed, uint8_t **out)
{
	if (!ensure_speed_swr(ctx, rate, channels))
		return -1;

	/* Carry the fractional remainder into the next chunk. The resampler
	 * is driven in whole samples, so rounding each chunk independently
	 * quantises the applied speed to multiples of 1/in_frames (~0.1% at
	 * 1024 frames): a requested 1.0005 is executed as 1.00098, and a
	 * requested 1.0004 as 1.0. Accumulating makes the long-run rate
	 * exact, and stops the compensation switching on and off as the
	 * request drifts across a rounding boundary. */
	double want = (double)in_frames / (double)speed + ctx->audio_speed_frac;
	int desired = (int)(want + 0.5);
	if (desired < 1)
		desired = 1;

	/* Bounded so a pathological speed or a clamped `desired` cannot let
	 * the debt run away and dump a correction into some later chunk. */
	double carry = want - (double)desired;
	if (carry > 1.0)
		carry = 1.0;
	else if (carry < -1.0)
		carry = -1.0;
	ctx->audio_speed_frac = carry;

	if (desired != in_frames &&
	    swr_set_compensation(ctx->speed_swr, desired - in_frames,
				 desired) < 0)
		return -1;

	int max_out = swr_get_out_samples(ctx->speed_swr, in_frames);
	if (max_out < desired)
		max_out = desired;
	max_out += 32;

	size_t need = (size_t)max_out * channels * sizeof(float);
	uint8_t *buf = ensure_scratch(&ctx->audio_speed_scratch,
				      &ctx->audio_speed_scratch_capacity, need);
	if (!buf)
		return -1;

	int got = swr_convert(ctx->speed_swr, &buf, max_out, &in, in_frames);
	if (got <= 0)
		return -1;

	*out = buf;
	return got;
}

/* ── Output bookkeeping ───────────────────────────────────── */

static void finalize_audio_output(struct irl_source *ctx,
				  const struct obs_source_audio *obs_audio,
				  int64_t chunk_pts_ns,
				  uint64_t stream_duration_ns)
{
	ctx->latest_audio_buffered_end_pts_ns =
		chunk_pts_ns + (int64_t)stream_duration_ns;

	if (obs_audio->samples_per_sec > 0) {
		uint64_t audio_duration_ns =
			(uint64_t)obs_audio->frames * 1000000000ULL /
			(uint64_t)obs_audio->samples_per_sec;
		ctx->latest_audio_obs_end_ts_ns =
			obs_audio->timestamp + audio_duration_ns;
		ctx->audio_last_chunk_obs_duration_ns = audio_duration_ns;
	} else {
		ctx->latest_audio_obs_end_ts_ns = obs_audio->timestamp;
		ctx->audio_last_chunk_obs_duration_ns = 0;
	}

	ctx->audio_last_chunk_stream_duration_ns = stream_duration_ns;
	ctx->audio_last_frames_out = obs_audio->frames;
	ctx->audio_last_samples_per_sec = obs_audio->samples_per_sec;

	uint64_t after_output = os_gettime_ns();
	if (ctx->latest_audio_obs_end_ts_ns > after_output) {
		ctx->audio_last_obs_lead_ns =
			(int64_t)(ctx->latest_audio_obs_end_ts_ns - after_output);
	} else {
		ctx->audio_last_obs_lead_ns = 0;
	}

	ctx->total_audio_frames++;
}

/* ── Hidden backlog trim ──────────────────────────────────── */

static bool should_hide_audio_backlog(const struct irl_source *ctx)
{
	return !ctx->config.low_latency_audio && !ctx->audio_out_primed;
}

/* Before playback primes, nothing has been audible yet, so excess
 * startup backlog can be dropped for free. This is the only trim
 * path. Once audio is live, content is never skipped: the read loop
 * stops ingesting above a fill ceiling (transport backpressure) and
 * playback bleeds the backlog off at up to the catch-up speed. */
static bool maybe_trim_hidden_audio_backlog(struct irl_source *ctx, int fill_ms,
					    int chunk_count)
{
	if (!os_atomic_load_bool(&ctx->config.adaptive_speed))
		return false;
	if (!should_hide_audio_backlog(ctx))
		return false;
	if (chunk_count <= 1)
		return false;

	int out_rate = ctx->audio_buf.sample_rate;
	int chunk_ms = 0;
	if (out_rate > 0 && ctx->decoded_frame_samples > 0) {
		chunk_ms = (int)((int64_t)ctx->decoded_frame_samples * 1000LL /
				 out_rate);
	}
	if (chunk_ms <= 0)
		chunk_ms = 21;

	/* Keep enough to satisfy the prime threshold, which includes
	 * the OBS-side lead. */
	int chunk_samples = ctx->decoded_frame_samples > 0
				    ? ctx->decoded_frame_samples
				    : 960;
	int target_ms = (int)os_atomic_load_long(&ctx->config.buffer_target_ms);
	int keep_ms = target_ms + chunk_ms;
	if (out_rate > 0) {
		keep_ms += (int)(audio_output_lead_ns(ctx, chunk_samples,
						      out_rate) /
				 1000000ULL);
	}
	if (fill_ms <= keep_ms + AUDIO_TRIM_TRIGGER_MS)
		return false;

	int post_fill_ms = fill_ms;
	int post_chunks = chunk_count;
	int trimmed = audio_buffer_trim_to_keep_ms(&ctx->audio_buf, keep_ms,
						   1, &post_fill_ms,
						   &post_chunks);
	if (trimmed <= 0)
		return false;

	ctx->audio_resync_skipped_chunks += (uint64_t)trimmed;
	ctx->audio_hidden_trimmed_chunks += (uint64_t)trimmed;
	blog(LOG_INFO,
	     "[irl-source] Audio trim: dropped %d hidden buffered chunk%s before playback (fill=%dms target=%dms)",
	     trimmed, trimmed == 1 ? "" : "s", post_fill_ms, target_ms);
	return true;
}

/* ── Concealment ──────────────────────────────────────────── */

static bool emit_concealment_silence(struct irl_source *ctx, int frames)
{
	int out_rate = ctx->audio_buf.sample_rate;
	size_t silence_bytes = (size_t)frames * ctx->audio_buf.frame_size;
	uint8_t *silence = ensure_scratch(&ctx->audio_pump_scratch,
					  &ctx->audio_pump_scratch_capacity,
					  silence_bytes);
	if (!silence)
		return false;
	memset(silence, 0, silence_bytes);

	shape_silence_from_last(silence, silence_bytes,
				ctx->audio_buf.channels,
				ctx->audio_buf.frame_size, out_rate,
				ctx->audio_out_last_sample,
				ctx->audio_out_last_channels,
				ctx->audio_out_last_valid);
	/* Only the first concealment chunk decays from real audio;
	 * subsequent ones are pure silence. */
	ctx->audio_out_last_valid = false;

	struct obs_source_audio obs_audio = {0};
	obs_audio.data[0] = silence;
	obs_audio.frames = (uint32_t)frames;
	obs_audio.format = AUDIO_FORMAT_FLOAT;
	obs_audio.speakers = (enum speaker_layout)ctx->audio_buf.channels;
	obs_audio.samples_per_sec = (uint32_t)out_rate;
	obs_audio.timestamp = irl_audio_output_claim(ctx, frames, out_rate);
	obs_source_output_audio(ctx->source, &obs_audio);

	/* Stream PTS does not advance during concealment: the video
	 * mapping offset grows by the outage length, which matches the
	 * real playout delay until the hidden trim pulls it back. */
	finalize_audio_output(ctx, &obs_audio,
			      ctx->latest_audio_buffered_end_pts_ns, 0);
	return true;
}

/* ── Offset re-anchor ─────────────────────────────────────── */

/* The audio->OBS playout offset is (obs clock end) - (stream PTS end)
 * of the latest chunk handed to OBS; the video path adds this same
 * offset to every frame PTS for lip sync. Concealment freezes the
 * stream-PTS side while advancing the obs side, so a delivery stall
 * inflates the offset by the outage length. Once primed the only
 * recovery is the speed-drain bleeding buffer backlog, which does
 * nothing when the concealed audio was dropped rather than delayed:
 * the latency then sticks and every later blip stacks onto it.
 *
 * When the offset drifts too far past its primed baseline AND the
 * buffer has already drained back to target, reclaim it with one
 * declared re-anchor: restart the output clock line and drop the
 * stale mapping so the next chunk rebuilds it fresh. This costs a
 * single concealed splice but caps the latency instead of letting it
 * ratchet up without bound. */
static void irl_audio_maybe_reanchor_offset(struct irl_source *ctx,
					    uint64_t now, uint64_t chunk_ns)
{
	if (ctx->latest_audio_obs_end_ts_ns == 0 ||
	    ctx->latest_audio_buffered_end_pts_ns <= 0)
		return;

	int64_t offset_ns = (int64_t)ctx->latest_audio_obs_end_ts_ns -
			    ctx->latest_audio_buffered_end_pts_ns;

	/* The offset's absolute value is arbitrary (it carries the
	 * stream's PTS epoch); only its drift from the primed baseline is
	 * meaningful, so anchor the comparison the first time we see a
	 * valid offset after priming. */
	if (!ctx->audio_playout_offset_baseline_set) {
		ctx->audio_playout_offset_baseline_ns = offset_ns;
		ctx->audio_playout_offset_baseline_set = true;
		return;
	}

	int64_t margin_ns = (int64_t)AUDIO_OFFSET_REANCHOR_MARGIN_MS * 1000000LL;
	int64_t excess_ns = offset_ns - ctx->audio_playout_offset_baseline_ns;
	if (excess_ns <= margin_ns)
		return;

	/* Only reclaim latency the speed-drain cannot. While backlog is
	 * queued the inflation is real buffered audio, and draining it at
	 * up to the catch-up speed preserves every sample, so leave it
	 * entirely to the speed controller (content is never skipped). We
	 * step in only
	 * once the buffer is back at/below target, where the residual
	 * offset is phantom: concealment silence with no backing audio
	 * (the concealed packets were dropped, not merely late), which
	 * no speed-up can ever recover. Re-anchoring here skips nothing. */
	if (audio_buffer_fill_ms_locked(&ctx->audio_buf) >
	    ctx->audio_buf.target_ms)
		return;

	/* audio_state_lock is already held (see irl_pump_audio_once). The
	 * fill query above takes and releases the buffer mutex underneath it,
	 * which is the documented order (state lock, then buffer mutex). */
	ctx->audio_out_anchor_ns = now + chunk_ns;
	ctx->audio_out_samples = 0;
	ctx->latest_audio_obs_end_ts_ns = 0;
	ctx->latest_audio_buffered_end_pts_ns = 0;
	ctx->audio_playout_offset_baseline_set = false;
	ctx->audio_conceal_fade_pending = true;

	ctx->audio_offset_reanchors++;
	ctx->audio_quality_events++;
	blog(LOG_WARNING,
	     "[irl-source] Audio latency drifted +%lldms past baseline (>%dms) with buffer at/below target; re-anchoring output clock",
	     (long long)(excess_ns / 1000000LL),
	     AUDIO_OFFSET_REANCHOR_MARGIN_MS);
}

/* ── Unwinnable drain detection ───────────────────────────── */

/* Called once per emitted chunk with the fill and speed that produced it. */
static void audio_check_drain_progress(struct irl_source *ctx, int fill_ms,
				       float speed)
{
	int target_ms = (int)os_atomic_load_long(&ctx->config.buffer_target_ms);
	bool at_full_authority = speed >= audio_speed_max(ctx) - 0.0005f &&
				 fill_ms > target_ms + AUDIO_SPEED_DEADBAND_MS;

	if (!at_full_authority) {
		ctx->audio_drain_stuck_since_us = 0;
		return;
	}

	uint64_t now_us = (uint64_t)av_gettime();
	if (ctx->audio_drain_stuck_since_us == 0) {
		ctx->audio_drain_stuck_since_us = now_us;
		ctx->audio_drain_stuck_fill_ms = fill_ms;
		return;
	}

	/* Coming down, just slowly: not stuck. */
	if (fill_ms <= ctx->audio_drain_stuck_fill_ms -
			       AUDIO_DRAIN_STUCK_PROGRESS_MS) {
		ctx->audio_drain_stuck_since_us = now_us;
		ctx->audio_drain_stuck_fill_ms = fill_ms;
		return;
	}

	if (now_us - ctx->audio_drain_stuck_since_us < AUDIO_DRAIN_STUCK_US)
		return;
	if (ctx->audio_drain_warn_time_us != 0 &&
	    now_us - ctx->audio_drain_warn_time_us < AUDIO_DRAIN_STUCK_US)
		return;
	ctx->audio_drain_warn_time_us = now_us;

	blog(LOG_WARNING,
	     "[irl-source] Audio buffer stuck at %dms (target %dms) with playback at +%.0f%% for %llus: "
	     "the sender is delivering faster than real time, so the buffer cannot drain and latency stays here. "
	     "Video stays in sync with it; check the sender's frame rate and clock",
	     fill_ms, target_ms, (double)((speed - 1.0f) * 100.0f),
	     (unsigned long long)((now_us - ctx->audio_drain_stuck_since_us) /
				  1000000ULL));
}

/* ── Pump ─────────────────────────────────────────────────── */

/* Stand the low-latency output clock down until real audio returns.
 *
 * Low-latency mode deliberately emits no concealment, so an empty input
 * cannot advance the sample counter. The output clock then sits still while
 * wall clock moves, and the stall check below reads that as a stalled audio
 * thread — which it is not. Restarting it there re-anchors, waits one lead,
 * and trips again, so a silent input produced a restart every ~150ms for as
 * long as it stayed silent: log spam, and audio_output_restarts /
 * audio_quality_events climbing on a source that is merely quiet.
 *
 * Drop the stale mapping instead and let the normal prime path establish one
 * new clock when a real chunk arrives. Counted as an underrun, which is what
 * it is. Buffered mode is untouched: its concealment keeps the counter
 * moving, so a late clock there really is an output-side stall. */
static void suspend_low_latency_audio_clock(struct irl_source *ctx,
					    uint64_t lag_ns)
{
	ctx->audio_out_primed = false;
	ctx->audio_out_anchor_ns = 0;
	ctx->audio_out_samples = 0;
	ctx->latest_audio_obs_end_ts_ns = 0;
	ctx->latest_audio_buffered_end_pts_ns = 0;
	ctx->audio_last_obs_lead_ns = 0;
	ctx->audio_playout_offset_baseline_set = false;
	ctx->audio_conceal_fade_pending = true;

	ctx->audio_underruns++;
	ctx->audio_quality_events++;
	irl_mark_audio_recovery(ctx, AUDIO_RECOVERY_HOLD_US);
	blog(LOG_WARNING,
	     "[irl-source] Low-latency audio input empty for %llums; suspending output clock until audio resumes",
	     (unsigned long long)(lag_ns / 1000000ULL));
}

/* Caller holds audio_state_lock for the whole call (irl_audio_thread). See
 * the declaration in receiver-internal.h: nothing on this path may re-take
 * it. Buffer-mutex calls (peek/read/fill) nest underneath it, which is the
 * documented lock order. */
bool irl_pump_audio_once(struct irl_source *ctx)
{
	bool low_latency = ctx->config.low_latency_audio;
	int out_rate = ctx->audio_buf.sample_rate;
	int out_channels = ctx->audio_buf.channels;
	int bytes_per_sample = ctx->audio_buf.bytes_per_sample;
	int base_samples = ctx->decoded_frame_samples;

	if (!ctx->audio_buf.data || out_rate <= 0 || out_channels <= 0 ||
	    bytes_per_sample <= 0)
		return false;
	if (base_samples <= 0)
		base_samples = 960;

	uint64_t chunk_ns =
		(uint64_t)base_samples * 1000000000ULL / (uint64_t)out_rate;
	uint64_t lead_ns = audio_output_lead_ns(ctx, base_samples, out_rate);
	uint64_t now = os_gettime_ns();

	/* Read before the primed block below, which needs to know whether the
	 * input is empty to tell a stalled output thread from a quiet source.
	 * The trim underneath only acts before priming, so hoisting it here
	 * changes nothing about when it runs. */
	int64_t peek = 0;
	int fill_ms = 0;
	int chunk_count = 0;
	bool has_audio = audio_buffer_peek_state(&ctx->audio_buf, &peek,
						 &fill_ms, &chunk_count);
	/* The receiver thread reads this for the stats line; audio_state_lock
	 * is already held for the whole pump, so the publish is covered.
	 * peek_state took and released the buffer mutex underneath it, which
	 * is the documented order (audio_state_lock before the buffer
	 * mutex). */
	if (fill_ms > ctx->audio_fill_peak_ms)
		ctx->audio_fill_peak_ms = fill_ms;

	if (has_audio &&
	    maybe_trim_hidden_audio_backlog(ctx, fill_ms, chunk_count))
		return true;

	if (ctx->audio_out_primed) {
		/* Cap runaway concealment latency before it desyncs A/V.
		 * Runs even on a healthy-lead cycle: the offset inflates
		 * from past outages, not from the current queue depth. */
		if (!low_latency)
			irl_audio_maybe_reanchor_offset(ctx, now, chunk_ns);

		uint64_t next_ts = audio_output_next_ts(ctx, out_rate);

		/* Enough queued ahead of wall clock — nothing to do. */
		if (next_ts >= now + lead_ns)
			return false;

		/* Output clock fell far behind wall clock: the audio
		 * thread was stalled. Restart the clock line once,
		 * declared and counted, instead of letting OBS add
		 * permanent audio buffering for a late source. */
		if (now > next_ts &&
		    now - next_ts > AUDIO_OUT_MAX_LAG_MS * 1000000ULL) {
			/* An empty low-latency input is a quiet source, not a
			 * stalled thread: nothing there can advance the
			 * counter. Stand the clock down instead. */
			if (low_latency && !has_audio) {
				suspend_low_latency_audio_clock(ctx,
								now - next_ts);
				return false;
			}
			ctx->audio_output_restarts++;
			ctx->audio_quality_events++;
			blog(LOG_WARNING,
			     "[irl-source] Audio output stalled %llums; restarting output clock",
			     (unsigned long long)((now - next_ts) / 1000000ULL));
			ctx->audio_out_anchor_ns = now + chunk_ns;
			ctx->audio_out_samples = 0;
			ctx->audio_conceal_fade_pending = true;
		}
	}

	if (!ctx->audio_out_primed) {
		int prime_ms = 0;
		if (!low_latency) {
			prime_ms = ctx->audio_buf.target_ms +
				   (int)(lead_ns / 1000000ULL);
		}
		if (!has_audio || fill_ms < prime_ms)
			return false;

		ctx->audio_out_primed = true;
		ctx->audio_out_anchor_ns = now + chunk_ns;
		ctx->audio_out_samples = 0;
		blog(LOG_INFO,
		     "[irl-source] Audio output primed (fill=%dms lead=%dms rate=%d)",
		     fill_ms, (int)(lead_ns / 1000000ULL), out_rate);
	}

	if (!has_audio) {
		/* Low-latency mode emits no concealment. A brief gap resumes
		 * on the clock line it left off; a long one already had that
		 * clock suspended above, and re-primes when audio returns. */
		if (low_latency)
			return false;

		if (!irl_audio_recovery_active(ctx)) {
			blog(LOG_INFO,
			     "[irl-source] Audio underrun: concealing with silence");
		}
		ctx->audio_underruns++;
		ctx->audio_quality_events++;
		irl_mark_audio_recovery(ctx, AUDIO_RECOVERY_HOLD_US);
		ctx->audio_conceal_fade_pending = true;
		return emit_concealment_silence(ctx, base_samples);
	}

	/* Low-latency mode has no speed control; cap runaway backlog
	 * by skipping old chunks (latency wins over continuity here). */
	if (low_latency && fill_ms > AUDIO_LL_MAX_FILL_MS) {
		int chunk_ms = (int)(chunk_ns / 1000000ULL);
		int post_fill = fill_ms;
		int post_chunks = chunk_count;
		int trimmed = audio_buffer_trim_to_keep_ms(
			&ctx->audio_buf, chunk_ms * 2 > 0 ? chunk_ms * 2 : 42,
			1, &post_fill, &post_chunks);
		if (trimmed > 0) {
			ctx->audio_resync_skipped_chunks += (uint64_t)trimmed;
			ctx->audio_quality_events++;
			fill_ms = post_fill;
		}
	}

	size_t frame_bytes =
		(size_t)base_samples * ctx->audio_buf.frame_size;
	uint8_t *in_buf = ensure_scratch(&ctx->audio_pump_scratch,
					 &ctx->audio_pump_scratch_capacity,
					 frame_bytes);
	if (!in_buf)
		return false;

	int64_t chunk_pts_ns = 0;
	size_t got = audio_buffer_read_pts(&ctx->audio_buf, in_buf,
					   frame_bytes, &chunk_pts_ns);
	if (got == 0)
		return false;

	int in_frames = (int)(got / (size_t)(out_channels * bytes_per_sample));
	uint64_t stream_duration_ns =
		(uint64_t)in_frames * 1000000000ULL / (uint64_t)out_rate;

	float speed = compute_buffered_output_speed(ctx, fill_ms);
	audio_check_drain_progress(ctx, fill_ms, speed);
	uint8_t *emit_buf = in_buf;
	uint32_t frames_out = (uint32_t)in_frames;

	if (!low_latency && os_atomic_load_bool(&ctx->config.adaptive_speed)) {
		uint8_t *speed_buf = NULL;
		int speed_frames = apply_output_speed(ctx, in_buf, in_frames,
						      out_rate, out_channels,
						      speed, &speed_buf);
		if (speed_frames > 0) {
			emit_buf = speed_buf;
			frames_out = (uint32_t)speed_frames;
		}
	}

	if (ctx->fade_in_pending) {
		ctx->fade_in_frames_remaining =
			out_rate * IRL_FADE_DURATION_MS / 1000;
		ctx->fade_in_pending = false;
		ctx->audio_conceal_fade_pending = false;
	}
	if (ctx->fade_in_frames_remaining > 0) {
		int total_fade = out_rate * IRL_FADE_DURATION_MS / 1000;
		float *s = (float *)emit_buf;
		int nf = (int)frames_out;
		for (int f = 0; f < nf && ctx->fade_in_frames_remaining > 0;
		     f++) {
			int into = total_fade - ctx->fade_in_frames_remaining;
			float gain = (float)into / (float)total_fade;
			for (int ch = 0; ch < out_channels; ch++)
				s[f * out_channels + ch] *= gain;
			ctx->fade_in_frames_remaining--;
		}
	} else if (ctx->audio_conceal_fade_pending) {
		audio_apply_fade_in(emit_buf, (int)frames_out, out_channels,
				    out_rate);
		ctx->audio_conceal_fade_pending = false;
	}

	struct obs_source_audio obs_audio = {0};
	obs_audio.data[0] = emit_buf;
	obs_audio.frames = frames_out;
	obs_audio.format = AUDIO_FORMAT_FLOAT;
	obs_audio.speakers = (enum speaker_layout)out_channels;
	obs_audio.samples_per_sec = (uint32_t)out_rate;
	obs_audio.timestamp = irl_audio_output_claim(ctx, (int)frames_out,
						     out_rate);

	obs_source_output_audio(ctx->source, &obs_audio);
	remember_last_sample(emit_buf, (int)frames_out, out_channels,
			     ctx->audio_out_last_sample,
			     &ctx->audio_out_last_channels,
			     &ctx->audio_out_last_valid);
	finalize_audio_output(ctx, &obs_audio, chunk_pts_ns,
			      stream_duration_ns);
	return true;
}

/* ── Decoded-frame intake (receiver thread) ───────────────── */

void irl_handle_audio_frame(struct irl_source *ctx, AVFrame *frame)
{
	AVStream *as = NULL;
	if (ctx->fmt_ctx && ctx->audio_stream_idx >= 0)
		as = ctx->fmt_ctx->streams[ctx->audio_stream_idx];

	int out_channels = frame->ch_layout.nb_channels;
	int out_rate = frame->sample_rate;
	int bytes_per_sample = sizeof(float);

	if (ctx->audio_buf.sample_rate != out_rate ||
	    ctx->audio_buf.channels != out_channels) {
		irl_mutex_lock(&ctx->audio_state_lock);
		int target_ms = (int)os_atomic_load_long(
			&ctx->config.buffer_target_ms);
		int min_ms =
			(int)os_atomic_load_long(&ctx->config.buffer_min_ms);
		int max_ms =
			(int)os_atomic_load_long(&ctx->config.buffer_max_ms);
		bool reconfigured = true;
		if (ctx->audio_buf.data) {
			reconfigured = audio_buffer_reconfigure(
				&ctx->audio_buf, out_rate, out_channels,
				bytes_per_sample, target_ms, min_ms, max_ms);
		} else {
			reconfigured = audio_buffer_init(
				&ctx->audio_buf, out_rate, out_channels,
				bytes_per_sample, target_ms, min_ms, max_ms);
		}
		ctx->audio_out_primed = false;
		ctx->audio_out_anchor_ns = 0;
		ctx->audio_out_samples = 0;
		ctx->audio_out_last_valid = false;
		ctx->latest_audio_buffered_end_pts_ns = 0;
		ctx->latest_audio_stream_pts_ns = 0;
		ctx->latest_audio_obs_end_ts_ns = 0;
		ctx->startup_audio_warmup_remaining_ms =
			IRL_STARTUP_AUDIO_WARMUP_MS;
		irl_mutex_unlock(&ctx->audio_state_lock);
		if (!reconfigured)
			return;
	}

	int64_t input_pts = audio_frame_pts(frame);
	if (input_pts == AV_NOPTS_VALUE) {
		if (ctx->pts_state.initialised) {
			input_pts = ctx->pts_state.last_pts +
				    ctx->pts_state.last_duration;
		} else {
			blog(LOG_WARNING,
			     "[irl-source] Dropping audio frame without valid PTS");
			return;
		}
	}

	int64_t duration = frame->duration;
	if (duration <= 0 && as && out_rate > 0 && frame->nb_samples > 0) {
		duration = av_rescale_q(frame->nb_samples,
					(AVRational){1, out_rate},
					as->time_base);
	}
	if (duration <= 0)
		duration = 1;

	int64_t corrected_pts;
	int silence_ms = 0;
	enum pts_action action = pts_repair_evaluate(
		&ctx->pts_state, input_pts, duration, &corrected_pts,
		&silence_ms);
	bool inserted_silence = false;

	int frame_ms = audio_frame_duration_ms(frame->nb_samples, out_rate);
	if (ctx->startup_audio_warmup_remaining_ms > 0) {
		ctx->startup_audio_warmup_remaining_ms -= frame_ms;
		if (ctx->startup_audio_warmup_remaining_ms < 0)
			ctx->startup_audio_warmup_remaining_ms = 0;
		return;
	}

	if (action == PTS_ACTION_SILENCE && silence_ms > 0) {
		size_t silence_bytes =
			audio_buffer_ms_to_bytes(&ctx->audio_buf, silence_ms);
		uint8_t *silence = ensure_scratch(
			&ctx->audio_resample_scratch,
			&ctx->audio_resample_scratch_capacity, silence_bytes);
		if (silence) {
			memset(silence, 0, silence_bytes);
			shape_silence_from_last(silence, silence_bytes,
						ctx->audio_buf.channels,
						ctx->audio_buf.frame_size,
						ctx->audio_buf.sample_rate,
						ctx->audio_last_sample,
						ctx->audio_last_sample_channels,
						ctx->audio_last_sample_valid);
			int64_t silence_pts_ns =
				av_rescale_q(corrected_pts,
					     (AVRational){ctx->pts_state.tb_num,
							  ctx->pts_state.tb_den},
					     (AVRational){1, 1000000000}) -
				(int64_t)silence_ms * 1000000LL;
			if (silence_pts_ns < 0)
				silence_pts_ns = 0;
			audio_buffer_write_pts(&ctx->audio_buf, silence,
					       silence_bytes, silence_pts_ns);
			ctx->silence_insertions++;
			ctx->audio_quality_events++;
			inserted_silence = true;
		}
	} else if (action == PTS_ACTION_RESET) {
		irl_mutex_lock(&ctx->audio_state_lock);
		audio_buffer_flush(&ctx->audio_buf);
		irl_reset_stream_timing_state(ctx);
		irl_mark_audio_recovery(ctx, AUDIO_RECOVERY_HOLD_US);
		ctx->audio_quality_events++;
		irl_mutex_unlock(&ctx->audio_state_lock);
	}

	if (action != PTS_ACTION_PASS) {
		ctx->pts_last_gap_ms = ctx->pts_state.last_action_gap_ms;
		if (ctx->pts_last_gap_ms > ctx->pts_max_gap_ms)
			ctx->pts_max_gap_ms = ctx->pts_last_gap_ms;

		bool frame_sized_normalization =
			action == PTS_ACTION_INTERPOLATE && frame_ms > 0 &&
			ctx->pts_last_gap_ms <= frame_ms + 2;
		if (frame_sized_normalization) {
			ctx->pts_normalizations++;
		} else {
			ctx->pts_repairs++;
			if (action == PTS_ACTION_INTERPOLATE)
				ctx->pts_interpolations++;
		}
		if (action == PTS_ACTION_RESET)
			ctx->pts_resets++;
	}

	uint8_t *interleaved = NULL;
	int out_samples = frame->nb_samples;

	if (frame->format != AV_SAMPLE_FMT_FLT) {
		if (!ctx->swr_ctx || ctx->swr_in_rate != frame->sample_rate ||
		    ctx->swr_in_channels != frame->ch_layout.nb_channels ||
		    ctx->swr_in_format != frame->format) {
			if (ctx->swr_ctx)
				swr_free(&ctx->swr_ctx);
			AVChannelLayout out_layout;
			av_channel_layout_default(&out_layout, out_channels);
			if (swr_alloc_set_opts2(&ctx->swr_ctx, &out_layout,
						AV_SAMPLE_FMT_FLT, out_rate,
						&frame->ch_layout,
						frame->format,
						frame->sample_rate, 0,
						NULL) < 0 ||
			    !ctx->swr_ctx) {
				av_channel_layout_uninit(&out_layout);
				return;
			}
			if (swr_init(ctx->swr_ctx) < 0) {
				swr_free(&ctx->swr_ctx);
				av_channel_layout_uninit(&out_layout);
				return;
			}
			av_channel_layout_uninit(&out_layout);
			ctx->swr_in_rate = frame->sample_rate;
			ctx->swr_in_channels = frame->ch_layout.nb_channels;
			ctx->swr_in_format = frame->format;
		}

		int soft_comp_samples = audio_soft_compensation_samples(
			ctx, duration, out_rate, frame->nb_samples);
		if (soft_comp_samples != 0) {
			swr_set_compensation(ctx->swr_ctx, soft_comp_samples,
					     frame->nb_samples);
		}

		int max_out = swr_get_out_samples(ctx->swr_ctx,
						  frame->nb_samples);
		if (soft_comp_samples < 0)
			soft_comp_samples = -soft_comp_samples;
		max_out += soft_comp_samples + 32;
		size_t need = (size_t)max_out * out_channels * bytes_per_sample;
		interleaved = ensure_scratch(&ctx->audio_resample_scratch,
					     &ctx->audio_resample_scratch_capacity,
					     need);
		if (!interleaved)
			return;

		out_samples = swr_convert(ctx->swr_ctx, &interleaved, max_out,
					  (const uint8_t **)frame->extended_data,
					  frame->nb_samples);
		if (out_samples <= 0)
			return;
	} else {
		interleaved = frame->data[0];
	}

	size_t data_bytes =
		(size_t)out_samples * out_channels * bytes_per_sample;
	if (inserted_silence)
		audio_apply_fade_in(interleaved, out_samples, out_channels,
				    out_rate);
	if (os_atomic_load_bool(&ctx->config.wait_for_keyframe) &&
	    ctx->video_stream_idx >= 0 && !ctx->first_keyframe_received) {
		return;
	}

	int64_t frame_pts_ns = av_rescale_q(
		corrected_pts,
		(AVRational){ctx->pts_state.tb_num, ctx->pts_state.tb_den},
		(AVRational){1, 1000000000});
	audio_buffer_write_pts(&ctx->audio_buf, interleaved, data_bytes,
			       frame_pts_ns);
	remember_last_sample(interleaved, out_samples, out_channels,
			     ctx->audio_last_sample,
			     &ctx->audio_last_sample_channels,
			     &ctx->audio_last_sample_valid);

	irl_mutex_lock(&ctx->audio_state_lock);
	ctx->latest_audio_stream_pts_ns = frame_pts_ns;
	ctx->decoded_frame_samples = out_samples;
	irl_mutex_unlock(&ctx->audio_state_lock);
}
