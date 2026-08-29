#pragma once

#include "../include/irl-source.h"

/* Log an action against a URL, stripping credentials while keeping protocol/host/port. */
void irl_log_input_url(const char *action, const char *url);
/* Advance the sample counter output clock and return the OBS timestamp for this chunk. */
uint64_t irl_audio_output_claim(struct irl_source *ctx, int frames,
				int out_rate);
/* Reset timing state common to both buffered and low-latency audio modes. */
void irl_reset_stream_timing_state(struct irl_source *ctx);
/* Reset audio-only timing state (output clock, speed trim, etc). */
void irl_reset_audio_timing_state(struct irl_source *ctx);
/* Mark the start of an audio recovery window (no trim, no backlog drain). */
void irl_mark_audio_recovery(struct irl_source *ctx, uint64_t duration_us);
/* True if an audio recovery window is still active. */
bool irl_audio_recovery_active(const struct irl_source *ctx);
/* Open the stream URL, probe format, and initialize decoders. Returns false to reconnect. */
bool irl_open_stream(struct irl_source *ctx);
/* Close the FFmpeg demuxer and decoders (called before reconnect and on shutdown). */
void irl_close_ffmpeg(struct irl_source *ctx);
/* Runs after a successful stream open: clears queues, resets timing, logs connection. */
void irl_prepare_new_connection(struct irl_source *ctx);
/* Sleep until the reconnect interval elapses or the thread is stopped. Returns false to exit. */
bool irl_wait_for_reconnect(struct irl_source *ctx);
/* Handle an av_read_frame error: close the stream, fade out audio, schedule reconnect. */
void irl_handle_stream_read_error(struct irl_source *ctx, int read_ret);
/* Caller must hold audio_state_lock: the pump owns the output clock and the
 * playout mapping outright, so it reads and writes them without re-taking the
 * lock anywhere below this call. The lock is NOT recursive — an inner
 * irl_mutex_lock(&ctx->audio_state_lock) on this path self-deadlocks the
 * audio thread. */
bool irl_pump_audio_once(struct irl_source *ctx);
/* Decode an audio packet and pass each frame to irl_handle_audio_frame. */
void irl_handle_audio_packet(struct irl_source *ctx, AVPacket *pkt,
			     AVFrame *frame);
/* Decode a video packet and pass each frame to irl_handle_video_frame. */
void irl_handle_video_packet(struct irl_source *ctx, AVPacket *pkt,
			     AVFrame *frame);
/* Resample, repair PTS, and write a decoded audio frame to the jitter buffer. */
void irl_handle_audio_frame(struct irl_source *ctx, AVFrame *frame);
/* Apply keyframe gate and corruption policy, then push video to the pacing queue. */
void irl_handle_video_frame(struct irl_source *ctx, AVFrame *frame);
/* Clone a decoded video frame into the receiver-to-video-thread queue. */
void irl_video_queue_push(struct irl_source *ctx, AVFrame *frame,
			  int64_t pts_ns);
/* Request the video thread to clear the async video output (called by receiver thread). */
void irl_video_request_clear(struct irl_source *ctx);
/* Video thread entry point: paces frames to their due times and outputs them to OBS. */
void *irl_video_thread(void *data);
/* Log a periodic stats line with fill levels, speed, drops, and quality events. */
void irl_log_receiver_stats(struct irl_source *ctx);
