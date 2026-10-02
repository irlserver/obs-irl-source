//! Decoded video intake. Runs on the video thread, next to the decoder (see
//! [`crate::video::decode`] for why that is not the receiver).

use std::sync::atomic::Ordering::Relaxed;

use irl_core::video_time;

use crate::shared::Shared;

/// Video-thread-owned decode and intake state. Only the two flags the audio
/// path also touches are shared ([`crate::shared::VideoFlags`]); everything
/// here belongs to one thread.
#[derive(Default)]
pub struct DecodeState {
    /// Packet-level keyframe gate: the decoder is not fed until a key packet
    /// arrives.
    pub pkt_gate_open: bool,
    /// When the packet gate started waiting (FFmpeg µs domain).
    pub pkt_gate_start_us: u64,
    /// Consecutive decode errors. Video is never flushed on a burst; the gate
    /// and the corrupt-frame handling cover it.
    pub decode_errors: i32,
    /// Throttle for decoder warnings (FFmpeg µs domain).
    pub last_warning_us: u64,
    /// Log throttles.
    pub skip_logged: bool,
    pub hold_logged: bool,
    /// Previous decoded PTS (ns) for the frame-interval EMA.
    pub prev_pts_ns: i64,
    /// Last seen dimensions, for mid-stream resolution changes.
    pub last_width: i32,
    pub last_height: i32,
}

impl DecodeState {
    /// Per-connection reset: a new decoder, so nothing carries over.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// An audio PTS reset broke the timeline, so the interval estimate and the
    /// decoder-error bookkeeping no longer describe this stream.
    ///
    /// The keyframe gates are deliberately untouched — the connection did not
    /// change and video has not lost its reference frames.
    pub fn reset_timeline(&mut self) {
        self.prev_pts_ns = 0;
        self.decode_errors = 0;
        self.last_warning_us = 0;
        self.skip_logged = false;
        self.hold_logged = false;
    }
}

/// One decoded frame: PTS validation, keyframe gate, HEVC corrupt hold,
/// corrupt-frame accounting, resolution-change detection, rescale to ns and the
/// frame-interval EMA.
///
/// Returns the frame to pace (a new reference carrying the nanosecond PTS), or
/// `None` when it should not be shown.
pub fn handle_frame(
    shared: &Shared,
    state: &mut DecodeState,
    frame: &ffmpeg::Frame,
    tb: ffmpeg::Rational,
    codec_id: ffmpeg::AVCodecID,
) -> Option<ffmpeg::Frame> {
    let Some(pts) = frame.best_effort_pts() else {
        if !state.skip_logged {
            irl_warn!("Dropping video frame without valid PTS");
            state.skip_logged = true;
        }
        return None;
    };

    let (width, height) = (frame.width(), frame.height());
    let is_key = frame.is_key();
    let first_keyframe = shared.video_flags.first_keyframe.load(Relaxed);

    // The frame-level backstop only gates when Wait For Keyframe is on; the
    // first-keyframe bookkeeping runs either way.
    if !first_keyframe && !is_key && shared.hot.wait_for_keyframe.load(Relaxed) {
        if shared.conn.total_video_frames.load(Relaxed) == 0 {
            irl_debug!("Waiting for keyframe (dropped non-keyframe)");
        }
        return None;
    }

    if !first_keyframe && is_key {
        shared.video_flags.first_keyframe.store(true, Relaxed);
        shared.video_flags.corrupted.store(false, Relaxed);
        // `hw_frames_ctx` on the decoded frame is the ground truth for whether
        // hardware decode is actually in use; the stream-open log only reports
        // what was requested.
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
        shared.video_flags.corrupted.store(false, Relaxed);
    }

    // Damage the decoder reported on this frame. Both flags, because the
    // decoders disagree on which to set: h264dec sets decode_error_flags on a
    // frame it concealed and AV_FRAME_FLAG_CORRUPT only on frames before its
    // first recovery point, while the HEVC decoder never sets
    // decode_error_flags at all and reports its one kind of damage — a missing
    // reference — as AV_FRAME_FLAG_CORRUPT on every frame predicted from it,
    // until the next IDR/CRA.
    let frame_corrupt = frame.is_corrupt();
    let frame_damaged = frame_corrupt || frame.decode_error_flags() != 0;

    // HEVC has no error concealment: a reference that never arrived is
    // synthesized as a flat mid-gray picture (hevc/refs.c
    // generate_missing_ref; under hwaccel it is whatever stale surface the pool
    // hands back), and everything predicted from it is gray with residuals
    // painted on top. That is a worse picture than the last good frame, so hold
    // it back and let OBS keep showing what it has; the chain heals at the next
    // keyframe, and the decoder clears the flag there. H.264 keeps the
    // passthrough: its concealment patches a damaged frame from the previous
    // one, which is a usable picture, and its AV_FRAME_FLAG_CORRUPT never fires
    // past the keyframe gate.
    if frame_corrupt && codec_id == ffmpeg::AVCodecID::AV_CODEC_ID_HEVC {
        shared.conn.video_corrupt_held.fetch_add(1, Relaxed);
        if !state.hold_logged {
            irl_warn!(
                "HEVC frame predicted from a missing reference; holding the last good frame until the next keyframe"
            );
            state.hold_logged = true;
        }
        return None;
    }
    if state.hold_logged {
        irl_info!(
            "Keyframe received, HEVC video resumed ({} frames held this connection)",
            shared.conn.video_corrupt_held.load(Relaxed)
        );
        state.hold_logged = false;
    }

    if shared.video_flags.corrupted.load(Relaxed) || frame_damaged {
        if !state.skip_logged {
            irl_warn!("Passing through corrupt video frames to preserve cadence");
            state.skip_logged = true;
        }
    } else if state.skip_logged {
        irl_info!("Clean video frame received, normal video cadence restored");
        state.skip_logged = false;
    }

    if state.last_width != 0
        && state.last_height != 0
        && (width != state.last_width || height != state.last_height)
    {
        irl_info!(
            "Resolution changed: {}x{} -> {}x{}",
            state.last_width,
            state.last_height,
            width,
            height
        );
        // The fallback clock re-anchors on the next frame.
        shared.conn.video_ts_init.store(false, Relaxed);
    }
    state.last_width = width;
    state.last_height = height;
    shared.conn.last_video_width.store(width, Relaxed);
    shared.conn.last_video_height.store(height, Relaxed);

    // Convert PTS to nanoseconds against the time base the decoder was opened
    // with, which travels with it rather than being read off a format context
    // this thread does not own.
    let pts_ns = ffmpeg::rescale_q(pts, tb, ffmpeg::NS_TIME_BASE);

    // Frame interval EMA, for the frame rate the stats line reports and the
    // lead warning's frame budget. Measured rather than taken from
    // avg_frame_rate, which live SRT/RTMP demuxers routinely leave unset or
    // wrong. Out-of-range deltas (PTS repair, discontinuities, reordering) are
    // skipped rather than smoothed in.
    let delta = pts_ns - state.prev_pts_ns;
    let measurable = state.prev_pts_ns != 0;
    state.prev_pts_ns = pts_ns;

    if measurable {
        let prev = shared.conn.video_frame_interval_ns.load(Relaxed);
        shared
            .conn
            .video_frame_interval_ns
            .store(video_time::interval_ema(prev, delta), Relaxed);
    }

    let total = shared.conn.total_video_frames.fetch_add(1, Relaxed) + 1;
    if total == 1 {
        irl_info!("First video frame decoded");
    }

    // The pacing queue owns its entries, so it gets its own reference with the
    // nanosecond PTS written into it.
    let mut queued = frame.new_ref().ok()?;
    queued.set_pts(pts_ns);
    Some(queued)
}
