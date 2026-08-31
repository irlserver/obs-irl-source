//! Video thread loop and pacing (port of `src/receiver-video.c:13-287`). W2-C.
//!
//! The thread pops decoded frames off [`VideoChannel`](crate::shared::VideoChannel),
//! copies them out of the hardware pool immediately (which returns the
//! decoder's surface), then holds them in a thread-private pacing queue until
//! their mapped timestamp is due — the way OBS's own media source paces in
//! `mp_media_sleep`. Handing libobs a frame early makes libobs hold it, and
//! past `MAX_ASYNC_FRAMES` (30) held frames `cache_video` silently discards the
//! whole queue, so this queue is what keeps libobs's async queue about one
//! frame deep.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use ffmpeg::{AVPixelFormat, Frame, FramePool, Scaler};
use irl_core::consts;
use irl_core::pacing::{DueVerdict, PacedFrame, PacingQueue};

use crate::shared::{Shared, VideoDecoder, VideoMsg};
use crate::video::VideoSink;
use crate::video::decode;
use crate::video::intake::DecodeState;

/// A frame waiting for its due time. `pts_ns` and `bytes` are cached at
/// intake: the pacing queue re-derives due times from the PTS every cycle, and
/// the byte total bounds the queue.
pub struct Paced {
    frame: Frame,
    pts_ns: i64,
    bytes: usize,
}

impl Paced {
    fn new(frame: Frame) -> Self {
        let pts_ns = frame.pts();
        // `av_image_get_buffer_size(fmt, w, h, 1)`, as `pacing_frame_bytes`.
        let bytes = frame.image_buffer_size().unwrap_or(0);
        Self {
            frame,
            pts_ns,
            bytes,
        }
    }

    /// The system-memory frame itself.
    pub fn frame(&self) -> &Frame {
        &self.frame
    }
}

impl PacedFrame for Paced {
    fn pts_ns(&self) -> i64 {
        self.pts_ns
    }

    fn bytes(&self) -> usize {
        self.bytes
    }
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
    /// Reusable destination for `receive_frame`.
    scratch: Frame,
    /// Reusable output list for one packet's frames.
    decoded: Vec<Frame>,
    /// Decode and intake state, owned outright by this thread.
    state: DecodeState,
    pacing: PacingQueue<Paced>,
    /// Last known stream-PTS → OBS-clock offset and when it was taken.
    pub(crate) playout_offset_ns: i64,
    pub(crate) playout_offset_time_ns: u64,
    /// Recycled destinations for `av_hwframe_transfer_data`.
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
    /// Throttle for the "Video lead" line.
    pub(crate) lead_warn_time_ns: u64,
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
            lead_warn_time_ns: 0,
            sink,
        }
    }

    /// The thread body (`irl_video_thread`).
    pub fn run(&mut self) {
        while self.shared.is_active() {
            let wait = self.run_once(obs::time::gettime_ns());
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

    /// One pass of the loop body, at OBS clock `now_ns`. Returns how long to
    /// sleep before the next pass; zero means "go round again immediately",
    /// which is the C's `continue`.
    pub fn run_once(&mut self, now_ns: u64) -> Duration {
        if self.shared.video.take_clear() {
            // The receiver already dropped `video_queue`; the paced frames
            // behind it must go too, or the blank would be repainted a lead
            // later.
            self.pacing.drain();
            // A cleared source is showing nothing; no reason to keep a lead's
            // worth of recycled buffers resident while it does. The next frame
            // rebuilds the pool.
            self.xfer_pool_release();
            // The offset belongs to the connection that just ended; the next
            // one brings its own PTS epoch.
            self.playout_offset_ns = 0;
            self.playout_offset_time_ns = 0;
            self.sink.output_video_none();
            return Duration::ZERO;
        }

        self.decode_intake();
        // Before both the emit and the sleep below, so each cycle schedules
        // against the offset as it is now rather than as it was when the
        // frames were decoded.
        self.pacing_reschedule();
        self.pacing_emit_due(now_ns);
        self.publish_counters();
        // Fresh clock for the sleep, as in the C: the emit above may have
        // taken long enough to make the next frame due already.
        self.sleep_hint(obs::time::gettime_ns())
    }

    /// Exit path: drop everything, including the decoder this thread owns.
    fn finish(&mut self) {
        self.pacing.drain();
        self.decoded.clear();
        self.xfer_pool_release();
        self.shared.video.drain();
        self.decoder = None;
    }

    /* ── Pacing ───────────────────────────────────────────── */

    /// Decode packets into the pacing queue until it holds its lead.
    ///
    /// This is the whole point of the design: the queue's soft bound is
    /// [`consts::VIDEO_DECODE_LEAD_MS`] of *media*, not the stream's latency,
    /// so only a quarter second of decoded frames is ever resident however deep
    /// the Target Buffer is. Everything behind that stays compressed in the
    /// channel. Each frame is copied out of the hardware pool immediately, so a
    /// decoder surface is pinned only for the length of one transfer.
    fn decode_intake(&mut self) {
        let shared = self.shared.clone();
        if shared.video_flags.timeline_reset.swap(false, Relaxed) {
            self.state.reset_timeline();
        }
        // A decoder handover is taken even with no room: it produces nothing
        // by itself, and leaving it behind the queue would strand a reconnect
        // until the old connection's frames drained.
        while shared.video.next_is_decoder() {
            if let Some(VideoMsg::Decoder(decoder)) = shared.video.pop() {
                self.decoder = Some(*decoder);
                self.state.reset();
            }
        }
        while self.pacing.has_room() {
            let Some(msg) = shared.video.pop() else {
                return;
            };
            match msg {
                VideoMsg::Decoder(decoder) => {
                    // A new connection. Anything the old decoder produced is
                    // already paced or was cleared with the disconnect.
                    self.decoder = Some(*decoder);
                    self.state.reset();
                }
                VideoMsg::Packet(packet) => {
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
                    }
                    for frame in produced.drain(..) {
                        self.pace_decoded(frame);
                    }
                    self.decoded = produced;
                }
            }
        }
    }

    /// Copy one decoded frame out of the hardware pool and schedule it.
    ///
    /// Public as the seam the pacing tests use to put a frame in front of the
    /// loop without a decoder; production reaches it only through
    /// [`Self::decode_intake`].
    pub fn pace_decoded(&mut self, frame: Frame) {
        let sysmem = self.to_sysmem(&frame);
        let due_ns = match &sysmem {
            Some(f) => self.due_time(f),
            None => 0,
        };
        // Releases the decoder's surface before the next packet is sent.
        drop(frame);
        if let Some(f) = sysmem {
            self.pacing.push(Paced::new(f), due_ns);
        }
    }

    /// `pacing_reschedule`: re-derive every queued frame's due time from the
    /// offset as it stands now.
    ///
    /// The audio side reclaims playout latency two ways, and both would leave
    /// paced video behind for the depth of this queue. The speed controller
    /// moves the offset continuously, so a frame frozen at intake shows ~5% of
    /// its residence late for the whole drain; a re-anchor steps the offset
    /// outright. Rescheduling against one offset per cycle preserves the
    /// spacing between frames and moves the whole queue with the audio it is
    /// mapped to.
    fn pacing_reschedule(&mut self) {
        if self.pacing.is_empty() {
            return;
        }
        let Some(offset_ns) = self.playout_offset() else {
            return;
        };
        self.pacing.reschedule(|pts_ns| {
            let due = pts_ns + offset_ns;
            if due > 0 { due as u64 } else { 0 }
        });
    }

    /// `pacing_emit_due`: emit every frame whose moment has arrived. Over the
    /// ceilings the head goes out early rather than being dropped — too-early
    /// video is what the un-paced path did all the time, and it beats a hole
    /// in the picture.
    fn pacing_emit_due(&mut self, now_ns: u64) {
        while let Some(verdict) = self.pacing.due_now(now_ns) {
            if let DueVerdict::Wait(_) = verdict {
                return;
            }
            // `due_now` keeps the head in place, so its due time is still the
            // timestamp the frame was scheduled for.
            let due_ns = self.pacing.next_due().unwrap_or(0);
            let Some(paced) = self.pacing.pop() else {
                return;
            };
            self.output_frame(paced.frame(), due_ns);
        }
    }

    /// Mirror the pacing counters for the stats line.
    fn publish_counters(&self) {
        let lifetime = &self.shared.lifetime;
        lifetime.pacing_now.store(self.pacing.len() as i32, Relaxed);
        lifetime
            .pacing_peak
            .store(self.pacing.peak() as i32, Relaxed);
        lifetime.pacing_bytes.store(self.pacing.bytes(), Relaxed);
        lifetime
            .pacing_overflows
            .store(self.pacing.overflows(), Relaxed);
    }

    /// How long to sleep: until the head frame is due, capped at
    /// `VIDEO_PACING_MAX_WAIT_MS`, never below 1 ms, and zero when the head is
    /// already due (go round again).
    fn sleep_hint(&self, now_ns: u64) -> Duration {
        let mut wait_ms = consts::VIDEO_PACING_MAX_WAIT_MS;
        if let Some(due_ns) = self.pacing.next_due() {
            let until_ns = due_ns as i64 - now_ns as i64;
            if until_ns <= consts::VIDEO_PACING_SLACK_NS {
                return Duration::ZERO; // due already; go round again
            }
            let ms = (until_ns / 1_000_000) as u64;
            if ms < wait_ms {
                wait_ms = ms;
            }
        }
        Duration::from_millis(wait_ms.max(1))
    }

    /* ── Test seams ───────────────────────────────────────── */

    /// Whether the pacing queue can take another frame (test seam; the thread
    /// passes this to [`crate::shared::VideoChannel::wait`]).
    pub fn pacing_has_room(&self) -> bool {
        self.pacing.has_room()
    }

    /// Frames currently paced.
    pub fn paced_len(&self) -> usize {
        self.pacing.len()
    }

    /// Due time of the head frame.
    pub fn next_due_ns(&self) -> Option<u64> {
        self.pacing.next_due()
    }

    /// Frames emitted early because a pacing ceiling bound.
    pub fn pacing_overflows(&self) -> u64 {
        self.pacing.overflows()
    }
}
