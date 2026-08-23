/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

#include <libavutil/imgutils.h>

#include "receiver-internal.h"

/* ── Video output queue ───────────────────────────────────── */

static void video_queue_drain_locked(struct irl_source *ctx)
{
	while (ctx->video_queue_count > 0) {
		AVFrame *f = ctx->video_queue[ctx->video_queue_head];
		ctx->video_queue[ctx->video_queue_head] = NULL;
		ctx->video_queue_head =
			(ctx->video_queue_head + 1) % IRL_VIDEO_QUEUE_SIZE;
		ctx->video_queue_count--;
		av_frame_free(&f);
	}
}

static void video_pinned_update_locked(struct irl_source *ctx)
{
	int pinned = ctx->video_queue_count + ctx->video_in_flight;
	if (pinned > ctx->video_pinned_peak)
		ctx->video_pinned_peak = pinned;
}

/* Ask the video thread to blank the source. Queued frames are dropped
 * here so nothing decoded before the disconnect can repaint after the
 * clear; a frame already being converted is handled by the ordering in
 * irl_video_thread(), which re-checks the flag after each output. */
void irl_video_request_clear(struct irl_source *ctx)
{
	irl_mutex_lock(&ctx->video_queue_lock);
	video_queue_drain_locked(ctx);
	ctx->video_clear_pending = true;
	irl_cond_signal(&ctx->video_queue_cond);
	irl_mutex_unlock(&ctx->video_queue_lock);
}

void irl_video_queue_push(struct irl_source *ctx, AVFrame *frame,
			  int64_t pts_ns)
{
	AVFrame *clone = av_frame_alloc();
	if (!clone)
		return;
	if (av_frame_ref(clone, frame) < 0) {
		av_frame_free(&clone);
		return;
	}
	clone->pts = pts_ns;

	irl_mutex_lock(&ctx->video_queue_lock);
	if (ctx->video_queue_count >= IRL_VIDEO_QUEUE_SIZE) {
		/* Video thread is stalled; keep the freshest frames and
		 * never make the receiver (and therefore audio) wait. */
		AVFrame *oldest = ctx->video_queue[ctx->video_queue_head];
		ctx->video_queue[ctx->video_queue_head] = NULL;
		ctx->video_queue_head =
			(ctx->video_queue_head + 1) % IRL_VIDEO_QUEUE_SIZE;
		ctx->video_queue_count--;
		ctx->video_queue_drops++;
		av_frame_free(&oldest);
	}
	int tail = (ctx->video_queue_head + ctx->video_queue_count) %
		   IRL_VIDEO_QUEUE_SIZE;
	ctx->video_queue[tail] = clone;
	ctx->video_queue_count++;
	video_pinned_update_locked(ctx);
	irl_cond_signal(&ctx->video_queue_cond);
	irl_mutex_unlock(&ctx->video_queue_lock);
}

/* ── Pacing queue (video thread only) ─────────────────────── */

static size_t pacing_frame_bytes(const AVFrame *f)
{
	int size = av_image_get_buffer_size(f->format, f->width, f->height, 1);
	return size > 0 ? (size_t)size : 0;
}

static bool pacing_has_room(const struct irl_source *ctx)
{
	return ctx->pacing_count < IRL_VIDEO_PACING_MAX_FRAMES &&
	       ctx->pacing_bytes < IRL_VIDEO_PACING_MAX_BYTES;
}

static void pacing_push(struct irl_source *ctx, AVFrame *frame, uint64_t due_ns)
{
	int tail = (ctx->pacing_head + ctx->pacing_count) %
		   IRL_VIDEO_PACING_MAX_FRAMES;
	ctx->pacing_queue[tail].frame = frame;
	/* frame->pts is in nanoseconds here: the receiver thread rescaled it
	 * in irl_handle_video_frame() before queueing. */
	ctx->pacing_queue[tail].pts_ns = frame->pts;
	ctx->pacing_queue[tail].due_ns = due_ns;
	ctx->pacing_queue[tail].bytes = pacing_frame_bytes(frame);
	ctx->pacing_bytes += ctx->pacing_queue[tail].bytes;
	ctx->pacing_count++;
	if (ctx->pacing_count > ctx->pacing_peak)
		ctx->pacing_peak = ctx->pacing_count;
}

static struct irl_pacing_frame pacing_pop(struct irl_source *ctx)
{
	struct irl_pacing_frame e = ctx->pacing_queue[ctx->pacing_head];
	ctx->pacing_queue[ctx->pacing_head].frame = NULL;
	ctx->pacing_head = (ctx->pacing_head + 1) % IRL_VIDEO_PACING_MAX_FRAMES;
	ctx->pacing_count--;
	ctx->pacing_bytes -= e.bytes;
	return e;
}

static void pacing_drain(struct irl_source *ctx)
{
	while (ctx->pacing_count > 0) {
		struct irl_pacing_frame e = pacing_pop(ctx);
		av_frame_free(&e.frame);
	}
	ctx->pacing_bytes = 0;
}

/* Move everything the receiver has decoded into the pacing queue, copying it
 * out of the hardware frame pool on the way so the decoder gets its surfaces
 * back. Runs before every emit and wait, because holding a decoded frame in
 * video_queue is what stalls the decoder. */
static void pacing_intake(struct irl_source *ctx)
{
	for (;;) {
		irl_mutex_lock(&ctx->video_queue_lock);
		if (ctx->video_queue_count == 0 || !pacing_has_room(ctx)) {
			irl_mutex_unlock(&ctx->video_queue_lock);
			return;
		}
		AVFrame *f = ctx->video_queue[ctx->video_queue_head];
		ctx->video_queue[ctx->video_queue_head] = NULL;
		ctx->video_queue_head =
			(ctx->video_queue_head + 1) % IRL_VIDEO_QUEUE_SIZE;
		ctx->video_queue_count--;
		ctx->video_in_flight = 1;
		video_pinned_update_locked(ctx);
		irl_mutex_unlock(&ctx->video_queue_lock);

		AVFrame *sw = irl_video_to_sysmem(ctx, f);
		uint64_t due = sw ? irl_video_due_time(ctx, sw) : 0;
		av_frame_free(&f);

		irl_mutex_lock(&ctx->video_queue_lock);
		ctx->video_in_flight = 0;
		irl_mutex_unlock(&ctx->video_queue_lock);

		if (sw)
			pacing_push(ctx, sw, due);
	}
}

/* Re-derive every queued frame's due time from the offset as it stands now.
 *
 * The audio side reclaims playout latency two ways, and both used to leave
 * paced video behind for the depth of this queue. The speed controller moves
 * the offset continuously — a chunk emitted at +5% advances the OBS end by
 * frames_out/rate against a stream end that advanced by in_frames/rate — so a
 * frame frozen at intake showed ~5% of its residence late for the whole drain.
 * A re-anchor steps the offset outright, and video kept the pre-step schedule
 * until the queue emptied.
 *
 * Rescheduling against one offset per cycle preserves the spacing between
 * frames (their due times differ only by their PTS deltas) and moves the whole
 * queue with the audio it is mapped to, so video rides the same drain instead
 * of trailing it. */
static void pacing_reschedule(struct irl_source *ctx)
{
	int64_t offset_ns;
	if (ctx->pacing_count == 0 ||
	    !irl_video_playout_offset(ctx, &offset_ns))
		return;

	for (int i = 0; i < ctx->pacing_count; i++) {
		int idx = (ctx->pacing_head + i) % IRL_VIDEO_PACING_MAX_FRAMES;
		int64_t due = ctx->pacing_queue[idx].pts_ns + offset_ns;
		ctx->pacing_queue[idx].due_ns = due > 0 ? (uint64_t)due : 0;
	}
}

/* Emit every frame whose moment has arrived. Over the ceilings the head goes
 * out early rather than being dropped: too-early video is what the un-paced
 * path did all the time, and it beats a hole in the picture. */
static void pacing_emit_due(struct irl_source *ctx, uint64_t now)
{
	while (ctx->pacing_count > 0) {
		bool over = !pacing_has_room(ctx);
		uint64_t due = ctx->pacing_queue[ctx->pacing_head].due_ns;

		if (!over && (int64_t)(due - now) > IRL_VIDEO_PACING_SLACK_NS)
			return;
		if (over)
			ctx->pacing_overflows++;

		struct irl_pacing_frame e = pacing_pop(ctx);
		irl_video_output_frame(ctx, e.frame, e.due_ns);
		av_frame_free(&e.frame);
	}
}

void *irl_video_thread(void *data)
{
	struct irl_source *ctx = data;

	while (os_atomic_load_bool(&ctx->thread_active)) {
		bool clear;
		irl_mutex_lock(&ctx->video_queue_lock);
		clear = ctx->video_clear_pending;
		ctx->video_clear_pending = false;
		irl_mutex_unlock(&ctx->video_queue_lock);

		if (clear) {
			/* video_queue was already dropped by the requester;
			 * the paced frames behind it must go too, or the
			 * blank would be repainted a lead later. */
			pacing_drain(ctx);
			/* A cleared source is showing nothing; no reason to
			 * keep a lead's worth of recycled buffers resident
			 * while it does. The next frame rebuilds the pool. */
			irl_video_xfer_pool_release(ctx);
			/* The offset belongs to the connection that just
			 * ended; the next one brings its own PTS epoch. */
			ctx->video_playout_offset_ns = 0;
			ctx->video_playout_offset_time_ns = 0;
			obs_source_output_video(ctx->source, NULL);
			continue;
		}

		pacing_intake(ctx);
		/* Before both the emit and the sleep below, so each cycle
		 * schedules against the offset as it is now rather than as it
		 * was when the frames were decoded. */
		pacing_reschedule(ctx);
		pacing_emit_due(ctx, os_gettime_ns());

		irl_mutex_lock(&ctx->video_queue_lock);
		ctx->video_pacing_now = ctx->pacing_count;
		ctx->video_pacing_peak = ctx->pacing_peak;
		ctx->video_pacing_bytes = ctx->pacing_bytes;
		ctx->video_pacing_overflows = ctx->pacing_overflows;
		irl_mutex_unlock(&ctx->video_queue_lock);

		/* Sleep until the next frame is due, or until the receiver
		 * pushes, a clear arrives, or the thread is stopped. */
		uint32_t wait_ms = IRL_VIDEO_PACING_MAX_WAIT_MS;
		if (ctx->pacing_count > 0) {
			uint64_t now = os_gettime_ns();
			int64_t until = (int64_t)ctx->pacing_queue[ctx->pacing_head]
						.due_ns -
					(int64_t)now;
			if (until <= IRL_VIDEO_PACING_SLACK_NS)
				continue; /* due already; go round again */
			uint32_t ms = (uint32_t)(until / 1000000LL);
			if (ms < wait_ms)
				wait_ms = ms;
		}

		irl_mutex_lock(&ctx->video_queue_lock);
		/* Re-check under the lock: a push or clear between the work
		 * above and here would otherwise be slept through. */
		if (!ctx->video_clear_pending && ctx->video_queue_count == 0 &&
		    os_atomic_load_bool(&ctx->thread_active)) {
			if (wait_ms == 0)
				wait_ms = 1;
			irl_cond_timedwait(&ctx->video_queue_cond,
					   &ctx->video_queue_lock, wait_ms);
		}
		irl_mutex_unlock(&ctx->video_queue_lock);
	}

	pacing_drain(ctx);
	irl_video_xfer_pool_release(ctx);
	irl_mutex_lock(&ctx->video_queue_lock);
	video_queue_drain_locked(ctx);
	irl_mutex_unlock(&ctx->video_queue_lock);
	return NULL;
}

/* ── Decoded frame handling (receiver thread) ─────────────── */

static int64_t video_frame_pts(const AVFrame *frame)
{
	if (frame->best_effort_timestamp != AV_NOPTS_VALUE)
		return frame->best_effort_timestamp;
	if (frame->pts != AV_NOPTS_VALUE)
		return frame->pts;
	return AV_NOPTS_VALUE;
}

void irl_handle_video_frame(struct irl_source *ctx, AVFrame *frame)
{
	int64_t pts = video_frame_pts(frame);
	if (pts == AV_NOPTS_VALUE) {
		if (!ctx->video_skip_logged) {
			blog(LOG_WARNING,
			     "[irl-source] Dropping video frame without valid PTS");
			ctx->video_skip_logged = true;
		}
		return;
	}
	frame->pts = pts;

	if (!ctx->first_keyframe_received) {
		if (!irl_video_is_keyframe(frame)) {
			if (ctx->total_video_frames == 0)
				blog(LOG_DEBUG,
				     "[irl-source] Waiting for keyframe (dropped non-keyframe)");
			return;
		}

		ctx->first_keyframe_received = true;
		ctx->video_corrupted = false;
		/* hw_frames_ctx on the decoded frame is the ground truth
		 * for whether hardware decode is actually in use; the
		 * stream-open log only reports what was requested. */
		blog(LOG_INFO,
		     "[irl-source] First keyframe received (%dx%d fmt=%d %s decode)",
		     frame->width, frame->height, frame->format,
		     frame->hw_frames_ctx ? "hardware" : "software");
	}

	if (irl_video_is_keyframe(frame))
		ctx->video_corrupted = false;

	/* Damage the decoder reported on this frame. Both flags, because the
	 * decoders disagree on which to set: h264dec sets decode_error_flags
	 * on a frame it concealed and AV_FRAME_FLAG_CORRUPT only on frames
	 * before its first recovery point, while the HEVC decoder never sets
	 * decode_error_flags at all and reports its one kind of damage — a
	 * missing reference — as AV_FRAME_FLAG_CORRUPT on every frame
	 * predicted from it, until the next IDR/CRA. Checking only
	 * decode_error_flags left HEVC damage invisible. */
	bool frame_corrupt = (frame->flags & AV_FRAME_FLAG_CORRUPT) != 0;
	bool frame_damaged = frame_corrupt || frame->decode_error_flags != 0;
	if (frame_damaged)
		ctx->video_corrupt_frames++;

	/* HEVC has no error concealment: a reference that never arrived is
	 * synthesized as a flat mid-gray picture (hevc/refs.c
	 * generate_missing_ref; under hwaccel it is whatever stale surface
	 * the pool hands back), and everything predicted from it is gray with
	 * residuals painted on top. That is a worse picture than the last good
	 * frame, so hold it back and let OBS keep showing what it has; the
	 * chain heals at the next keyframe, and the decoder clears the flag
	 * there. H.264 keeps the passthrough: its concealment patches a
	 * damaged frame from the previous one, which is a usable picture, and
	 * its AV_FRAME_FLAG_CORRUPT never fires past the keyframe gate. */
	if (frame_corrupt && ctx->video_dec_ctx &&
	    ctx->video_dec_ctx->codec_id == AV_CODEC_ID_HEVC) {
		ctx->video_corrupt_held++;
		if (!ctx->video_hold_logged) {
			blog(LOG_WARNING,
			     "[irl-source] HEVC frame predicted from a missing reference; holding the last good frame until the next keyframe");
			ctx->video_hold_logged = true;
		}
		return;
	}
	if (ctx->video_hold_logged) {
		blog(LOG_INFO,
		     "[irl-source] Keyframe received, HEVC video resumed (%llu frames held this connection)",
		     (unsigned long long)ctx->video_corrupt_held);
		ctx->video_hold_logged = false;
	}

	if (ctx->video_corrupted || frame_damaged) {
		if (!ctx->video_skip_logged) {
			blog(LOG_WARNING,
			     "[irl-source] Passing through corrupt video frames to preserve cadence");
			ctx->video_skip_logged = true;
		}
	} else if (ctx->video_skip_logged) {
		blog(LOG_INFO,
		     "[irl-source] Clean video frame received, normal video cadence restored");
		ctx->video_skip_logged = false;
	}

	if (ctx->last_video_width && ctx->last_video_height &&
	    (frame->width != ctx->last_video_width ||
	     frame->height != ctx->last_video_height)) {
		blog(LOG_INFO,
		     "[irl-source] Resolution changed: %dx%d -> %dx%d",
		     ctx->last_video_width, ctx->last_video_height,
		     frame->width, frame->height);
		ctx->video_ts_init = false;
	}
	ctx->last_video_width = frame->width;
	ctx->last_video_height = frame->height;

	/* Convert PTS to nanoseconds here: the video thread must not
	 * touch fmt_ctx, which this thread frees on reconnect while
	 * queued frames may still be in flight. */
	int64_t pts_ns = 0;
	if (ctx->fmt_ctx && ctx->video_stream_idx >= 0) {
		AVStream *vs =
			ctx->fmt_ctx->streams[ctx->video_stream_idx];
		pts_ns = av_rescale_q(frame->pts, vs->time_base,
				      (AVRational){1, 1000000000});

		/* Frame interval EMA, for the video thread's estimate of how
		 * many frames a given output lead parks in the libobs async
		 * queue. Measured rather than taken from avg_frame_rate,
		 * which live SRT/RTMP demuxers routinely leave unset or
		 * wrong. Out-of-range deltas (PTS repair, discontinuities,
		 * reordering) are skipped rather than smoothed in. */
		int64_t delta = pts_ns - ctx->video_prev_pts_ns;
		bool usable_delta = ctx->video_prev_pts_ns != 0 &&
				    delta >= IRL_VIDEO_INTERVAL_MIN_NS &&
				    delta <= IRL_VIDEO_INTERVAL_MAX_NS;
		ctx->video_prev_pts_ns = pts_ns;

		irl_mutex_lock(&ctx->audio_state_lock);
		ctx->latest_video_stream_pts_ns = pts_ns;
		if (usable_delta) {
			if (ctx->video_frame_interval_ns == 0)
				ctx->video_frame_interval_ns = delta;
			else
				ctx->video_frame_interval_ns +=
					(delta - ctx->video_frame_interval_ns) /
					8;
		}
		irl_mutex_unlock(&ctx->audio_state_lock);
	}

	irl_video_queue_push(ctx, frame, pts_ns);
	ctx->total_video_frames++;
	if (ctx->total_video_frames == 1)
		blog(LOG_INFO, "[irl-source] First video frame queued");
}
