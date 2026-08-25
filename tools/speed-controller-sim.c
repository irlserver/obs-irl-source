/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * speed-controller-sim.c — offline closed-loop simulation of the audio
 * speed controller (ramp + trim) in src/receiver-audio.c.
 *
 * Not built, not a CI target, not linked to anything. See tools/README.md.
 *
 * IMPORTANT: the buffer level here is CONTINUOUS. On real audio it moves in
 * whole decoded chunks (21.3ms for 1024-sample AAC), so the sub-millisecond
 * steady-state errors this prints are a property of the model and not of the
 * plugin — see docs/audio-timing-pitfalls.md. What the numbers below are good
 * for is the shape of the response: settling versus limit-cycling, and
 * whether a transient leaks into the trim.
 *
 * IMPORTANT: the controller below is REPLICATED from receiver-audio.c, not
 * linked, because the real one reads struct irl_source. The constants and
 * both update rules are copied verbatim; if you change them there, change
 * them here and re-run, or this file quietly starts simulating a controller
 * that no longer exists.
 *
 *   cc -O1 -o /tmp/sim tools/speed-controller-sim.c -lm && /tmp/sim
 */

#include <math.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdio.h>

/* ── Copied from src/receiver-audio.c ─────────────────────── */

#define AUDIO_SPEED_MIN 0.98f
#define AUDIO_SPEED_MAX 1.05f
#define AUDIO_SPEED_DEADBAND_MS 20
#define AUDIO_SPEED_SMOOTHING 0.05f
#define AUDIO_SPEED_DEADBAND_SLOPE 0.002f
#define AUDIO_SPEED_TRIM_GAIN 0.0025
#define AUDIO_SPEED_TRIM_MAX 0.01f
#define AUDIO_SPEED_TRIM_ERR_WINDOW_MS (3 * AUDIO_SPEED_DEADBAND_MS)

/* ── Copied from src/irl-source.c (buffer sizing) ─────────── */

#define IRL_BUFFER_MIN_DIVISOR 2
#define IRL_BUFFER_MIN_FLOOR_MS 20
#define IRL_BUFFER_MAX_EXTRA_MS 200

/* One pump cycle is one decoded audio chunk: 1024 frames at 48kHz. */
#define DT 0.0213

static int target_ms = 120;

static int min_ms(void)
{
	int m = target_ms / IRL_BUFFER_MIN_DIVISOR;
	return m < IRL_BUFFER_MIN_FLOOR_MS ? IRL_BUFFER_MIN_FLOOR_MS : m;
}

static int max_ms(void)
{
	return target_ms + IRL_BUFFER_MAX_EXTRA_MS;
}

struct loop {
	float current_speed;
	float trim;
	double fill_ms;
	bool trim_enabled;
};

static float ramp_for(int fill_ms)
{
	int err = fill_ms - target_ms;
	int low = target_ms - AUDIO_SPEED_DEADBAND_MS;
	int high = target_ms + AUDIO_SPEED_DEADBAND_MS;

	if (err > AUDIO_SPEED_DEADBAND_MS) {
		int span = max_ms() - high;
		float t = span > 0 ? (float)(fill_ms - high) / (float)span
				   : 1.0f;
		if (t > 1.0f)
			t = 1.0f;
		float edge = 1.0f + AUDIO_SPEED_DEADBAND_SLOPE;
		return edge + (AUDIO_SPEED_MAX - edge) * t;
	}
	if (err < -AUDIO_SPEED_DEADBAND_MS) {
		int span = low - min_ms();
		float t = span > 0 ? (float)(low - fill_ms) / (float)span
				   : 1.0f;
		if (t > 1.0f)
			t = 1.0f;
		float edge = 1.0f - AUDIO_SPEED_DEADBAND_SLOPE;
		return edge - (edge - AUDIO_SPEED_MIN) * t;
	}
	return 1.0f + AUDIO_SPEED_DEADBAND_SLOPE * (float)err /
			      (float)AUDIO_SPEED_DEADBAND_MS;
}

static void update_trim(struct loop *L, int fill_ms, float ramp, bool recovery)
{
	if (!L->trim_enabled || recovery)
		return;

	int err_ms = fill_ms - target_ms;
	if (err_ms > AUDIO_SPEED_TRIM_ERR_WINDOW_MS ||
	    err_ms < -AUDIO_SPEED_TRIM_ERR_WINDOW_MS)
		return;

	double step = AUDIO_SPEED_TRIM_GAIN * ((double)err_ms / 1000.0) * DT;

	bool pinned_high = ramp >= AUDIO_SPEED_MAX - 0.0005f;
	bool pinned_low = ramp <= AUDIO_SPEED_MIN + 0.0005f;
	if ((pinned_high && step > 0.0) || (pinned_low && step < 0.0))
		return;

	double next = (double)L->trim + step;
	if (next > AUDIO_SPEED_TRIM_MAX)
		next = AUDIO_SPEED_TRIM_MAX;
	else if (next < -(double)AUDIO_SPEED_TRIM_MAX)
		next = -(double)AUDIO_SPEED_TRIM_MAX;
	L->trim = (float)next;
}

static void step_loop(struct loop *L, double sender_rate, bool recovery)
{
	int fill_ms = (int)L->fill_ms;
	float ramp = ramp_for(fill_ms);

	update_trim(L, fill_ms, ramp, recovery);

	float target = ramp + L->trim;
	if (L->current_speed <= 0.0f)
		L->current_speed = 1.0f;
	L->current_speed += (target - L->current_speed) * AUDIO_SPEED_SMOOTHING;
	if (L->current_speed < AUDIO_SPEED_MIN)
		L->current_speed = AUDIO_SPEED_MIN;
	if (L->current_speed > AUDIO_SPEED_MAX)
		L->current_speed = AUDIO_SPEED_MAX;

	/* Buffer: media arrives at the sender's rate, leaves at playback
	 * speed. Both in seconds of media per second of wall clock. */
	L->fill_ms += (sender_rate - (double)L->current_speed) * DT * 1000.0;
	if (L->fill_ms < 0.0)
		L->fill_ms = 0.0;
}

static void run_secs(struct loop *L, double rate, double secs, bool recovery)
{
	int n = (int)(secs / DT);
	for (int i = 0; i < n; i++)
		step_loop(L, rate, recovery);
}

static void steady(const char *what, double rate, bool trim_on)
{
	struct loop L = {1.0f, 0.0f, (double)target_ms, trim_on};
	run_secs(&L, rate, 300.0, false);
	printf("  %-24s %-8s fill %7.1fms (err %+7.1f)  speed %.4f  trim %+.3f%%\n",
	       what, trim_on ? "trim on" : "trim off", L.fill_ms,
	       L.fill_ms - target_ms, L.current_speed,
	       (double)L.trim * 100.0);
}

/* A 3s delivery outage followed by the whole backlog landing at once.
 * The trim must learn nothing from this: it is a network event, not a
 * clock. This is the failure mode that makes naive integrators unusable. */
static void stall(bool trim_on)
{
	struct loop L = {1.0f, 0.0f, (double)target_ms, trim_on};
	run_secs(&L, 1.0, 120.0, false);
	float before = L.trim;

	run_secs(&L, 0.0, 3.0, true); /* nothing arriving; concealment */
	L.fill_ms += 3000.0;          /* backlog lands */
	run_secs(&L, 1.0, 300.0, false);

	printf("  %-24s %-8s trim %+.4f%% -> %+.4f%%  fill %.1fms  %s\n",
	       "3s stall + 3s backlog", trim_on ? "trim on" : "trim off",
	       (double)before * 100.0, (double)L.trim * 100.0, L.fill_ms,
	       fabs((double)L.trim) < 0.001 ? "ok (learned nothing)"
					    : "LEAKED");
}

/* Replicated from apply_output_speed() in receiver-audio.c: the whole-sample
 * chunk request plus the carried fractional remainder. Returns the effective
 * speed actually realised over `chunks` chunks of `n` frames. */
static double applied_speed(float speed, int n, int chunks)
{
	long long out = 0;
	double frac = 0.0;

	for (int i = 0; i < chunks; i++) {
		double want = (double)n / (double)speed + frac;
		int desired = (int)(want + 0.5);
		if (desired < 1)
			desired = 1;
		double carry = want - (double)desired;
		if (carry > 1.0)
			carry = 1.0;
		else if (carry < -1.0)
			carry = -1.0;
		frac = carry;
		out += desired;
	}
	return (double)n * chunks / (double)out;
}

int main(void)
{
	const double rates[] = {1.0, 1.0001, 1.003, 0.997, 1.06};
	const char *names[] = {"exact realtime", "crystal drift +0.01%",
			       "sender fast +0.3%", "sender slow -0.3%",
			       "unwinnable sender +6%"};

	printf("steady state after 300s (target %dms)\n", target_ms);
	for (int i = 0; i < 5; i++) {
		steady(names[i], rates[i], false);
		steady(names[i], rates[i], true);
	}

	printf("convergence, sender +0.3%%, trim on\n");
	struct loop L = {1.0f, 0.0f, (double)target_ms, true};
	for (int t = 30; t <= 300; t += 30) {
		run_secs(&L, 1.003, 30.0, false);
		printf("    t=%3ds  fill %6.1fms  trim %+.4f%%  speed %.4f\n",
		       t, L.fill_ms, (double)L.trim * 100.0, L.current_speed);
	}

	printf("anti-windup\n");
	stall(false);
	stall(true);

	/* The trim must not oscillate at any supported buffer target: the
	 * ramp's slopes change with min_ms/max_ms, and with them the
	 * damping the trim relies on. */
	printf("target sweep, sender +0.3%%, trim on, 400s\n");
	const int targets[] = {40, 80, 120, 300, 500};
	int fails = 0;
	for (int i = 0; i < 5; i++) {
		target_ms = targets[i];
		struct loop S = {1.0f, 0.0f, (double)target_ms, true};
		run_secs(&S, 1.003, 300.0, false);

		double peak = S.fill_ms, trough = S.fill_ms;
		for (int k = 0; k < (int)(100.0 / DT); k++) {
			step_loop(&S, 1.003, false);
			if (S.fill_ms > peak)
				peak = S.fill_ms;
			if (S.fill_ms < trough)
				trough = S.fill_ms;
		}
		bool settled = (peak - trough) < 5.0;
		if (!settled)
			fails++;
		printf("    target %3dms  fill %6.1fms (err %+5.1f)  trim %+.4f%%  swing %.1fms  %s\n",
		       target_ms, S.fill_ms, S.fill_ms - target_ms,
		       (double)S.trim * 100.0, peak - trough,
		       settled ? "settled" : "OSCILLATING");
	}

	/* How faithfully the requested speed is actually applied. The
	 * resampler is driven in whole samples per chunk, so rounding each
	 * chunk independently quantises the applied speed to multiples of
	 * 1/in_frames (~0.1% at 1024). That is the region the deadband slope
	 * and the trim both live in, so the request was either discarded or
	 * doubled; apply_output_speed() carries the fractional remainder to
	 * fix it. Replicated from receiver-audio.c, same caveat as above. */
	printf("applied vs requested speed (1024-frame chunks)\n");
	const float req[] = {1.0f,    1.0002f, 1.0005f, 1.001f,
			     1.002f,  1.005f,  1.01f,   0.9995f,
			     0.998f,  0.99f};
	for (size_t i = 0; i < sizeof(req) / sizeof(req[0]); i++) {
		double got = applied_speed(req[i], 1024, 4000);
		double err = (got - (double)req[i]) * 100.0;
		bool bad = fabs(err) > 0.005;
		if (bad)
			fails++;
		printf("    req %+7.4f%%  applied %+7.4f%%  err %+7.4f%%  %s\n",
		       ((double)req[i] - 1.0) * 100.0, (got - 1.0) * 100.0,
		       err, bad ? "FAIL" : "ok");
	}

	printf("%s\n", fails ? "FAILURES" : "all settled");
	return fails != 0;
}
