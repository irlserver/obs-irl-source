/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * pts-repair.h — PTS discontinuity detection and repair
 *
 * Handles three gap ranges:
 *   small  (<  small_gap_ms):  interpolate from previous frame
 *   medium (>= small_gap_ms, < large_gap_ms): insert silence
 *   large  (>= large_gap_ms): full timestamp reset
 */

#pragma once

#include <stdint.h>
#include <stdbool.h>

/* ── Repair action returned to the caller ─────────────────── */

enum pts_action {
	PTS_ACTION_PASS,        /* PTS is fine, use as-is */
	PTS_ACTION_INTERPOLATE, /* PTS repaired by interpolation */
	PTS_ACTION_SILENCE,     /* Silence should be inserted before this frame */
	PTS_ACTION_RESET,       /* Full timestamp reset (large gap) */
};

/* ── Per-stream repair state ──────────────────────────────── */

struct pts_repair {
	/* Last known-good PTS and its duration (both in stream time-base) */
	int64_t last_pts;
	int64_t last_duration;

	/* Stream time-base (e.g. {1, 48000} for 48 kHz audio) */
	int tb_num;
	int tb_den;

	/* Configurable thresholds (milliseconds) */
	int small_gap_ms;
	int large_gap_ms;

	/* Whether we have a valid reference PTS yet */
	bool initialised;
};

/* ── API ──────────────────────────────────────────────────── */

/**
 * Initialise repair state.  Call once per stream / reconnect.
 */
void pts_repair_init(struct pts_repair *r, int small_gap_ms, int large_gap_ms,
		     int tb_num, int tb_den);

/** Reset state (e.g. after reconnect). */
void pts_repair_reset(struct pts_repair *r);

/**
 * Evaluate a frame's PTS and decide what to do.
 *
 * @param pts       the frame's PTS in stream time-base units
 * @param duration  the frame's duration in stream time-base units
 * @param[out] corrected_pts  the PTS to actually use (may be adjusted)
 * @param[out] silence_ms     if action == PTS_ACTION_SILENCE, the ms of silence to insert
 *
 * Returns the recommended action.
 */
enum pts_action pts_repair_evaluate(struct pts_repair *r, int64_t pts,
				    int64_t duration,
				    int64_t *corrected_pts,
				    int *silence_ms);
