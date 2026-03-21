/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * pts-repair.c — PTS discontinuity detection and repair
 *
 * Three gap ranges:
 *   small  (<  small_gap_ms):  interpolate from previous PTS
 *   medium (>= small_gap_ms, < large_gap_ms): insert silence
 *   large  (>= large_gap_ms): full timestamp reset
 */

#include "../include/pts-repair.h"

/* ── Helpers ──────────────────────────────────────────────── */

static int ts_to_ms(const struct pts_repair *r, int64_t ts)
{
	if (r->tb_den == 0)
		return 0;
	return (int)(ts * 1000 * r->tb_num / r->tb_den);
}

/* ── Public API ───────────────────────────────────────────── */

void pts_repair_init(struct pts_repair *r, int small_gap_ms, int large_gap_ms,
		     int tb_num, int tb_den)
{
	r->last_pts = 0;
	r->last_duration = 0;
	r->tb_num = tb_num;
	r->tb_den = tb_den;
	r->small_gap_ms = small_gap_ms;
	r->large_gap_ms = large_gap_ms;
	r->initialised = false;
}

void pts_repair_reset(struct pts_repair *r)
{
	r->last_pts = 0;
	r->last_duration = 0;
	r->initialised = false;
}

enum pts_action pts_repair_evaluate(struct pts_repair *r, int64_t pts,
				    int64_t duration,
				    int64_t *corrected_pts,
				    int *silence_ms)
{
	*silence_ms = 0;

	/* First frame — just record and pass through */
	if (!r->initialised) {
		r->last_pts = pts;
		r->last_duration = duration > 0 ? duration : 1;
		r->initialised = true;
		*corrected_pts = pts;
		return PTS_ACTION_PASS;
	}

	/* Expected PTS = last_pts + last_duration */
	int64_t expected = r->last_pts + r->last_duration;
	int64_t gap = pts - expected;

	/* Convert gap to milliseconds for threshold comparison */
	int gap_ms = ts_to_ms(r, gap >= 0 ? gap : -gap);
	bool is_backward = gap < 0;

	/* Backward jump or tiny gap — likely reorder, pass through */
	if (is_backward || gap_ms < 1) {
		r->last_pts = pts;
		r->last_duration = duration > 0 ? duration : r->last_duration;
		*corrected_pts = pts;
		return PTS_ACTION_PASS;
	}

	enum pts_action action;

	if (gap_ms < r->small_gap_ms) {
		/* Small gap — interpolate: use expected PTS */
		*corrected_pts = expected;
		action = PTS_ACTION_INTERPOLATE;
	} else if (gap_ms < r->large_gap_ms) {
		/* Medium gap — insert silence, then use original PTS */
		*corrected_pts = pts;
		*silence_ms = gap_ms;
		action = PTS_ACTION_SILENCE;
	} else {
		/* Large gap — full reset */
		*corrected_pts = pts;
		action = PTS_ACTION_RESET;
	}

	r->last_pts = *corrected_pts;
	r->last_duration = duration > 0 ? duration : r->last_duration;

	return action;
}
