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

#include "../include/irl-source.h"

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

/* Convert stream PTS to OBS nanosecond timestamp, anchored to the
 * system clock at the time of the first frame.  This preserves the
 * inter-frame timing from the stream (smooth playback) while keeping
 * timestamps in OBS's clock domain. */
static uint64_t frame_timestamp(struct irl_source *ctx, const AVFrame *frame)
{
	AVStream *vs = ctx->fmt_ctx->streams[ctx->video_stream_idx];
	int64_t pts_ns = (int64_t)(frame->pts * 1000000000LL *
				   vs->time_base.num / vs->time_base.den);

	if (!ctx->video_ts_init) {
		ctx->video_sys_base = os_gettime_ns();
		ctx->video_pts_base = pts_ns;
		ctx->video_ts_init = true;
	}

	return ctx->video_sys_base + (uint64_t)(pts_ns - ctx->video_pts_base);
}

/* ── Video output ─────────────────────────────────────────── */

void irl_video_output_frame(struct irl_source *ctx, AVFrame *frame)
{
	/* Hardware-decoded frames (NVDEC/D3D11VA/VAAPI) need to be transferred to CPU */
	AVFrame *sw_frame = NULL;
	if (frame->hw_frames_ctx) {
		sw_frame = av_frame_alloc();
		if (!sw_frame)
			return;
		if (av_hwframe_transfer_data(sw_frame, frame, 0) < 0) {
			av_frame_free(&sw_frame);
			return;
		}
		sw_frame->pts = frame->pts;
		sw_frame->colorspace = frame->colorspace;
		sw_frame->color_range = frame->color_range;
		sw_frame->color_trc = frame->color_trc;
		sw_frame->color_primaries = frame->color_primaries;
		sw_frame->flags = frame->flags;
		frame = sw_frame;
	}

	enum video_format obs_fmt = avpixfmt_to_obs(frame->format);

	/* If format not directly supported, convert to NV12 via swscale */
	if (obs_fmt == VIDEO_FORMAT_NONE) {
		if (!ctx->sws_ctx || ctx->sws_src_w != frame->width ||
		    ctx->sws_src_h != frame->height ||
		    ctx->sws_src_fmt != frame->format) {
			if (ctx->sws_ctx)
				sws_freeContext(ctx->sws_ctx);

			blog(LOG_INFO,
			     "[irl-source] Converting pixel format %d to NV12 via swscale (%dx%d)",
			     frame->format, frame->width, frame->height);

			ctx->sws_ctx = sws_getContext(
				frame->width, frame->height, frame->format,
				frame->width, frame->height, AV_PIX_FMT_NV12,
				SWS_FAST_BILINEAR, NULL, NULL, NULL);
			ctx->sws_src_w = frame->width;
			ctx->sws_src_h = frame->height;
			ctx->sws_src_fmt = frame->format;
		}

		if (!ctx->sws_ctx)
			return;

		int y_size = frame->width * frame->height;
		int uv_size = y_size / 2;
		uint8_t *nv12_data = malloc(y_size + uv_size);
		if (!nv12_data)
			return;

		uint8_t *dst_planes[2] = {nv12_data, nv12_data + y_size};
		int dst_strides[2] = {frame->width, frame->width};

		sws_scale(ctx->sws_ctx, (const uint8_t *const *)frame->data,
			  frame->linesize, 0, frame->height, dst_planes,
			  dst_strides);

		struct obs_source_frame obs_frame = {0};
		obs_frame.width = frame->width;
		obs_frame.height = frame->height;
		obs_frame.format = VIDEO_FORMAT_NV12;
		obs_frame.data[0] = dst_planes[0];
		obs_frame.data[1] = dst_planes[1];
		obs_frame.linesize[0] = dst_strides[0];
		obs_frame.linesize[1] = dst_strides[1];
		obs_frame.timestamp = frame_timestamp(ctx, frame);
		setup_color_params(&obs_frame, frame, VIDEO_FORMAT_NV12);

		obs_source_output_video(ctx->source, &obs_frame);
		free(nv12_data);
		if (sw_frame)
			av_frame_free(&sw_frame);
		return;
	}

	/* Direct output for natively supported formats (zero-copy) */
	struct obs_source_frame obs_frame = {0};
	obs_frame.width = frame->width;
	obs_frame.height = frame->height;
	obs_frame.format = obs_fmt;
	obs_frame.timestamp = frame_timestamp(ctx, frame);
	setup_color_params(&obs_frame, frame, obs_fmt);

	for (int i = 0; i < AV_NUM_DATA_POINTERS; i++) {
		obs_frame.data[i] = frame->data[i];
		obs_frame.linesize[i] = abs(frame->linesize[i]);
	}

	obs_source_output_video(ctx->source, &obs_frame);
	if (sw_frame)
		av_frame_free(&sw_frame);
}
