/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * audio-buffer.h — Codec-agnostic jitter buffer
 *
 * Ring buffer sized in milliseconds (not frames).  Adapts automatically
 * to any sample rate, channel count, and sample format.
 */

#pragma once

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>
#include <util/threading.h> /* OBS cross-platform pthread wrappers */

/* ── Audio buffer ─────────────────────────────────────────── */

struct audio_buffer {
	uint8_t *data;       /* ring-buffer storage */
	size_t capacity;     /* total capacity in bytes */
	size_t head;         /* write position */
	size_t tail;         /* read position */
	size_t fill;         /* current fill in bytes */

	/* Stream format (set once per session) */
	int sample_rate;
	int channels;
	int bytes_per_sample; /* bytes per single sample (e.g. 4 for float32) */
	int frame_size;       /* bytes_per_sample * channels */

	/* Configuration (milliseconds) */
	int target_ms;
	int min_ms;
	int max_ms;

	pthread_mutex_t lock;
};

/* ── API ──────────────────────────────────────────────────── */

/**
 * Initialise the buffer.  Allocates storage for `max_ms` at the given format.
 * Call after the first decoded audio frame reveals the stream parameters.
 */
void audio_buffer_init(struct audio_buffer *buf, int sample_rate, int channels,
		       int bytes_per_sample, int target_ms, int min_ms,
		       int max_ms);

/** Release all resources. */
void audio_buffer_free(struct audio_buffer *buf);

/** Reset buffer to empty without freeing (e.g. on reconnect). */
void audio_buffer_flush(struct audio_buffer *buf);

/**
 * Write decoded PCM samples into the buffer.
 * Returns the number of bytes actually written (may be less if buffer is full).
 */
size_t audio_buffer_write(struct audio_buffer *buf, const uint8_t *samples,
			  size_t bytes);

/**
 * Read up to `max_bytes` of PCM from the buffer.
 * Returns the number of bytes read.
 */
size_t audio_buffer_read(struct audio_buffer *buf, uint8_t *out,
			 size_t max_bytes);

/** Current fill level in milliseconds. */
int audio_buffer_fill_ms(const struct audio_buffer *buf);

/** True when the buffer has at least `min_ms` worth of data. */
bool audio_buffer_ready(const struct audio_buffer *buf);

/**
 * Read up to `max_bytes` and apply a linear fade-out (1.0 → 0.0).
 * Used on disconnect to avoid audio clicks/pops.
 * Assumes float sample format.
 */
size_t audio_buffer_read_with_fade_out(struct audio_buffer *buf, uint8_t *out,
				       size_t max_bytes);

/**
 * Calculate bytes needed for a given duration at current format.
 */
size_t audio_buffer_ms_to_bytes(const struct audio_buffer *buf, int ms);
