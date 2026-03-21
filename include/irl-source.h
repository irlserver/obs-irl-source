/*
 * obs-irl-source: IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 *
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * Codec/protocol/bitrate-agnostic live source with jitter buffering,
 * PTS repair, adaptive playback speed, and first-keyframe gating.
 */

#pragma once

#define OBS_IRL_SOURCE_VERSION "0.2.1"

#include <obs-module.h>
#include <util/platform.h>
#include <libavformat/avformat.h>
#include <libavcodec/avcodec.h>
#include <libswresample/swresample.h>
#include <libswscale/swscale.h>
#include <libavutil/time.h>
#include <libavutil/hwcontext.h>

#include "audio-buffer.h"
#include "pts-repair.h"

/* ── Forward declarations ─────────────────────────────────── */

struct irl_source;

/* ── Configuration defaults ───────────────────────────────── */

#define IRL_DEFAULT_RECONNECT_DELAY 2
#define IRL_DEFAULT_NETWORK_BUFFER_MB 2
#define IRL_DEFAULT_BUFFER_TARGET_MS 80
#define IRL_DEFAULT_BUFFER_MIN_MS 40
#define IRL_DEFAULT_BUFFER_MAX_MS 200
#define IRL_DEFAULT_ADAPTIVE_SPEED true
#define IRL_DEFAULT_SPEED_MIN 0.95f
#define IRL_DEFAULT_SPEED_MAX 1.05f
#define IRL_DEFAULT_SMALL_GAP_MS 70
#define IRL_DEFAULT_LARGE_GAP_MS 2000
#define IRL_DEFAULT_HW_DECODE 0 /* 0 = auto, 1 = off */
#define IRL_DEFAULT_WAIT_KEYFRAME true

/* Audio fade duration on disconnect/reconnect (avoids clicks/pops) */
#define IRL_FADE_DURATION_MS 50

/* ── Source configuration ─────────────────────────────────── */

struct irl_config {
	/* General */
	char *url;
	int reconnect_delay;
	int network_buffer_mb;

	/* Audio buffer */
	int buffer_target_ms;
	int buffer_min_ms;
	int buffer_max_ms;
	bool adaptive_speed;
	float speed_min;
	float speed_max;

	/* PTS repair */
	int small_gap_ms;
	int large_gap_ms;

	/* Advanced */
	char *ffmpeg_options;
	int hw_decode;
	bool wait_for_keyframe;
};

/* ── Main source context ──────────────────────────────────── */

struct irl_source {
	obs_source_t *source;
	struct irl_config config;

	/* Receiver / demux thread */
	pthread_t receiver_thread;
	volatile bool thread_active;
	volatile bool reconnecting;

	/* FFmpeg state (owned by receiver thread) */
	AVFormatContext *fmt_ctx;
	AVCodecContext *audio_dec_ctx;
	AVCodecContext *video_dec_ctx;
	AVBufferRef *hw_device_ctx;
	int audio_stream_idx;
	int video_stream_idx;
	bool using_hw_decode;

	/* Resampler (planar → interleaved float) */
	SwrContext *swr_ctx;

	/* Video scaler (for format conversion to OBS) */
	struct SwsContext *sws_ctx;
	int sws_src_w;
	int sws_src_h;
	enum AVPixelFormat sws_src_fmt;

	/* Video timestamp sync (anchors stream PTS to system clock) */
	bool video_ts_init;
	uint64_t video_sys_base;  /* os_gettime_ns() at first frame */
	int64_t video_pts_base;   /* stream PTS at first frame (in ns) */

	/* Audio jitter buffer */
	struct audio_buffer audio_buf;

	/* PTS repair state */
	struct pts_repair pts_state;

	/* Adaptive speed controller */
	float current_speed;
	uint64_t last_speed_adjust_time;

	/* Running output PTS: tracks the actual playback position in the
	 * jitter buffer rather than using the latest decoded frame's PTS.
	 * Without this, the buffer decouples data from timestamps, causing
	 * OBS to see gaps and produce garbled/robotic audio. */
	int64_t audio_output_pts_ns;
	bool audio_output_pts_init;

	/* Keyframe gate */
	bool first_keyframe_received;

	/* Pre-keyframe audio staging (circular buffer of decoded frames) */
	uint8_t *pre_kf_audio_data;
	size_t pre_kf_audio_size;
	size_t pre_kf_audio_capacity;

	/* Audio fade state */
	bool fade_in_pending;
	int fade_in_frames_remaining;

	/* Resolution tracking (for mid-stream changes) */
	int last_video_width;
	int last_video_height;

	/* Statistics */
	uint64_t total_audio_frames;
	uint64_t total_video_frames;
	uint64_t pts_repairs;
	uint64_t silence_insertions;
	uint64_t last_stats_time;
};

/* ── Lifecycle (irl-source.c) ─────────────────────────────── */

void *irl_source_create(obs_data_t *settings, obs_source_t *source);
void irl_source_destroy(void *data);
void irl_source_update(void *data, obs_data_t *settings);
void irl_source_tick(void *data, float seconds);
const char *irl_source_get_name(void *unused);

/* ── Settings (settings.c) ────────────────────────────────── */

obs_properties_t *irl_source_get_properties(void *data);
void irl_source_get_defaults(obs_data_t *settings);

/* ── Receiver thread (receiver.c) ─────────────────────────── */

void *irl_receiver_thread(void *data);
void irl_receiver_stop(struct irl_source *ctx);

/* ── Audio buffer (audio-buffer.c) ────────────────────────── */
/* See audio-buffer.h */

/* ── Adaptive speed (audio-speed.c) ───────────────────────── */

void irl_speed_apply(struct irl_source *ctx, struct obs_source_audio *audio);

/* ── Video handler (video-handler.c) ──────────────────────── */

void irl_video_output_frame(struct irl_source *ctx, AVFrame *frame);
bool irl_video_is_keyframe(const AVFrame *frame);

/* ── PTS repair (pts-repair.c) ────────────────────────────── */
/* See pts-repair.h */
