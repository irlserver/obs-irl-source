//! Video thread loop and pacing.
//!
//! The thread pops compressed packets off
//! [`VideoChannel`](crate::shared::VideoChannel), decodes each one as it
//! approaches its due time, copies the frame out of the hardware pool at once
//! (which returns the decoder's surface), then holds it in a thread-private
//! pacing queue until its mapped timestamp is due, the way OBS's own media
//! source paces in `mp_media_sleep`. Handing libobs a frame early makes libobs
//! hold it, and past `MAX_ASYNC_FRAMES` (30) held frames `cache_video` silently
//! discards the whole queue, so this queue keeps libobs's async queue about one
//! frame deep.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use ffmpeg::{AVPixelFormat, Frame, FramePool, Scaler};
use irl_core::consts;
use irl_core::pacing::{DueVerdict, PacingQueue};
use irl_core::video_delay::{DelayRaise, DelayRelax, VideoDelay};

use crate::shared::{Shared, TimedPacket, VideoDecoder, VideoMsg};
use crate::video::VideoSink;
use crate::video::decode;
use crate::video::intake::{self, DecodeState};

/// A frame waiting for its due time. `received_ns` is when the packet it was
/// decoded from reached this thread, which is what its arrival margin is
/// measured from. `base_ns` is its due time before the video delay: mapped
/// through the audio playout, or on the video-only fallback when there is no
/// mapping to re-derive it from.
pub struct Paced {
    frame: Frame,
    pts_ns: i64,
    received_ns: u64,
    base_ns: u64,
}

impl Paced {
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// OBS clock at which the packet behind this frame arrived.
    pub fn received_ns(&self) -> u64 {
        self.received_ns
    }

    /// The frame's undelayed due time minus its PTS: the offset the video
    /// delay is measured against.
    fn offset_ns(&self) -> i64 {
        self.base_ns as i64 - self.pts_ns
    }
}

/// Turns a queued packet into the frame it decodes to, in place of a decoder:
/// stamped in nanoseconds, it then goes through the same intake a decoded
/// H.264 frame does.
pub type DecodeStub = Box<dyn FnMut(&TimedPacket) -> Option<Frame> + Send>;

/// The video thread's wait for the audio playout mapping before it anchors
/// libobs's play head; see [`VideoThread::awaiting_audio_mapping`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnchorWait {
    /// Not waiting: no frame has needed the mapping yet, or it exists.
    Idle,
    /// Holding video since this OBS time.
    Waiting(u64),
    /// Audio never primed; the fallback anchors this connection.
    GaveUp,
}

/// Video-thread-owned state. Nothing here is locked: the receiver thread never
/// touches it, and the counters it mirrors into [`LifetimeStats`] once per
/// cycle are atomics.
///
/// [`LifetimeStats`]: crate::shared::LifetimeStats
pub struct VideoThread {
    pub(crate) shared: Arc<Shared>,
    /// The video decoder, handed over by the receiver when a connection opens.
    /// Owned here because *when* a packet is decoded is this thread's decision.
    decoder: Option<VideoDecoder>,
    /// Stands in for the decoder in tests; see [`Self::with_decode_stub`].
    decode_stub: Option<DecodeStub>,
    /// Reusable destination for `receive_frame`.
    scratch: Frame,
    /// Reusable output list for one packet's frames.
    decoded: Vec<Frame>,
    state: DecodeState,
    pacing: PacingQueue<Paced>,
    /// Last known stream-PTS → OBS-clock offset and when it was taken.
    pub(crate) playout_offset_ns: i64,
    pub(crate) playout_offset_time_ns: u64,
    /// Recycled destinations for `av_hwframe_transfer_data`. Safe to drop
    /// with pooled buffers still alive in the pacing queue: the pool lingers
    /// internally until its last buffer is returned.
    pub(crate) xfer_pool: Option<FramePool>,
    /// Latched when a backend rejects a caller-allocated destination.
    pub(crate) xfer_pool_broken: bool,
    /// Built on the first unmappable frame; owns the swscale context.
    pub(crate) scaler: Option<Scaler>,
    /// Source geometry the "Converting pixel format" line last reported.
    pub(crate) sws_src: Option<(i32, i32, AVPixelFormat)>,
    /// Persistent NV12 destination for the swscale path.
    pub(crate) nv12_scratch: Vec<u8>,
    /// Video-only fallback anchor (used until audio publishes a mapping).
    pub(crate) ts_init: bool,
    pub(crate) sys_base: u64,
    pub(crate) pts_base: i64,
    /// Set while libobs's async play head is unanchored, so the next frame out
    /// goes at its due time rather than a lead early. See
    /// [`Self::emit_slack_ns`] and [`Self::settle_anchor_candidate`].
    anchor_pending: bool,
    /// Where the wait for the audio playout mapping stands. See
    /// [`Self::awaiting_audio_mapping`].
    anchor_wait: AnchorWait,
    /// The floor under the video schedule, so that video which reaches this
    /// thread too late to be paced against the audio playout still can be.
    /// See [`Self::settle_anchor_candidate`].
    pub(crate) delay: VideoDelay,
    /// The offset the schedule runs at: the audio playout offset this cycle,
    /// or the newest frame's on the fallback. The delay it implies is what
    /// the stats report.
    offset_ns: Option<i64>,
    /// The floor moved this cycle, and its own log line said so.
    floor_moved: bool,
    /// Whether the last published delay was above zero, so the line that
    /// says the audio playout moved past the floor is edge-triggered.
    delay_shown: bool,
    /// The OBS clock, read for every timing decision this thread makes.
    /// Injectable so tests can step it; production reads `os_gettime_ns`.
    now_ns: Box<dyn Fn() -> u64 + Send>,
    /// The OBS canvas tick. Injectable so tests can drive pacing without a
    /// running libobs — `obs_get_frame_interval_ns` reads libobs's global
    /// video state and faults when `obs_startup` never ran.
    canvas_tick_ns: Box<dyn Fn() -> Option<u64> + Send>,
    pub(crate) sink: Box<dyn VideoSink>,
}

impl VideoThread {
    /// Production thread state: frames go to the source itself.
    pub fn new(shared: Arc<Shared>) -> Self {
        let sink = Box::new(shared.source);
        Self::with_sink(shared, sink)
    }

    /// Thread state with an explicit sink (tests, where no libobs is running).
    pub fn with_sink(shared: Arc<Shared>, sink: Box<dyn VideoSink>) -> Self {
        Self {
            shared,
            decoder: None,
            decode_stub: None,
            scratch: Frame::new().expect("frame allocation"),
            decoded: Vec::new(),
            state: DecodeState::default(),
            pacing: PacingQueue::new(
                consts::VIDEO_DECODE_LEAD_MS * 1_000_000,
                consts::VIDEO_PACING_MAX_FRAMES,
                consts::VIDEO_PACING_MAX_BYTES,
            ),
            playout_offset_ns: 0,
            playout_offset_time_ns: 0,
            xfer_pool: None,
            xfer_pool_broken: false,
            scaler: None,
            sws_src: None,
            nv12_scratch: Vec::new(),
            ts_init: false,
            sys_base: 0,
            pts_base: 0,
            // A fresh source has libobs's `last_frame_ts` at 0 too.
            anchor_pending: true,
            anchor_wait: AnchorWait::Idle,
            delay: VideoDelay::default(),
            offset_ns: None,
            floor_moved: false,
            delay_shown: false,
            now_ns: Box::new(obs::time::gettime_ns),
            canvas_tick_ns: Box::new(obs::time::canvas_frame_interval_ns),
            sink,
        }
    }

    pub fn run(&mut self) {
        while self.shared.is_active() {
            let wait = self.run_once();
            if wait.is_zero() {
                continue;
            }
            // Sleep until the next frame is due, or until the receiver pushes,
            // a clear arrives, or the thread is stopped. The predicate is
            // re-checked under the lock, so a push between the work above and
            // here is not slept through.
            self.shared.video.wait(
                wait,
                self.pacing.has_room(),
                &self.shared.flags.thread_active,
            );
        }

        self.finish();
    }

    /// One pass of the loop body. Returns how long to sleep before the next
    /// pass; zero means "go round again immediately".
    pub fn run_once(&mut self) -> Duration {
        let now_ns = self.now_ns();
        if self.shared.video.take_clear() {
            // The receiver already dropped `video_queue`; the paced frames
            // behind it must go too, or the blank would be repainted a lead
            // later.
            self.pacing.drain();
            // A cleared source is showing nothing; no reason to keep a lead's
            // worth of recycled buffers resident while it does. The next frame
            // rebuilds the pool.
            self.xfer_pool = None;
            // The offset belongs to the connection that just ended; the next
            // one brings its own PTS epoch.
            self.playout_offset_ns = 0;
            self.playout_offset_time_ns = 0;
            // `obs_source_output_video(NULL)` resets libobs's `last_frame_ts`
            // to 0, so the next frame re-anchors the play head.
            self.anchor_pending = true;
            self.anchor_wait = AnchorWait::Idle;
            // The floor was sized for the connection that just ended.
            self.reset_delay();
            self.publish_delay();
            self.sink.output_video_none();
            return Duration::ZERO;
        }

        // Sampled once for both the emit and the sleep, so a canvas frame
        // rate changed mid-cycle cannot make them disagree.
        let slack_ns = self.emit_slack_ns();

        self.decode_intake();
        // Before both the emit and the sleep below, so each cycle schedules
        // against the offset as it is now rather than as it was when the
        // frames were decoded.
        self.pacing_reschedule();
        if !self.anchor_pending {
            self.tend_floor(now_ns);
        }
        if self.anchor_pending && self.awaiting_audio_mapping(now_ns) {
            // Nothing goes out until audio has published where video belongs.
            // The audio pump wakes this thread the moment it does; the sleep
            // is only the backstop.
            self.publish_counters();
            self.publish_delay();
            return Duration::from_millis(consts::VIDEO_PACING_MAX_WAIT_MS);
        }
        self.pacing_emit_due(now_ns, slack_ns);
        self.publish_counters();
        self.publish_delay();
        // Fresh clock for the sleep: the emit above may have taken long
        // enough to make the next frame due already.
        self.sleep_hint(self.now_ns(), slack_ns)
    }

    /// Exit path: drop everything, including the decoder this thread owns.
    fn finish(&mut self) {
        self.pacing.drain();
        self.decoded.clear();
        self.xfer_pool = None;
        self.shared.video.drain();
        self.decoder = None;
    }

    /* ── Pacing ───────────────────────────────────────────── */

    /// Decode packets into the pacing queue until it holds its lead.
    ///
    /// The queue's soft bound is [`consts::VIDEO_DECODE_LEAD_MS`] of *media*,
    /// not the stream's latency, so only that much decoded video is resident
    /// however deep the Target Buffer is; everything behind it stays
    /// compressed in the channel. Each frame is copied out of the hardware pool
    /// at once, so a decoder surface is pinned only for one transfer.
    fn decode_intake(&mut self) {
        let shared = self.shared.clone();
        if shared.video_flags.timeline_reset.swap(false, Relaxed) {
            self.state.reset_timeline();
            // The floor and the liveness history carry the old PTS epoch.
            self.reset_delay();
        }
        // A decoder handover is taken even with no room: it produces nothing
        // by itself, and leaving it behind the queue would strand a reconnect
        // until the old connection's frames drained.
        while shared.video.next_is_decoder() {
            if let Some(VideoMsg::Decoder(decoder)) = shared.video.pop() {
                self.adopt_decoder(*decoder);
            }
        }
        while self.pacing.has_room() {
            let Some(msg) = shared.video.pop() else {
                return;
            };
            match msg {
                VideoMsg::Decoder(decoder) => self.adopt_decoder(*decoder),
                VideoMsg::Packet(packet) => {
                    let received_ns = packet.received_ns;
                    let mut produced = std::mem::take(&mut self.decoded);
                    if let Some(decoder) = self.decoder.as_mut() {
                        decode::decode_packet(
                            decoder,
                            &mut self.scratch,
                            &shared,
                            &mut self.state,
                            &packet.packet,
                            &mut produced,
                        );
                    } else if let Some(frame) =
                        self.decode_stub.as_mut().and_then(|stub| stub(&packet))
                    {
                        produced.extend(intake::handle_frame(
                            &shared,
                            &mut self.state,
                            &frame,
                            ffmpeg::NS_TIME_BASE,
                            ffmpeg::AVCodecID::AV_CODEC_ID_H264,
                        ));
                    }
                    for frame in produced.drain(..) {
                        self.pace_decoded(frame, received_ns);
                    }
                    self.decoded = produced;
                }
            }
        }
    }

    /// A new connection's decoder. Anything the old one produced is already
    /// paced or was cleared with the disconnect.
    fn adopt_decoder(&mut self, decoder: VideoDecoder) {
        self.decoder = Some(decoder);
        self.state.reset();
        self.reset_delay();
    }

    /// Copy one decoded frame out of the hardware pool and schedule it.
    /// `received_ns` is when the packet it came from reached this thread.
    ///
    /// Public as the seam the pacing tests use to put a frame in front of the
    /// loop without a decoder; production reaches it only through
    /// `decode_intake`.
    pub fn pace_decoded(&mut self, frame: Frame, received_ns: u64) {
        let sysmem = self.to_sysmem(&frame);
        let base_ns = match &sysmem {
            Some(f) => self.base_due(f),
            None => 0,
        };
        // Releases the decoder's surface before the next packet is sent.
        drop(frame);
        if let Some(f) = sysmem {
            let pts_ns = f.pts();
            self.delay.note_packet(received_ns, pts_ns);
            self.offset_ns = Some(base_ns as i64 - pts_ns);
            let due_ns = self.delay.due_ns(pts_ns, base_ns);
            let bytes = f.image_buffer_size().unwrap_or(0);
            let paced = Paced {
                frame: f,
                pts_ns,
                received_ns,
                base_ns,
            };
            self.pacing.push(paced, pts_ns, bytes, due_ns);
        }
    }

    /// Re-derive every queued frame's due time from the offset as it stands
    /// now and the floor as it stands now.
    ///
    /// The audio side reclaims playout latency two ways, and both would leave
    /// paced video behind for the depth of this queue. The speed controller
    /// moves the offset continuously, so a frame frozen at intake shows ~5% of
    /// its residence late for the whole drain; a re-anchor steps the offset
    /// outright. Rescheduling against one offset per cycle preserves the
    /// spacing between frames and moves the whole queue with the audio it is
    /// mapped to. Without a mapping each frame keeps the fallback due time it
    /// was given at intake, and only the floor is re-applied.
    fn pacing_reschedule(&mut self) {
        let offset_ns = self.playout_offset();
        if offset_ns.is_some() {
            self.offset_ns = offset_ns;
        }
        let delay = &self.delay;
        self.pacing.reschedule(|pts_ns, paced| {
            if let Some(offset_ns) = offset_ns {
                paced.base_ns = (pts_ns + offset_ns).max(0) as u64;
            }
            delay.due_ns(pts_ns, paced.base_ns)
        });
    }

    /// The floor's per-cycle work once the play head is anchored: a slew in
    /// progress steps on, and a window of late frames that ran its course is
    /// judged.
    fn tend_floor(&mut self, now_ns: u64) {
        let Some(offset_ns) = self.offset_ns else {
            return;
        };
        let mut moved = false;
        if let Some(step) = self.delay.ramp(now_ns, self.delay_ramp_rate(), offset_ns) {
            moved = true;
            if step.done {
                irl_info!("Video delay settled at {}ms", step.delay_ns / 1_000_000);
            }
        }
        if let Some(raise) = self.delay.expire(now_ns, offset_ns, self.canvas_tick_ns()) {
            moved = true;
            self.announce_delay_raise(raise, false);
        }
        if moved {
            self.floor_moved = true;
            self.pacing_reschedule();
        }
    }

    /// Emit every frame whose moment has arrived. Over the ceilings the head
    /// goes out early rather than being dropped: early video beats a hole in
    /// the picture.
    fn pacing_emit_due(&mut self, now_ns: u64, slack_ns: i64) {
        let tick_ns = self.canvas_tick_ns();
        if self.anchor_pending {
            self.settle_anchor_candidate(now_ns, tick_ns);
        }
        loop {
            // While the play head needs anchoring, a hard ceiling must not
            // force the head out early: anchoring from an early frame is the
            // offset this whole path exists to avoid.
            let Some(verdict) = self.pacing.due_now(now_ns, slack_ns, !self.anchor_pending) else {
                return;
            };
            if let DueVerdict::Wait(_) = verdict {
                return;
            }
            // `due_now` keeps the head in place, so its due time is still the
            // timestamp the frame was scheduled for.
            let due_ns = self.pacing.next_due().unwrap_or(0);
            let Some(paced) = self.pacing.pop() else {
                return;
            };
            if !self.anchor_pending {
                let offset_ns = paced.offset_ns();
                // Measured at hand-over: a frame that reaches libobs before its
                // due time is shown on time, whether or not it had the full
                // delivery lead (that lead is an oversleep allowance for frames
                // the queue holds, and this one may never have waited).
                if let Some(raise) = self.delay.note(now_ns, paced.pts_ns, offset_ns, tick_ns) {
                    self.floor_moved = true;
                    self.pacing_reschedule();
                    self.announce_delay_raise(raise, false);
                }
                // And at arrival, which is what says whether the floor is
                // higher than this stream needs.
                if let Some(relax) = self.delay.note_arrival(
                    now_ns,
                    paced.received_ns,
                    paced.pts_ns,
                    offset_ns,
                    tick_ns,
                ) {
                    self.announce_delay_relax(relax);
                }
            }
            let submitted = self.output_frame(paced.frame(), due_ns);
            // The play head is only anchored by a frame libobs actually
            // received, at its real due time. A conversion that failed
            // submitted nothing, and an early frame would anchor early, so
            // neither clears the flag.
            if self.anchor_pending && verdict == DueVerdict::Emit && submitted {
                self.anchor_pending = false;
            }
        }
    }

    /// Whether video must keep waiting for the audio playout mapping before
    /// it hands libobs the frame that anchors its play head.
    ///
    /// libobs anchors the play head to the *arrival* of the first frame after
    /// a start or a clear and only advances it by wall-clock deltas, so that
    /// frame's timing error is the connection's lip-sync error for good.
    /// Before audio primes the only schedule is the video-only fallback, which
    /// disagrees with the mapping audio will publish by ~100 ms, in a
    /// direction set by whether the audio warm-up or the first keyframe wins.
    ///
    /// So while an audio stream is present, video holds until the mapping
    /// exists and anchors from it. A stream whose audio never primes is let
    /// through on the fallback once the prime is overdue by
    /// [`consts::VIDEO_ANCHOR_WAIT_MARGIN_MS`].
    fn awaiting_audio_mapping(&mut self, now_ns: u64) -> bool {
        if !self.shared.flags.audio_present.load(Relaxed) {
            return false;
        }
        if self.mapping_published() {
            self.anchor_wait = AnchorWait::Idle;
            return false;
        }
        let since_ns = match self.anchor_wait {
            AnchorWait::GaveUp => return false,
            AnchorWait::Waiting(since_ns) => since_ns,
            AnchorWait::Idle => {
                self.anchor_wait = AnchorWait::Waiting(now_ns);
                now_ns
            }
        };
        // Outside low-latency mode the target carries the audio hold; in it,
        // the hold is queued on its own.
        let low_latency_hold_ms = if self.shared.cfg.low_latency_audio {
            self.shared.conn.audio_hold_ms.load(Relaxed)
        } else {
            0
        };
        let expected_ms = i64::from(consts::STARTUP_AUDIO_WARMUP_MS)
            + i64::from(self.shared.hot.watermarks().target_ms)
            + i64::from(low_latency_hold_ms)
            + i64::from(consts::AUDIO_OUT_LEAD_MS)
            + consts::AUDIO_HOLD_PRIME_WAIT_MS as i64
            + consts::VIDEO_ANCHOR_WAIT_MARGIN_MS;
        let waited_ms = (now_ns.saturating_sub(since_ns) / 1_000_000) as i64;
        if waited_ms < expected_ms {
            return true;
        }
        irl_warn!("Audio did not prime within {expected_ms}ms; anchoring video on its own clock");
        self.anchor_wait = AnchorWait::GaveUp;
        false
    }

    /// Settle the frame that will anchor libobs's play head.
    ///
    /// Two things are decided for each head frame, in this order.
    ///
    /// First its arrival margin. Nothing has been shown yet, so a frame whose
    /// packet reached this thread less than a canvas tick before it is due
    /// raises the video floor on the spot ([`VideoDelay::before_anchor`]) and
    /// the queue is rescheduled onto it. The allowance is a tick for the
    /// decode still to come, not the delivery lead: the lead covers the pacing
    /// timer oversleeping, and a frame handed over on arrival never sleeps.
    /// This keeps a sender whose video trails its audio by more than Target
    /// Buffer covers from playing unpaced, with libobs dropping a frame
    /// whenever two arrive inside one canvas tick.
    ///
    /// Only the newest frame in hand is measured (nothing queued behind it and
    /// the channel empty, so its arrival is a live one). The receiver pushes
    /// the probe backlog in one burst stamped with a single arrival time, so
    /// older frames look late by up to the probe span when they were merely
    /// buffered. An on-time head that is not the newest anchors unmeasured; a
    /// sender that really is late is caught by the hand-over measurement after
    /// the anchor.
    ///
    /// Second, with audio present, whether the head is stale. The anchoring
    /// frame must go out at its due time, so heads already past due are
    /// dropped until one is on time: they map to audio the warm-up discarded
    /// or already played, and anchoring on them would make the connection late
    /// by that much. The exception is a sender later than the delay ceiling
    /// covers, which has no on-time frame at all: once the delay sits at its
    /// ceiling the newest frame anchors however late it is, and the connection
    /// plays unpaced from there (#33).
    ///
    /// "Past due" is measured against a canvas tick, not the emit slack: libobs
    /// quantises display to its ticks anyway, and a coarse timer can oversleep
    /// by most of one, which must not make it drop every candidate in turn.
    /// Without audio there is nothing to be in sync with, and the fallback
    /// frames go out a tick after they arrive.
    fn settle_anchor_candidate(&mut self, now_ns: u64, tick_ns: u64) {
        let audio_present = self.shared.flags.audio_present.load(Relaxed);
        let mut dropped = 0u32;
        let mut raised: Option<DelayRaise> = None;
        while let (Some(due_ns), Some(head)) = (self.pacing.next_due(), self.pacing.head()) {
            let (received_ns, pts_ns, offset_ns) =
                (head.received_ns, head.pts_ns, head.offset_ns());
            let on_time = now_ns as i64 - due_ns as i64 <= tick_ns as i64;
            let newest_in_hand = self.pacing.len() == 1 && self.shared.video.is_empty();
            // With audio to be in sync with, only live video says anything
            // about the sender: video still catching up to live (a relay
            // replaying from its last keyframe next to live audio) is behind
            // its audio for now, not for good.
            let measurable = newest_in_hand && (!audio_present || self.delay.live(now_ns));
            let raise = if measurable {
                self.delay
                    .before_anchor(received_ns, pts_ns, offset_ns, tick_ns)
            } else {
                None
            };
            if let Some(raise) = raise {
                self.floor_moved = true;
                self.pacing_reschedule();
                // One line for the whole settlement, from the first delay to
                // the last.
                raised = Some(match raised {
                    Some(first) => DelayRaise {
                        from_ns: first.from_ns,
                        ..raise
                    },
                    None => raise,
                });
                continue;
            }
            if !audio_present || on_time {
                break;
            }
            // Late past the delay ceiling on the newest frame: no delay makes
            // this stream on time, so it anchors as it is rather than dropping
            // every frame while waiting for one that is (#33).
            if newest_in_hand && self.delay.at_ceiling(offset_ns) {
                break;
            }
            self.pacing.pop();
            dropped += 1;
        }
        if let Some(raise) = raised {
            self.announce_delay_raise(raise, true);
        }
        if dropped > 0 {
            irl_info!(
                "Dropped {dropped} stale video frame(s) before anchoring to the audio playout"
            );
        }
    }

    /// Say why the floor rose, with what the user can do about it. The queue
    /// has already been rescheduled onto it.
    fn announce_delay_raise(&self, raise: DelayRaise, at_anchor: bool) {
        let to_ms = raise.to_ns / 1_000_000;
        let by_ms = (raise.to_ns - raise.from_ns) / 1_000_000;
        let audio_present = self.shared.flags.audio_present.load(Relaxed);
        // Whether the audio hold can still move to cover this: it follows a
        // sender that keeps its video behind its audio, by playing slower
        // until the buffer holds the skew. A Sync Offset on top of that would
        // correct the same skew twice, so it is only suggested where the hold
        // cannot move.
        let hold_follows =
            !self.shared.cfg.low_latency_audio && self.shared.hot.adaptive_speed.load(Relaxed);
        let remedy = if hold_follows {
            format!(
                "Audio is held back to match if the sender keeps its video this far behind; if the video delay stays, raise Target Buffer by at least {to_ms}ms"
            )
        } else {
            format!(
                "If the sound runs ahead of the picture, set Sync Offset to +{to_ms}ms in Advanced Audio Properties, or raise Target Buffer by at least {to_ms}ms"
            )
        };
        match (at_anchor, audio_present) {
            (true, true) => irl_warn!(
                "Video reaches the plugin too late to be shown in time with its audio; delaying video by {to_ms}ms. {remedy}"
            ),
            (true, false) => {
                irl_info!("Video reaches the plugin as it falls due; delaying video by {to_ms}ms")
            }
            (false, true) => irl_warn!(
                "Video ran late on {} frames in the last {}ms; delaying video by {by_ms}ms more, {to_ms}ms in all. {remedy}",
                raise.frames,
                consts::VIDEO_DELAY_WINDOW_MS
            ),
            (false, false) => irl_info!(
                "Video ran late on {} frames in the last {}ms; delaying video by {by_ms}ms more, {to_ms}ms in all",
                raise.frames,
                consts::VIDEO_DELAY_WINDOW_MS
            ),
        }
        if raise.capped {
            irl_warn!(
                "Video is late by more than the {}ms delay ceiling; frames past it go out on arrival, unpaced",
                consts::VIDEO_DELAY_MAX_MS
            );
        }
    }

    /// A whole window of frames needed less than the floor: say so when that
    /// is on screen. The slew itself runs from [`Self::tend_floor`].
    fn announce_delay_relax(&mut self, relax: DelayRelax) {
        if relax.to_ns >= relax.from_ns {
            return;
        }
        self.floor_moved = true;
        irl_info!(
            "No frame in the last {}s needed more than {}ms of the {}ms video delay; playing video {:.0}% fast until it is back in step with its audio",
            consts::VIDEO_DELAY_RELAX_WINDOW_MS / 1000,
            relax.to_ns / 1_000_000,
            relax.from_ns / 1_000_000,
            self.delay_ramp_rate() * 100.0
        );
    }

    /// How fast a slew takes the delay back: the Catch-Up Speed, the same
    /// promise the audio side makes about a backlog. Video has no pitch, so
    /// the rate is far less noticeable here than it is there.
    fn delay_ramp_rate(&self) -> f64 {
        f64::from(self.shared.hot.max_speed() - 1.0).max(0.0)
    }

    /// Mirror the delay the schedule carries for the stats, and say so once
    /// when the audio playout moving (not the floor) takes it to or from
    /// zero: an audio hold that finished building, or a re-anchor that put
    /// the playout back below what video needs.
    fn publish_delay(&mut self) {
        let delay_ns = self
            .offset_ns
            .map_or(0, |offset_ns| self.delay.delay_ns(offset_ns));
        self.shared.conn.video_delay_ns.store(delay_ns, Relaxed);
        let floor_moved = std::mem::take(&mut self.floor_moved);
        let shown = delay_ns >= 1_000_000;
        if shown == self.delay_shown {
            return;
        }
        self.delay_shown = shown;
        if floor_moved {
            return;
        }
        if shown {
            irl_info!(
                "The audio playout moved earlier than video can be shown; video now runs {}ms behind its audio",
                delay_ns / 1_000_000
            );
        } else {
            irl_info!(
                "The audio playout now waits long enough for video; video back in step with its audio"
            );
        }
    }

    /// Forget the floor: a new connection, a cleared source or a broken
    /// timeline sizes its own.
    fn reset_delay(&mut self) {
        self.delay.reset();
        self.offset_ns = None;
        self.floor_moved = true;
    }

    pub(crate) fn now_ns(&self) -> u64 {
        (self.now_ns)()
    }

    /// The canvas tick, or the default when libobs has not reported one.
    fn canvas_tick_ns(&self) -> u64 {
        (self.canvas_tick_ns)().unwrap_or(consts::VIDEO_CANVAS_TICK_DEFAULT_NS)
    }

    /// How early a frame is handed to libobs once the play head is anchored;
    /// see [`consts::VIDEO_PACING_LEAD_TICKS`].
    fn delivery_lead_ns(&self, tick_ns: u64) -> u64 {
        (tick_ns * consts::VIDEO_PACING_LEAD_TICKS).min(consts::VIDEO_PACING_MAX_LEAD_NS)
    }

    /// How early a frame is handed to libobs: the emit slack plus the delivery
    /// lead.
    ///
    /// The frame keeps its due time as its timestamp, so this changes when
    /// libobs *receives* it, not when it is shown (see
    /// [`consts::VIDEO_PACING_LEAD_TICKS`]).
    ///
    /// The exception is the frame that re-anchors libobs's play head.
    /// `get_closest_frame()` shows the first frame after `last_frame_ts` hits
    /// zero the moment it arrives, whatever its timestamp, and anchors the play
    /// head to it. A frame handed over two ticks early would run the whole
    /// connection two ticks ahead of the audio (~33ms at a 60fps canvas), so
    /// that frame goes at its due time.
    fn emit_slack_ns(&self) -> i64 {
        if self.anchor_pending {
            return consts::VIDEO_PACING_SLACK_NS;
        }
        self.delivery_lead_ns(self.canvas_tick_ns()) as i64 + consts::VIDEO_PACING_SLACK_NS
    }

    /// Mirror the pacing counters for the stats line.
    fn publish_counters(&self) {
        let lifetime = &self.shared.lifetime;
        lifetime.pacing_now.store(self.pacing.len() as i32, Relaxed);
        lifetime
            .pacing_peak
            .store(self.pacing.peak() as i32, Relaxed);
        lifetime.pacing_bytes.store(self.pacing.bytes(), Relaxed);
    }

    /// How long to sleep: until the head frame is due, capped at
    /// `VIDEO_PACING_MAX_WAIT_MS`, never below 1 ms, and zero when the head is
    /// already due (go round again).
    fn sleep_hint(&self, now_ns: u64, slack_ns: i64) -> Duration {
        let mut wait_ms = consts::VIDEO_PACING_MAX_WAIT_MS;
        if let Some(due_ns) = self.pacing.next_due() {
            let until_ns = due_ns as i64 - now_ns as i64;
            if until_ns <= slack_ns {
                return Duration::ZERO; // due already; go round again
            }
            // Wake at the moment the head enters the delivery lead, not at its
            // due time: sleeping the whole way would hand it over late by
            // exactly the lead.
            let ms = ((until_ns - slack_ns) / 1_000_000) as u64;
            if ms < wait_ms {
                wait_ms = ms;
            }
        }
        Duration::from_millis(wait_ms.max(1))
    }

    /* ── Test seams ───────────────────────────────────────── */

    /// Replace the OBS clock (tests only: `os_gettime_ns` is libobs's
    /// monotonic clock and cannot be stepped from here).
    #[must_use]
    pub fn with_clock(mut self, now_ns: Box<dyn Fn() -> u64 + Send>) -> Self {
        self.now_ns = now_ns;
        self
    }

    /// Replace the canvas-tick source (tests only).
    #[must_use]
    pub fn with_canvas_tick(mut self, tick: Box<dyn Fn() -> Option<u64> + Send>) -> Self {
        self.canvas_tick_ns = tick;
        self
    }

    /// Decode packets with `stub` while no decoder is installed (tests only:
    /// the bundled FFmpeg carries no decoder a synthetic packet could feed).
    #[must_use]
    pub fn with_decode_stub(mut self, stub: DecodeStub) -> Self {
        self.decode_stub = Some(stub);
        self
    }

    /// The pacing queue, read-only (test seam).
    #[doc(hidden)]
    pub fn pacing(&self) -> &PacingQueue<Paced> {
        &self.pacing
    }
}
