/*
 * obs-irl-source: IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 *
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * Codec/protocol/bitrate-agnostic live source with jitter buffering,
 * PTS repair, adaptive latency control, and first-keyframe gating.
 */

#pragma once

#ifndef OBS_IRL_SOURCE_VERSION
#define OBS_IRL_SOURCE_VERSION "0.4.0"
#endif

#include <obs-module.h>
#include <util/platform.h>
/*
 * For the os_atomic_* helpers that every cross-thread read of a hot config
 * field goes through. This is NOT for libobs's pthread wrappers —
 * irl-threading.h deliberately replaces those (see its header comment).
 *
 * The include used to arrive transitively via audio-buffer.h; when that
 * switched to irl-threading.h the atomics lost their declarations and every
 * translation unit touching a hot field failed to build on all three
 * platforms. Keep it explicit here, where the atomics are actually used.
 */
#include <util/threading.h>
#include <libavformat/avformat.h>
#include <libavcodec/avcodec.h>
#include <libswresample/swresample.h>
#include <libswscale/swscale.h>
#include <libavutil/time.h>
#include <libavutil/hwcontext.h>
#include "audio-buffer.h"
#include "irl-threading.h"
#include "pts-repair.h"

/* The source id registered with OBS. Also what the obs-websocket vendor
 * matches on to tell an IRL source from any other input. */
#define IRL_SOURCE_ID "irl_source"

/* ── Forward declarations ─────────────────────────────────── */

struct irl_source;

/* ── Configuration defaults ───────────────────────────────── */

#define IRL_DEFAULT_RECONNECT_DELAY 2
#define IRL_DEFAULT_NETWORK_BUFFER_MB 2
#define IRL_DEFAULT_BUFFER_TARGET_MS 120
#define IRL_DEFAULT_ADAPTIVE_SPEED true

/* Range of the Target Buffer slider.
 *
 * The ceiling is not a limit of the controller — it is where holding the
 * cushion stops being free. Every millisecond of audio buffer is also a
 * millisecond of decoded video held in the pacing queue (see
 * IRL_VIDEO_PACING_MAX_FRAMES/BYTES), and the whole target is paid as
 * startup delay before playback primes. High-bitrate uplinks with deep
 * sender-side buffering do stall for several seconds, though, and 2s could
 * not ride those out, so the ceiling is set by what the video side can still
 * pace rather than by what the audio side needs. */
#define IRL_BUFFER_TARGET_MIN_MS 20
#define IRL_BUFFER_TARGET_MAX_MS 8000

/* Catch-up (drain) speed authority, as a percentage above native rate.
 *
 * The build direction stays fixed at an inaudible -2%; this is the drain
 * direction, which is the audible one — 5% is ~85 cents, obvious on music
 * and unremarkable on speech. Lower it to make a recovery slower but
 * inaudible, raise it to clear a backlog faster. Bounded below by the
 * speed trim's own +-1% authority (a ceiling under that would leave the
 * integral term with nothing to work in) and above by where the pitch
 * shift stops sounding like anything but a fast-forward. */
#define IRL_DEFAULT_CATCHUP_PERCENT 5
#define IRL_CATCHUP_PERCENT_MIN 2
#define IRL_CATCHUP_PERCENT_MAX 15
#define IRL_HW_DECODE_AUTO 0
#define IRL_HW_DECODE_OFF 1
#define IRL_HW_DECODE_NVDEC 2
#define IRL_DEFAULT_HW_DECODE IRL_HW_DECODE_AUTO
#define IRL_DEFAULT_WAIT_KEYFRAME true
#define IRL_DEFAULT_LOW_LATENCY_AUDIO false
#define IRL_DEFAULT_CLOSE_WHEN_INACTIVE false
#define IRL_DEFAULT_CLEAR_ON_DISCONNECT true

/* Min/max buffer are derived from the target rather than exposed as
 * settings: min is the speed controller's low watermark, max is where
 * drain speed peaks (and sizes the ring at 4x). Keeps users from
 * creating broken configurations like min > target. */
#define IRL_BUFFER_MIN_DIVISOR 2
#define IRL_BUFFER_MIN_FLOOR_MS 20
#define IRL_BUFFER_MAX_EXTRA_MS 200

/* PTS repair thresholds (formerly settings; nobody could reason
 * about them without reading the source). Below small_gap is
 * decoder timestamp wobble, above large_gap the stream
 * fundamentally changed. */
#define IRL_SMALL_GAP_MS 70
#define IRL_LARGE_GAP_MS 2000

/* Audio fade duration on disconnect/reconnect (avoids clicks/pops) */
#define IRL_FADE_DURATION_MS 50

/* Decode/resample/PTS-repair audio in the background on startup,
 * but discard a short window before sending anything to OBS. This
 * avoids AAC/decoder warm-up artifacts without adding steady-state delay. */
#define IRL_STARTUP_AUDIO_WARMUP_MS 150

/* Full-bleed backlog policy: once the local jitter buffer holds this
 * much audio, the receiver stops reading and lets the transport hold
 * the rest (TCP/RTMP backpressure; SRT bounds its own backlog via the
 * latency window). Playback bleeds the excess at up to the configured
 * catch-up speed, so nothing audible is ever skipped. Must stay well
 * under the ring buffer capacity (4x buffer_max_ms) or writes would drop
 * old data. */
#define IRL_BLEED_PACE_FILL_MS 1000

/* Concealment inflates the audio->OBS playout offset with no bounded
 * recovery once primed (see irl_audio_maybe_reanchor_offset). This far
 * past the primed baseline the accumulated latency is treated as
 * unrecoverable by the speed-drain and reclaimed with one declared
 * re-anchor. Set above the worst normal buffer swing (buffer_max is
 * only ~200ms over target) so ordinary adaptive-speed excursions
 * never trip it; only a real outage's worth of concealment does.
 *
 * Lives here rather than next to its use in receiver-audio.c because the
 * video lead threshold below is expressed in terms of it. */
#define AUDIO_OFFSET_REANCHOR_MARGIN_MS 400

/* Reporting threshold for the video output lead.
 *
 * libobs schedules async video itself: obs_source_output_video() queues the
 * frame and ready_async_frame() releases it once the queue's play head
 * (last_frame_ts, which advances at wall-clock rate) reaches its timestamp.
 * At MAX_ASYNC_FRAMES (30) queued frames cache_video() drops the incoming
 * frame, throws the whole queue away and resets last_frame_ts, silently.
 *
 * What lands in that queue is the *growth* in lead since the play head last
 * anchored, not the lead itself — a large steady lead queues nothing. This
 * threshold is therefore a reporting aid, not a limit that gets enforced: an
 * earlier version clamped the lead against it and, because it compared the
 * absolute lead to the configured target rather than measuring growth, held
 * video half a second ahead of audio on a stream whose lead was merely large
 * and steady.
 *
 * The budget is expressed in frames because that is what libobs counts: the
 * same 400ms is 12 frames at 30fps and 48 at 120fps. Floored at the audio
 * re-anchor margin, below which a lead is still within what concealment can
 * legitimately have added.
 */
#define IRL_OBS_ASYNC_FRAME_BUDGET 24
#define IRL_VIDEO_LEAD_WARN_INTERVAL_NS 10000000000ULL

/* Bounds on the measured frame interval (250fps..10fps) and the estimate
 * used before enough frames have arrived to measure one. */
#define IRL_VIDEO_INTERVAL_MIN_NS 4000000LL
#define IRL_VIDEO_INTERVAL_MAX_NS 100000000LL
#define IRL_VIDEO_INTERVAL_DEFAULT_NS 33333333LL

/* Video output pacing.
 *
 * Video PTS is mapped through the audio playout offset for lip sync, which
 * puts each frame's correct display moment roughly one audio-buffer ahead of
 * now. Handing libobs a frame that early makes libobs hold it, and libobs
 * throws its whole async queue away past 30 held frames. So the plugin keeps
 * the frame itself and hands it over when it is due, the way OBS's own media
 * source paces (mp_media_sleep in shared/media-playback). libobs then holds
 * about one frame, and its 30-frame limit stops being reachable at any lead
 * or frame rate.
 *
 * Holding the frames is the cost: an N-millisecond lead means N milliseconds
 * of decoded video in memory, which is unavoidable — libobs was storing the
 * same thing, just capped and silently discarded. Bounded two ways, since
 * frame count alone means nothing across resolutions: whichever of the frame
 * and byte ceilings binds first. Past either, frames are emitted before they
 * are due, which is exactly the old behaviour, and counted so it is visible.
 */
/* The frame ceiling has to carry the largest Target Buffer at the highest
 * frame rate anyone streams: the lead is the audio buffer, so 8s at 120fps
 * is 960 frames. At 512 the count bound, not the byte bound, was what
 * decided when pacing gave up — and it did so at a different latency for
 * every frame rate. The byte ceiling below is the one that should bind. */
#define IRL_VIDEO_PACING_MAX_FRAMES 1024
/* The byte ceiling has to carry a realistic lead at 4K: one 3840x2160 frame
 * is ~12MB at 8-bit (NV12) and ~24MB at 10-bit (P010), so 512MB was only
 * ~700ms / ~350ms of 4K60 — a queue permanently over its ceiling on exactly
 * the streams pacing matters most for. 1GiB is ~1.4s / ~0.7s at 4K60, and it
 * is a ceiling, not a reservation: memory is only held while the audio lead
 * actually parks that much video. */
#define IRL_VIDEO_PACING_MAX_BYTES (1024u * 1024u * 1024u)
/* Emit rather than sleep again when this close to due: another wakeup costs
 * more than the timing error it would remove. */
#define IRL_VIDEO_PACING_SLACK_NS 1000000LL
/* How far ahead of its due time a frame is handed to libobs, in OBS canvas
 * ticks. The frame still carries its due time as its timestamp, so libobs
 * shows it at the same moment either way; what the lead buys is that the
 * frame is already queued when the render tick it belongs to runs.
 *
 * ready_async_frame() advances its play head by exact wall-clock deltas and
 * takes the frame whose timestamp it has just passed, so a frame already in
 * the async queue lands on a deterministic tick. A frame handed over at its
 * due time has not been queued yet when that tick runs, and slips to the
 * next one — but only sometimes, because what decides it is this thread's
 * wakeup jitter (a condvar timeout, so millisecond-granular at best, and far
 * coarser on a Windows box whose timer resolution nothing has raised). For a
 * 30fps source on a 60fps canvas that is the difference between every frame
 * holding two ticks and frames alternating between one and three: judder, on
 * exactly the panning shots where it is most visible.
 *
 * Two ticks covers that jitter. It is still a queue depth of one to four
 * source frames, far under the 30 at which cache_video() drops the whole
 * async queue, which is what the pacing queue exists to prevent.
 *
 * The lead is suppressed for one frame whenever libobs's play head is
 * unanchored (source start, and after the clear that sets last_frame_ts back
 * to 0). get_closest_frame() shows that frame the moment it arrives, whatever
 * its timestamp, and anchors last_frame_ts to it; since the play head only
 * ever advances by wall-clock deltas afterwards, anchoring it from a frame
 * delivered a lead early would run the whole connection a lead ahead of the
 * schedule the audio mapping set — a permanent video-leads-audio offset, in
 * the more noticeable direction. One frame at zero lead costs nothing: the
 * jitter this guards against is invisible on a frame that is being shown on
 * arrival regardless. */
#define IRL_VIDEO_PACING_LEAD_TICKS 2
/* Ceiling on that lead, for a canvas running at an unusually low frame rate.
 * Past this the queue depth stops being the thing worth optimising. */
#define IRL_VIDEO_PACING_MAX_LEAD_NS 50000000ULL
/* Canvas tick used when libobs has not reported one yet (60fps). */
#define IRL_VIDEO_CANVAS_TICK_DEFAULT_NS 16666667ULL
/* Ceiling on a single pacing sleep, so a clear or a shutdown is never left
 * waiting on a frame that is due far in the future. */
#define IRL_VIDEO_PACING_MAX_WAIT_MS 50
/* How long the video thread keeps using the last audio playout offset after
 * the published mapping goes away. Long enough to cover a re-anchor, which
 * republishes on the pump's very next chunk (~21ms), and far short of the
 * 2s reconnect delay, so a new connection never inherits the old epoch. */
#define IRL_VIDEO_OFFSET_HOLD_NS 500000000ULL

/* Abort a blocking read/connect through the FFmpeg interrupt callback
 * after this long without progress. A dead-but-open connection (uplink
 * loss in a dead zone) otherwise hangs av_read_frame forever with no
 * reconnect. Connect plus stream probe normally completes in under 3s. */
#define IRL_IO_STALL_TIMEOUT_US 10000000ULL

/* One frame waiting for its moment.
 *
 * `pts_ns` is the frame's stream PTS; `due_ns` is where the audio playout
 * offset currently places it on the OBS clock. The due time is re-derived
 * from pts_ns every pacing cycle rather than frozen at intake, because the
 * offset is a live quantity: the speed controller moves it continuously
 * (a chunk played at +5% advances the OBS side by frames_out/rate and the
 * stream side by in_frames/rate, so the offset shrinks by ~4.8% of that
 * chunk), and a re-anchor steps it.
 *
 * A frozen due time meant a queued frame was scheduled against whatever the
 * offset happened to be when it was decoded, and kept that schedule for its
 * whole residence — so every reclaim the audio side performed left video
 * behind by the reclaimed amount until the queue drained. Re-deriving is
 * what puts video on the same latency-reclaim mechanism as the audio it is
 * supposed to stay level with. */
struct irl_pacing_frame {
	AVFrame *frame;
	int64_t pts_ns;
	uint64_t due_ns;
	size_t bytes;
};

/* ── Source configuration ─────────────────────────────────── */

/* Fields marked hot are swapped in place by irl_source_update() while the
 * worker threads are running, so every cross-thread access goes through
 * os_atomic_*. The rest are only written while the threads are stopped;
 * irl_thread_create/irl_thread_join supply the happens-before edge for
 * those. */
struct irl_config {
	/* General */
	char *url;
	volatile long reconnect_delay; /* hot */
	int network_buffer_mb;

	/* Audio buffer. Only target_ms is a user setting; min/max are
	 * derived from it in config_load(). */
	volatile long buffer_target_ms; /* hot */
	volatile long buffer_min_ms;    /* hot */
	volatile long buffer_max_ms;    /* hot */
	volatile bool adaptive_speed;   /* hot */
	/* Percent above native rate the drain may reach. See
	 * IRL_DEFAULT_CATCHUP_PERCENT. */
	volatile long catchup_percent; /* hot */

	/* PTS repair (constants, kept here so pts_repair_init has one
	 * source of truth) */
	int small_gap_ms;
	int large_gap_ms;

	/* Advanced */
	char *ffmpeg_options;
	int hw_decode;
	volatile bool wait_for_keyframe; /* hot */
	bool low_latency_audio;
	bool close_when_inactive; /* hot, but OBS-thread only */
	/* OBS's media source calls this clear_on_media_end and defaults it
	 * on; same meaning here, minus the local-file cases. */
	volatile bool clear_on_disconnect; /* hot */
};

/* ── Main source context ──────────────────────────────────── */

struct irl_source {
	obs_source_t *source;
	struct irl_config config;

	/* Receiver / demux thread */
	irl_thread_t receiver_thread;
	irl_thread_t audio_thread;
	irl_thread_t video_thread;
	irl_mutex_t audio_state_lock;
	volatile bool thread_active;
	volatile bool reconnecting;

	/* Video output queue (receiver thread → video thread).
	 * Decouples the GPU→CPU frame transfer and format conversion
	 * from the receiver thread so a GPU stall cannot starve audio
	 * decode. Depth stays small because queued HW frames pin
	 * decoder surface-pool entries (covered by extra_hw_frames at
	 * decoder open, which budgets this queue plus the two frames
	 * in flight around it). Queued frame->pts is in nanoseconds; the
	 * receiver converts before queueing because it may close
	 * fmt_ctx while frames are still in flight. */
#define IRL_VIDEO_QUEUE_SIZE 4
	irl_mutex_t video_queue_lock;
	irl_cond_t video_queue_cond;
	AVFrame *video_queue[IRL_VIDEO_QUEUE_SIZE];
	int video_queue_head;
	int video_queue_count;
	uint64_t video_queue_drops;
	/* Decoder surfaces this plugin pins at once, for checking the
	 * extra_hw_frames budget against reality rather than against a
	 * reading of the code: frames sitting in the queue plus the one the
	 * video thread has popped and is converting. The frame the decoder
	 * has just handed the receiver thread is not counted (it lives on
	 * the other thread and is unref'd immediately), so the pool
	 * requirement is this peak plus one. Cumulative for the source —
	 * a two-hour stream's high-water mark is the interesting number.
	 * Both guarded by video_queue_lock. */
	int video_in_flight;
	int video_pinned_peak;

	/* Pacing queue: decoded frames in system memory, waiting for their
	 * due time. Video-thread-private — the receiver thread never touches
	 * it, and a clear is routed through video_clear_pending, which the
	 * video thread consumes — so unlike video_queue above it needs no
	 * lock. Entries hold no decoder surfaces: irl_video_to_sysmem()
	 * copies out of the hardware pool precisely so this queue can be
	 * deep. video_pacing_* are read for stats without the lock, like
	 * video_queue_drops. */
	struct irl_pacing_frame pacing_queue[IRL_VIDEO_PACING_MAX_FRAMES];
	int pacing_head;
	int pacing_count;
	size_t pacing_bytes;
	int pacing_peak;
	uint64_t pacing_overflows;
	/* Set while libobs's async play head is unanchored, so the next frame
	 * out must go at its due time rather than a lead early. See
	 * IRL_VIDEO_PACING_LEAD_TICKS. Video-thread-private. */
	bool pacing_anchor_pending;
	/* Last audio playout offset the video thread saw, and when. Also
	 * video-thread-private. A re-anchor zeroes the published mapping for
	 * the one pump iteration it takes to rebuild it; rescheduling the
	 * whole queue against the video-only fallback in that window would
	 * move every frame onto a different clock, so the last good offset is
	 * held instead. The hold expires well short of a reconnect, whose
	 * fresh PTS epoch must not inherit the previous connection's offset. */
	int64_t video_playout_offset_ns;
	uint64_t video_playout_offset_time_ns;

	/* Recycled destination buffers for the GPU→CPU transfer in
	 * irl_video_to_sysmem(). Without a pool every frame heap-allocates
	 * and frees its full pixel buffer — ~12MB per 4K NV12 frame, 60
	 * times a second — and allocations that size go straight to the OS,
	 * so each one pays page zeroing plus thousands of soft page faults
	 * when the copy first writes into it. Video-thread-private, like the
	 * pacing queue whose entries the buffers end up in; rebuilt when the
	 * transfer geometry changes and released on clear/exit through
	 * irl_video_xfer_pool_release(). The broken flag latches a backend
	 * that refuses caller-allocated destinations, falling back for good
	 * to the old per-frame path. */
	AVBufferPool *video_xfer_pool;
	int video_xfer_pool_w;
	int video_xfer_pool_h;
	enum AVPixelFormat video_xfer_pool_fmt;
	bool video_xfer_pool_broken;

	/* Published copies of the four above, mirrored under
	 * video_queue_lock once per pacing cycle so the stats line on the
	 * receiver thread has something synchronised to read. */
	int video_pacing_now;
	int video_pacing_peak;
	size_t video_pacing_bytes;
	uint64_t video_pacing_overflows;
	/* Set by the receiver thread on disconnect, consumed by the video
	 * thread. Guarded by video_queue_lock. The clear has to run on the
	 * video thread so it cannot be undone by a frame that was already
	 * mid-conversion when the disconnect was noticed. */
	bool video_clear_pending;

	/* FFmpeg state (owned by receiver thread) */
	AVFormatContext *fmt_ctx;
	/* Armed before each blocking FFmpeg I/O call; interrupt_cb
	 * aborts the call when it has been blocked past the stall
	 * timeout. Receiver-thread owned (interrupt_cb runs on the
	 * calling thread). */
	uint64_t io_start_us;
	AVCodecContext *audio_dec_ctx;
	AVCodecContext *video_dec_ctx;
	AVBufferRef *hw_device_ctx;
	enum AVHWDeviceType hw_device_type;
	int audio_stream_idx;
	int video_stream_idx;
	/* "This connection carries audio", published for the video thread.
	 *
	 * audio_stream_idx itself is receiver-thread state: it indexes
	 * fmt_ctx->streams, the receiver thread rewrites both on every
	 * reconnect, and nothing outside that thread can read either safely.
	 * The video thread needs the fact, not the index — before the audio
	 * playout mapping exists it still has to know whether to hold video
	 * back for audio that is about to prime — so the fact is mirrored
	 * here and goes through os_atomic_*, like the hot config fields. Set
	 * wherever audio_stream_idx is assigned, and only there. */
	volatile bool audio_stream_present;
	/* What the previous connection of this receiver thread carried, so
	 * a reconnect can probe fast and detect when the short probe missed
	 * a stream the last session had. Receiver-thread owned; survives
	 * irl_close_ffmpeg, cleared at thread start so a settings-forced
	 * restart probes in full. */
	bool prev_had_video;
	bool prev_had_audio;
	bool using_hw_decode;
	/* Tri-state: -1 = not yet attempted, 0 = map fails, falling back
	 * to transfer_data, 1 = map succeeded at least once. Used to
	 * skip the doomed map attempt on the second frame onwards once
	 * we've learned the platform can't map. */
	int hw_map_ok;

	/* Resampler (planar → interleaved float) */
	SwrContext *swr_ctx;
	int swr_in_rate;
	int swr_in_channels;
	enum AVSampleFormat swr_in_format;

	/* Video scaler (for format conversion to OBS) */
	struct SwsContext *sws_ctx;
	int sws_src_w;
	int sws_src_h;
	enum AVPixelFormat sws_src_fmt;
	uint8_t *sws_nv12_buf;       /* receiver-thread-owned NV12 scratch */
	size_t sws_nv12_buf_capacity;
	/* Destination wrapper for sws_scale_frame(). Describes sws_nv12_buf;
	 * never owns pixel data, so it is freed with av_frame_free() alone. */
	AVFrame *sws_dst_frame;

	/* Video timestamp sync (anchors stream PTS to system clock) */
	bool video_ts_init;
	uint64_t video_sys_base;  /* os_gettime_ns() at first frame */
	int64_t video_pts_base;   /* stream PTS at first frame (in ns) */
	/* Previous decoded PTS, receiver-thread-owned; feeds the frame
	 * interval EMA. */
	int64_t video_prev_pts_ns;
	/* Throttle for the lead-cap warning, video-thread-owned. */
	uint64_t video_lead_warn_time_ns;

	/* Audio output clock.  OBS timestamps are a pure sample
	 * counter anchored once at prime time:
	 *   ts = anchor + samples/rate
	 * Contiguous by construction, so OBS's timestamp smoothing
	 * always takes the seamless-append path.  The wall clock is
	 * only consulted for pacing and stall detection. */
	bool audio_out_primed;
	uint64_t audio_out_anchor_ns;
	uint64_t audio_out_samples;
	uint64_t audio_output_restarts;

	/* Concealment during a delivery stall advances the OBS output
	 * clock while the stream PTS it maps against stays frozen, so the
	 * audio->OBS playout offset (and the video lip-sync mapping built
	 * on it) inflates by the outage length with no bounded recovery
	 * once primed. Capture the offset at prime as a baseline; the pump
	 * re-anchors when it drifts too far past it, instead of letting
	 * latency ratchet up permanently on every connection blip. */
	int64_t audio_playout_offset_baseline_ns;
	bool audio_playout_offset_baseline_set;
	uint64_t audio_offset_reanchors;

	/* Output-side speed resampler (audio thread).  Playback speed
	 * is applied here via swr compensation because changing the
	 * samples_per_sec submitted to OBS forces libobs to rebuild
	 * its per-source resampler with no crossfade (audible click
	 * per change). */
	SwrContext *speed_swr;
	int speed_swr_rate;
	int speed_swr_channels;
	/* Fractional output-sample debt carried between chunks (audio
	 * thread).  The resampler is driven in whole samples per chunk, so
	 * without this the applied speed is quantised to multiples of
	 * 1/chunk — about 0.1% at 1024 frames.  Everything the controller
	 * asks for below that either rounds away to 1.0 or gets executed at
	 * twice its size, which makes both the deadband slope and the trim
	 * meaningless, and makes the compensation chatter on and off as the
	 * request crosses a rounding boundary.  Carrying the remainder makes
	 * the long-run rate exact at any requested speed. */
	double audio_speed_frac;
	uint8_t *audio_speed_scratch;      /* audio thread */
	size_t audio_speed_scratch_capacity;

	/* Dropout concealment state (audio thread) */
	float audio_out_last_sample[8];
	int audio_out_last_channels;
	bool audio_out_last_valid;
	bool audio_conceal_fade_pending;

	/* Stream PTS tracking for A/V sync and re-sync mode */
	int64_t latest_audio_stream_pts_ns;
	int64_t latest_video_stream_pts_ns;

	/* Video lead diagnostics, all guarded by audio_state_lock.
	 *
	 * video_frame_interval_ns is an EMA of decoded PTS deltas, written
	 * by the receiver thread and read by the video thread to estimate
	 * how many frames a given lead parks in the libobs async queue.
	 * video_lead_ns is the lead the PTS mapping asked for before the
	 * cap (the uncapped value is the diagnostic: it shows the ratchet),
	 * written by the video thread and read by the OBS thread. */
	int64_t video_frame_interval_ns;
	int64_t video_lead_ns;
	int64_t video_lead_peak_ns;
	uint64_t video_lead_excess;

	/* Latest audio already queued to OBS, in OBS clock domain.
	 * Used to align video to actual audio playout instead of
	 * approximating from the plugin-side jitter-buffer fill. */
	uint64_t latest_audio_obs_end_ts_ns;
	int64_t latest_audio_buffered_end_pts_ns;

	/* Audio jitter buffer */
	struct audio_buffer audio_buf;

	/* Per-thread scratch buffers (no lock needed; each is owned by
	 * exactly one thread).  Grown on demand to avoid per-frame
	 * malloc, which is a real latency source on lossy IRL streams
	 * where decode/output bursts coincide with allocator pressure. */
	uint8_t *audio_pump_scratch;       /* audio thread */
	size_t audio_pump_scratch_capacity;
	uint8_t *audio_resample_scratch;   /* receiver thread */
	size_t audio_resample_scratch_capacity;

	/* PTS repair state */
	struct pts_repair pts_state;

	/* Buffered audio correction state */
	float current_speed;

	/* Persistent component of playback speed (audio thread).
	 *
	 * The ramp in compute_buffered_output_speed() is proportional: it
	 * only produces a speed away from 1.0 while the buffer sits away
	 * from target. That is the right shape for a transient — a stall's
	 * backlog drains and the ramp relaxes — but it cannot hold a
	 * *constant*. A sender whose media clock runs at 1.003x delivers
	 * 3ms of extra audio every second forever, and the only ramp
	 * position that consumes it is one with a permanent level error, so
	 * the buffer parks off-target and the latency parks with it, right
	 * up until the offset re-anchor concedes and splices.
	 *
	 * The trim is the integral term that removes that standing error.
	 * It accumulates only in the ramp's linear region, where the level
	 * genuinely reports the sender's rate, and is clamped to ±1% —
	 * enough for any real crystal (<0.01%) or framerate mismatch
	 * (~0.1%), far below audibility, and small enough that the ramp
	 * keeps essentially all of its authority for actual transients.
	 *
	 * It converges to the sender's rate without ever measuring it. */
	float audio_speed_trim;
	uint64_t audio_speed_trim_last_us;
	uint64_t audio_underruns;
	uint64_t audio_resync_skipped_chunks;
	uint64_t audio_hidden_trimmed_chunks;
	uint64_t audio_quality_events;
	int64_t audio_last_obs_lead_ns;
	uint64_t audio_last_chunk_stream_duration_ns;
	uint64_t audio_last_chunk_obs_duration_ns;
	uint32_t audio_last_frames_out;
	uint32_t audio_last_samples_per_sec;
	uint64_t audio_recovery_until_us;

	/* Decoded frame size (samples per frame).  Used as the output
	 * chunk size so OBS's smoothing advance matches our push rate.
	 * AAC = 1024, Opus = 960.  If mismatched, smoothing drifts
	 * and periodically resets audio_ts → "audio is lagging". */
	int decoded_frame_samples;

	/* Consecutive decode error counters.  Only flush the decoder
	 * after 3+ consecutive errors — a single corrupt packet should
	 * not reset the decoder state (losing reference frames). */
	int audio_decode_errors;
	int video_decode_errors;
	/* Detection of a drain that cannot win: the audio thread owns these. */
	uint64_t audio_drain_stuck_since_us;
	int audio_drain_stuck_fill_ms;
	uint64_t audio_drain_warn_time_us;
	/* Jitter-buffer high-water mark, same sampling argument as
	 * video_lead_peak_ns: the backlog excursion that drives everything
	 * here is transient, and `buf` at log time usually misses it. */
	int audio_fill_peak_ms;
	/* avcodec_send_packet() returned EAGAIN, meaning the decoder did not
	 * accept the packet and it must be resent after draining output.
	 * Rare when the frame pool is adequately sized, which is exactly why
	 * a non-zero count is worth seeing: it is the signal that decoder
	 * surfaces are exhausted. */
	uint64_t video_pkt_eagain;
	uint64_t audio_pkt_eagain;
	/* Packets still refused after the drain-and-resend retry, and so
	 * genuinely lost. This is the number that costs picture quality;
	 * the eagain counters above only say the condition was hit. */
	uint64_t video_pkt_dropped;
	uint64_t audio_pkt_dropped;
	uint64_t audio_decoder_flushes;
	/* Always 0: the video decoder is no longer flushed on a corruption
	 * burst (see receiver-decode.c). Kept so scripts and websocket
	 * clients that read it keep working. */
	uint64_t video_decoder_flushes;
	uint64_t audio_last_decoder_flush_time_us;
	uint64_t audio_last_decoder_warning_time_us;
	uint64_t video_last_decoder_warning_time_us;

	/* Video corruption tracking.  Set when send_packet fails
	 * (HW decoders may not set decode_error_flags reliably).
	 * Cleared on next keyframe. */
	bool video_corrupted;
	bool video_skip_logged;
	/* Decoded frames the decoder itself flagged as damaged
	 * (decode_error_flags or AV_FRAME_FLAG_CORRUPT), and the subset
	 * held back instead of shown: HEVC frames predicted from a missing
	 * reference, which come out flat gray. Receiver thread writes. */
	uint64_t video_corrupt_frames;
	uint64_t video_corrupt_held;
	bool video_hold_logged;

	/* Keyframe gate.  Packet-level: don't feed the decoder at all
	 * until a key packet arrives (avoids reference-miss error spam
	 * and decoder churn on join).  When enabled, the frame-level
	 * backstop gates decoded output until first_keyframe_received. */
	bool first_keyframe_received;
	bool video_pkt_gate_open;
	uint64_t video_pkt_gate_start_us;

	/* Audio fade state */
	bool fade_in_pending;
	int fade_in_frames_remaining;
	int startup_audio_warmup_remaining_ms;
	float audio_last_sample[8];
	int audio_last_sample_channels;
	bool audio_last_sample_valid;

	/* Resolution tracking (for mid-stream changes) */
	int last_video_width;
	int last_video_height;

	/* One-shot fit-to-canvas for a freshly added source (OBS thread
	 * only: set at create, consumed in tick). */
	bool fit_pending;

	/* Set by the media-control Stop/Pause action, cleared by
	 * Restart/Play and by a settings update. Keeps the source down
	 * across show/activate, which would otherwise restart it. OBS
	 * thread only, like the callbacks that touch it. */
	bool media_stopped;

	/* Statistics */
	uint64_t total_audio_frames;
	uint64_t total_video_frames;
	uint64_t pts_repairs;
	uint64_t pts_normalizations;
	uint64_t pts_interpolations;
	uint64_t pts_resets;
	int pts_last_gap_ms;
	int pts_max_gap_ms;
	uint64_t silence_insertions;
	uint64_t reconnect_count;
	uint64_t last_stats_time;
};

/* ── Lifecycle (irl-source.c) ─────────────────────────────── */

void *irl_source_create(obs_data_t *settings, obs_source_t *source);
void irl_source_destroy(void *data);
void irl_source_update(void *data, obs_data_t *settings);
void irl_source_activate(void *data);
void irl_source_deactivate(void *data);
void irl_source_show(void *data);
void irl_source_hide(void *data);
void irl_source_tick(void *data, float seconds);
const char *irl_source_get_name(void *unused);

/* Media controls (OBS_SOURCE_CONTROLLABLE_MEDIA). Drive the media
 * controls dock, and obs-websocket's TriggerMediaInputAction /
 * GetMediaInputStatus. */
void irl_source_media_play_pause(void *data, bool pause);
void irl_source_media_restart(void *data);
void irl_source_media_stop(void *data);
enum obs_media_state irl_source_media_get_state(void *data);

/* ── Settings (settings.c) ────────────────────────────────── */

obs_properties_t *irl_source_get_properties(void *data);
void irl_source_get_defaults(obs_data_t *settings);

/* ── Receiver thread (receiver.c) ─────────────────────────── */

void *irl_receiver_thread(void *data);
void *irl_audio_thread(void *data);
void irl_receiver_stop(struct irl_source *ctx);

/* ── Audio buffer (audio-buffer.c) ────────────────────────── */
/* See audio-buffer.h */

/* ── Video handler (video-handler.c) ──────────────────────── */

bool irl_video_output_frame(struct irl_source *ctx, AVFrame *frame,
			    uint64_t timestamp);
AVFrame *irl_video_to_sysmem(struct irl_source *ctx, AVFrame *frame);
void irl_video_xfer_pool_release(struct irl_source *ctx);
uint64_t irl_video_due_time(struct irl_source *ctx, const AVFrame *frame);
bool irl_video_playout_offset(struct irl_source *ctx, int64_t *offset_ns);
bool irl_video_is_keyframe(const AVFrame *frame);

/* ── PTS repair (pts-repair.c) ────────────────────────────── */
/* See pts-repair.h */

/* ── obs-websocket vendor (websocket-vendor.c) ────────────── */

void irl_websocket_vendor_register(void);
