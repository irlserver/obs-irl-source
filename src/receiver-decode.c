/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

#include "receiver-internal.h"

#define DECODER_FLUSH_COOLDOWN_US 350000
#define DECODER_WARNING_INTERVAL_US 1000000

static bool should_log_decoder_warning(uint64_t *last_warning_time_us,
				       uint64_t now_us)
{
	if (*last_warning_time_us != 0 &&
	    now_us - *last_warning_time_us < DECODER_WARNING_INTERVAL_US) {
		return false;
	}
	*last_warning_time_us = now_us;
	return true;
}

static bool should_flush_decoder(uint64_t *last_flush_time_us, uint64_t now_us)
{
	if (*last_flush_time_us != 0 &&
	    now_us - *last_flush_time_us < DECODER_FLUSH_COOLDOWN_US) {
		return false;
	}
	*last_flush_time_us = now_us;
	return true;
}

static void reinit_audio_pts_repair(struct irl_source *ctx)
{
	pts_repair_reset(&ctx->pts_state);
	if (ctx->fmt_ctx && ctx->audio_stream_idx >= 0) {
		AVStream *as = ctx->fmt_ctx->streams[ctx->audio_stream_idx];
		pts_repair_init(&ctx->pts_state, ctx->config.small_gap_ms,
				ctx->config.large_gap_ms, as->time_base.num,
				as->time_base.den);
	}
}

/* Drain everything the decoder has ready. Lifted verbatim out of
 * irl_handle_audio_packet() so the EAGAIN retry below can drain without
 * duplicating the error handling; the state machine is unchanged. */
static void drain_audio_frames(struct irl_source *ctx, AVFrame *frame)
{
	for (;;) {
		int ret = avcodec_receive_frame(ctx->audio_dec_ctx, frame);
		if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF)
			break;
		if (ret < 0) {
			ctx->audio_decode_errors++;
			if (ctx->audio_decode_errors >= 3) {
				uint64_t now_us = (uint64_t)av_gettime();
				bool do_flush = should_flush_decoder(
					&ctx->audio_last_decoder_flush_time_us,
					now_us);
				if (should_log_decoder_warning(
					    &ctx->audio_last_decoder_warning_time_us,
					    now_us)) {
					blog(LOG_WARNING,
					     "[irl-source] Audio decoder receive: corruption burst (%d consecutive errors)%s",
					     ctx->audio_decode_errors,
					     do_flush ? ", resetting audio state"
						      : ", reset cooldown active");
				}
				if (do_flush) {
					avcodec_flush_buffers(ctx->audio_dec_ctx);
					ctx->audio_decoder_flushes++;
					ctx->audio_quality_events++;
					irl_mutex_lock(&ctx->audio_state_lock);
					audio_buffer_flush(&ctx->audio_buf);
					irl_reset_audio_timing_state(ctx);
					irl_mark_audio_recovery(ctx, 2500000ULL);
					irl_mutex_unlock(&ctx->audio_state_lock);
					reinit_audio_pts_repair(ctx);
				}
				ctx->audio_decode_errors = 0;
			}
			break;
		}

		ctx->audio_decode_errors = 0;
		irl_handle_audio_frame(ctx, frame);
		av_frame_unref(frame);
	}
}

/* Decode an audio packet and pass each frame to irl_handle_audio_frame. */
void irl_handle_audio_packet(struct irl_source *ctx, AVPacket *pkt,
			     AVFrame *frame)
{
	int ret = avcodec_send_packet(ctx->audio_dec_ctx, pkt);
	if (ret == AVERROR(EAGAIN)) {
		/* The decoder did not take the packet. FFmpeg's contract is
		 * to read output and resend the same packet; returning here
		 * would silently discard it. */
		ctx->audio_pkt_eagain++;
		drain_audio_frames(ctx, frame);
		ret = avcodec_send_packet(ctx->audio_dec_ctx, pkt);
		if (ret == AVERROR(EAGAIN))
			ctx->audio_pkt_dropped++;
	}
	if (ret < 0 && ret != AVERROR(EAGAIN) && ret != AVERROR_EOF) {
		ctx->audio_decode_errors++;
		if (ctx->audio_decode_errors >= 3) {
			uint64_t now_us = (uint64_t)av_gettime();
			bool do_flush = should_flush_decoder(
				&ctx->audio_last_decoder_flush_time_us, now_us);
			if (should_log_decoder_warning(
				    &ctx->audio_last_decoder_warning_time_us,
				    now_us)) {
				blog(LOG_WARNING,
				     "[irl-source] Audio decoder: corruption burst (%d consecutive errors)%s",
				     ctx->audio_decode_errors,
				     do_flush ? ", flushing"
					      : ", suppressing repeated flush");
			}
			if (do_flush) {
				avcodec_flush_buffers(ctx->audio_dec_ctx);
				ctx->audio_decoder_flushes++;
				ctx->audio_quality_events++;
			}
			ctx->audio_decode_errors = 0;
		}
	} else {
		ctx->audio_decode_errors = 0;
	}

	drain_audio_frames(ctx, frame);
}

/* The video decoder is never flushed on a corruption burst, unlike the audio
 * one above. avcodec_flush_buffers() empties the reference picture buffer
 * and clears the decoder's recovery state, and neither the H.264 nor the
 * HEVC decoder can produce a real picture again until the next IDR/CRA:
 * h264dec paints every frame gray until it sees a recovery point
 * (h264_slice.c, `!h->frame_recovered`), and the HEVC decoder synthesizes
 * each missing reference as a flat mid-gray frame (hevc/refs.c
 * generate_missing_ref) that every later P-frame is predicted from. So the
 * flush turned "a few damaged frames" into a whole GOP of gray — one to two
 * seconds at the keyframe intervals IRL encoders use — on exactly the lossy
 * streams it was meant to help. A decoder error on a live stream is a
 * property of the packet, not of the decoder's state; the next intact
 * packet decodes fine without any reset, and the reference chain heals at
 * the next keyframe either way. The burst is still counted and logged so it
 * shows up in diagnostics. */
static void note_video_decode_error(struct irl_source *ctx, const char *stage)
{
	ctx->video_decode_errors++;
	ctx->video_corrupted = true;
	if (ctx->video_decode_errors < 3)
		return;
	uint64_t now_us = (uint64_t)av_gettime();
	if (should_log_decoder_warning(&ctx->video_last_decoder_warning_time_us,
				       now_us)) {
		blog(LOG_WARNING,
		     "[irl-source] Video decoder %s: corruption burst (%d consecutive errors), waiting for the next keyframe",
		     stage, ctx->video_decode_errors);
	}
	ctx->video_decode_errors = 0;
}

/* Video counterpart of drain_audio_frames(): same loop as before, moved so
 * the EAGAIN retry can reuse it. */
static void drain_video_frames(struct irl_source *ctx, AVFrame *frame)
{
	for (;;) {
		int ret = avcodec_receive_frame(ctx->video_dec_ctx, frame);
		if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF)
			break;
		if (ret < 0) {
			note_video_decode_error(ctx, "receive");
			break;
		}

		ctx->video_decode_errors = 0;
		irl_handle_video_frame(ctx, frame);
		av_frame_unref(frame);
	}
}

/* Decode a video packet and pass each frame to irl_handle_video_frame. */
void irl_handle_video_packet(struct irl_source *ctx, AVPacket *pkt,
			     AVFrame *frame)
{
	/* Packet-level keyframe gate: pre-keyframe packets only produce
	 * reference-miss error spam and garbage frames that the
	 * frame-level gate discards anyway. The timeout covers demuxers
	 * that never set AV_PKT_FLAG_KEY; the decoder then finds the
	 * keyframe itself. */
	if (os_atomic_load_bool(&ctx->config.wait_for_keyframe) &&
	    !ctx->first_keyframe_received && !ctx->video_pkt_gate_open) {
		if (pkt->flags & AV_PKT_FLAG_KEY) {
			ctx->video_pkt_gate_open = true;
		} else {
			uint64_t now_us = (uint64_t)av_gettime();
			if (ctx->video_pkt_gate_start_us == 0)
				ctx->video_pkt_gate_start_us = now_us;
			if (now_us - ctx->video_pkt_gate_start_us < 5000000ULL)
				return;
			ctx->video_pkt_gate_open = true;
		}
	}

	int ret = avcodec_send_packet(ctx->video_dec_ctx, pkt);
	if (ret == AVERROR(EAGAIN)) {
		/* The decoder refused the packet: it has output waiting and,
		 * on fixed-pool hardware decoders, no free surface until we
		 * take it. FFmpeg's contract is to read the output and resend
		 * the same packet — falling through would discard it, and
		 * with a reference frame that costs artifacts until the next
		 * keyframe rather than one dropped frame.
		 *
		 * One retry, deliberately not a loop. A single drain frees
		 * every surface the decoder was waiting on, and this runs on
		 * the receiver thread, which also feeds audio intake: a
		 * decoder that returned EAGAIN without producing frames would
		 * spin here and starve the jitter buffer, trading a dropped
		 * packet for the underrun cascade. If the retry fails too,
		 * count it and behave as before. */
		ctx->video_pkt_eagain++;
		drain_video_frames(ctx, frame);
		ret = avcodec_send_packet(ctx->video_dec_ctx, pkt);
		if (ret == AVERROR(EAGAIN))
			ctx->video_pkt_dropped++;
	}
	if (ret < 0 && ret != AVERROR(EAGAIN) && ret != AVERROR_EOF) {
		note_video_decode_error(ctx, "send");
	} else {
		ctx->video_decode_errors = 0;
	}

	drain_video_frames(ctx, frame);
}
