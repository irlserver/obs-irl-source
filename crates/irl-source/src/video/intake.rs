//! Decoded video intake on the receiver thread (port of
//! `irl_handle_video_frame`, `receiver-video.c:289-438`). W2-C.

use std::sync::atomic::Ordering::Relaxed;

use irl_core::video_time;

use crate::receiver::ReceiverFlags;
use crate::shared::Shared;
use crate::video::output;

/// Nanosecond time base every queued PTS is rescaled into: the video thread
/// must not touch the format context, which the receiver frees on reconnect
/// while queued frames may still be in flight.
const NS_TIME_BASE: ffmpeg::Rational = ffmpeg::Rational::new(1, 1_000_000_000);

/// Receiver-thread video intake state (beyond [`ReceiverFlags`]).
///
/// The C kept all of it on `struct irl_source`; the decomposition puts the
/// cross-function flags in [`ReceiverFlags`] and the counters in the shared
/// stats, which leaves this a marker for the intake's identity and lifetime.
pub struct VideoIntake {
    _private: (),
}

impl VideoIntake {
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Per-connection reset.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// One decoded frame: PTS validation, keyframe gate, HEVC corrupt hold,
    /// corrupt-frame accounting, resolution-change detection, rescale to ns,
    /// frame-interval EMA publish, push onto `shared.video`.
    pub fn handle_frame(
        &mut self,
        shared: &Shared,
        flags: &mut ReceiverFlags,
        frame: &ffmpeg::Frame,
        tb: ffmpeg::Rational,
        codec_id: ffmpeg::AVCodecID,
    ) {
        let Some(pts) = frame.best_effort_pts() else {
            if !flags.video_skip_logged {
                irl_warn!("Dropping video frame without valid PTS");
                flags.video_skip_logged = true;
            }
            return;
        };

        let (width, height) = (frame.width(), frame.height());
        let is_key = output::is_keyframe(frame);

        // The frame-level backstop only gates when Wait For Keyframe is on
        // (master 64dcd0f); the first-keyframe bookkeeping runs either way.
        if !flags.first_keyframe_received
            && !is_key
            && shared.hot.wait_for_keyframe.load(Relaxed)
        {
            if shared.conn.total_video_frames.load(Relaxed) == 0 {
                irl_debug!("Waiting for keyframe (dropped non-keyframe)");
            }
            return;
        }

        if !flags.first_keyframe_received && is_key {
            flags.first_keyframe_received = true;
            flags.video_corrupted = false;
            // `hw_frames_ctx` on the decoded frame is the ground truth for
            // whether hardware decode is actually in use; the stream-open log
            // only reports what was requested.
            irl_info!(
                "First keyframe received ({}x{} fmt={} {} decode)",
                width,
                height,
                frame.pix_fmt() as i32,
                if frame.is_hw() {
                    "hardware"
                } else {
                    "software"
                }
            );
        }

        if is_key {
            flags.video_corrupted = false;
        }

        // Damage the decoder reported on this frame. Both flags, because the
        // decoders disagree on which to set: h264dec sets decode_error_flags on
        // a frame it concealed and AV_FRAME_FLAG_CORRUPT only on frames before
        // its first recovery point, while the HEVC decoder never sets
        // decode_error_flags at all and reports its one kind of damage — a
        // missing reference — as AV_FRAME_FLAG_CORRUPT on every frame predicted
        // from it, until the next IDR/CRA.
        let frame_corrupt = frame.is_corrupt();
        let frame_damaged = frame_corrupt || frame.decode_error_flags() != 0;
        if frame_damaged {
            shared.conn.video_corrupt_frames.fetch_add(1, Relaxed);
        }

        // HEVC has no error concealment: a reference that never arrived is
        // synthesized as a flat mid-gray picture (hevc/refs.c
        // generate_missing_ref; under hwaccel it is whatever stale surface the
        // pool hands back), and everything predicted from it is gray with
        // residuals painted on top. That is a worse picture than the last good
        // frame, so hold it back and let OBS keep showing what it has; the
        // chain heals at the next keyframe, and the decoder clears the flag
        // there. H.264 keeps the passthrough: its concealment patches a damaged
        // frame from the previous one, which is a usable picture, and its
        // AV_FRAME_FLAG_CORRUPT never fires past the keyframe gate.
        if frame_corrupt && codec_id == ffmpeg::AVCodecID::AV_CODEC_ID_HEVC {
            shared.conn.video_corrupt_held.fetch_add(1, Relaxed);
            if !flags.video_hold_logged {
                irl_warn!(
                    "HEVC frame predicted from a missing reference; holding the last good frame until the next keyframe"
                );
                flags.video_hold_logged = true;
            }
            return;
        }
        if flags.video_hold_logged {
            irl_info!(
                "Keyframe received, HEVC video resumed ({} frames held this connection)",
                shared.conn.video_corrupt_held.load(Relaxed)
            );
            flags.video_hold_logged = false;
        }

        if flags.video_corrupted || frame_damaged {
            if !flags.video_skip_logged {
                irl_warn!("Passing through corrupt video frames to preserve cadence");
                flags.video_skip_logged = true;
            }
        } else if flags.video_skip_logged {
            irl_info!("Clean video frame received, normal video cadence restored");
            flags.video_skip_logged = false;
        }

        if flags.last_video_width != 0
            && flags.last_video_height != 0
            && (width != flags.last_video_width || height != flags.last_video_height)
        {
            irl_info!(
                "Resolution changed: {}x{} -> {}x{}",
                flags.last_video_width,
                flags.last_video_height,
                width,
                height
            );
            // The video thread re-anchors its fallback clock on the next frame.
            shared.conn.video_ts_init.store(false, Relaxed);
        }
        flags.last_video_width = width;
        flags.last_video_height = height;
        shared.conn.last_video_width.store(width, Relaxed);
        shared.conn.last_video_height.store(height, Relaxed);

        // Convert PTS to nanoseconds here, on the thread that owns the stream.
        let pts_ns = ffmpeg::rescale_q(pts, tb, NS_TIME_BASE);

        // Frame interval EMA, for the video thread's estimate of how many
        // frames a given output lead parks in the libobs async queue. Measured
        // rather than taken from avg_frame_rate, which live SRT/RTMP demuxers
        // routinely leave unset or wrong. Out-of-range deltas (PTS repair,
        // discontinuities, reordering) are skipped rather than smoothed in.
        let delta = pts_ns - flags.video_prev_pts_ns;
        let measurable = flags.video_prev_pts_ns != 0;
        flags.video_prev_pts_ns = pts_ns;

        {
            let mut state = shared.audio_state();
            state.latest_video_stream_pts_ns = pts_ns;
        }
        if measurable {
            let prev = shared.conn.video_frame_interval_ns.load(Relaxed);
            shared
                .conn
                .video_frame_interval_ns
                .store(video_time::interval_ema(prev, delta), Relaxed);
        }

        // The queue owns its entries, so it gets its own reference with the
        // nanosecond PTS written into it.
        if let Ok(mut queued) = frame.new_ref() {
            queued.set_pts(pts_ns);
            shared.video.push(queued, &shared.lifetime);
        }

        let total = shared.conn.total_video_frames.fetch_add(1, Relaxed) + 1;
        if total == 1 {
            irl_info!("First video frame queued");
        }
    }
}

impl Default for VideoIntake {
    fn default() -> Self {
        Self::new()
    }
}
