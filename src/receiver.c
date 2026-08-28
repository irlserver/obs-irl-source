/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * receiver.c — FFmpeg open/read thread (protocol-agnostic)
 *
 * Opens any FFmpeg-supported URL, decodes audio+video, and feeds
 * decoded frames to the jitter buffer / video handler.
 */

#include <errno.h>

#include "../include/irl-source.h"
#include "receiver-internal.h"

/* ── Main read loop ───────────────────────────────────────── */

void *irl_audio_thread(void *data)
{
	struct irl_source *ctx = data;

	while (os_atomic_load_bool(&ctx->thread_active)) {
		if (os_atomic_load_bool(&ctx->reconnecting)) {
			os_sleep_ms(1);
			continue;
		}

		bool pumped = false;
		for (int i = 0; i < 16 && os_atomic_load_bool(&ctx->thread_active);
		     i++) {
			/* The whole pump runs under audio_state_lock, so
			 * nothing it calls may take that lock again — the
			 * mutex is not recursive and a nested acquire hangs
			 * this thread, and with it the video thread waiting
			 * behind it. */
			irl_mutex_lock(&ctx->audio_state_lock);
			bool ok = irl_pump_audio_once(ctx);
			irl_mutex_unlock(&ctx->audio_state_lock);
			if (!ok)
				break;
			pumped = true;
		}

		if (!pumped)
			os_sleep_ms(1);
	}

	return NULL;
}

void *irl_receiver_thread(void *data)
{
	struct irl_source *ctx = data;
	/* Start of the current unbroken run of EAGAIN reads, or 0. */
	uint64_t eagain_since_us = 0;
	AVPacket *pkt = av_packet_alloc();
	AVFrame *frame = av_frame_alloc();
	if (!pkt || !frame) {
		blog(LOG_ERROR,
		     "[irl-source] Failed to allocate packet/frame, receiver exiting");
		av_packet_free(&pkt);
		av_frame_free(&frame);
		os_atomic_store_bool(&ctx->thread_active, false);
		return NULL;
	}

	irl_log_input_url("Receiver thread started for", ctx->config.url);

	/* A new thread means a new stream configuration (create, or a
	 * restart-forcing settings edit): nothing learned about the previous
	 * stream applies, so the first connect always probes in full. */
	ctx->prev_had_video = false;
	ctx->prev_had_audio = false;

	while (os_atomic_load_bool(&ctx->thread_active)) {
		if (!ctx->fmt_ctx) {
			eagain_since_us = 0;
			if (!irl_open_stream(ctx)) {
				if (!irl_wait_for_reconnect(ctx))
					break;
				continue;
			}
			irl_prepare_new_connection(ctx);
		}

		/* Backlog backpressure: above the fill ceiling, stop
		 * reading so the transport holds the excess and playback
		 * bleeds it off via speed. Bounded by buffer capacity
		 * so a burst between checks can never force the ring
		 * buffer to drop audible data. */
		if (ctx->audio_stream_idx >= 0 &&
		    !ctx->config.low_latency_audio) {
			int buffer_max_ms = (int)os_atomic_load_long(
				&ctx->config.buffer_max_ms);
			int pace_ms = buffer_max_ms * 3;
			if (pace_ms > IRL_BLEED_PACE_FILL_MS)
				pace_ms = IRL_BLEED_PACE_FILL_MS;
			/* The flat cap above is an absolute latency guard, but
			 * it must never fall to where playback cannot prime:
			 * priming waits for target + the OBS output lead, and a
			 * ceiling below that stops the read loop before the
			 * buffer ever reaches it, so the source would sit
			 * silent forever. buffer_max is target + 200, so this
			 * floor clears the prime threshold by ~220ms at every
			 * target. Only binds above a ~700ms target, which is
			 * why nothing hit it while the setting stopped at
			 * 500ms. */
			if (pace_ms < buffer_max_ms + 100)
				pace_ms = buffer_max_ms + 100;
			while (os_atomic_load_bool(&ctx->thread_active) &&
			       audio_buffer_fill_ms_locked(&ctx->audio_buf) >
				       pace_ms) {
				os_sleep_ms(5);
			}
			if (!os_atomic_load_bool(&ctx->thread_active))
				break;
		}

		ctx->io_start_us = (uint64_t)av_gettime();
		int ret = av_read_frame(ctx->fmt_ctx, pkt);
		/* Not an error: a non-blocking demuxer saying "nothing yet".
		 * Treating it as one tore down a healthy connection and
		 * reconnected for what is a normal empty poll.
		 *
		 * Bounded, because retrying resets io_start_us on every pass
		 * and the interrupt callback only measures one av_read_frame
		 * call: without the bound, a demuxer wedged in permanent
		 * EAGAIN would spin here forever, past the stall timeout that
		 * a wedged blocking read would have hit. Past that same
		 * timeout, fall through and let the read error path
		 * reconnect. */
		if (ret == AVERROR(EAGAIN)) {
			uint64_t now_us = (uint64_t)av_gettime();
			if (eagain_since_us == 0)
				eagain_since_us = now_us;
			if (now_us - eagain_since_us < IRL_IO_STALL_TIMEOUT_US) {
				av_packet_unref(pkt);
				av_usleep(1000);
				continue;
			}
		} else {
			eagain_since_us = 0;
		}
		if (ret < 0) {
			irl_handle_stream_read_error(ctx, ret);
			continue;
		}

		if (pkt->stream_index == ctx->audio_stream_idx &&
		    ctx->audio_dec_ctx) {
			irl_handle_audio_packet(ctx, pkt, frame);
		} else if (pkt->stream_index == ctx->video_stream_idx &&
			   ctx->video_dec_ctx) {
			irl_handle_video_packet(ctx, pkt, frame);
		}

		av_packet_unref(pkt);
		irl_log_receiver_stats(ctx);
	}

	irl_close_ffmpeg(ctx);
	av_packet_free(&pkt);
	av_frame_free(&frame);
	return NULL;
}

void irl_receiver_stop(struct irl_source *ctx)
{
	if (!os_atomic_load_bool(&ctx->thread_active))
		return;

	os_atomic_store_bool(&ctx->thread_active, false);
	irl_mutex_lock(&ctx->video_queue_lock);
	irl_cond_broadcast(&ctx->video_queue_cond);
	irl_mutex_unlock(&ctx->video_queue_lock);
	irl_thread_join(&ctx->video_thread);
	irl_thread_join(&ctx->audio_thread);
	irl_thread_join(&ctx->receiver_thread);
}
