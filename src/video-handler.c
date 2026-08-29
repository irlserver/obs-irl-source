/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * video-handler.c — Keyframe gate, frame output, format conversion
 *
 * Converts decoded AVFrames into OBS async video frames.
 */

#include <stdlib.h>

#include <libavutil/imgutils.h>

#include "../include/irl-source.h"

/* ── swscale backend selection ────────────────────────────── */

/*
 * FFmpeg 9.0 landed the swscale rewrite: conversions are decomposed into
 * elementary ops compiled into kernel chains (memcpy fast paths, chained x86
 * SIMD, AArch64 NEON, SPIR-V). The plugin's one swscale use is a same-size
 * pixel-format conversion, which is exactly the shape those chains target.
 *
 * Reaching them takes more than a flag. sws_getContext() sets is_legacy_init,
 * and sws_scale() refuses to run without it and calls straight into the legacy
 * scaler — swscale.h is blunt about the consequence: "The stateful legacy API
 * always implies SWS_BACKEND_LEGACY." Setting SWS_UNSTABLE on a getContext
 * context is silently ignored. The new backends exist only behind the dynamic
 * API: sws_alloc_context() with no sws_init_context(), driven by
 * sws_scale_frame(), which is what irl_convert_to_nv12 below does.
 *
 * With that in place SWS_UNSTABLE makes ff_sws_enabled_backends() offer the
 * new backends and prefer_ops_backend() route the conversion through the op
 * chain, falling back to the legacy pass on AVERROR(ENOTSUP). Upstream calls
 * the whole thing "for testing and debugging purposes only", with "semantics
 * subject to change at any point in time".
 *
 * As of 9.0 that fallback is what actually happens here, every time. The op
 * chain only builds a pass when the conversion needs no chroma resampling:
 * yuv420p -> yuv420p compiles, but yuv420p -> nv12 (and -> rgba, -> gray8)
 * all fail ff_sws_op_list_generate() with ENOTSUP, measured by pinning
 * SwsContext.backends to each backend in turn so nothing could silently
 * substitute the legacy scaler. Every format OBS makes us convert is
 * subsampled, so the flag currently costs one failed pass-generation per
 * format change and produces bit-identical output.
 *
 * It is wired up anyway because this is where upstream's work is going, and
 * the day the op chain learns subsampled formats it wants to be one env var
 * away rather than a refactor away. Opt in at runtime with IRL_SWS_UNSTABLE=1
 * before starting OBS; a runtime switch is what makes the comparison worth
 * anything, since the two paths can then be A/B'd against the same live
 * stream in one session instead of across two streams that never match. The
 * default stays on the legacy backend, so nothing built from here ships an
 * experimental converter by accident.
 */
#if LIBSWSCALE_VERSION_MAJOR >= 10
static bool sws_unstable_enabled(void)
{
	static int unstable = -1;

	if (unstable < 0) {
		const char *env = getenv("IRL_SWS_UNSTABLE");
		unstable = (env && *env && *env != '0') ? 1 : 0;
		blog(LOG_INFO, "[irl-source] swscale backend: %s",
		     unstable ? "experimental (IRL_SWS_UNSTABLE=1)" : "legacy");
	}

	return unstable == 1;
}
#endif

/*
 * Convert frame to NV12 in ctx->sws_nv12_buf. Returns false if the scaler
 * could not be built or the conversion failed.
 *
 * In dynamic mode the context carries no dimensions or formats — every
 * property comes from the frames, and sws_scale_frame() reconfigures itself
 * when they change. The src_w/src_h/src_fmt bookkeeping is kept anyway so the
 * "Converting pixel format ..." line still fires once per format rather than
 * once per frame.
 */
static bool irl_convert_to_nv12(struct irl_source *ctx, const AVFrame *frame,
				uint8_t *const dst_planes[2],
				const int dst_strides[2])
{
#if LIBSWSCALE_VERSION_MAJOR < 10
	/*
	 * Pre-9.0 fallback, reachable only through -DIRL_BUNDLED_FFMPEG=OFF
	 * against an older system FFmpeg. SwsContext is opaque there and the
	 * dynamic API predates the backends this exists to reach, so keep the
	 * legacy scaler rather than partially emulating the new one.
	 */
	if (!ctx->sws_ctx) {
		ctx->sws_ctx = sws_getContext(frame->width, frame->height,
					      frame->format, frame->width,
					      frame->height, AV_PIX_FMT_NV12,
					      SWS_FAST_BILINEAR, NULL, NULL,
					      NULL);
		if (!ctx->sws_ctx)
			return false;
	}

	sws_scale(ctx->sws_ctx, (const uint8_t *const *)frame->data,
		  frame->linesize, 0, frame->height, dst_planes, dst_strides);
	return true;
#else
	if (!ctx->sws_ctx) {
		ctx->sws_ctx = sws_alloc_context();
		if (!ctx->sws_ctx)
			return false;
		/* No sws_init_context(): that is what would mark the context
		 * legacy and lock it to the legacy backend. */
		ctx->sws_ctx->flags = sws_unstable_enabled() ? SWS_UNSTABLE : 0;
	}

	if (!ctx->sws_dst_frame) {
		ctx->sws_dst_frame = av_frame_alloc();
		if (!ctx->sws_dst_frame)
			return false;
	}

	AVFrame *dst = ctx->sws_dst_frame;
	dst->width = frame->width;
	dst->height = frame->height;
	dst->format = AV_PIX_FMT_NV12;
	dst->data[0] = dst_planes[0];
	dst->data[1] = dst_planes[1];
	dst->data[2] = NULL;
	dst->data[3] = NULL;
	dst->linesize[0] = dst_strides[0];
	dst->linesize[1] = dst_strides[1];
	dst->linesize[2] = 0;
	dst->linesize[3] = 0;

	/*
	 * dst->data[0] being set puts sws_scale_frame() on its user-provided
	 * buffer path, so it neither allocates nor references anything here.
	 *
	 * The colour properties have to be copied across. Dynamic mode reads
	 * them from the frames, so leaving the destination unspecified would
	 * invite a colourspace conversion that the legacy path never did —
	 * this is a pixel format change only. setup_color_params() reports the
	 * same properties to OBS from the source frame, so matching them here
	 * is also what keeps the two consistent.
	 */
	dst->colorspace = frame->colorspace;
	dst->color_range = frame->color_range;
	dst->color_primaries = frame->color_primaries;
	dst->color_trc = frame->color_trc;
	dst->chroma_location = frame->chroma_location;

	int ret = sws_scale_frame(ctx->sws_ctx, dst, frame);
	if (ret < 0) {
		char err[AV_ERROR_MAX_STRING_SIZE];
		av_strerror(ret, err, sizeof(err));
		blog(LOG_WARNING, "[irl-source] swscale conversion failed: %s",
		     err);
		return false;
	}

	return true;
#endif
}

/* ── Format mapping ───────────────────────────────────────── */

static enum video_format avpixfmt_to_obs(enum AVPixelFormat fmt)
{
	switch (fmt) {
	case AV_PIX_FMT_YUV420P:
	case AV_PIX_FMT_YUVJ420P:
		return VIDEO_FORMAT_I420;
	case AV_PIX_FMT_YUV420P10LE:
		return VIDEO_FORMAT_I010;
	case AV_PIX_FMT_NV12:
		return VIDEO_FORMAT_NV12;
	case AV_PIX_FMT_P010LE:
		return VIDEO_FORMAT_P010;
	case AV_PIX_FMT_YUV422P:
	case AV_PIX_FMT_YUVJ422P:
		return VIDEO_FORMAT_I422;
	case AV_PIX_FMT_YUV444P:
	case AV_PIX_FMT_YUVJ444P:
		return VIDEO_FORMAT_I444;
	case AV_PIX_FMT_UYVY422:
		return VIDEO_FORMAT_UYVY;
	case AV_PIX_FMT_YUYV422:
		return VIDEO_FORMAT_YUY2;
	case AV_PIX_FMT_RGBA:
		return VIDEO_FORMAT_RGBA;
	case AV_PIX_FMT_BGRA:
		return VIDEO_FORMAT_BGRA;
	default:
		return VIDEO_FORMAT_NONE;
	}
}

/* ── Keyframe detection ───────────────────────────────────── */

/* True if the frame is a keyframe (IDR/CRA/I-frame). */
bool irl_video_is_keyframe(const AVFrame *frame)
{
	return (frame->flags & AV_FRAME_FLAG_KEY) != 0;
}

/* ── Color space helpers ──────────────────────────────────── */

static enum video_colorspace
convert_color_space(enum AVColorSpace cs, enum AVColorTransferCharacteristic trc,
		    enum AVColorPrimaries prm)
{
	switch (cs) {
	case AVCOL_SPC_BT709:
		return VIDEO_CS_709;
	case AVCOL_SPC_SMPTE170M:
	case AVCOL_SPC_BT470BG:
		return VIDEO_CS_601;
	case AVCOL_SPC_BT2020_NCL:
	case AVCOL_SPC_BT2020_CL:
		if (trc == AVCOL_TRC_ARIB_STD_B67)
			return VIDEO_CS_2100_HLG;
		return VIDEO_CS_2100_PQ;
	default:
		break;
	}
	(void)prm;
	return VIDEO_CS_709;
}

static enum video_range_type convert_color_range(enum AVColorRange range)
{
	return range == AVCOL_RANGE_JPEG ? VIDEO_RANGE_FULL
					 : VIDEO_RANGE_PARTIAL;
}

static void setup_color_params(struct obs_source_frame *obs_frame,
			       const AVFrame *frame,
			       enum video_format out_fmt)
{
	enum video_colorspace cs = convert_color_space(
		frame->colorspace, frame->color_trc, frame->color_primaries);
	enum video_range_type range = convert_color_range(frame->color_range);
	obs_frame->full_range = (range == VIDEO_RANGE_FULL);

	video_format_get_parameters_for_format(cs, range, out_fmt,
					       obs_frame->color_matrix,
					       obs_frame->color_range_min,
					       obs_frame->color_range_max);
}

/* ── Timestamp sync ───────────────────────────────────────── */

/* Drift threshold for video PTS clamping (500ms).
 *
 * Instead of re-anchoring (which causes visible timeline jumps),
 * clamp the computed timestamp to a reasonable range:
 * - Too far behind wall clock: display immediately (use `now`)
 * - Too far ahead: cap at `now + 200ms`
 *
 * The anchor stays unchanged — normal frames self-correct after
 * the burst.  This produces smooth video with no visible skips.
 * Backward drift catches up via brief speedup (frames displayed
 * immediately until PTS catches up).  Forward drift is capped
 * so OBS doesn't hold the previous frame for too long. */
#define VIDEO_TS_CLAMP_NS 500000000LL   /* 500ms */
#define VIDEO_TS_CAP_NS   200000000ULL  /* 200ms forward cap */

/* Record how far ahead of wall clock the PTS mapping placed this frame.
 *
 * This used to clamp the lead as well, to keep libobs's async queue under its
 * 30-frame wipe threshold. That clamp is gone: it acted on the lead's
 * distance from the configured target, but what libobs actually queues is the
 * lead's *growth* since its play head last anchored. A large but steady lead
 * — a jitter buffer parked against the bleed ceiling because the sender
 * over-delivers, say — queues nothing, and clamping it only shifted video
 * ahead of audio. A 720p120 stream showed the cost plainly: a steady 1032ms
 * lead clamped to 520ms, so half a second of permanent desync bought against
 * a queue that was very likely one frame deep.
 *
 * The measurement stays, because it is the signal for whether video pacing
 * (which removes libobs's scheduler from the path entirely, and with it this
 * whole threshold) is doing its job. */
static void video_record_lead(struct irl_source *ctx, int64_t ts, uint64_t now,
			      int64_t frame_interval_ns)
{
	int64_t lead_ns = ts - (int64_t)now;

	if (frame_interval_ns <= 0)
		frame_interval_ns = IRL_VIDEO_INTERVAL_DEFAULT_NS;

	/* The lead libobs could absorb if the whole of it were growth. */
	int64_t budget_ns = IRL_OBS_ASYNC_FRAME_BUDGET * frame_interval_ns;
	int64_t floor_ns = AUDIO_OFFSET_REANCHOR_MARGIN_MS * 1000000LL;
	int64_t queue_safe_ns =
		os_atomic_load_long(&ctx->config.buffer_target_ms) * 1000000LL +
		(budget_ns < floor_ns ? floor_ns : budget_ns);

	irl_mutex_lock(&ctx->audio_state_lock);
	ctx->video_lead_ns = lead_ns;
	/* Keep the high-water mark too: stats are sampled every 30s, and an
	 * excursion that drains in ~17s is very likely to fall between two
	 * samples. The instantaneous value alone would read healthy on
	 * exactly the streams this is meant to diagnose. */
	if (lead_ns > ctx->video_lead_peak_ns)
		ctx->video_lead_peak_ns = lead_ns;
	if (lead_ns > queue_safe_ns)
		ctx->video_lead_excess++;
	irl_mutex_unlock(&ctx->audio_state_lock);

	if (lead_ns <= queue_safe_ns)
		return;

	/* Only a risk while the lead is still climbing — a steady lead of any
	 * size is free — so this is a "watch this" line, not a fault. */
	if (now - ctx->video_lead_warn_time_ns >=
	    IRL_VIDEO_LEAD_WARN_INTERVAL_NS) {
		ctx->video_lead_warn_time_ns = now;
		blog(LOG_INFO,
		     "[irl-source] Video lead %lldms is beyond what OBS can queue (%lldms at %.0ffps); "
		     "harmless while it holds steady, but a rise of that size would make OBS drop queued video",
		     (long long)(lead_ns / 1000000LL),
		     (long long)(queue_safe_ns / 1000000LL),
		     1000000000.0 / (double)frame_interval_ns);
	}
}

/* Convert stream PTS to OBS nanosecond timestamp.
 *
 * When audio is active, treat queued audio as the master playout
 * clock: map video PTS through the same stream-PTS → OBS-clock
 * offset used by the latest audio chunk already handed to OBS.
 * This keeps lip sync stable even when buffered audio or adaptive
 * audio speed changes the effective playout offset.
 *
 * If no audio playout mapping exists yet, fall back to the older
 * video-only wall-clock anchor. */
uint64_t irl_video_due_time(struct irl_source *ctx, const AVFrame *frame)
{
	/* frame->pts is pre-converted to nanoseconds by the receiver
	 * thread (see irl_video_queue_push); fmt_ctx must not be
	 * touched here, it can be freed mid-reconnect. */
	int64_t pts_ns = frame->pts;
	uint64_t now = os_gettime_ns();

	/* Snapshot audio-thread-owned fields under the lock. */
	uint64_t audio_obs_end_ts_ns;
	int64_t audio_buffered_end_pts_ns;
	int startup_warmup_ms;
	int64_t frame_interval_ns;
	irl_mutex_lock(&ctx->audio_state_lock);
	audio_obs_end_ts_ns = ctx->latest_audio_obs_end_ts_ns;
	audio_buffered_end_pts_ns = ctx->latest_audio_buffered_end_pts_ns;
	startup_warmup_ms = ctx->startup_audio_warmup_remaining_ms;
	frame_interval_ns = ctx->video_frame_interval_ns;
	irl_mutex_unlock(&ctx->audio_state_lock);

	/* No audio_stream_idx test: a published mapping already implies the
	 * pump handed OBS a real chunk, so it implies the audio stream. */
	if (audio_obs_end_ts_ns != 0 && audio_buffered_end_pts_ns > 0) {
		int64_t mapped = (int64_t)pts_ns +
				 ((int64_t)audio_obs_end_ts_ns -
				  audio_buffered_end_pts_ns);
		if (mapped < 0)
			mapped = 0;
		video_record_lead(ctx, mapped, now, frame_interval_ns);
		return (uint64_t)mapped;
	}

	if (!ctx->video_ts_init) {
		ctx->video_sys_base = now;
		ctx->video_pts_base = pts_ns;
		ctx->video_ts_init = true;
	}

	uint64_t computed = ctx->video_sys_base +
			    (uint64_t)(pts_ns - ctx->video_pts_base);
	int64_t drift = (int64_t)computed - (int64_t)now;

	/* Clamp without re-anchoring — no visible skip, anchor
	 * stays stable so subsequent frames self-correct. */
	if (drift < -(int64_t)VIDEO_TS_CLAMP_NS) {
		computed = now;
	} else if (drift > (int64_t)VIDEO_TS_CLAMP_NS) {
		computed = now + VIDEO_TS_CAP_NS;
	}

	/* Startup fallback before the audio playout mapping exists. Here the
	 * mapping cannot stand in for "there is audio" — the whole point is
	 * that it does not exist yet — so this reads the atomic mirror the
	 * receiver thread publishes rather than audio_stream_idx, which it
	 * rewrites underneath us on every reconnect. */
	if (os_atomic_load_bool(&ctx->audio_stream_present)) {
		int64_t audio_lead_ns = 0;
		if (audio_obs_end_ts_ns == 0) {
			audio_lead_ns = (int64_t)startup_warmup_ms * 1000000LL;
			if (!ctx->config.low_latency_audio) {
				audio_lead_ns +=
					os_atomic_load_long(
						&ctx->config.buffer_target_ms) *
					1000000LL;
			}
		}
		if (audio_lead_ns > 0)
			computed += (uint64_t)audio_lead_ns;
	}

	video_record_lead(ctx, (int64_t)computed, now, frame_interval_ns);
	return computed;
}

/* The current stream-PTS → OBS-clock offset, for re-deriving the due time of
 * frames already queued. Deliberately free of the side effects in
 * irl_video_due_time() above — the lead stats, the warning log and the
 * video-only fallback anchor all belong to a frame arriving, and running them
 * again for every queued frame on every pacing cycle would report the queue
 * rather than the stream.
 *
 * Returns false when there is no audio to slave to, or when the mapping has
 * been gone long enough that holding it would be a guess; callers keep the
 * due time the frame arrived with.
 *
 * Audio presence is read from the mapping fields rather than from
 * audio_stream_idx, which the receiver thread rewrites on every reconnect and
 * which nothing here could read without a race. The two are equivalent for
 * this purpose: the pump is the only writer of those fields, it only ever
 * publishes them for a real chunk it handed to OBS, and reset_runtime_state()
 * zeroes both them and the cache below before a connection starts. A stream
 * with no audio therefore never gets past the hold check. */
bool irl_video_playout_offset(struct irl_source *ctx, int64_t *offset_ns)
{
	uint64_t obs_end;
	int64_t buffered_end;
	irl_mutex_lock(&ctx->audio_state_lock);
	obs_end = ctx->latest_audio_obs_end_ts_ns;
	buffered_end = ctx->latest_audio_buffered_end_pts_ns;
	irl_mutex_unlock(&ctx->audio_state_lock);

	uint64_t now = os_gettime_ns();
	if (obs_end != 0 && buffered_end > 0) {
		ctx->video_playout_offset_ns =
			(int64_t)obs_end - buffered_end;
		ctx->video_playout_offset_time_ns = now;
	} else if (ctx->video_playout_offset_time_ns == 0 ||
		   now - ctx->video_playout_offset_time_ns >
			   IRL_VIDEO_OFFSET_HOLD_NS) {
		return false;
	}

	*offset_ns = ctx->video_playout_offset_ns;
	return true;
}

/* ── Video output ─────────────────────────────────────────── */

/* Plane alignment for pooled transfer destinations; what
 * av_frame_get_buffer() would pick on a modern x86 (AVX-512 stores). */
#define XFER_PLANE_ALIGN 64

/* Give `out` a pixel buffer from the recycled pool, laid out the way
 * av_hwframe_transfer_data() would have allocated one itself: the hardware
 * pool's software format, dimensions padded to 16 (backends copy in aligned
 * blocks; every one of them clips the copy to the source size). The caller
 * restores the display dimensions after the transfer, exactly as FFmpeg's
 * own transfer_data_alloc() does. */
static bool xfer_frame_from_pool(struct irl_source *ctx, AVFrame *out,
				 const AVFrame *src)
{
	const AVHWFramesContext *hwfc =
		(const AVHWFramesContext *)src->hw_frames_ctx->data;
	enum AVPixelFormat fmt = hwfc->sw_format;
	int w = FFALIGN(src->width, 16);
	int h = FFALIGN(src->height, 16);

	if (!ctx->video_xfer_pool || ctx->video_xfer_pool_w != w ||
	    ctx->video_xfer_pool_h != h || ctx->video_xfer_pool_fmt != fmt) {
		int size = av_image_get_buffer_size(fmt, w, h,
						    XFER_PLANE_ALIGN);
		if (size <= 0)
			return false;
		av_buffer_pool_uninit(&ctx->video_xfer_pool);
		ctx->video_xfer_pool = av_buffer_pool_init((size_t)size, NULL);
		if (!ctx->video_xfer_pool)
			return false;
		ctx->video_xfer_pool_w = w;
		ctx->video_xfer_pool_h = h;
		ctx->video_xfer_pool_fmt = fmt;
		blog(LOG_INFO,
		     "[irl-source] Transfer buffer pool: %dx%d fmt=%d, %.1f MB/frame",
		     w, h, fmt, (double)size / (1024.0 * 1024.0));
	}

	AVBufferRef *buf = av_buffer_pool_get(ctx->video_xfer_pool);
	if (!buf)
		return false;

	out->format = fmt;
	out->width = w;
	out->height = h;
	if (av_image_fill_arrays(out->data, out->linesize, buf->data, fmt, w,
				 h, XFER_PLANE_ALIGN) < 0) {
		av_buffer_unref(&buf);
		return false;
	}
	out->buf[0] = buf;
	return true;
}

/* Release the hardware frame transfer pool (called on resolution change or shutdown). */
void irl_video_xfer_pool_release(struct irl_source *ctx)
{
	/* Safe with pooled buffers still alive in the pacing queue: the pool
	 * lingers internally until its last buffer is returned. */
	av_buffer_pool_uninit(&ctx->video_xfer_pool);
	ctx->video_xfer_pool_w = 0;
	ctx->video_xfer_pool_h = 0;
	ctx->video_xfer_pool_fmt = AV_PIX_FMT_NONE;
}

/* Bring a decoded frame into system memory, releasing any decoder surface it
 * held. Returns a new reference the caller owns, or NULL.
 *
 * This deliberately does not use av_hwframe_map(AV_HWFRAME_MAP_READ), which
 * earlier gave VAAPI and VideoToolbox a zero-copy CPU view. A mapped frame
 * still pins the surface it maps, and pacing holds frames for the whole
 * output lead — hundreds of them at a high frame rate — so mapping would
 * exhaust the decoder pool within a few frames of the buffer filling. The
 * copy is what makes the surface reusable, so it is not optional here.
 *
 * On those two backends this costs one extra copy against the old path; on
 * D3D11VA and CUDA, where the map always fell back to a copy anyway, nothing
 * changes.
 *
 * The destination buffer comes from video_xfer_pool rather than a fresh
 * heap allocation: letting av_hwframe_transfer_data() allocate meant a full
 * frame's worth of malloc/free per frame, which at 4K goes straight to the
 * OS and pays page zeroing plus thousands of soft page faults per frame —
 * measured as a first-order CPU cost on multi-source 4K60 setups. */
AVFrame *irl_video_to_sysmem(struct irl_source *ctx, AVFrame *frame)
{
	AVFrame *out = av_frame_alloc();
	if (!out)
		return NULL;

	if (!frame->hw_frames_ctx) {
		/* Already system memory: take a reference so the pacing queue
		 * owns its entries uniformly. */
		if (av_frame_ref(out, frame) < 0) {
			av_frame_free(&out);
			return NULL;
		}
		return out;
	}

	bool pooled = !ctx->video_xfer_pool_broken &&
		      xfer_frame_from_pool(ctx, out, frame);
	if (pooled && av_hwframe_transfer_data(out, frame, 0) < 0) {
		/* This backend refuses a caller-allocated destination; stop
		 * offering one. The retry below with an empty frame is the
		 * old let-FFmpeg-allocate path, so behaviour degrades to
		 * exactly what shipped before the pool. */
		ctx->video_xfer_pool_broken = true;
		irl_video_xfer_pool_release(ctx);
		blog(LOG_WARNING,
		     "[irl-source] Pooled hw frame transfer rejected; falling back to per-frame allocation");
		av_frame_unref(out);
		pooled = false;
	}
	if (!pooled && av_hwframe_transfer_data(out, frame, 0) < 0) {
		av_frame_free(&out);
		return NULL;
	}
	if (pooled) {
		/* Undo the 16-alignment padding: OBS must see the display
		 * size, not the surface size. */
		out->width = frame->width;
		out->height = frame->height;
	}

	out->pts = frame->pts;
	out->colorspace = frame->colorspace;
	out->color_range = frame->color_range;
	out->color_trc = frame->color_trc;
	out->color_primaries = frame->color_primaries;
	out->flags = frame->flags;
	return out;
}

/* Hand a system-memory frame to OBS with the timestamp pacing scheduled it
 * for. `frame` must already have been through irl_video_to_sysmem(). Returns
 * true only when the frame was submitted. */
bool irl_video_output_frame(struct irl_source *ctx, AVFrame *frame,
			    uint64_t timestamp)
{
	enum video_format obs_fmt = avpixfmt_to_obs(frame->format);

	/* Negative linesize means the frame is laid out bottom-up. OBS's
	 * async path expects positive strides, so taking abs() would
	 * silently flip the image vertically. Route through swscale
	 * instead. Real-world FFmpeg decoders almost never produce this,
	 * but cheap to be defensive. */
	bool negative_stride = false;
	for (int i = 0; i < AV_NUM_DATA_POINTERS; i++) {
		if (frame->data[i] && frame->linesize[i] < 0) {
			negative_stride = true;
			break;
		}
	}
	if (negative_stride)
		obs_fmt = VIDEO_FORMAT_NONE;

	/* If format not directly supported, convert to NV12 via swscale */
	if (obs_fmt == VIDEO_FORMAT_NONE) {
		if (ctx->sws_src_w != frame->width ||
		    ctx->sws_src_h != frame->height ||
		    ctx->sws_src_fmt != frame->format) {
			blog(LOG_INFO,
			     "[irl-source] Converting pixel format %d to NV12 via swscale (%dx%d)",
			     frame->format, frame->width, frame->height);

			ctx->sws_src_w = frame->width;
			ctx->sws_src_h = frame->height;
			ctx->sws_src_fmt = frame->format;
		}

		size_t y_size = (size_t)frame->width * frame->height;
		size_t uv_size = y_size / 2;
		size_t need = y_size + uv_size;
		if (need > ctx->sws_nv12_buf_capacity) {
			uint8_t *next = realloc(ctx->sws_nv12_buf, need);
			if (!next)
				return false;
			ctx->sws_nv12_buf = next;
			ctx->sws_nv12_buf_capacity = need;
		}

		uint8_t *dst_planes[2] = {ctx->sws_nv12_buf,
					  ctx->sws_nv12_buf + y_size};
		int dst_strides[2] = {frame->width, frame->width};

		if (!irl_convert_to_nv12(ctx, frame, dst_planes, dst_strides))
			return false;

		struct obs_source_frame obs_frame = {0};
		obs_frame.width = frame->width;
		obs_frame.height = frame->height;
		obs_frame.format = VIDEO_FORMAT_NV12;
		obs_frame.data[0] = dst_planes[0];
		obs_frame.data[1] = dst_planes[1];
		obs_frame.linesize[0] = dst_strides[0];
		obs_frame.linesize[1] = dst_strides[1];
		obs_frame.timestamp = timestamp;
		setup_color_params(&obs_frame, frame, VIDEO_FORMAT_NV12);

		obs_source_output_video(ctx->source, &obs_frame);
		return true;
	}

	/* Direct output for natively supported formats (zero-copy) */
	struct obs_source_frame obs_frame = {0};
	obs_frame.width = frame->width;
	obs_frame.height = frame->height;
	obs_frame.format = obs_fmt;
	obs_frame.timestamp = timestamp;
	setup_color_params(&obs_frame, frame, obs_fmt);

	for (int i = 0; i < AV_NUM_DATA_POINTERS; i++) {
		obs_frame.data[i] = frame->data[i];
		/* Linesize is non-negative here: negative_stride above
		 * routes to the swscale path. */
		obs_frame.linesize[i] =
			frame->linesize[i] > 0 ? frame->linesize[i] : 0;
	}

	obs_source_output_video(ctx->source, &obs_frame);
	return true;
}
