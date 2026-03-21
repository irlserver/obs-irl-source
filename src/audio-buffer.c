/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * audio-buffer.c — Codec-agnostic jitter buffer
 *
 * Ring buffer sized in milliseconds.  Works with any sample rate,
 * channel count, and sample format.
 */

#include <stdlib.h>
#include <string.h>

#include "../include/audio-buffer.h"

/* ── Helpers ──────────────────────────────────────────────── */

static size_t ms_to_bytes(const struct audio_buffer *buf, int ms)
{
	if (buf->frame_size == 0 || buf->sample_rate == 0)
		return 0;
	return (size_t)((int64_t)ms * buf->sample_rate / 1000) *
	       buf->frame_size;
}

/* ── Public API ───────────────────────────────────────────── */

void audio_buffer_init(struct audio_buffer *buf, int sample_rate, int channels,
		       int bytes_per_sample, int target_ms, int min_ms,
		       int max_ms)
{
	memset(buf, 0, sizeof(*buf));

	buf->sample_rate = sample_rate;
	buf->channels = channels;
	buf->bytes_per_sample = bytes_per_sample;
	buf->frame_size = bytes_per_sample * channels;
	buf->target_ms = target_ms;
	buf->min_ms = min_ms;
	buf->max_ms = max_ms;

	/* Allocate for max_ms plus some headroom */
	buf->capacity = ms_to_bytes(buf, max_ms * 2);
	if (buf->capacity == 0)
		buf->capacity = 65536; /* fallback */
	buf->data = calloc(1, buf->capacity);

	pthread_mutex_init(&buf->lock, NULL);
}

void audio_buffer_free(struct audio_buffer *buf)
{
	if (!buf->data)
		return;

	pthread_mutex_destroy(&buf->lock);
	free(buf->data);
	memset(buf, 0, sizeof(*buf));
}

void audio_buffer_flush(struct audio_buffer *buf)
{
	if (!buf->data)
		return;

	pthread_mutex_lock(&buf->lock);
	buf->head = 0;
	buf->tail = 0;
	buf->fill = 0;
	pthread_mutex_unlock(&buf->lock);
}

size_t audio_buffer_write(struct audio_buffer *buf, const uint8_t *samples,
			  size_t bytes)
{
	if (!buf->data || bytes == 0)
		return 0;

	pthread_mutex_lock(&buf->lock);

	size_t avail = buf->capacity - buf->fill;
	size_t to_write = bytes < avail ? bytes : avail;

	if (to_write == 0) {
		pthread_mutex_unlock(&buf->lock);
		return 0;
	}

	/* Handle wrap-around */
	size_t first_chunk = buf->capacity - buf->head;
	if (first_chunk >= to_write) {
		memcpy(buf->data + buf->head, samples, to_write);
	} else {
		memcpy(buf->data + buf->head, samples, first_chunk);
		memcpy(buf->data, samples + first_chunk,
		       to_write - first_chunk);
	}

	buf->head = (buf->head + to_write) % buf->capacity;
	buf->fill += to_write;

	pthread_mutex_unlock(&buf->lock);
	return to_write;
}

size_t audio_buffer_read(struct audio_buffer *buf, uint8_t *out,
			 size_t max_bytes)
{
	if (!buf->data || max_bytes == 0)
		return 0;

	pthread_mutex_lock(&buf->lock);

	size_t to_read = max_bytes < buf->fill ? max_bytes : buf->fill;

	if (to_read == 0) {
		pthread_mutex_unlock(&buf->lock);
		return 0;
	}

	/* Handle wrap-around */
	size_t first_chunk = buf->capacity - buf->tail;
	if (first_chunk >= to_read) {
		memcpy(out, buf->data + buf->tail, to_read);
	} else {
		memcpy(out, buf->data + buf->tail, first_chunk);
		memcpy(out + first_chunk, buf->data,
		       to_read - first_chunk);
	}

	buf->tail = (buf->tail + to_read) % buf->capacity;
	buf->fill -= to_read;

	pthread_mutex_unlock(&buf->lock);
	return to_read;
}

int audio_buffer_fill_ms(const struct audio_buffer *buf)
{
	if (buf->frame_size == 0 || buf->sample_rate == 0)
		return 0;
	size_t samples = buf->fill / buf->frame_size;
	return (int)(samples * 1000 / buf->sample_rate);
}

bool audio_buffer_ready(const struct audio_buffer *buf)
{
	return audio_buffer_fill_ms(buf) >= buf->min_ms;
}

size_t audio_buffer_read_with_fade_out(struct audio_buffer *buf, uint8_t *out,
				       size_t max_bytes)
{
	size_t got = audio_buffer_read(buf, out, max_bytes);
	if (got == 0 || buf->frame_size == 0)
		return got;

	/* Apply linear gain ramp 1.0 → 0.0 over the entire read */
	int total_frames = (int)(got / buf->frame_size);
	if (total_frames <= 0)
		return got;

	float *samples = (float *)out;
	for (int f = 0; f < total_frames; f++) {
		float gain = 1.0f - (float)f / (float)total_frames;
		for (int ch = 0; ch < buf->channels; ch++)
			samples[f * buf->channels + ch] *= gain;
	}

	return got;
}

size_t audio_buffer_ms_to_bytes(const struct audio_buffer *buf, int ms)
{
	return ms_to_bytes(buf, ms);
}
