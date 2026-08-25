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
#include <string.h>

#include "../include/irl-source.h"
#include "receiver-internal.h"

/* ── Helpers ──────────────────────────────────────────────── */

void irl_log_input_url(const char *action, const char *url)
{
	char protocol[32] = {0};
	char hostname[256] = {0};
	int port = -1;

	/* Paths, userinfo, query parameters, and fragments can all contain
	 * credentials. av_url_split lets the log retain the useful endpoint
	 * identity without ever copying those components. */
	av_url_split(protocol, sizeof(protocol), NULL, 0, hostname,
		     sizeof(hostname), &port, NULL, 0, url ? url : "");

	if (!protocol[0]) {
		blog(LOG_INFO, "[irl-source] %s: <redacted>", action);
		return;
	}

	bool ipv6 = strchr(hostname, ':') != NULL;
	if (hostname[0] && port >= 0) {
		blog(LOG_INFO, "[irl-source] %s: %s://%s%s%s:%d", action,
		     protocol, ipv6 ? "[" : "", hostname, ipv6 ? "]" : "",
		     port);
	} else if (hostname[0]) {
		blog(LOG_INFO, "[irl-source] %s: %s://%s%s%s", action,
		     protocol, ipv6 ? "[" : "", hostname, ipv6 ? "]" : "");
	} else if (port >= 0) {
		blog(LOG_INFO, "[irl-source] %s: %s://<redacted>:%d", action,
		     protocol, port);
	} else {
		blog(LOG_INFO, "[irl-source] %s: %s://<redacted>", action,
		     protocol);
	}
}

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
	cfg->network_buffer_mb = IRL_DEFAULT_NETWORK_BUFFER_MB;

	cfg->buffer_target_ms =
		(int)obs_data_get_int(settings, "buffer_target_ms");
	if (cfg->buffer_target_ms <= 0)
		cfg->buffer_target_ms = IRL_DEFAULT_BUFFER_TARGET_MS;
	/* The slider bounds this, but a scene collection can carry anything,
	 * including a target saved by a build with a different ceiling. */
	if (cfg->buffer_target_ms < IRL_BUFFER_TARGET_MIN_MS)
		cfg->buffer_target_ms = IRL_BUFFER_TARGET_MIN_MS;
	if (cfg->buffer_target_ms > IRL_BUFFER_TARGET_MAX_MS)
		cfg->buffer_target_ms = IRL_BUFFER_TARGET_MAX_MS;
	cfg->buffer_min_ms = cfg->buffer_target_ms / IRL_BUFFER_MIN_DIVISOR;
	if (cfg->buffer_min_ms < IRL_BUFFER_MIN_FLOOR_MS)
		cfg->buffer_min_ms = IRL_BUFFER_MIN_FLOOR_MS;
	cfg->buffer_max_ms = cfg->buffer_target_ms + IRL_BUFFER_MAX_EXTRA_MS;
	cfg->adaptive_speed = obs_data_get_bool(settings, "adaptive_speed");

	cfg->catchup_percent =
		(int)obs_data_get_int(settings, "catchup_percent");
	if (cfg->catchup_percent < IRL_CATCHUP_PERCENT_MIN)
		cfg->catchup_percent = IRL_CATCHUP_PERCENT_MIN;
	if (cfg->catchup_percent > IRL_CATCHUP_PERCENT_MAX)
		cfg->catchup_percent = IRL_CATCHUP_PERCENT_MAX;

	cfg->small_gap_ms = IRL_SMALL_GAP_MS;
	cfg->large_gap_ms = IRL_LARGE_GAP_MS;

	const char *ff = obs_data_get_string(settings, "ffmpeg_options");
	cfg->ffmpeg_options = ff && *ff ? bstrdup(ff) : NULL;

	cfg->hw_decode = (int)obs_data_get_int(settings, "hw_decode");
	if (cfg->hw_decode < IRL_HW_DECODE_AUTO ||
	    cfg->hw_decode > IRL_HW_DECODE_NVDEC) {
		blog(LOG_WARNING,
		     "[irl-source] Unknown hardware decode mode %d; using Auto",
		     cfg->hw_decode);
		cfg->hw_decode = IRL_HW_DECODE_AUTO;
	}
#if !defined(_WIN32) && !defined(__linux__)
	/* NVDEC is only offered (and only compiled into the bundled FFmpeg)
	 * on Windows and Linux. A scene collection saved there can still
	 * carry the value here; degrade to Auto instead of forcing a CUDA
	 * device that cannot exist, which would leave the source videoless. */
	if (cfg->hw_decode == IRL_HW_DECODE_NVDEC) {
		blog(LOG_WARNING,
		     "[irl-source] NVDEC is not available on this platform; using Auto");
		cfg->hw_decode = IRL_HW_DECODE_AUTO;
	}
#endif
	cfg->wait_for_keyframe =
		obs_data_get_bool(settings, "wait_for_keyframe");
	cfg->low_latency_audio =
		obs_data_get_bool(settings, "low_latency_audio");
	cfg->close_when_inactive =
		obs_data_get_bool(settings, "close_when_inactive");
	cfg->clear_on_disconnect =
		obs_data_get_bool(settings, "clear_on_disconnect");
}

static bool str_differs(const char *a, const char *b)
{
	if (!a || !b)
		return a != b;
	return strcmp(a, b) != 0;
}

/* Which settings force a reconnect. url and ffmpeg_options are consumed
 * by avformat_open_input (and config_load frees the strings the receiver
 * thread is still reading), hw_decode picks the decoder at open, and
 * low_latency_audio latches priming/pump semantics across all three
 * threads. Everything else is re-read live every cycle, so it can be
 * swapped in place: a restart costs an SRT handshake and wipes every
 * stat counter via reset_runtime_state(). */
static bool config_requires_restart(const struct irl_config *cur,
				    const struct irl_config *next)
{
	return str_differs(cur->url, next->url) ||
	       str_differs(cur->ffmpeg_options, next->ffmpeg_options) ||
	       cur->hw_decode != next->hw_decode ||
	       cur->low_latency_audio != next->low_latency_audio;
}

static void config_apply_hot(struct irl_source *ctx,
			     const struct irl_config *next)
{
	irl_mutex_lock(&ctx->audio_state_lock);

	/* Grow the ring before publishing the new watermarks, and publish
	 * only if that succeeded: the receiver's backpressure ceiling is 3x
	 * buffer_max_ms and must never exceed ring capacity, or a burst
	 * between fill checks would push writes past the end and drop
	 * audio. On allocation failure the old target stays in force. */
	if (ctx->config.buffer_target_ms != next->buffer_target_ms) {
		if (audio_buffer_resize(&ctx->audio_buf,
					(int)next->buffer_target_ms,
					(int)next->buffer_min_ms,
					(int)next->buffer_max_ms)) {
			os_atomic_set_long(&ctx->config.buffer_target_ms,
					   next->buffer_target_ms);
			os_atomic_set_long(&ctx->config.buffer_min_ms,
					   next->buffer_min_ms);
			os_atomic_set_long(&ctx->config.buffer_max_ms,
					   next->buffer_max_ms);
		} else {
			blog(LOG_WARNING,
			     "[irl-source] Could not resize jitter buffer to %ldms; keeping %ldms",
			     next->buffer_target_ms,
			     ctx->config.buffer_target_ms);
		}
	}

	os_atomic_set_long(&ctx->config.reconnect_delay,
			   next->reconnect_delay);
	os_atomic_store_bool(&ctx->config.adaptive_speed,
			     next->adaptive_speed);
	os_atomic_set_long(&ctx->config.catchup_percent,
			   next->catchup_percent);
	os_atomic_store_bool(&ctx->config.wait_for_keyframe,
			     next->wait_for_keyframe);
	os_atomic_store_bool(&ctx->config.clear_on_disconnect,
			     next->clear_on_disconnect);
	ctx->config.close_when_inactive = next->close_when_inactive;

	irl_mutex_unlock(&ctx->audio_state_lock);
}

static void apply_async_audio_mode(struct irl_source *ctx)
{
	obs_source_set_async_unbuffered(ctx->source,
					ctx->config.low_latency_audio);
	obs_source_set_async_decoupled(ctx->source, false);
}

/* ── Fit to canvas ────────────────────────────────────────── */

struct fit_request {
	obs_source_t *source;
	struct obs_transform_info info;
};

static bool fit_scene_item(obs_scene_t *scene, obs_sceneitem_t *item,
			   void *param)
{
	UNUSED_PARAMETER(scene);
	struct fit_request *req = param;

	if (obs_sceneitem_get_source(item) == req->source &&
	    !obs_sceneitem_locked(item)) {
		req->info.crop_to_bounds = obs_sceneitem_get_bounds_crop(item);
		obs_sceneitem_set_info2(item, &req->info);
	}
	return true;
}

static bool fit_scene(void *param, obs_source_t *scene_source)
{
	obs_scene_t *scene = obs_scene_from_source(scene_source);
	if (scene)
		obs_scene_enum_items(scene, fit_scene_item, param);
	return true;
}

/* Size a newly added source to the canvas exactly the way the Fit to
 * Screen menu action does (same obs_transform_info, so the result is
 * indistinguishable from pressing it by hand). Fires once, and only for
 * a source that was created without a URL: anything restored from a
 * scene collection already has one, so saved layouts are never touched
 * and upgrading cannot move an existing scene item. */
static void fit_to_canvas(struct irl_source *ctx)
{
	struct obs_video_info ovi;
	if (!obs_get_video_info(&ovi))
		return;

	struct fit_request req = {.source = ctx->source};
	vec2_set(&req.info.pos, 0.0f, 0.0f);
	vec2_set(&req.info.scale, 1.0f, 1.0f);
	req.info.rot = 0.0f;
	req.info.alignment = OBS_ALIGN_LEFT | OBS_ALIGN_TOP;
	vec2_set(&req.info.bounds, (float)ovi.base_width,
		 (float)ovi.base_height);
	req.info.bounds_type = OBS_BOUNDS_SCALE_INNER;
	req.info.bounds_alignment = OBS_ALIGN_CENTER;

	obs_enum_scenes(fit_scene, &req);
}

static void reset_runtime_state(struct irl_source *ctx)
{
	ctx->first_keyframe_received = false;
	ctx->video_pkt_gate_open = false;
	ctx->video_pkt_gate_start_us = 0;
	/* Only reachable with the worker threads stopped, so this is the one
	 * place the flag is touched without video_queue_lock. Dropping a clear
	 * the video thread never got to is correct: the stop path decides for
	 * itself whether the frame stays. */
	ctx->video_clear_pending = false;
	/* Same window, same reason: the video thread drops its cached playout
	 * offset when it processes a clear, but a stop/start pair never gives
	 * it one — and a restart that completes inside the cache's hold would
	 * otherwise map the new stream's PTS epoch through the old stream's
	 * offset. Safe to touch here for the same reason as the flag above. */
	ctx->video_playout_offset_ns = 0;
	ctx->video_playout_offset_time_ns = 0;
	os_atomic_store_bool(&ctx->reconnecting, false);
	irl_mutex_lock(&ctx->audio_state_lock);
	audio_buffer_flush(&ctx->audio_buf);
	pts_repair_reset(&ctx->pts_state);
	irl_reset_stream_timing_state(ctx);
	ctx->current_speed = 1.0f;
	ctx->audio_output_restarts = 0;
	ctx->audio_underruns = 0;
	ctx->audio_resync_skipped_chunks = 0;
	ctx->audio_hidden_trimmed_chunks = 0;
	ctx->audio_quality_events = 0;
	ctx->audio_decoder_flushes = 0;
	ctx->video_decoder_flushes = 0;
	ctx->video_corrupt_frames = 0;
	ctx->video_corrupt_held = 0;
	ctx->fade_in_pending = false;
	ctx->fade_in_frames_remaining = 0;
	ctx->startup_audio_warmup_remaining_ms = 0;
	irl_mutex_unlock(&ctx->audio_state_lock);
	ctx->total_audio_frames = 0;
	ctx->total_video_frames = 0;
	ctx->pts_repairs = 0;
	ctx->pts_normalizations = 0;
	ctx->pts_interpolations = 0;
	ctx->pts_resets = 0;
	ctx->pts_last_gap_ms = 0;
	ctx->pts_max_gap_ms = 0;
	ctx->silence_insertions = 0;
	ctx->last_stats_time = 0;
}

static bool should_run_receiver(const struct irl_source *ctx)
{
	return ctx->config.url && !ctx->media_stopped &&
	       (!ctx->config.close_when_inactive ||
		obs_source_showing(ctx->source));
}

static void clear_async_video(struct irl_source *ctx)
{
	obs_source_output_video(ctx->source, NULL);
}

static void start_receiver(struct irl_source *ctx)
{
	if (os_atomic_load_bool(&ctx->thread_active) ||
	    !should_run_receiver(ctx))
		return;

	reset_runtime_state(ctx);
	os_atomic_store_bool(&ctx->thread_active, true);
	if (irl_thread_create(&ctx->audio_thread, irl_audio_thread, ctx) != 0) {
		blog(LOG_ERROR,
		     "[irl-source] Failed to create audio thread");
		os_atomic_store_bool(&ctx->thread_active, false);
		return;
	}
	if (irl_thread_create(&ctx->video_thread, irl_video_thread, ctx) != 0) {
		blog(LOG_ERROR,
		     "[irl-source] Failed to create video thread");
		os_atomic_store_bool(&ctx->thread_active, false);
		irl_thread_join(&ctx->audio_thread);
		return;
	}
	if (irl_thread_create(&ctx->receiver_thread, irl_receiver_thread,
			      ctx) != 0) {
		blog(LOG_ERROR,
		     "[irl-source] Failed to create receiver thread");
		os_atomic_store_bool(&ctx->thread_active, false);
		irl_mutex_lock(&ctx->video_queue_lock);
		irl_cond_broadcast(&ctx->video_queue_cond);
		irl_mutex_unlock(&ctx->video_queue_lock);
		irl_thread_join(&ctx->video_thread);
		irl_thread_join(&ctx->audio_thread);
	}
}

/* clear_video asks for the frame to be dropped because the stream stopped,
 * so it is subject to clear_on_disconnect. Callers that stop the source
 * outright (no URL, teardown) decide for themselves. */
static void stop_receiver(struct irl_source *ctx, bool clear_video)
{
	irl_receiver_stop(ctx);
	reset_runtime_state(ctx);
	if (clear_video &&
	    os_atomic_load_bool(&ctx->config.clear_on_disconnect))
		clear_async_video(ctx);
}

/* ── Stats proc_handler callback ──────────────────────────── */

static void irl_source_get_stats(void *data, calldata_t *cd)
{
	struct irl_source *ctx = data;

	/* Snapshot all shared mutable state under the lock so the stats
	 * blob is internally consistent and we don't race the receiver
	 * thread reconfiguring the audio buffer. */
	irl_mutex_lock(&ctx->audio_state_lock);
	int buffer_fill_ms = audio_buffer_fill_ms_locked(&ctx->audio_buf);
	float current_speed = ctx->current_speed;
	uint64_t total_audio_frames = ctx->total_audio_frames;
	uint64_t total_video_frames = ctx->total_video_frames;
	uint64_t pts_repairs = ctx->pts_repairs;
	uint64_t pts_normalizations = ctx->pts_normalizations;
	uint64_t pts_interpolations = ctx->pts_interpolations;
	uint64_t pts_resets = ctx->pts_resets;
	int pts_last_gap_ms = ctx->pts_last_gap_ms;
	int pts_max_gap_ms = ctx->pts_max_gap_ms;
	uint64_t silence_insertions = ctx->silence_insertions;
	uint64_t audio_underruns = ctx->audio_underruns;
	uint64_t audio_resync_skipped_chunks =
		ctx->audio_resync_skipped_chunks;
	uint64_t audio_hidden_trimmed_chunks =
		ctx->audio_hidden_trimmed_chunks;
	uint64_t audio_quality_events = ctx->audio_quality_events;
	uint64_t audio_output_restarts = ctx->audio_output_restarts;
	int64_t obs_lead_ms = ctx->audio_last_obs_lead_ns / 1000000LL;
	uint64_t audio_decoder_flushes = ctx->audio_decoder_flushes;
	uint64_t video_decoder_flushes = ctx->video_decoder_flushes;
	uint64_t video_corrupt_frames = ctx->video_corrupt_frames;
	uint64_t video_corrupt_held = ctx->video_corrupt_held;
	uint64_t reconnect_count = ctx->reconnect_count;
	bool video_ts_init = ctx->video_ts_init;
	uint64_t video_sys_base = ctx->video_sys_base;
	int64_t video_pts_base = ctx->video_pts_base;
	int64_t latest_video_stream_pts_ns = ctx->latest_video_stream_pts_ns;
	int64_t video_lead_ms = ctx->video_lead_ns / 1000000LL;
	uint64_t video_lead_excess = ctx->video_lead_excess;
	irl_mutex_unlock(&ctx->audio_state_lock);

	calldata_set_int(cd, "buffer_fill_ms", buffer_fill_ms);
	calldata_set_float(cd, "current_speed", (double)current_speed);
	calldata_set_bool(cd, "adaptive_latency_control",
			  os_atomic_load_bool(&ctx->config.adaptive_speed));
	calldata_set_bool(cd, "reconnecting",
			  os_atomic_load_bool(&ctx->reconnecting));
	calldata_set_int(cd, "total_audio_frames",
			 (long long)total_audio_frames);
	calldata_set_int(cd, "total_video_frames",
			 (long long)total_video_frames);
	calldata_set_int(cd, "pts_repairs", (long long)pts_repairs);
	calldata_set_int(cd, "pts_normalizations",
			 (long long)pts_normalizations);
	calldata_set_int(cd, "pts_interpolations",
			 (long long)pts_interpolations);
	calldata_set_int(cd, "pts_resets", (long long)pts_resets);
	calldata_set_int(cd, "pts_last_gap_ms", pts_last_gap_ms);
	calldata_set_int(cd, "pts_max_gap_ms", pts_max_gap_ms);
	calldata_set_int(cd, "silence_insertions",
			 (long long)silence_insertions);
	calldata_set_int(cd, "audio_underruns",
			 (long long)audio_underruns);
	calldata_set_int(cd, "audio_resync_skipped_chunks",
			 (long long)audio_resync_skipped_chunks);
	calldata_set_int(cd, "audio_hidden_trimmed_chunks",
			 (long long)audio_hidden_trimmed_chunks);
	calldata_set_int(cd, "audio_quality_events",
			 (long long)audio_quality_events);
	calldata_set_int(cd, "audio_output_restarts",
			 (long long)audio_output_restarts);
	calldata_set_int(cd, "obs_lead_ms", (long long)obs_lead_ms);
	calldata_set_int(cd, "audio_decoder_flushes",
			 (long long)audio_decoder_flushes);
	calldata_set_int(cd, "video_decoder_flushes",
			 (long long)video_decoder_flushes);
	calldata_set_int(cd, "video_corrupt_frames",
			 (long long)video_corrupt_frames);
	calldata_set_int(cd, "video_corrupt_held",
			 (long long)video_corrupt_held);
	calldata_set_int(cd, "video_lead_ms", (long long)video_lead_ms);
	calldata_set_int(cd, "video_lead_excess",
			 (long long)video_lead_excess);

	/* Stream delay: how far behind real-time the video output is.
	 * Computed as wall_clock - anchored_video_PTS.  Includes SRT
	 * latency, decode time, and any buffering.  Useful for
	 * monitoring end-to-end latency in stats overlays. */
	int64_t stream_delay_ms = 0;
	if (video_ts_init && latest_video_stream_pts_ns != 0) {
		int64_t video_wall_ns = (int64_t)video_sys_base +
					(latest_video_stream_pts_ns -
					 video_pts_base);
		stream_delay_ms =
			((int64_t)os_gettime_ns() - video_wall_ns) / 1000000;
		if (stream_delay_ms < 0)
			stream_delay_ms = 0;
	}
	calldata_set_int(cd, "stream_delay_ms",
			 (long long)stream_delay_ms);
	calldata_set_bool(cd, "low_latency_audio",
			  ctx->config.low_latency_audio);
	calldata_set_int(cd, "reconnect_count", (long long)reconnect_count);
}

/* ── Lifecycle ────────────────────────────────────────────── */

const char *irl_source_get_name(void *unused)
{
	UNUSED_PARAMETER(unused);
	return obs_module_text("SourceName");
}

void *irl_source_create(obs_data_t *settings, obs_source_t *source)
{
	struct irl_source *ctx = bzalloc(sizeof(*ctx));
	ctx->source = source;
	ctx->current_speed = 1.0f;
	/* Bail out rather than run on primitives that were never created:
	 * every lock/unlock below this point would be undefined behaviour.
	 * Nothing is registered with libobs yet, so freeing ctx and returning
	 * NULL is the whole cleanup — libobs logs the failure and never calls
	 * irl_source_destroy for a create that returned NULL. */
	if (irl_mutex_init(&ctx->audio_state_lock) != 0) {
		blog(LOG_ERROR,
		     "[irl-source] Failed to create audio state lock");
		bfree(ctx);
		return NULL;
	}
	if (irl_mutex_init(&ctx->video_queue_lock) != 0) {
		blog(LOG_ERROR,
		     "[irl-source] Failed to create video queue lock");
		irl_mutex_destroy(&ctx->audio_state_lock);
		bfree(ctx);
		return NULL;
	}
	if (irl_cond_init(&ctx->video_queue_cond) != 0) {
		blog(LOG_ERROR,
		     "[irl-source] Failed to create video queue condition variable");
		irl_mutex_destroy(&ctx->video_queue_lock);
		irl_mutex_destroy(&ctx->audio_state_lock);
		bfree(ctx);
		return NULL;
	}

	config_load(&ctx->config, settings);
	apply_async_audio_mode(ctx);

	/* A source the user just added has no URL yet; one restored from a
	 * scene collection always does. See fit_to_canvas(). */
	ctx->fit_pending = ctx->config.url == NULL;

	/* Register stats proc_handler so scripts/overlays can query state.
	 * The obs-websocket vendor extension calls this same proc, so a field
	 * added here also belongs in irl_stat_fields[] (websocket-vendor.c)
	 * to reach websocket clients. */
	proc_handler_t *ph = obs_source_get_proc_handler(source);
	proc_handler_add(
		ph,
		"void get_stats(out int buffer_fill_ms, "
		"out float current_speed, out bool adaptive_latency_control, "
		"out bool reconnecting, "
		"out int total_audio_frames, out int total_video_frames, "
		"out int pts_repairs, out int pts_normalizations, "
		"out int pts_interpolations, out int pts_resets, "
		"out int pts_last_gap_ms, out int pts_max_gap_ms, "
		"out int silence_insertions, out int audio_underruns, "
		"out int audio_resync_skipped_chunks, "
		"out int audio_hidden_trimmed_chunks, "
		"out int audio_quality_events, "
		"out int audio_output_restarts, out int obs_lead_ms, "
		"out int audio_decoder_flushes, "
		"out int video_decoder_flushes, "
		"out int video_corrupt_frames, out int video_corrupt_held, "
		"out int video_lead_ms, out int video_lead_excess, "
		"out int stream_delay_ms, out bool low_latency_audio, "
		"out int reconnect_count)",
		irl_source_get_stats, ctx);

	if (ctx->config.url) {
		irl_log_input_url("Created with URL", ctx->config.url);
		start_receiver(ctx);
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

	stop_receiver(ctx, false);
	audio_buffer_free(&ctx->audio_buf);
	irl_mutex_destroy(&ctx->audio_state_lock);
	irl_cond_destroy(&ctx->video_queue_cond);
	irl_mutex_destroy(&ctx->video_queue_lock);

	free(ctx->audio_pump_scratch);
	free(ctx->audio_resample_scratch);
	free(ctx->audio_speed_scratch);
	free(ctx->sws_nv12_buf);

	if (ctx->swr_ctx)
		swr_free(&ctx->swr_ctx);
	if (ctx->speed_swr)
		swr_free(&ctx->speed_swr);
	/* sws_free_context(), not sws_freeContext(): the scaler is built with
	 * sws_alloc_context() and driven by sws_scale_frame(). The pre-9.0
	 * fallback in video-handler.c still uses sws_getContext(), so the
	 * teardown has to follow the same version split. */
	if (ctx->sws_ctx) {
#if LIBSWSCALE_VERSION_MAJOR >= 10
		sws_free_context(&ctx->sws_ctx);
#else
		sws_freeContext(ctx->sws_ctx);
		ctx->sws_ctx = NULL;
#endif
	}
	/* Never owns pixel data — it only ever describes sws_nv12_buf, which
	 * is freed above. */
	if (ctx->sws_dst_frame)
		av_frame_free(&ctx->sws_dst_frame);
	/* The video thread releases this on exit; kept here as well for a
	 * source destroyed without its threads ever starting. */
	irl_video_xfer_pool_release(ctx);
	if (ctx->hw_device_ctx)
		av_buffer_unref(&ctx->hw_device_ctx);

	config_free(&ctx->config);
	bfree(ctx);
}

void irl_source_update(void *data, obs_data_t *settings)
{
	struct irl_source *ctx = data;

	struct irl_config next = {0};
	config_load(&next, settings);

	/* Editing the source is a request to have it running again. */
	ctx->media_stopped = false;

	if (os_atomic_load_bool(&ctx->thread_active) &&
	    !config_requires_restart(&ctx->config, &next)) {
		config_apply_hot(ctx, &next);
		config_free(&next);
		/* close_when_inactive may have just turned on while the
		 * source is hidden. */
		if (!should_run_receiver(ctx))
			stop_receiver(ctx, true);
		return;
	}

	stop_receiver(ctx, false);
	config_free(&ctx->config);
	ctx->config = next; /* takes ownership of the loaded strings */
	apply_async_audio_mode(ctx);

	/* Either the source is not going to run at all, or a restart-forcing
	 * edit just dropped the connection: both leave a frame on screen that
	 * belongs to a stream that is gone. Clearing is decided against the
	 * config that was just installed, not the one being replaced.
	 *
	 * Ordering matters: this has to happen before the receiver restarts,
	 * or the NULL frame could land after the new stream delivered its
	 * first one and blank a live picture. Safe to do directly rather than
	 * via irl_video_request_clear() because the threads are stopped here
	 * — the video thread drains the queue as it exits, and there is no
	 * frame in flight to repaint over the clear. */
	if (!should_run_receiver(ctx) ||
	    os_atomic_load_bool(&ctx->config.clear_on_disconnect))
		clear_async_video(ctx);

	start_receiver(ctx);
}

void irl_source_activate(void *data)
{
	struct irl_source *ctx = data;

	if (!ctx || !ctx->config.close_when_inactive)
		return;

	start_receiver(ctx);
}

void irl_source_deactivate(void *data)
{
	struct irl_source *ctx = data;

	if (!ctx || !ctx->config.close_when_inactive)
		return;

	if (!obs_source_showing(ctx->source))
		stop_receiver(ctx, true);
}

void irl_source_show(void *data)
{
	struct irl_source *ctx = data;

	if (!ctx || !ctx->config.close_when_inactive)
		return;

	start_receiver(ctx);
}

void irl_source_hide(void *data)
{
	struct irl_source *ctx = data;

	if (!ctx || !ctx->config.close_when_inactive)
		return;

	stop_receiver(ctx, true);
}

/* ── Media controls ───────────────────────────────────────── */

/* A live stream has nothing to seek or pause, so the four callbacks
 * reduce to "run the receiver" and "don't". They exist because
 * OBS_SOURCE_CONTROLLABLE_MEDIA is what makes the source addressable
 * through obs-websocket's TriggerMediaInputAction / GetMediaInputStatus,
 * which is how NOALBS's !fix reconnects a stalled feed, and it is also
 * what puts the source in the media controls dock. */

void irl_source_media_restart(void *data)
{
	struct irl_source *ctx = data;
	if (!ctx)
		return;

	blog(LOG_INFO, "[irl-source] Media restart requested");

	/* An explicit restart overrides a previous Stop. */
	ctx->media_stopped = false;
	stop_receiver(ctx, true);
	start_receiver(ctx);

	if (os_atomic_load_bool(&ctx->thread_active))
		obs_source_media_started(ctx->source);
}

void irl_source_media_stop(void *data)
{
	struct irl_source *ctx = data;
	if (!ctx)
		return;

	blog(LOG_INFO, "[irl-source] Media stop requested");

	ctx->media_stopped = true;
	stop_receiver(ctx, false);
	/* Unconditional, unlike a disconnect: the frame is gone because
	 * the user asked for the source to stop, which is not what
	 * clear_on_disconnect decides. Matches ffmpeg_source_stop(). */
	clear_async_video(ctx);
	/* No obs_source_media_ended() here: libobs already fires
	 * "media_stopped" for the stop action, and a live stream has no
	 * end to report. */
}

/* Pause is the only honest reading of "stop receiving" for a live
 * stream: there is no paused position to resume from, so unpausing
 * reconnects. */
void irl_source_media_play_pause(void *data, bool pause)
{
	if (pause)
		irl_source_media_stop(data);
	else
		irl_source_media_restart(data);
}

enum obs_media_state irl_source_media_get_state(void *data)
{
	struct irl_source *ctx = data;
	if (!ctx)
		return OBS_MEDIA_STATE_NONE;

	if (!ctx->config.url)
		return OBS_MEDIA_STATE_NONE;
	/* Stopped by the user, or not running because the source is
	 * hidden with "Close Stream When Inactive" on. Never ENDED: a
	 * live stream has no end to reach. */
	if (!os_atomic_load_bool(&ctx->thread_active))
		return OBS_MEDIA_STATE_STOPPED;
	if (os_atomic_load_bool(&ctx->reconnecting))
		return OBS_MEDIA_STATE_OPENING;

	/* Connected, but nothing on screen yet: the first connection
	 * attempt is still in avformat_open_input, or the keyframe gate
	 * has not opened. video_ts_init is receiver-thread state, read
	 * under the same lock the stats snapshot uses. */
	irl_mutex_lock(&ctx->audio_state_lock);
	bool playing = ctx->video_ts_init || ctx->audio_out_primed;
	irl_mutex_unlock(&ctx->audio_state_lock);

	return playing ? OBS_MEDIA_STATE_PLAYING : OBS_MEDIA_STATE_BUFFERING;
}

void irl_source_tick(void *data, float seconds)
{
	UNUSED_PARAMETER(seconds);
	struct irl_source *ctx = data;

	/* Reconnection is handled inside the receiver thread via
	 * sleep + retry, so the only polled work is the one-shot fit.
	 * Waiting for a non-zero source size means the scene item exists
	 * and the stream resolution is known. */
	if (ctx->fit_pending && obs_source_get_width(ctx->source) > 0 &&
	    obs_source_get_height(ctx->source) > 0) {
		ctx->fit_pending = false;
		fit_to_canvas(ctx);
	}
}
