#pragma once

#include "../include/irl-source.h"

void irl_log_input_url(const char *action, const char *url);
uint64_t irl_audio_output_claim(struct irl_source *ctx, int frames,
				int out_rate);
void irl_reset_stream_timing_state(struct irl_source *ctx);
void irl_reset_audio_timing_state(struct irl_source *ctx);
void irl_mark_audio_recovery(struct irl_source *ctx, uint64_t duration_us);
bool irl_audio_recovery_active(const struct irl_source *ctx);
bool irl_open_stream(struct irl_source *ctx);
void irl_close_ffmpeg(struct irl_source *ctx);
void irl_prepare_new_connection(struct irl_source *ctx);
bool irl_wait_for_reconnect(struct irl_source *ctx);
void irl_handle_stream_read_error(struct irl_source *ctx, int read_ret);
/* Caller must hold audio_state_lock: the pump owns the output clock and the
 * playout mapping outright, so it reads and writes them without re-taking the
 * lock anywhere below this call. The lock is NOT recursive — an inner
 * irl_mutex_lock(&ctx->audio_state_lock) on this path self-deadlocks the
 * audio thread. */
bool irl_pump_audio_once(struct irl_source *ctx);
void irl_handle_audio_packet(struct irl_source *ctx, AVPacket *pkt,
			     AVFrame *frame);
void irl_handle_video_packet(struct irl_source *ctx, AVPacket *pkt,
			     AVFrame *frame);
void irl_handle_audio_frame(struct irl_source *ctx, AVFrame *frame);
void irl_handle_video_frame(struct irl_source *ctx, AVFrame *frame);
void irl_video_queue_push(struct irl_source *ctx, AVFrame *frame,
			  int64_t pts_ns);
void irl_video_request_clear(struct irl_source *ctx);
void *irl_video_thread(void *data);
void irl_log_receiver_stats(struct irl_source *ctx);
