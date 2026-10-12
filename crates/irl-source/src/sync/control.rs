//! Timecode sync on the receiver thread: the delay line of held packets, the
//! controller in `irl_core::sync`, and the glue between them and the
//! pipeline (the NTP reference, the audio playout mapping, the priming gate,
//! the log).
//!
//! [`SyncStage`] is that glue, generic over what it holds so the A/V
//! simulation can drive it with stand-in packets; [`SyncControl`] is the
//! stage the receiver runs, over `ffmpeg::Packet`s.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use irl_core::Timecode;
use irl_core::consts;
use irl_core::sync::{
    DelayLine, Observation, PresentationMap, SyncController, SyncEvent, TcReason, VideoCodec,
    fold_present_bias,
};

use crate::shared::{AudioState, Shared};

/// The receiver's sync stage: reads the timecode and PTS off each packet
/// and holds `ffmpeg::Packet`s.
pub struct SyncControl {
    stage: SyncStage<ffmpeg::Packet>,
    /// The connection's video stream, for the SEI scan, the PTS rescale and
    /// the codec diagnosis. `-1` while no video stream is open.
    video_stream_idx: i32,
    video_tb: ffmpeg::Rational,
    video_codec: VideoCodec,
}

impl SyncControl {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            stage: SyncStage::new(shared, super::offset_generation()),
            video_stream_idx: -1,
            video_tb: ffmpeg::Rational::new(0, 1),
            video_codec: VideoCodec::None,
        }
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

    /// Drop the delay line and every measurement. On disconnect, on a new
    /// connection, and at the end of a run.
    pub fn reset(&mut self) {
        self.stage.reset(super::offset_generation());
    }

    pub fn is_full(&self) -> bool {
        self.stage.is_full()
    }

    pub fn next_release_ns(&self) -> Option<u64> {
        self.stage.next_release_ns()
    }

    pub fn pop_due(&mut self, now_ns: u64) -> Option<ffmpeg::Packet> {
        self.stage.pop_due(now_ns)
    }

    /// Fold a freshly read packet in and decide whether the delay line keeps
    /// it. `true` means the line took it and the caller must not dispatch it.
    pub fn intercept(&mut self, pkt: &ffmpeg::Packet) -> bool {
        let is_video = self.video_stream_idx >= 0 && pkt.stream_index() == self.video_stream_idx;
        let info = PacketInfo {
            timecode: is_video
                .then(|| irl_core::sei::find_timecode(pkt.data()))
                .flatten(),
            pts_ns: is_video
                .then(|| pkt.pts_or_dts())
                .flatten()
                .map(|pts| ffmpeg::rescale_q(pts, self.video_tb, ffmpeg::NS_TIME_BASE)),
            bytes: pkt.size().max(0) as usize,
        };
        let ctx = Context {
            now_ns: obs::time::gettime_ns(),
            utc_now_ns: super::ntp::utc_now_ns(),
            enabled: super::is_enabled(),
            offset_ms: super::offset_ms(),
            offset_generation: super::offset_generation(),
            video_codec: self.video_codec,
        };
        self.stage.intercept(&ctx, &info, || pkt.new_ref().ok())
    }
}

/// What the stage needs to know from outside the source for one packet.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// OBS clock.
    pub now_ns: u64,
    /// UTC per the NTP reference, or `None` while unsynced.
    pub utc_now_ns: Option<i64>,
    /// The master switch. The source's own opt-in is read from its `Shared`.
    pub enabled: bool,
    pub offset_ms: i32,
    pub offset_generation: u32,
    pub video_codec: VideoCodec,
}

/// What the stage reads off one packet.
#[derive(Debug, Clone, Copy)]
pub struct PacketInfo {
    /// The SEI timecode, on a video packet that carries one.
    pub timecode: Option<Timecode>,
    /// The PTS in nanoseconds, on a video packet that has one.
    pub pts_ns: Option<i64>,
    pub bytes: usize,
}

/// The delay line and its controller for one source, over packets of type
/// `P`. Receiver-thread-owned; the handful of values the audio thread also
/// touches live in [`crate::shared::SyncAudioLink`] and
/// [`crate::shared::SyncFlags`].
pub struct SyncStage<P> {
    shared: Arc<Shared>,
    ctl: SyncController,
    delay: DelayLine<P>,
    low_latency_said: bool,
}

impl<P> SyncStage<P> {
    /// A stage that has seen nothing. Publishes an Off snapshot, so the
    /// registry never shows a previous run's status.
    pub fn new(shared: Arc<Shared>, offset_generation: u32) -> Self {
        let mut this = Self {
            shared,
            ctl: SyncController::new(offset_generation),
            delay: DelayLine::new(consts::SYNC_DELAY_MAX_PACKETS, consts::SYNC_DELAY_MAX_BYTES),
            low_latency_said: false,
        };
        this.reset(offset_generation);
        this
    }

    /// Drop the delay line and every measurement, and say so to the
    /// registry.
    pub fn reset(&mut self, offset_generation: u32) {
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
        self.ctl = SyncController::new(offset_generation);
        super::publish(self.shared.source, &self.ctl.snapshot());
    }

    /// The delay line is at a ceiling: the read loop must stop taking packets
    /// and drain until there is room, so the excess is held by the transport
    /// rather than by this process.
    pub fn is_full(&self) -> bool {
        self.delay.is_full()
    }

    /// When the next held packet comes due, if any is held.
    pub fn next_release_ns(&self) -> Option<u64> {
        self.delay.next_release_ns()
    }

    /// What the source would publish right now.
    pub fn snapshot(&self) -> irl_core::SyncSnapshot {
        self.ctl.snapshot()
    }

    /// Release the head packet if its moment has come. The caller dispatches
    /// it exactly as it would a packet straight off the demuxer.
    pub fn pop_due(&mut self, now_ns: u64) -> Option<P> {
        let pkt = self.delay.pop_due(now_ns)?;
        // The line is flowing again, which is the condition the priming gate
        // is really waiting on.
        self.ctl.note_released();
        Some(pkt)
    }

    /// Measure the packet, move the hold, and decide whether the delay line
    /// keeps it. `true` means the line took it (through `take`, which gives
    /// the line its own reference) and the caller must not dispatch it.
    ///
    /// Measuring and holding are one call because the read loop has no use
    /// for them apart: measuring without holding would be a stats feature,
    /// and holding without measuring cannot know for how long.
    pub fn intercept(
        &mut self,
        ctx: &Context,
        info: &PacketInfo,
        take: impl FnOnce() -> Option<P>,
    ) -> bool {
        self.observe(ctx, info);
        self.hold(ctx.now_ns, info.bytes, take)
    }

    fn observe(&mut self, ctx: &Context, info: &PacketInfo) {
        let Self {
            shared,
            ctl,
            delay,
            low_latency_said,
        } = self;

        // Low Latency Audio has no speed controller to absorb a hold change
        // and no jitter buffer to anchor in, so every correction would be an
        // audible skip or gap. The two settings ask for opposite things.
        let low_latency = &shared.cfg.low_latency_audio;
        if *low_latency
            && ctx.enabled
            && shared.hot.sync_enabled.load(Relaxed)
            && !*low_latency_said
        {
            *low_latency_said = true;
            irl_warn!("Sync: not applied while Low Latency Audio is on; turn one of them off");
        }

        // The pre-roll the audio output handed back at prime, if any. The
        // flag is checked first so nearly every packet skips the lock.
        let anchor_defer_ns = shared
            .sync_flags
            .anchor_defer_pending
            .load(Relaxed)
            .then(|| {
                let defer = std::mem::take(&mut shared.audio_state().sync.anchor_defer_ns);
                shared.sync_flags.anchor_defer_pending.store(false, Relaxed);
                defer
            });

        let has_audio = shared.flags.audio_present.load(Relaxed);
        let target_ms = shared.hot.watermarks().target_ms;
        let mut presentation = || PresentationMap {
            audio: shared.audio_state().playout_mapping(),
            audio_excess_ns: shared.audio_buf().as_ref().map_or(0, |buf| {
                settling_excess_ms(buf.fill_ms() - target_ms) * 1_000_000
            }),
        };

        let out = ctl.observe(Observation {
            now_ns: ctx.now_ns,
            utc_now_ns: ctx.utc_now_ns,
            wanted: ctx.enabled && shared.hot.sync_enabled.load(Relaxed) && !*low_latency,
            offset_ms: ctx.offset_ms,
            offset_generation: ctx.offset_generation,
            // Without audio, the fallback shows video about as it is released.
            nominal_pipeline_ns: if has_audio {
                i64::from(target_ms + consts::AUDIO_OUT_LEAD_MS) * 1_000_000
            } else {
                0
            },
            has_audio,
            decoder_interval_ns: shared.conn.video_frame_interval_ns.load(Relaxed),
            video_codec: ctx.video_codec,
            timecode: info.timecode,
            pkt_pts_ns: info.pts_ns,
            presentation: &mut presentation,
            anchor_defer_ns,
        });

        if out.shift_delay_ns > 0 {
            delay.shift_later(out.shift_delay_ns);
        }
        if out.forget_bias {
            shared.audio_state().sync.present_bias_ns = None;
        } else if let Some(raw_ns) = out.bias_sample_ns {
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

    fn hold(&mut self, now_ns: u64, bytes: usize, take: impl FnOnce() -> Option<P>) -> bool {
        if !self.ctl.should_hold(self.delay.len()) {
            return false;
        }

        // Once anything is queued, everything must queue behind it or the
        // decoder would see packets out of order.
        let release_ns = now_ns + self.ctl.hold_ns() as u64;

        if !self.delay.is_full()
            && let Some(pkt) = take()
            && self.delay.push(pkt, bytes, release_ns)
        {
            return true;
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

/// One log line per transition.
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
        SyncEvent::Implausible { latency_ms } => irl_warn!(
            "Sync: timecode reads {latency_ms}ms from this machine's clock, which no network explains; ignoring it. Check that the sender's clock is on NTP"
        ),
        SyncEvent::PreRollTaken { defer_ms, hold_ms } => {
            irl_info!("Sync: delay line took over the {defer_ms}ms pre-roll; holding {hold_ms}ms")
        }
    }
}

/// How much of the jitter buffer's distance from its target the speed
/// controller will take out on its own: the part beyond its deadband. Inside
/// the deadband it does not act, so presentation stays where it is.
fn settling_excess_ms(excess_ms: i32) -> i64 {
    let beyond = (excess_ms.abs() - consts::AUDIO_SPEED_DEADBAND_MS).max(0);
    i64::from(beyond * excess_ms.signum())
}

// ── Audio-thread side ─────────────────────────────────────────

/// Where a fresh output clock should start.
///
/// Every restart of the clock line goes through this, not just the first one
/// at prime. A restart that anchors at "now" throws away the placement sync
/// made, and the loop can only put it back through the speed controller at
/// 20-50ms a second, so a single outage would cost the alignment on top of
/// the audio. Re-placing costs nothing extra, because the clock is being
/// moved either way.
///
/// Returns `now + chunk_ns`, what the plugin does without sync, whenever sync
/// has nothing to say: sync off, no timecodes, no NTP reference, or a
/// placement already in the past, which a clock cannot start behind. The
/// callers need no branch of their own.
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

/// True while the audio output must not prime yet.
pub fn prime_held(shared: &Shared) -> bool {
    shared.sync_flags.prime_hold.load(Relaxed)
}
