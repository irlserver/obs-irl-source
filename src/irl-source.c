/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * irl-source.c — Source lifecycle: create, destroy, update, tick
 */

#include <stdlib.h>

#include "../include/irl-source.h"

/* ── Helpers ──────────────────────────────────────────────── */

static void config_free(struct irl_config *cfg)
{
	if (cfg->url) {
		bfree(cfg->url);
		cfg->url = NULL;
	}
	if (cfg->ffmpeg_options) {
		bfree(cfg->ffmpeg_options);
		cfg->ffmpeg_options = NULL;
	}
}

static void config_load(struct irl_config *cfg, obs_data_t *settings)
{
	config_free(cfg);

	const char *url = obs_data_get_string(settings, "url");
	cfg->url = url && *url ? bstrdup(url) : NULL;

	cfg->reconnect_delay =
		(int)obs_data_get_int(settings, "reconnect_delay");
	cfg->network_buffer_mb =
		(int)obs_data_get_int(settings, "network_buffer_mb");

	cfg->buffer_target_ms =
		(int)obs_data_get_int(settings, "buffer_target_ms");
	cfg->buffer_min_ms = (int)obs_data_get_int(settings, "buffer_min_ms");
	cfg->buffer_max_ms = (int)obs_data_get_int(settings, "buffer_max_ms");
	cfg->adaptive_speed = obs_data_get_bool(settings, "adaptive_speed");
	cfg->speed_min =
		(float)obs_data_get_double(settings, "speed_min");
	cfg->speed_max =
		(float)obs_data_get_double(settings, "speed_max");

	cfg->small_gap_ms = (int)obs_data_get_int(settings, "small_gap_ms");
	cfg->large_gap_ms = (int)obs_data_get_int(settings, "large_gap_ms");

	const char *ff = obs_data_get_string(settings, "ffmpeg_options");
	cfg->ffmpeg_options = ff && *ff ? bstrdup(ff) : NULL;

	cfg->hw_decode = (int)obs_data_get_int(settings, "hw_decode");
	cfg->wait_for_keyframe =
		obs_data_get_bool(settings, "wait_for_keyframe");
}

/* ── Stats proc_handler callback ──────────────────────────── */

static void irl_source_get_stats(void *data, calldata_t *cd)
{
	struct irl_source *ctx = data;
	calldata_set_int(cd, "buffer_fill_ms",
			 audio_buffer_fill_ms(&ctx->audio_buf));
	calldata_set_float(cd, "current_speed",
			   (double)ctx->current_speed);
	calldata_set_bool(cd, "reconnecting", ctx->reconnecting);
	calldata_set_int(cd, "total_audio_frames",
			 (long long)ctx->total_audio_frames);
	calldata_set_int(cd, "total_video_frames",
			 (long long)ctx->total_video_frames);
	calldata_set_int(cd, "pts_repairs",
			 (long long)ctx->pts_repairs);
	calldata_set_int(cd, "silence_insertions",
			 (long long)ctx->silence_insertions);
}

/* ── Lifecycle ────────────────────────────────────────────── */

const char *irl_source_get_name(void *unused)
{
	UNUSED_PARAMETER(unused);
	return obs_module_text("IRL Source (irlserver.com)");
}

void *irl_source_create(obs_data_t *settings, obs_source_t *source)
{
	struct irl_source *ctx = bzalloc(sizeof(*ctx));
	ctx->source = source;
	ctx->current_speed = 1.0f;

	config_load(&ctx->config, settings);

	/* Register stats proc_handler so scripts/overlays can query state */
	proc_handler_t *ph = obs_source_get_proc_handler(source);
	proc_handler_add(
		ph,
		"void get_stats(out int buffer_fill_ms, "
		"out float current_speed, out bool reconnecting, "
		"out int total_audio_frames, out int total_video_frames, "
		"out int pts_repairs, out int silence_insertions)",
		irl_source_get_stats, ctx);

	/* Start the receiver thread if we have a URL */
	if (ctx->config.url) {
		blog(LOG_INFO, "[irl-source] Created with URL: %s",
		     ctx->config.url);
		ctx->thread_active = true;
		pthread_create(&ctx->receiver_thread, NULL,
			       irl_receiver_thread, ctx);
	} else {
		blog(LOG_INFO, "[irl-source] Created with no URL configured");
	}

	return ctx;
}

void irl_source_destroy(void *data)
{
	struct irl_source *ctx = data;
	if (!ctx)
		return;

	irl_receiver_stop(ctx);
	audio_buffer_free(&ctx->audio_buf);

	if (ctx->swr_ctx)
		swr_free(&ctx->swr_ctx);
	if (ctx->sws_ctx)
		sws_freeContext(ctx->sws_ctx);
	if (ctx->hw_device_ctx)
		av_buffer_unref(&ctx->hw_device_ctx);

	if (ctx->pre_kf_audio_data)
		bfree(ctx->pre_kf_audio_data);

	config_free(&ctx->config);
	bfree(ctx);
}

void irl_source_update(void *data, obs_data_t *settings)
{
	struct irl_source *ctx = data;

	/* Stop existing receiver */
	irl_receiver_stop(ctx);

	/* Reload config */
	config_load(&ctx->config, settings);

	/* Restart if we have a URL */
	if (ctx->config.url) {
		/* Reset keyframe gate and buffers */
		ctx->first_keyframe_received = false;
		ctx->pre_kf_audio_size = 0;
		audio_buffer_flush(&ctx->audio_buf);
		pts_repair_reset(&ctx->pts_state);
		ctx->current_speed = 1.0f;
		ctx->audio_output_pts_init = false;

		ctx->thread_active = true;
		pthread_create(&ctx->receiver_thread, NULL,
			       irl_receiver_thread, ctx);
	}
}

void irl_source_tick(void *data, float seconds)
{
	UNUSED_PARAMETER(seconds);
	struct irl_source *ctx = data;

	/* Reconnection is handled inside the receiver thread via
	 * sleep + retry, so there's nothing to poll here. */
	UNUSED_PARAMETER(ctx);
}
