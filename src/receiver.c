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

#include <stdlib.h>
#include <string.h>

#ifdef _MSC_VER
#define strtok_r strtok_s
#endif

#include "../include/irl-source.h"

/* ── Internal helpers ─────────────────────────────────────── */

static void apply_demuxer_options(AVDictionary **opts, const char *url,
				  const char *extra, int network_buffer_mb)
{
	/* Live-stream tuned defaults.
	 * HEVC/H.265 over SRT needs enough probe data to capture a keyframe
	 * with SPS/PPS — 500KB/0.5s is too small for typical 2-4s GOPs. */
	av_dict_set(opts, "probesize", "5000000", 0);   /* 5 MB */
	av_dict_set(opts, "analyzeduration", "5000000", 0); /* 5 s */
	av_dict_set(opts, "fflags", "+discardcorrupt+genpts", 0);
	av_dict_set(opts, "reconnect", "1", 0);
	av_dict_set(opts, "reconnect_streamed", "1", 0);

	/* Network buffer: absorbs transport-level jitter before decoding.
	 * Higher values = more resilient to network spikes, but add latency. */
	if (network_buffer_mb > 0) {
		char buf_size[32];
		snprintf(buf_size, sizeof(buf_size), "%d",
			 network_buffer_mb * 1024 * 1024);
		av_dict_set(opts, "buffer_size", buf_size, 0);
	}

	/* SRT-specific: set receive buffer and latency */
	if (url && strstr(url, "srt://")) {
		av_dict_set(opts, "latency", "200000", 0); /* 200ms default */
		if (network_buffer_mb > 0) {
			char recv_buf[32];
			snprintf(recv_buf, sizeof(recv_buf), "%d",
				 network_buffer_mb * 1024 * 1024);
			av_dict_set(opts, "recv_buffer_size", recv_buf, 0);
		}
	}

	/* User-provided overrides */
	if (extra && *extra) {
		/* Parse "key1=val1 key2=val2" format */
		char *dup = av_strdup(extra);
		char *saveptr = NULL;
		char *token = strtok_r(dup, " ", &saveptr);
		while (token) {
			char *eq = strchr(token, '=');
			if (eq) {
				*eq = '\0';
				av_dict_set(opts, token, eq + 1, 0);
			}
			token = strtok_r(NULL, " ", &saveptr);
		}
		av_free(dup);
	}
}

/* Hardware decode preference order — tries each until one works.
 * Covers NVIDIA (CUDA), Intel (QSV/VAAPI), AMD (D3D11VA/VAAPI). */
static const enum AVHWDeviceType hw_device_types[] = {
#ifdef _WIN32
	AV_HWDEVICE_TYPE_D3D11VA,     /* AMD + Intel + NVIDIA on Windows */
	AV_HWDEVICE_TYPE_CUDA,        /* NVIDIA NVDEC */
#elif defined(__APPLE__)
	AV_HWDEVICE_TYPE_VIDEOTOOLBOX, /* Apple VideoToolbox */
#else
	AV_HWDEVICE_TYPE_VAAPI,       /* Intel + AMD on Linux */
	AV_HWDEVICE_TYPE_CUDA,        /* NVIDIA on Linux */
#endif
	AV_HWDEVICE_TYPE_NONE,        /* sentinel */
};


static AVCodecContext *open_decoder(struct irl_source *src, AVStream *stream,
				    bool try_hw)
{
	const AVCodec *codec = avcodec_find_decoder(stream->codecpar->codec_id);
	if (!codec)
		return NULL;

	AVCodecContext *ctx = avcodec_alloc_context3(codec);
	if (!ctx)
		return NULL;

	if (avcodec_parameters_to_context(ctx, stream->codecpar) < 0) {
		avcodec_free_context(&ctx);
		return NULL;
	}

	ctx->thread_count = 0; /* auto */

	/* Try hardware decoding for video streams */
	if (try_hw && stream->codecpar->codec_type == AVMEDIA_TYPE_VIDEO) {
		for (int i = 0; hw_device_types[i] != AV_HWDEVICE_TYPE_NONE;
		     i++) {
			if (src->hw_device_ctx)
				break;
			int err = av_hwdevice_ctx_create(
				&src->hw_device_ctx, hw_device_types[i], NULL,
				NULL, 0);
			if (err == 0) {
				blog(LOG_INFO,
				     "[irl-source] Using hardware device: %s",
				     av_hwdevice_get_type_name(
					     hw_device_types[i]));
			} else {
				src->hw_device_ctx = NULL;
			}
		}
		if (src->hw_device_ctx)
			ctx->hw_device_ctx =
				av_buffer_ref(src->hw_device_ctx);
	}

	if (avcodec_open2(ctx, codec, NULL) < 0) {
		/* If hw decode failed, retry with software */
		if (ctx->hw_device_ctx) {
			avcodec_free_context(&ctx);
			av_buffer_unref(&src->hw_device_ctx);
			blog(LOG_INFO,
			     "[irl-source] Hardware decode failed, falling back to software");
			return open_decoder(src, stream, false);
		}
		avcodec_free_context(&ctx);
		return NULL;
	}

	if (ctx->hw_device_ctx)
		src->using_hw_decode = true;

	return ctx;
}

static void close_ffmpeg(struct irl_source *ctx)
{
	/* Free resampler/scaler so a reconnected stream with different
	 * audio format doesn't get stale contexts producing corrupt output. */
	if (ctx->swr_ctx) {
		swr_free(&ctx->swr_ctx);
		ctx->swr_ctx = NULL;
	}
	if (ctx->sws_ctx) {
		sws_freeContext(ctx->sws_ctx);
		ctx->sws_ctx = NULL;
	}

	if (ctx->audio_dec_ctx) {
		avcodec_free_context(&ctx->audio_dec_ctx);
		ctx->audio_dec_ctx = NULL;
	}
	if (ctx->video_dec_ctx) {
		avcodec_free_context(&ctx->video_dec_ctx);
		ctx->video_dec_ctx = NULL;
	}
	if (ctx->fmt_ctx) {
		avformat_close_input(&ctx->fmt_ctx);
		ctx->fmt_ctx = NULL;
	}
	ctx->audio_stream_idx = -1;
	ctx->video_stream_idx = -1;
	ctx->using_hw_decode = false;
}

/* Interrupt callback: returns 1 to abort blocking I/O when shutting down.
 * Without this, av_read_frame() blocks forever if the remote dies without
 * sending TCP FIN (common IRL scenario — phone loses signal). */
static int interrupt_cb(void *opaque)
{
	struct irl_source *ctx = opaque;
	return !ctx->thread_active;
}

static bool open_stream(struct irl_source *ctx)
{
	AVDictionary *opts = NULL;
	apply_demuxer_options(&opts, ctx->config.url, ctx->config.ffmpeg_options,
			      ctx->config.network_buffer_mb);

	blog(LOG_INFO, "[irl-source] Connecting to: %s", ctx->config.url);

	/* Pre-allocate format context to install the interrupt callback
	 * before avformat_open_input performs blocking I/O. */
	ctx->fmt_ctx = avformat_alloc_context();
	if (!ctx->fmt_ctx) {
		blog(LOG_ERROR, "[irl-source] Failed to allocate format context");
		av_dict_free(&opts);
		return false;
	}
	ctx->fmt_ctx->interrupt_callback.callback = interrupt_cb;
	ctx->fmt_ctx->interrupt_callback.opaque = ctx;

	int ret = avformat_open_input(&ctx->fmt_ctx, ctx->config.url, NULL,
				      &opts);
	av_dict_free(&opts);
	if (ret < 0) {
		char errbuf[AV_ERROR_MAX_STRING_SIZE];
		av_strerror(ret, errbuf, sizeof(errbuf));
		blog(LOG_WARNING, "[irl-source] Failed to open input: %s",
		     errbuf);
		return false;
	}

	blog(LOG_INFO, "[irl-source] Input opened, probing streams...");

	if (avformat_find_stream_info(ctx->fmt_ctx, NULL) < 0) {
		blog(LOG_WARNING,
		     "[irl-source] Failed to find stream info");
		avformat_close_input(&ctx->fmt_ctx);
		return false;
	}

	ctx->audio_stream_idx = -1;
	ctx->video_stream_idx = -1;

	for (unsigned i = 0; i < ctx->fmt_ctx->nb_streams; i++) {
		AVStream *s = ctx->fmt_ctx->streams[i];
		if (s->codecpar->codec_type == AVMEDIA_TYPE_VIDEO &&
		    ctx->video_stream_idx < 0) {
			bool try_hw = (ctx->config.hw_decode == 0);
			ctx->video_dec_ctx =
				open_decoder(ctx, s, try_hw);
			if (ctx->video_dec_ctx) {
				ctx->video_stream_idx = (int)i;
				blog(LOG_INFO,
				     "[irl-source] Video stream %u: %s %dx%d%s",
				     i, avcodec_get_name(s->codecpar->codec_id),
				     s->codecpar->width, s->codecpar->height,
				     ctx->using_hw_decode ? " (NVDEC)" : " (SW)");
			} else {
				blog(LOG_WARNING,
				     "[irl-source] Failed to open video decoder for stream %u (%s)",
				     i, avcodec_get_name(s->codecpar->codec_id));
			}
		} else if (s->codecpar->codec_type == AVMEDIA_TYPE_AUDIO &&
			   ctx->audio_stream_idx < 0) {
			ctx->audio_dec_ctx =
				open_decoder(ctx, s, false);
			if (ctx->audio_dec_ctx) {
				ctx->audio_stream_idx = (int)i;
				blog(LOG_INFO,
				     "[irl-source] Audio stream %u: %s %dHz %dch",
				     i, avcodec_get_name(s->codecpar->codec_id),
				     s->codecpar->sample_rate,
				     s->codecpar->ch_layout.nb_channels);
			} else {
				blog(LOG_WARNING,
				     "[irl-source] Failed to open audio decoder for stream %u",
				     i);
			}
		}
	}

	if (ctx->video_stream_idx < 0 && ctx->audio_stream_idx < 0) {
		blog(LOG_WARNING,
		     "[irl-source] No usable audio or video streams found");
		close_ffmpeg(ctx);
		return false;
	}

	blog(LOG_INFO, "[irl-source] Stream opened (video=%d, audio=%d)",
	     ctx->video_stream_idx, ctx->audio_stream_idx);

	/* Initialise PTS repair for audio stream */
	if (ctx->audio_stream_idx >= 0) {
		AVStream *as =
			ctx->fmt_ctx->streams[ctx->audio_stream_idx];
		pts_repair_init(&ctx->pts_state, ctx->config.small_gap_ms,
				ctx->config.large_gap_ms, as->time_base.num,
				as->time_base.den);
	}

	return true;
}

/* ── Decoded frame handling ───────────────────────────────── */

static void handle_audio_frame(struct irl_source *ctx, AVFrame *frame)
{
	/* Determine output format: planar float → interleaved float for OBS */
	int out_channels = frame->ch_layout.nb_channels;
	int out_rate = frame->sample_rate;
	int bytes_per_sample = sizeof(float);

	/* Init or reinit audio buffer on format change */
	if (ctx->audio_buf.sample_rate != out_rate ||
	    ctx->audio_buf.channels != out_channels) {
		audio_buffer_free(&ctx->audio_buf);
		audio_buffer_init(&ctx->audio_buf, out_rate, out_channels,
				  bytes_per_sample, ctx->config.buffer_target_ms,
				  ctx->config.buffer_min_ms,
				  ctx->config.buffer_max_ms);
	}

	/* PTS repair */
	int64_t corrected_pts;
	int silence_ms = 0;
	enum pts_action action = pts_repair_evaluate(
		&ctx->pts_state, frame->pts, frame->duration, &corrected_pts,
		&silence_ms);

	if (action == PTS_ACTION_SILENCE && silence_ms > 0) {
		/* Insert silence into the buffer */
		size_t silence_bytes =
			audio_buffer_ms_to_bytes(&ctx->audio_buf, silence_ms);
		uint8_t *silence = calloc(1, silence_bytes);
		if (silence) {
			audio_buffer_write(&ctx->audio_buf, silence,
					   silence_bytes);
			free(silence);
			ctx->silence_insertions++;
		}
	} else if (action == PTS_ACTION_RESET) {
		audio_buffer_flush(&ctx->audio_buf);
		ctx->audio_output_pts_init = false;
		ctx->first_keyframe_received = false;
		ctx->video_ts_init = false;
		ctx->pre_kf_audio_size = 0;
	}

	if (action != PTS_ACTION_PASS)
		ctx->pts_repairs++;

	/* Resample from decoded format to interleaved float if needed */
	uint8_t *interleaved = NULL;
	int out_samples = frame->nb_samples;

	if (frame->format != AV_SAMPLE_FMT_FLT ||
	    frame->ch_layout.nb_channels != out_channels) {
		/* Set up resampler */
		if (!ctx->swr_ctx) {
			ctx->swr_ctx = swr_alloc();
			AVChannelLayout out_layout;
			av_channel_layout_default(&out_layout, out_channels);
			swr_alloc_set_opts2(&ctx->swr_ctx, &out_layout,
					    AV_SAMPLE_FMT_FLT, out_rate,
					    &frame->ch_layout,
					    frame->format, frame->sample_rate,
					    0, NULL);
			swr_init(ctx->swr_ctx);
		}

		int max_out = swr_get_out_samples(ctx->swr_ctx,
						  frame->nb_samples);
		interleaved =
			malloc((size_t)max_out * out_channels * bytes_per_sample);
		if (!interleaved)
			return;

		out_samples = swr_convert(ctx->swr_ctx, &interleaved, max_out,
					  (const uint8_t **)frame->extended_data,
					  frame->nb_samples);
		if (out_samples <= 0) {
			free(interleaved);
			return;
		}
	} else {
		/* Already interleaved float — use directly */
		interleaved = frame->data[0];
	}

	size_t data_bytes =
		(size_t)out_samples * out_channels * bytes_per_sample;

	/* Keyframe gate: buffer audio until first video keyframe */
	if (ctx->config.wait_for_keyframe && !ctx->first_keyframe_received) {
		/* Stage audio in pre-keyframe buffer */
		if (!ctx->pre_kf_audio_data) {
			ctx->pre_kf_audio_capacity =
				audio_buffer_ms_to_bytes(&ctx->audio_buf, 500);
			ctx->pre_kf_audio_data =
				bmalloc(ctx->pre_kf_audio_capacity);
			ctx->pre_kf_audio_size = 0;
		}

		size_t avail = ctx->pre_kf_audio_capacity -
			       ctx->pre_kf_audio_size;
		size_t to_copy = data_bytes < avail ? data_bytes : avail;
		if (to_copy > 0) {
			memcpy(ctx->pre_kf_audio_data + ctx->pre_kf_audio_size,
			       interleaved, to_copy);
			ctx->pre_kf_audio_size += to_copy;
		}

		if (interleaved != frame->data[0])
			free(interleaved);
		return;
	}

	audio_buffer_write(&ctx->audio_buf, interleaved, data_bytes);
	if (interleaved != frame->data[0])
		free(interleaved);

	/* If output is suppressed after a PTS snap, skip everything.
	 * When suppression ends, snap PTS fresh and resume — the
	 * 100ms gap already exceeds OBS's 70ms smoothing threshold. */
	if (ctx->audio_snap_suppress_until) {
		if (os_gettime_ns() < ctx->audio_snap_suppress_until)
			return;
		ctx->audio_output_pts_ns =
			(int64_t)os_gettime_ns() - 5000000LL;
		ctx->audio_snap_suppress_until = 0;
	} else if (!ctx->audio_output_pts_init) {
		ctx->audio_output_pts_ns =
			(int64_t)os_gettime_ns() - 5000000LL;
		ctx->audio_output_pts_init = true;
	} else {
		int64_t target =
			(int64_t)os_gettime_ns() - 5000000LL;
		int64_t error =
			target - ctx->audio_output_pts_ns;
		int64_t abs_err =
			error >= 0 ? error : -error;
		if (abs_err > 50000000LL) {
			ctx->audio_output_pts_ns = target;
			ctx->audio_snap_suppress_until =
				os_gettime_ns() + 100000000ULL;
			return;
		}
		ctx->audio_output_pts_ns += error / 4;
	}

	/* Output audio to OBS in a loop until the buffer is at or below
	 * target.  A single output per decoded frame cannot keep up when
	 * the decoded frame is larger than the output chunk (e.g. AAC
	 * 1024 samples = 21.3ms vs 20ms chunk), causing the buffer to
	 * overflow and silently drop audio. */
	while (audio_buffer_ready(&ctx->audio_buf)) {
		int chunk_ms = 20;
		size_t frame_bytes = audio_buffer_ms_to_bytes(
			&ctx->audio_buf, chunk_ms);
		uint8_t *out_buf = malloc(frame_bytes);
		if (!out_buf)
			break;

		size_t got = audio_buffer_read(&ctx->audio_buf, out_buf,
					       frame_bytes);
		if (got == 0) {
			free(out_buf);
			break;
		}

		/* Fade-in after reconnect to avoid click */
		if (ctx->fade_in_pending) {
			ctx->fade_in_frames_remaining =
				out_rate * IRL_FADE_DURATION_MS / 1000;
			ctx->fade_in_pending = false;
		}
		if (ctx->fade_in_frames_remaining > 0) {
			int total_fade =
				out_rate * IRL_FADE_DURATION_MS / 1000;
			float *s = (float *)out_buf;
			int nf = (int)(got /
				       (out_channels * bytes_per_sample));
			for (int f = 0;
			     f < nf &&
			     ctx->fade_in_frames_remaining > 0;
			     f++) {
				int into = total_fade -
					   ctx->fade_in_frames_remaining;
				float gain =
					(float)into / (float)total_fade;
				for (int ch = 0; ch < out_channels; ch++)
					s[f * out_channels + ch] *= gain;
				ctx->fade_in_frames_remaining--;
			}
		}

		uint32_t frames_out =
			(uint32_t)(got /
				   (out_channels * bytes_per_sample));

		struct obs_source_audio obs_audio = {0};
		obs_audio.data[0] = out_buf;
		obs_audio.frames = frames_out;
		obs_audio.format = AUDIO_FORMAT_FLOAT;
		obs_audio.speakers = (enum speaker_layout)out_channels;
		obs_audio.timestamp =
			(uint64_t)ctx->audio_output_pts_ns;
		obs_audio.samples_per_sec = (uint32_t)out_rate;

		/* Adaptive speed: must run after samples_per_sec
		 * is set so it can scale the value. */
		if (ctx->config.adaptive_speed)
			irl_speed_apply(ctx, &obs_audio);

		obs_source_output_audio(ctx->source, &obs_audio);

		/* Advance running PTS by actual samples output */
		ctx->audio_output_pts_ns +=
			(int64_t)frames_out * 1000000000LL / out_rate;
		ctx->total_audio_frames++;

		free(out_buf);

		/* Stop once buffer is at or below target */
		if (audio_buffer_fill_ms(&ctx->audio_buf) <=
		    ctx->config.buffer_target_ms)
			break;
	}
}

static void handle_video_frame(struct irl_source *ctx, AVFrame *frame)
{
	/* Keyframe gate */
	if (!ctx->first_keyframe_received) {
		if (!irl_video_is_keyframe(frame)) {
			if (ctx->total_video_frames == 0)
				blog(LOG_DEBUG,
				     "[irl-source] Waiting for keyframe (dropped non-keyframe)");
			return;
		}

		ctx->first_keyframe_received = true;
		blog(LOG_INFO,
		     "[irl-source] First keyframe received (%dx%d fmt=%d)",
		     frame->width, frame->height, frame->format);

		/* Release any buffered pre-keyframe audio */
		if (ctx->pre_kf_audio_size > 0 && ctx->audio_buf.data) {
			audio_buffer_write(&ctx->audio_buf,
					   ctx->pre_kf_audio_data,
					   ctx->pre_kf_audio_size);
			ctx->pre_kf_audio_size = 0;
		}
	}

	/* Detect mid-stream resolution changes (adaptive bitrate, phone rotation) */
	if (ctx->last_video_width && ctx->last_video_height &&
	    (frame->width != ctx->last_video_width ||
	     frame->height != ctx->last_video_height)) {
		blog(LOG_INFO,
		     "[irl-source] Resolution changed: %dx%d -> %dx%d",
		     ctx->last_video_width, ctx->last_video_height,
		     frame->width, frame->height);
		ctx->video_ts_init = false; /* re-anchor timestamps */
	}
	ctx->last_video_width = frame->width;
	ctx->last_video_height = frame->height;

	irl_video_output_frame(ctx, frame);
	ctx->total_video_frames++;
	if (ctx->total_video_frames == 1)
		blog(LOG_INFO, "[irl-source] First video frame output");
}

/* ── Main read loop ───────────────────────────────────────── */

void *irl_receiver_thread(void *data)
{
	struct irl_source *ctx = data;
	AVPacket *pkt = av_packet_alloc();
	AVFrame *frame = av_frame_alloc();

	blog(LOG_INFO, "[irl-source] Receiver thread started for: %s",
	     ctx->config.url ? ctx->config.url : "(null)");

	while (ctx->thread_active) {
		if (!ctx->fmt_ctx) {
			if (!open_stream(ctx)) {
				/* Reconnect after delay */
				ctx->reconnecting = true;
				blog(LOG_INFO,
				     "[irl-source] Reconnecting in %ds...",
				     ctx->config.reconnect_delay);
				for (int i = 0;
				     i < ctx->config.reconnect_delay * 10 &&
				     ctx->thread_active;
				     i++) {
					av_usleep(100000); /* 100ms */
				}
				ctx->reconnecting = false;
				continue;
			}
			/* Reset state for new connection */
			ctx->first_keyframe_received = false;
			ctx->video_ts_init = false;
			ctx->pre_kf_audio_size = 0;
		}

		int ret = av_read_frame(ctx->fmt_ctx, pkt);
		if (ret < 0) {
			char errbuf[AV_ERROR_MAX_STRING_SIZE];
			av_strerror(ret, errbuf, sizeof(errbuf));
			blog(LOG_WARNING,
			     "[irl-source] Stream read error: %s (video_frames=%llu, audio_frames=%llu)",
			     errbuf,
			     (unsigned long long)ctx->total_video_frames,
			     (unsigned long long)ctx->total_audio_frames);

			/* Fade out remaining audio to avoid click/pop */
			if (ctx->audio_buf.data && ctx->audio_buf.fill > 0) {
				size_t fade_bytes = audio_buffer_ms_to_bytes(
					&ctx->audio_buf,
					IRL_FADE_DURATION_MS);
				if (fade_bytes > ctx->audio_buf.fill)
					fade_bytes = ctx->audio_buf.fill;
				if (fade_bytes > 0) {
					uint8_t *fade_buf = malloc(fade_bytes);
					if (fade_buf) {
						size_t got =
							audio_buffer_read_with_fade_out(
								&ctx->audio_buf,
								fade_buf,
								fade_bytes);
						if (got > 0) {
							struct obs_source_audio
								a = {0};
							a.data[0] = fade_buf;
							a.frames = (uint32_t)(
								got /
								(ctx->audio_buf
									 .channels *
								 ctx->audio_buf
									 .bytes_per_sample));
							a.format =
								AUDIO_FORMAT_FLOAT;
							a.speakers =
								(enum speaker_layout)
									ctx->audio_buf
										.channels;
							a.samples_per_sec =
								(uint32_t)
									ctx->audio_buf
										.sample_rate;
							a.timestamp =
								os_gettime_ns();
							obs_source_output_audio(
								ctx->source,
								&a);
						}
						free(fade_buf);
					}
				}
			}

			/* OBS_SOURCE_ASYNC_VIDEO holds the last frame on screen
			 * when no new frames arrive.  This is intentional:
			 * viewers see a frozen image instead of black during
			 * disconnection.  Do NOT call
			 * obs_source_output_video(NULL) here. */

			close_ffmpeg(ctx);
			pts_repair_reset(&ctx->pts_state);
			audio_buffer_flush(&ctx->audio_buf);
			ctx->audio_output_pts_init = false;
			ctx->current_speed = 1.0f;
			ctx->last_speed_adjust_time = 0;
			ctx->fade_in_pending = true;
			continue;
		}

		if (pkt->stream_index == ctx->audio_stream_idx &&
		    ctx->audio_dec_ctx) {
			ret = avcodec_send_packet(ctx->audio_dec_ctx, pkt);
			if (ret < 0 && ret != AVERROR(EAGAIN) &&
			    ret != AVERROR_EOF) {
				/* Decoder in bad state (bitrate starvation,
				 * corrupt packets).  Flush to recover instead
				 * of letting audio break permanently. */
				avcodec_flush_buffers(ctx->audio_dec_ctx);
			}
			while (ret >= 0) {
				ret = avcodec_receive_frame(ctx->audio_dec_ctx,
							   frame);
				if (ret < 0)
					break;
				handle_audio_frame(ctx, frame);
				av_frame_unref(frame);
			}
		} else if (pkt->stream_index == ctx->video_stream_idx &&
			   ctx->video_dec_ctx) {
			ret = avcodec_send_packet(ctx->video_dec_ctx, pkt);
			if (ret < 0 && ret != AVERROR(EAGAIN) &&
			    ret != AVERROR_EOF) {
				avcodec_flush_buffers(ctx->video_dec_ctx);
			}
			while (ret >= 0) {
				ret = avcodec_receive_frame(ctx->video_dec_ctx,
							   frame);
				if (ret < 0)
					break;
				handle_video_frame(ctx, frame);
				av_frame_unref(frame);
			}
		}

		av_packet_unref(pkt);

		/* Periodic stats logging (every 30 seconds) */
		uint64_t now = os_gettime_ns();
		if (now - ctx->last_stats_time > 30000000000ULL) {
			ctx->last_stats_time = now;
			blog(LOG_INFO,
			     "[irl-source] Stats: video=%llu audio=%llu "
			     "buf=%dms speed=%.3f pts_repairs=%llu "
			     "silence=%llu res=%dx%d",
			     (unsigned long long)ctx->total_video_frames,
			     (unsigned long long)ctx->total_audio_frames,
			     audio_buffer_fill_ms(&ctx->audio_buf),
			     (double)ctx->current_speed,
			     (unsigned long long)ctx->pts_repairs,
			     (unsigned long long)ctx->silence_insertions,
			     ctx->last_video_width,
			     ctx->last_video_height);
		}
	}

	close_ffmpeg(ctx);
	av_packet_free(&pkt);
	av_frame_free(&frame);
	return NULL;
}

void irl_receiver_stop(struct irl_source *ctx)
{
	if (!ctx->thread_active)
		return;

	ctx->thread_active = false;
	pthread_join(ctx->receiver_thread, NULL);
}
