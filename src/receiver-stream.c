/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

#include <stdlib.h>
#include <string.h>

#ifdef _MSC_VER
#define strtok_r strtok_s
#endif

#include "receiver-internal.h"

/* Protocol test on the scheme itself, not a substring of the whole URL: a
 * query parameter or a path segment must not decide which options apply. */
static bool url_has_scheme(const char *url, const char *scheme)
{
	if (!url)
		return false;
	size_t len = strlen(scheme);
	return strncmp(url, scheme, len) == 0 &&
	       strncmp(url + len, "://", 3) == 0;
}

static void apply_demuxer_options(AVDictionary **opts, const char *url,
				  const char *extra, int network_buffer_mb,
				  bool fast_probe)
{
	/* A reconnect probes fast: the previous session already showed what
	 * the stream carries, and every probe byte is time the feed stays
	 * dark after a `!fix`. irl_open_stream retries with the full probe
	 * if the short one comes up missing a stream the last session had
	 * (some encoders advertise audio late). */
	if (fast_probe) {
		av_dict_set(opts, "probesize", "1000000", 0);
		av_dict_set(opts, "analyzeduration", "1000000", 0);
	} else {
		av_dict_set(opts, "probesize", "5000000", 0);
		av_dict_set(opts, "analyzeduration", "5000000", 0);
	}
	/* No +discardcorrupt. mpegts.c marks a PES corrupt on any continuity
	 * counter discontinuity, and AVFMT_FLAG_DISCARD_CORRUPT then drops the
	 * whole packet in demux.c before the decoder ever sees it. On a lossy
	 * IRL uplink that silently deletes video: a 1080p frame spans ~100+ TS
	 * packets while an audio frame is one, so a per-packet loss rate too
	 * small to dent audio takes out most video frames. Measured on a
	 * report of "stuck at 15-20fps": audio ran at 99% of real time while
	 * decoded video sat at 12.8fps, with no decode errors and no drops
	 * counted anywhere in the plugin, because the packets died in
	 * libavformat. OBS's own media source does not set this flag.
	 *
	 * Damaged packets now reach the decoder, which conceals and keeps
	 * emitting frames — that is what irl_handle_video_frame()'s
	 * video_corrupted passthrough was written for, and it was unreachable
	 * for demux-level corruption as long as this flag was set. Artifacts
	 * beat a hole in the cadence. */
	av_dict_set(opts, "fflags", "+genpts", 0);
	/* mpegts only (the SRT/UDP/RIST carriers). When the ingest socket
	 * survives an encoder swap — a relay (SLS, MediaMTX, belabox cloud)
	 * keeps serving this connection while a different encoder reconnects
	 * upstream — the new session's PMT can lay out different PIDs
	 * (Moblin muxes video/audio on 256/257; GStreamer's mpegtsmux picks
	 * its own layout). Without this flag libavformat answers new PIDs
	 * with brand-new AVStreams, the latched stream indexes stop
	 * matching, and the feed stalls until the I/O timeout forces a
	 * reconnect. With it the demuxer maps the new PIDs onto the
	 * existing streams and only the PTS jump remains, which is
	 * pts-repair's job anyway. Same-encoder restarts never needed it:
	 * Moblin and belacoder both re-emit byte-identical version-0 PMTs,
	 * which mpegts.c skips on the version+CRC check. */
	av_dict_set(opts, "merge_pmt_versions", "1", 0);
	/* udp:// only. A burst the receive ring cannot absorb is a fatal
	 * read error by default, turning one hiccup into a full
	 * disconnect/reconnect cycle (fade-out, keyframe wait). Losing the
	 * overrun packets and letting the decoder conceal is strictly
	 * smoother — the same call as the +discardcorrupt removal above:
	 * artifacts beat a hole in the cadence. */
	av_dict_set(opts, "overrun_nonfatal", "1", 0);
	/* rtmp(s):// only; ignored elsewhere. Declare live intent so the
	 * server serves the live edge instead of "any", and shrink the
	 * client buffer hint from FFmpeg's 3000ms default — nginx-rtmp
	 * family servers pace delivery by it, so the default parks three
	 * seconds of latency server-side. */
	av_dict_set(opts, "rtmp_live", "live", 0);
	av_dict_set(opts, "rtmp_buffer", "1000", 0);
	/* Reaches every TCP-based transport (rtmp, http, the tcp under
	 * tls). Receive-side it only affects our acks and control replies,
	 * but Nagle delaying those on a lossy uplink is pure harm. */
	av_dict_set(opts, "tcp_nodelay", "1", 0);
	/* http(s) inputs only; harmless no-ops elsewhere. An FFmpeg-internal
	 * reconnect keeps the decoders and the keyframe gate warm, so it is
	 * always smoother than falling out to the plugin's reconnect loop;
	 * cover connect-time network errors too, not just mid-stream drops.
	 * The interrupt_cb stall timeout still bounds all of it. */
	av_dict_set(opts, "reconnect", "1", 0);
	av_dict_set(opts, "reconnect_streamed", "1", 0);
	av_dict_set(opts, "reconnect_on_network_error", "1", 0);

	/*
	 * FFmpeg 9.0 flipped tls_verify to default on. The bundled stack has
	 * no OpenSSL, and the mbedTLS backend only ever loads the CA chain
	 * named by ca_file — there is no system trust store fallback the way
	 * tls_openssl.c gets one from SSL_CTX_set_default_verify_paths. On 9.0
	 * that turns every https:// and rtmps:// ingest into a handshake
	 * failure ("certificate not trusted") no matter how valid the cert is.
	 *
	 * Restoring the pre-9.0 default keeps working setups working, and it
	 * is the honest one for this workload besides: IRL ingests are
	 * routinely self-signed or addressed by bare IP. Users who do want
	 * verification can turn it back on per source through FFmpeg Options
	 * ("tls_verify=1 ca_file=/path/to/ca.pem"), which is parsed below and
	 * therefore overwrites this.
	 */
	av_dict_set(opts, "tls_verify", "0", 0);

	/*
	 * The receive buffer is a byte count, and FFmpeg 9.0 spells that two
	 * different ways, so both names have to be set to cover the protocols
	 * this plugin ingests.
	 *
	 * "buffer_size" is bytes for udp:// (setsockopt SO_RCVBUF), and for
	 * rtp:// and rtsp://, which forward it verbatim to the udp:// they open.
	 * librist is the one protocol that reuses the name for something else:
	 * there it is the RIST recovery window in *milliseconds*, declared with
	 * a max of 30000. A byte count therefore makes av_opt_set_dict() on the
	 * URLContext fail with AVERROR(ERANGE) ("Result too large") and
	 * avformat_open_input aborts before a socket is ever opened. rist://
	 * keeps librist's own recovery default instead, which is tuned per
	 * stream through the URL's buffer= parameter that rist_parse_address2()
	 * reads.
	 *
	 * "recv_buffer_size" is bytes for tcp:// (SO_RCVBUF) and for libsrt
	 * (SRTO_UDP_RCVBUF), and nothing else declares it. It reaches tcp://
	 * from every protocol layered on top of it, because rtmp_open,
	 * http_open_cnx and ff_tls_open_underlying each thread the caller's
	 * option dictionary down into the transport they open — so this is what
	 * gives the setting any effect at all on rtmp(s):// and http(s)://.
	 *
	 * Each name is ignored by the protocols that want the other one, so
	 * neither needs a scheme test beyond the rist:// exclusion.
	 */
	if (network_buffer_mb > 0) {
		char bytes[32];
		snprintf(bytes, sizeof(bytes), "%d",
			 network_buffer_mb * 1024 * 1024);
		if (!url_has_scheme(url, "rist"))
			av_dict_set(opts, "buffer_size", bytes, 0);
		av_dict_set(opts, "recv_buffer_size", bytes, 0);

		/* udp:// also has a userspace ring between its receive
		 * thread and the demuxer, sized in 188-byte TS packets
		 * (default 7*4096 ≈ 5.3MB). Grow it with the setting, never
		 * shrink it — the setting's 2MB default is below FFmpeg's
		 * own default ring. */
		int fifo_pkts =
			(int)((int64_t)network_buffer_mb * 1024 * 1024 / 188);
		if (fifo_pkts > 7 * 4096) {
			char fifo[32];
			snprintf(fifo, sizeof(fifo), "%d", fifo_pkts);
			av_dict_set(opts, "fifo_size", fifo, 0);
		}
	}

	if (url_has_scheme(url, "srt"))
		av_dict_set(opts, "latency", "200000", 0);

	if (extra && *extra) {
		char *dup = av_strdup(extra);
		if (!dup)
			return;
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

static const enum AVHWDeviceType hw_device_types[] = {
#ifdef _WIN32
	AV_HWDEVICE_TYPE_D3D11VA,
	AV_HWDEVICE_TYPE_CUDA,
#elif defined(__APPLE__)
	AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
#else
	AV_HWDEVICE_TYPE_VAAPI,
	AV_HWDEVICE_TYPE_CUDA,
#endif
	AV_HWDEVICE_TYPE_NONE,
};

static const enum AVHWDeviceType nvdec_device_types[] = {
	AV_HWDEVICE_TYPE_CUDA,
	AV_HWDEVICE_TYPE_NONE,
};

/* Explicit NVDEC must not let libavcodec choose the software pixel format
 * when a stream or driver cannot provide CUDA frames. Auto deliberately keeps
 * the default FFmpeg negotiation, including its existing software fallback. */
static enum AVPixelFormat nvdec_get_format(AVCodecContext *ctx,
					   const enum AVPixelFormat *formats)
{
	for (const enum AVPixelFormat *fmt = formats; *fmt != AV_PIX_FMT_NONE;
	     fmt++) {
		for (int i = 0;; i++) {
			const AVCodecHWConfig *config =
				avcodec_get_hw_config(ctx->codec, i);
			if (!config)
				break;
			if (config->device_type == AV_HWDEVICE_TYPE_CUDA &&
			    (config->methods &
			     AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX) &&
			    config->pix_fmt == *fmt)
				return *fmt;
		}
	}

	blog(LOG_ERROR,
	     "[irl-source] NVDEC requested, but the decoder offered no CUDA hardware format");
	return AV_PIX_FMT_NONE;
}

static AVCodecContext *open_decoder(struct irl_source *src, AVStream *stream,
				    int hw_decode_mode)
{
	bool is_video = stream->codecpar->codec_type == AVMEDIA_TYPE_VIDEO;
	bool try_hw = is_video && hw_decode_mode != IRL_HW_DECODE_OFF;
	bool force_nvdec = is_video && hw_decode_mode == IRL_HW_DECODE_NVDEC;
	const enum AVHWDeviceType *device_types =
		force_nvdec ? nvdec_device_types : hw_device_types;
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

	ctx->pkt_timebase = stream->time_base;
	if (stream->codecpar->codec_type == AVMEDIA_TYPE_AUDIO) {
		ctx->thread_count = 1;
		ctx->thread_type = 0;
	} else {
		/* Low-delay decode: don't hold frames for B-frame
		 * reordering. IRL encoders essentially never emit
		 * B-frames, so that buffer is pure latency; if a stream
		 * does contain them, frames come out in decode order
		 * and video may judder slightly instead of lagging.
		 *
		 * Frame threading adds thread_count-1 frames of
		 * pipeline latency on software decode, so cap it
		 * instead of letting FFmpeg use every core. Hardware
		 * decode ignores both settings. */
		ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
		ctx->thread_count = 4;
		/* Concealment of lossy input: keep FFmpeg's guess_mvs+deblock
		 * default and add favor_inter, which patches damaged
		 * macroblocks from the previous frame instead of guessing
		 * spatially — on moving IRL content temporal patches are far
		 * less visible. H.264 only in practice; HEVC has no
		 * error-resilience path and ignores it. */
		ctx->error_concealment |= FF_EC_FAVOR_INTER;
		/* Spec-noncompliant speedups on the software path (hardware
		 * decoders ignore it), for the machines where hardware decode
		 * fell back to software and every cycle counts. */
		ctx->flags2 |= AV_CODEC_FLAG2_FAST;
		/* The video output queue holds decoded HW frames, each
		 * pinning a decoder surface; give the pool matching
		 * headroom or the decoder can stall waiting for a
		 * surface the queue is sitting on. Fixed-pool decoders
		 * (D3D11VA, VAAPI) are where that bites, and it looks
		 * like frozen video with clean audio.
		 *
		 * Count the surfaces this plugin can pin at once, not
		 * just the queue: IRL_VIDEO_QUEUE_SIZE queued, plus the
		 * one the video thread has popped and is transferring in
		 * irl_video_output_frame(), plus the one just returned by
		 * avcodec_receive_frame() and not yet unref'd. The clone
		 * irl_video_queue_push() takes references that same
		 * surface, so it does not add a third. */
		ctx->extra_hw_frames = IRL_VIDEO_QUEUE_SIZE + 2;
	}

	if (try_hw) {
		for (int i = 0; device_types[i] != AV_HWDEVICE_TYPE_NONE;
		     i++) {
			if (src->hw_device_ctx)
				break;
			int err = av_hwdevice_ctx_create(
				&src->hw_device_ctx, device_types[i], NULL,
				NULL, 0);
			if (err == 0) {
				src->hw_device_type = device_types[i];
				blog(LOG_INFO,
				     "[irl-source] Using hardware device: %s",
				     av_hwdevice_get_type_name(
					     device_types[i]));
			} else {
				char errbuf[AV_ERROR_MAX_STRING_SIZE];
				av_strerror(err, errbuf, sizeof(errbuf));
				blog(LOG_INFO,
				     "[irl-source] Hardware device %s unavailable: %s",
				     av_hwdevice_get_type_name(
					     device_types[i]),
				     errbuf);
				src->hw_device_ctx = NULL;
			}
		}
		if (force_nvdec && !src->hw_device_ctx) {
			blog(LOG_ERROR,
			     "[irl-source] NVDEC was selected, but no CUDA device is available");
			avcodec_free_context(&ctx);
			return NULL;
		}
		if (src->hw_device_ctx) {
			ctx->hw_device_ctx = av_buffer_ref(src->hw_device_ctx);
			if (force_nvdec)
				ctx->get_format = nvdec_get_format;
		}
	}

	int open_ret = avcodec_open2(ctx, codec, NULL);
	if (open_ret < 0) {
		char errbuf[AV_ERROR_MAX_STRING_SIZE];
		av_strerror(open_ret, errbuf, sizeof(errbuf));
		if (ctx->hw_device_ctx && !force_nvdec) {
			avcodec_free_context(&ctx);
			av_buffer_unref(&src->hw_device_ctx);
			src->hw_device_type = AV_HWDEVICE_TYPE_NONE;
			blog(LOG_INFO,
			     "[irl-source] Hardware decode failed, falling back to software");
			return open_decoder(src, stream, IRL_HW_DECODE_OFF);
		}
		if (ctx->hw_device_ctx) {
			av_buffer_unref(&src->hw_device_ctx);
			src->hw_device_type = AV_HWDEVICE_TYPE_NONE;
		}
		if (force_nvdec)
			blog(LOG_ERROR,
			     "[irl-source] NVDEC decoder failed (%s); software fallback is disabled",
			     errbuf);
		else
			blog(LOG_WARNING,
			     "[irl-source] Decoder failed to open: %s", errbuf);
		avcodec_free_context(&ctx);
		return NULL;
	}

	if (ctx->hw_device_ctx)
		src->using_hw_decode = true;

	return ctx;
}

void irl_close_ffmpeg(struct irl_source *ctx)
{
	if (ctx->swr_ctx) {
		swr_free(&ctx->swr_ctx);
		ctx->swr_ctx = NULL;
	}
	ctx->swr_in_rate = 0;
	ctx->swr_in_channels = 0;
	ctx->swr_in_format = AV_SAMPLE_FMT_NONE;
	/* sws_ctx is owned by the video thread (it converts queued
	 * frames that may outlive this connection); it is recreated on
	 * parameter change and freed at source destroy. */

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
	/* Release the HW device with the connection it was created for.
	 * Keeping it across reconnects made the device-creation loop
	 * silently skip (no logging) and attach a stale device, so
	 * reconnects behaved differently from fresh connects. */
	if (ctx->hw_device_ctx)
		av_buffer_unref(&ctx->hw_device_ctx);
	ctx->audio_stream_idx = -1;
	os_atomic_store_bool(&ctx->audio_stream_present, false);
	ctx->video_stream_idx = -1;
	ctx->using_hw_decode = false;
	ctx->hw_device_type = AV_HWDEVICE_TYPE_NONE;
	ctx->hw_map_ok = -1;
}

static int interrupt_cb(void *opaque)
{
	struct irl_source *ctx = opaque;

	if (!os_atomic_load_bool(&ctx->thread_active))
		return 1;
	if (ctx->io_start_us != 0 &&
	    (uint64_t)av_gettime() - ctx->io_start_us >
		    IRL_IO_STALL_TIMEOUT_US) {
		return 1;
	}
	return 0;
}

static bool open_stream_attempt(struct irl_source *ctx, bool fast_probe)
{
	AVDictionary *opts = NULL;
	apply_demuxer_options(&opts, ctx->config.url, ctx->config.ffmpeg_options,
			      ctx->config.network_buffer_mb, fast_probe);

	irl_log_input_url("Connecting to", ctx->config.url);

	ctx->fmt_ctx = avformat_alloc_context();
	if (!ctx->fmt_ctx) {
		blog(LOG_ERROR, "[irl-source] Failed to allocate format context");
		av_dict_free(&opts);
		return false;
	}
	ctx->fmt_ctx->interrupt_callback.callback = interrupt_cb;
	ctx->fmt_ctx->interrupt_callback.opaque = ctx;

	ctx->io_start_us = (uint64_t)av_gettime();
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

	ctx->io_start_us = (uint64_t)av_gettime();
	if (avformat_find_stream_info(ctx->fmt_ctx, NULL) < 0) {
		blog(LOG_WARNING, "[irl-source] Failed to find stream info");
		avformat_close_input(&ctx->fmt_ctx);
		return false;
	}

	ctx->audio_stream_idx = -1;
	os_atomic_store_bool(&ctx->audio_stream_present, false);
	ctx->video_stream_idx = -1;
	ctx->hw_device_type = AV_HWDEVICE_TYPE_NONE;

	for (unsigned i = 0; i < ctx->fmt_ctx->nb_streams; i++) {
		AVStream *s = ctx->fmt_ctx->streams[i];
		if (s->codecpar->codec_type == AVMEDIA_TYPE_VIDEO &&
		    ctx->video_stream_idx < 0) {
			ctx->video_dec_ctx =
				open_decoder(ctx, s, ctx->config.hw_decode);
			if (ctx->video_dec_ctx) {
				ctx->video_stream_idx = (int)i;
				/* This reports the requested decode path;
				 * the first-keyframe log reports the ground
				 * truth from the actual decoded frame. */
				bool hw_attached =
					ctx->video_dec_ctx->hw_device_ctx !=
					NULL;
				blog(LOG_INFO,
				     "[irl-source] Video stream %u: %s %dx%d (%s requested, using_hw=%d)",
				     i, avcodec_get_name(s->codecpar->codec_id),
				     s->codecpar->width, s->codecpar->height,
				     hw_attached && ctx->hw_device_type !=
							    AV_HWDEVICE_TYPE_NONE
					     ? av_hwdevice_get_type_name(
						       ctx->hw_device_type)
					     : "SW",
				     ctx->using_hw_decode ? 1 : 0);
			} else {
				blog(LOG_WARNING,
				     "[irl-source] Failed to open video decoder for stream %u (%s)",
				     i, avcodec_get_name(s->codecpar->codec_id));
			}
		} else if (s->codecpar->codec_type == AVMEDIA_TYPE_AUDIO &&
			   ctx->audio_stream_idx < 0) {
			ctx->audio_dec_ctx =
				open_decoder(ctx, s, IRL_HW_DECODE_OFF);
			if (ctx->audio_dec_ctx) {
				ctx->audio_stream_idx = (int)i;
				os_atomic_store_bool(
					&ctx->audio_stream_present, true);
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
		irl_close_ffmpeg(ctx);
		return false;
	}

	blog(LOG_INFO, "[irl-source] Stream opened (video=%d, audio=%d)",
	     ctx->video_stream_idx, ctx->audio_stream_idx);

	if (ctx->audio_stream_idx >= 0) {
		AVStream *as = ctx->fmt_ctx->streams[ctx->audio_stream_idx];
		pts_repair_init(&ctx->pts_state, ctx->config.small_gap_ms,
				ctx->config.large_gap_ms, as->time_base.num,
				as->time_base.den);
	}

	return true;
}

bool irl_open_stream(struct irl_source *ctx)
{
	/* Reconnects to a stream this thread has already carried probe with
	 * a fraction of the first-connect budget; the wall-clock cost of a
	 * probe is dead air on the program feed. The short probe can miss a
	 * stream some encoders advertise late, so a result thinner than the
	 * previous session is thrown away and re-probed in full rather than
	 * trusted. prev_had_* is cleared at receiver-thread start, so a
	 * settings-forced restart (new URL, new options) always probes in
	 * full. */
	bool fast = ctx->prev_had_video || ctx->prev_had_audio;
	if (fast) {
		if (open_stream_attempt(ctx, true)) {
			bool missing = (ctx->prev_had_video &&
					ctx->video_stream_idx < 0) ||
				       (ctx->prev_had_audio &&
					ctx->audio_stream_idx < 0);
			if (!missing) {
				ctx->prev_had_video =
					ctx->video_stream_idx >= 0;
				ctx->prev_had_audio =
					ctx->audio_stream_idx >= 0;
				return true;
			}
			blog(LOG_INFO,
			     "[irl-source] Fast probe missed a stream the previous session had (video=%d, audio=%d), re-probing in full",
			     ctx->video_stream_idx, ctx->audio_stream_idx);
			irl_close_ffmpeg(ctx);
		}
	}

	if (!open_stream_attempt(ctx, false))
		return false;

	ctx->prev_had_video = ctx->video_stream_idx >= 0;
	ctx->prev_had_audio = ctx->audio_stream_idx >= 0;
	return true;
}

void irl_prepare_new_connection(struct irl_source *ctx)
{
	os_atomic_store_bool(&ctx->reconnecting, false);
	ctx->first_keyframe_received = false;
	ctx->video_pkt_gate_open = false;
	ctx->video_pkt_gate_start_us = 0;
	ctx->video_ts_init = false;
	irl_mutex_lock(&ctx->audio_state_lock);
	ctx->fade_in_pending = true;
	ctx->fade_in_frames_remaining = 0;
	ctx->startup_audio_warmup_remaining_ms = IRL_STARTUP_AUDIO_WARMUP_MS;
	irl_mutex_unlock(&ctx->audio_state_lock);
}

bool irl_wait_for_reconnect(struct irl_source *ctx)
{
	os_atomic_store_bool(&ctx->reconnecting, true);
	ctx->reconnect_count++;
	/* Sampled once: a delay edited mid-wait should apply to the next
	 * attempt, not stretch or truncate the one already counting down. */
	int delay_s = (int)os_atomic_load_long(&ctx->config.reconnect_delay);
	blog(LOG_INFO, "[irl-source] Reconnecting in %ds...", delay_s);
	for (int i = 0; i < delay_s * 10 &&
			os_atomic_load_bool(&ctx->thread_active);
	     i++) {
		av_usleep(100000);
	}
	os_atomic_store_bool(&ctx->reconnecting, false);
	return os_atomic_load_bool(&ctx->thread_active);
}

/* Caller must hold audio_state_lock: the timestamp claim advances
 * the shared output clock that the audio pump also uses. */
static void fade_out_buffered_audio(struct irl_source *ctx)
{
	int buffered_ms = audio_buffer_fill_ms_locked(&ctx->audio_buf);
	if (!ctx->audio_buf.data || buffered_ms <= 0 ||
	    !ctx->audio_out_primed)
		return;

	size_t fade_bytes =
		audio_buffer_ms_to_bytes(&ctx->audio_buf, IRL_FADE_DURATION_MS);
	size_t buffered_bytes =
		audio_buffer_ms_to_bytes(&ctx->audio_buf, buffered_ms);
	if (fade_bytes > buffered_bytes)
		fade_bytes = buffered_bytes;
	if (fade_bytes == 0)
		return;

	uint8_t *fade_buf = malloc(fade_bytes);
	if (!fade_buf)
		return;

	size_t got = audio_buffer_read_with_fade_out(&ctx->audio_buf, fade_buf,
						     fade_bytes);
	if (got > 0) {
		uint32_t fade_frames = (uint32_t)(
			got / (ctx->audio_buf.channels *
			       ctx->audio_buf.bytes_per_sample));
		struct obs_source_audio a = {0};
		a.data[0] = fade_buf;
		a.frames = fade_frames;
		a.format = AUDIO_FORMAT_FLOAT;
		a.speakers = (enum speaker_layout)ctx->audio_buf.channels;
		a.samples_per_sec = (uint32_t)ctx->audio_buf.sample_rate;
		a.timestamp = irl_audio_output_claim(
			ctx, (int)fade_frames, ctx->audio_buf.sample_rate);
		obs_source_output_audio(ctx->source, &a);
	}

	free(fade_buf);
}

void irl_handle_stream_read_error(struct irl_source *ctx, int read_ret)
{
	char errbuf[AV_ERROR_MAX_STRING_SIZE];
	os_atomic_store_bool(&ctx->reconnecting, true);
	av_strerror(read_ret, errbuf, sizeof(errbuf));
	blog(LOG_WARNING,
	     "[irl-source] Stream read error: %s (video_frames=%llu, audio_frames=%llu)",
	     errbuf, (unsigned long long)ctx->total_video_frames,
	     (unsigned long long)ctx->total_audio_frames);

	irl_close_ffmpeg(ctx);
	pts_repair_reset(&ctx->pts_state);

	/* Blank the source instead of leaving the last decoded frame frozen
	 * on screen, matching what OBS's own media source does on media end
	 * (its clear_on_media_end, likewise on by default). The audio fade-out
	 * below is the same idea for the other half of the stream. */
	if (os_atomic_load_bool(&ctx->config.clear_on_disconnect))
		irl_video_request_clear(ctx);

	irl_mutex_lock(&ctx->audio_state_lock);
	fade_out_buffered_audio(ctx);
	audio_buffer_flush(&ctx->audio_buf);
	irl_reset_stream_timing_state(ctx);
	irl_mark_audio_recovery(ctx, 2500000ULL);
	ctx->fade_in_pending = true;
	ctx->video_corrupt_frames = 0;
	ctx->video_corrupt_held = 0;
	irl_mutex_unlock(&ctx->audio_state_lock);

	ctx->current_speed = 1.0f;
	ctx->audio_output_restarts = 0;
	ctx->audio_underruns = 0;
	ctx->audio_resync_skipped_chunks = 0;
	ctx->audio_hidden_trimmed_chunks = 0;
	ctx->audio_quality_events = 0;
	ctx->audio_decoder_flushes = 0;
	ctx->video_decoder_flushes = 0;
	ctx->pts_repairs = 0;
	ctx->pts_normalizations = 0;
	ctx->pts_interpolations = 0;
	ctx->pts_resets = 0;
	ctx->pts_last_gap_ms = 0;
	ctx->pts_max_gap_ms = 0;
	ctx->silence_insertions = 0;
	ctx->total_audio_frames = 0;
	ctx->total_video_frames = 0;
	ctx->last_stats_time = 0;
}

void irl_log_receiver_stats(struct irl_source *ctx)
{
	uint64_t now = os_gettime_ns();
	if (now - ctx->last_stats_time <= 30000000000ULL)
		return;

	ctx->last_stats_time = now;

	/* Snapshot what other threads write before formatting any of it.
	 * The audio thread owns the playout offset and the buffer
	 * high-water mark; the video thread owns the lead figures; the
	 * pinned-surface peak belongs to video_queue_lock.
	 *
	 * Two separate acquisitions, never nested: no path in the plugin
	 * holds video_queue_lock and audio_state_lock at once (the video
	 * thread drops the queue lock before irl_video_output_frame, and
	 * irl_handle_video_frame drops the state lock before pushing), and
	 * a stats line is the last place that edge should be introduced.
	 *
	 * av_drift is computed inside the lock rather than from separate
	 * reads: its three inputs are only meaningful against each other,
	 * and the audio thread updates them together. */
	irl_mutex_lock(&ctx->audio_state_lock);
	/* Drift of the audio->OBS playout offset from its primed baseline.
	 * Stays near 0 when healthy; a climbing value is concealment
	 * inflating the video lip-sync mapping (see receiver-audio.c). */
	int64_t av_drift_ms = 0;
	if (ctx->audio_playout_offset_baseline_set &&
	    ctx->latest_audio_obs_end_ts_ns != 0 &&
	    ctx->latest_audio_buffered_end_pts_ns > 0) {
		av_drift_ms = ((int64_t)ctx->latest_audio_obs_end_ts_ns -
			       ctx->latest_audio_buffered_end_pts_ns -
			       ctx->audio_playout_offset_baseline_ns) /
			      1000000LL;
	}
	int audio_fill_peak_ms = ctx->audio_fill_peak_ms;
	int64_t video_lead_ms = ctx->video_lead_ns / 1000000LL;
	int64_t video_lead_peak_ms = ctx->video_lead_peak_ns / 1000000LL;
	uint64_t video_lead_excess = ctx->video_lead_excess;
	int64_t video_frame_interval_ns = ctx->video_frame_interval_ns;
	irl_mutex_unlock(&ctx->audio_state_lock);

	irl_mutex_lock(&ctx->video_queue_lock);
	int video_pinned_peak = ctx->video_pinned_peak;
	int video_pacing_now = ctx->video_pacing_now;
	int video_pacing_peak = ctx->video_pacing_peak;
	size_t video_pacing_bytes = ctx->video_pacing_bytes;
	uint64_t video_pacing_overflows = ctx->video_pacing_overflows;
	irl_mutex_unlock(&ctx->video_queue_lock);

	int buffer_fill_ms = audio_buffer_fill_ms_locked(&ctx->audio_buf);

	blog(LOG_INFO,
	     "[irl-source] Stats: video=%llu audio=%llu "
	     "buf=%dms peak=%dms target=%dms speed=%.3f ctrl=%s pts_repairs=%llu "
	     "norm=%llu interp=%llu silence=%llu resets=%llu "
	     "last_gap=%dms max_gap=%dms underruns=%llu resync_skips=%llu "
	     "hidden_trims=%llu quality_events=%llu "
	     "audio_flushes=%llu video_flushes=%llu corrupt=%llu held=%llu vq_drops=%llu "
	     "obs_lead=%lldms chunk=%u@%u "
	     "stream_chunk=%llums obs_chunk=%llums "
	     "restarts=%llu av_drift=%lldms reanchors=%llu "
	     "vlead=%lldms peak=%lldms excess=%llu vfps=%.1f "
	     "pinned_peak=%d/%d paced=%d/%d(%zuMB) early=%llu eagain=%llu/%llu pktdrop=%llu/%llu res=%dx%d",
	     (unsigned long long)ctx->total_video_frames,
	     (unsigned long long)ctx->total_audio_frames,
	     buffer_fill_ms,
	     audio_fill_peak_ms,
	     (int)os_atomic_load_long(&ctx->config.buffer_target_ms),
	     (double)ctx->current_speed,
	     os_atomic_load_bool(&ctx->config.adaptive_speed) ? "on" : "off",
	     (unsigned long long)ctx->pts_repairs,
	     (unsigned long long)ctx->pts_normalizations,
	     (unsigned long long)ctx->pts_interpolations,
	     (unsigned long long)ctx->silence_insertions,
	     (unsigned long long)ctx->pts_resets,
	     ctx->pts_last_gap_ms, ctx->pts_max_gap_ms,
	     (unsigned long long)ctx->audio_underruns,
	     (unsigned long long)ctx->audio_resync_skipped_chunks,
	     (unsigned long long)ctx->audio_hidden_trimmed_chunks,
	     (unsigned long long)ctx->audio_quality_events,
	     (unsigned long long)ctx->audio_decoder_flushes,
	     (unsigned long long)ctx->video_decoder_flushes,
	     (unsigned long long)ctx->video_corrupt_frames,
	     (unsigned long long)ctx->video_corrupt_held,
	     (unsigned long long)ctx->video_queue_drops,
	     (long long)(ctx->audio_last_obs_lead_ns / 1000000LL),
	     ctx->audio_last_frames_out, ctx->audio_last_samples_per_sec,
	     (unsigned long long)(ctx->audio_last_chunk_stream_duration_ns /
				  1000000ULL),
	     (unsigned long long)(ctx->audio_last_chunk_obs_duration_ns /
				  1000000ULL),
	     (unsigned long long)ctx->audio_output_restarts,
	     (long long)av_drift_ms,
	     (unsigned long long)ctx->audio_offset_reanchors,
	     (long long)video_lead_ms,
	     (long long)video_lead_peak_ms,
	     (unsigned long long)video_lead_excess,
	     video_frame_interval_ns > 0
		     ? 1000000000.0 / (double)video_frame_interval_ns
		     : 0.0,
	     /* peak pinned surfaces vs what extra_hw_frames budgeted;
	      * the pool must cover peak + the decoder's own frame. */
	     video_pinned_peak, IRL_VIDEO_QUEUE_SIZE + 2,
	     video_pacing_now, video_pacing_peak,
	     video_pacing_bytes / (1024u * 1024u),
	     (unsigned long long)video_pacing_overflows,
	     (unsigned long long)ctx->video_pkt_eagain,
	     (unsigned long long)ctx->audio_pkt_eagain,
	     (unsigned long long)ctx->video_pkt_dropped,
	     (unsigned long long)ctx->audio_pkt_dropped,
	     ctx->last_video_width, ctx->last_video_height);
}
