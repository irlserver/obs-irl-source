//! The per-source timecode sync controller as it sits on the receiver thread:
//! the delay line of encoded packets, the pure loop in `irl_core::sync`, and
//! the glue between them and the pipeline (the NTP reference, the audio
//! playout mapping, the priming gate, the log). Port of the C branch's
//! `sync-control.c` (Spillmaker/obs-irl-source `nal-timecodes`).
//!
//! Known limitation, inherited: the delay line drains on the receiver thread,
//! which is also the thread that blocks in `av_read_frame`. A total delivery
//! gap therefore stalls releases even though the line holds data that is due.
//! That is no worse than the plugin behaves without a delay line — the gap
//! costs the same concealment either way — but the buffering here does not
//! add stall resilience.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use irl_core::consts;
use irl_core::sync::{
    DelayLine, Observation, PresentationMap, SyncController, SyncEvent, TcReason, VideoCodec,
    fold_present_bias,
};

use crate::shared::{AudioState, Shared};

/// Everything timecode sync keeps on one source. Receiver-thread-owned; the
/// handful of values the audio thread also touches live in
/// [`crate::shared::SyncAudioLink`] and [`crate::shared::SyncFlags`].
pub struct SyncControl {
    shared: Arc<Shared>,
    ctl: SyncController,
    delay: DelayLine<ffmpeg::Packet>,
    /// The connection's video stream, for the SEI scan, the PTS rescale and
    /// the codec diagnosis. `-1` while no video stream is open.
    video_stream_idx: i32,
    video_tb: ffmpeg::Rational,
    video_codec: VideoCodec,
}

impl SyncControl {
    /// Fresh state for a run (the C `reset_runtime_state` half: publishes an
    /// Off snapshot so the registry never shows a previous run's status).
    pub fn new(shared: Arc<Shared>) -> Self {
        let mut this = Self {
            ctl: SyncController::new(super::offset_generation()),
            delay: DelayLine::new(consts::SYNC_DELAY_MAX_PACKETS, consts::SYNC_DELAY_MAX_BYTES),
            video_stream_idx: -1,
            video_tb: ffmpeg::Rational::new(0, 1),
            video_codec: VideoCodec::None,
            shared,
        };
        this.reset();
        this
    }

    /// The connection opened a video stream.
    pub fn set_video_stream(
        &mut self,
        index: i32,
        time_base: ffmpeg::Rational,
        codec_id: ffmpeg::AVCodecID,
    ) {
        self.video_stream_idx = index;
        self.video_tb = time_base;
        self.video_codec = if codec_id == ffmpeg::AVCodecID::AV_CODEC_ID_HEVC {
            VideoCodec::Hevc
        } else {
            VideoCodec::Other(ffmpeg::codec_name(codec_id))
        };
    }

    /// The connection closed.
    pub fn clear_video_stream(&mut self) {
        self.video_stream_idx = -1;
        self.video_codec = VideoCodec::None;
    }

    /// `irl_sync_reset`: drop the delay line and every measurement, and say
    /// so to the registry. On disconnect, on a new connection, and at the end
    /// of a run.
    pub fn reset(&mut self) {
        self.delay.clear();
        {
            let mut state = self.shared.audio_state();
            state.sync.present_bias_ns = None;
            state.sync.anchor_defer_ns = 0;
        }
        self.shared
            .sync_flags
            .anchor_defer_pending
            .store(false, Relaxed);
        self.shared.sync_flags.prime_hold.store(false, Relaxed);
        self.ctl = SyncController::new(super::offset_generation());
        super::publish(self.shared.source, &self.ctl.snapshot());
    }

    /// The delay line is at a ceiling: the read loop must stop reading and
    /// drain until there is room, so the excess is held by the transport
    /// rather than by this process.
    pub fn is_full(&self) -> bool {
        self.delay.is_full()
    }

    /// Fold a freshly read packet into the controller — extract its timecode
    /// if it has one, measure, move the hold — and then decide whether the
    /// delay line keeps it. `true` means the line took it and the caller must
    /// not dispatch it.
    ///
    /// The two halves are one call because the read loop has no use for them
    /// apart: measuring without holding would be a stats feature, and holding
    /// without measuring cannot know for how long.
    pub fn intercept(&mut self, pkt: &ffmpeg::Packet) -> bool {
        self.observe(pkt);
        self.hold(pkt)
    }

    /// Release the head packet if its moment has come. The caller dispatches
    /// it exactly as it would a packet straight off the demuxer.
    pub fn pop_due(&mut self, now_ns: u64) -> Option<ffmpeg::Packet> {
        let pkt = self.delay.pop_due(now_ns)?;
        // The line has started flowing again, which is the condition the
        // priming gate is really waiting on.
        self.ctl.note_released();
        Some(pkt)
    }

    fn observe(&mut self, pkt: &ffmpeg::Packet) {
        let Self {
            shared,
            ctl,
            delay,
            video_stream_idx,
            video_tb,
            video_codec,
        } = self;
        let now_ns = obs::time::gettime_ns();

        // The pre-roll the audio output handed back at prime, if any. The
        // flag is checked first so nearly every packet skips the lock.
        let anchor_defer_ns = shared
            .sync_flags
            .anchor_defer_pending
            .load(Relaxed)
            .then(|| {
                let defer = {
                    let mut state = shared.audio_state();
                    std::mem::take(&mut state.sync.anchor_defer_ns)
                };
                shared.sync_flags.anchor_defer_pending.store(false, Relaxed);
                defer
            });

        let is_video = *video_stream_idx >= 0 && pkt.stream_index() == *video_stream_idx;
        let timecode = if is_video {
            irl_core::sei::find_timecode(pkt.data())
        } else {
            None
        };
        let pkt_pts_ns = is_video
            .then(|| pkt.pts_or_dts())
            .flatten()
            .map(|pts| ffmpeg::rescale_q(pts, *video_tb, ffmpeg::NS_TIME_BASE));

        let wanted = super::is_enabled() && shared.hot.sync_enabled.load(Relaxed);

        let mut presentation = || PresentationMap {
            audio: shared.audio_state().playout_mapping(),
            has_audio: shared.flags.audio_present.load(Relaxed),
            video_fallback_offset_ns: shared
                .conn
                .video_ts_init
                .load(Relaxed)
                .then(|| shared.conn.video_fallback_offset_ns.load(Relaxed)),
        };

        let out = ctl.observe(Observation {
            now_ns,
            utc_now_ns: super::ntp::utc_now_ns(),
            wanted,
            offset_ms: super::offset_ms(),
            offset_generation: super::offset_generation(),
            buffer_target_ms: shared.hot.watermarks().target_ms,
            decoder_interval_ns: shared.conn.video_frame_interval_ns.load(Relaxed),
            video_codec: *video_codec,
            timecode,
            pkt_pts_ns,
            presentation: &mut presentation,
            anchor_defer_ns,
        });

        if out.shift_delay_ns > 0 {
            delay.shift_later(out.shift_delay_ns);
        }
        if let Some(raw_ns) = out.bias_sample_ns {
            let mut state = shared.audio_state();
            state.sync.present_bias_ns =
                Some(fold_present_bias(state.sync.present_bias_ns, raw_ns));
        }
        if out.prime_hold != shared.sync_flags.prime_hold.load(Relaxed) {
            shared.sync_flags.prime_hold.store(out.prime_hold, Relaxed);
        }
        for event in &out.events {
            log_event(event);
        }
        if let Some(snap) = out.publish {
            super::publish(shared.source, &snap);
        }
    }

    fn hold(&mut self, pkt: &ffmpeg::Packet) -> bool {
        if !self.ctl.should_hold(self.delay.len()) {
            return false;
        }

        // Once anything is queued, everything must queue behind it or the
        // decoder would see packets out of order.
        let release_ns = obs::time::gettime_ns() + self.ctl.hold_ns() as u64;

        if !self.delay.is_full()
            && let Ok(clone) = pkt.new_ref()
        {
            let bytes = clone.size().max(0) as usize;
            if self.delay.push(clone, bytes, release_ns) {
                return true;
            }
        }

        // A ceiling reached despite the read loop's backpressure, or an
        // allocation failure. Passing the packet straight through keeps
        // decode going at the cost of it arriving out of order once; dropping
        // it would cost reference frames until the next keyframe. Reported
        // rather than absorbed silently, because it means sync is no longer
        // holding what it claims to.
        if self.delay.note_overflow() == 0 {
            irl_warn!(
                "Sync delay line could not take a packet ({} queued, {} bytes); alignment will drift until it recovers",
                self.delay.len(),
                self.delay.bytes()
            );
        }
        false
    }
}

/// The C branch's log lines, one per transition.
fn log_event(event: &SyncEvent) {
    match *event {
        SyncEvent::TcReason { reason, codec } => match reason {
            TcReason::NoClock => irl_warn!(
                "Sync: no NTP reference on this machine yet; check the server in the IRL Sync dock"
            ),
            TcReason::Codec => irl_warn!(
                "Sync: stream is {}, but SEI timecodes only exist on H.265/HEVC; switch the sender's codec",
                codec.unwrap_or("not H.265")
            ),
            TcReason::Absent => irl_warn!(
                "Sync: H.265 stream carries no time_code SEI; enable Timecodes and set an NTP pool on the sender (Moblin: Settings > Streams > Video > Timecodes)"
            ),
            TcReason::Ok => irl_info!("Sync: timecodes detected"),
        },
        SyncEvent::FpsLearned(fps) => {
            irl_info!("Sync: sender is stamping {fps} frames per second");
        }
        SyncEvent::Engaged {
            fps,
            origin,
            latency_ms,
            offset_ms,
            hold_ms,
            bias_ms,
        } => irl_info!(
            "Sync engaged: {fps:.2}fps (from {origin}), feed latency {latency_ms}ms, offset {offset_ms}ms, holding {hold_ms}ms ({bias_ms}ms of that is seed bias)"
        ),
        SyncEvent::SeedMissed { error_ms } => irl_info!(
            "Sync seed missed by {error_ms}ms ({}); correcting",
            if error_ms >= 0 {
                "late, fast to fix"
            } else {
                "early, slow to fix"
            }
        ),
        SyncEvent::TooSlow {
            latency_ms,
            offset_ms,
            required_ms,
        } => irl_warn!(
            "Sync: feed arrives {latency_ms}ms late, past the {offset_ms}ms offset; raise the offset to at least {required_ms}ms"
        ),
        SyncEvent::BackInReach => irl_info!("Sync: feed back within the offset"),
        SyncEvent::PreRollTaken { defer_ms, hold_ms } => {
            irl_info!("Sync: delay line took over the {defer_ms}ms pre-roll; holding {hold_ms}ms")
        }
    }
}

// ── Audio-thread side ─────────────────────────────────────────

/// Where a fresh output clock should start (`audio_fresh_anchor_ns`).
///
/// Every restart of the clock line goes through this, not just the first one
/// at prime. A restart that anchors at "now" throws away the placement sync
/// made, and the loop can only put it back through the speed controller at
/// 20-50ms a second — so a single outage would cost the alignment on top of
/// the audio. Re-placing costs nothing extra, because the clock is being
/// moved either way.
///
/// Returns `now + chunk_ns` — what the plugin does without sync — whenever
/// sync has nothing to say: sync off, no timecodes, no NTP reference, or a
/// placement already in the past, which a clock cannot start behind. So a
/// source with sync off gets the old behaviour, and the callers need no
/// branch of their own.
///
/// The caller holds `audio_state` (it is the pump); the buffer peek nests
/// under it, which is the documented lock order.
pub fn fresh_anchor_ns(shared: &Shared, state: &mut AudioState, now: u64, chunk_ns: u64) -> u64 {
    let fallback_ns = now + chunk_ns;

    let Some(peek_ns) = shared
        .audio_buf()
        .as_ref()
        .and_then(|buf| buf.peek_state())
        .map(|peek| peek.oldest_pts_ns)
    else {
        return fallback_ns;
    };
    let Some((anchor_ns, wait_ns)) =
        irl_core::sync::playout_anchor(peek_ns, state.sync.present_bias_ns, now)
    else {
        return fallback_ns;
    };

    // Hand the wait back so the delay line can absorb it.
    state.sync.anchor_defer_ns = wait_ns;
    shared.sync_flags.anchor_defer_pending.store(true, Relaxed);

    // Reported here rather than folded into the caller's log line, so the
    // plugin's own messages read the same with sync on or off.
    irl_info!(
        "IRLSync placed the audio output clock {}ms ahead of now",
        (anchor_ns - now) / 1_000_000
    );
    anchor_ns
}

/// `irl_sync_prime_held`: true while the audio output must not prime yet.
pub fn prime_held(shared: &Shared) -> bool {
    shared.sync_flags.prime_hold.load(Relaxed)
}
