//! End-to-end simulation of the plugin under bad network conditions.
//!
//! Everything this plugin is *for* happens when the connection misbehaves, and
//! none of it was testable: a stall, a burst, a sender whose clock is not wall
//! clock. Those were validated by streaming for an hour and reading a stats
//! line. This drives the real jitter buffer, PTS repair, speed controller,
//! output clock, packet queue and pacing queue against a synthetic sender on a
//! virtual clock, and asserts the invariants the design actually promises.
//!
//! The seams were already there: `AudioSink`/`VideoSink` are traits, the pump
//! takes both of its clocks by injection, `irl-core` is pure, and `Shared` can
//! be built without libobs.
//!
//! **What it does not cover.** The demuxer, and the video decoder itself: the
//! bundled FFmpeg carries only the decoders the plugin needs (no rawvideo), so
//! there is no way to synthesize a packet a decoder here would accept. Video is
//! therefore driven at the two ends of the decoder — real packets into the
//! channel, and decoded frames injected through `VideoThread::pace_decoded` —
//! which is enough to pin the queue bounds, the decode lead and the
//! independence of video output from the receiver thread.

#![allow(dead_code)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use irl_core::consts;
use obs_irl_source::audio::AudioPump;
use obs_irl_source::audio::hold;
use obs_irl_source::config::Config;
use obs_irl_source::receiver::ReceiverFlags;
use obs_irl_source::receiver::audio_in::AudioIntake;
use obs_irl_source::shared::Shared;

use common::{CHANNELS, CHUNK_FRAMES, CHUNK_NS, RATE, Recorder};

// ── The simulated plugin ──────────────────────────────────────

/// What the network is doing to the stream this tick.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Link {
    /// Media arrives on schedule.
    Up,
    /// Nothing arrives, and the receiver is blocked in `av_read_frame`.
    Down,
}

struct Sim {
    shared: Arc<Shared>,
    clock: Arc<AtomicU64>,
    pump: AudioPump,
    audio_out: Recorder,
    intake: AudioIntake,
    flags: ReceiverFlags,

    /// The sender's media clock, in seconds of media per second of wall clock.
    sender_rate: f64,
    /// Media the sender has produced but not yet handed over (a stall's
    /// backlog).
    pending: Vec<i64>,
    /// Buffer fill sampled once per tick, so a test can look at the level the
    /// loop actually holds rather than one sample of a value that dithers
    /// between two grid points a chunk apart.
    fill_history: Vec<i32>,
    /// Playback speed sampled once per tick.
    speed_history: Vec<f32>,
    /// Deliver everything the sender has produced only every N ticks, instead
    /// of as it is produced. A remux hop (MediaMTX, an RTMP relay) hands its
    /// clients batches rather than a smooth stream, and the jitter buffer sees
    /// a sawtooth rather than a steady arrival.
    burst_ticks: u32,
    since_delivery: u32,
    /// Sender-side PTS of the next chunk, in nanoseconds.
    next_pts_ns: i64,
    /// Fractional chunk debt, so a non-integer sender rate is exact over time.
    chunk_debt: f64,
    /// Every chunk PTS handed to the plugin, in order.
    delivered: Vec<i64>,
    /// The sender also carries video, stamped this far behind its audio
    /// (`None`: no video stream). One video packet reaches the receiver per
    /// audio chunk, in mux order, as it does for real: the receiver's skew
    /// reading is driven from here, the decoder is not.
    video_skew_ns: Option<i64>,
}

impl Sim {
    fn new(target_ms: i32) -> Self {
        Self::with_mode(target_ms, false)
    }

    /// A sim with Low Latency Audio set as given.
    fn with_mode(target_ms: i32, low_latency_audio: bool) -> Self {
        let shared = common::shared(
            common::stream_config(low_latency_audio),
            common::hot_values(target_ms),
        );

        let clock = Arc::new(AtomicU64::new(1_000_000_000));
        let audio_out = Recorder::default();
        let pump = {
            let ns = Arc::clone(&clock);
            let us = Arc::clone(&clock);
            AudioPump::with_sink(Arc::clone(&shared), Box::new(audio_out.clone()))
                .with_clock(Box::new(move || ns.load(Relaxed)))
                .with_us_clock(Box::new(move || us.load(Relaxed) / 1000))
        };

        let flags = ReceiverFlags {
            has_audio_stream: true,
            ..Default::default()
        };

        // What the decode path does when the audio decoder opens; without it
        // the intake has no PTS-repair state and discards every frame.
        let mut intake = AudioIntake::default();
        intake.init_pts_repair(ffmpeg::NS_TIME_BASE);

        Self {
            intake,
            shared,
            clock,
            pump,
            audio_out,
            flags,
            sender_rate: 1.0,
            pending: Vec::new(),
            fill_history: Vec::new(),
            speed_history: Vec::new(),
            burst_ticks: 1,
            since_delivery: 0,
            next_pts_ns: 0,
            chunk_debt: 0.0,
            delivered: Vec::new(),
            video_skew_ns: None,
        }
    }

    /// Give the sender a video stream stamped `skew_ms` behind its audio.
    fn with_video_trailing_by(mut self, skew_ms: i64) -> Self {
        self.shared.flags.video_present.store(true, Relaxed);
        self.video_skew_ns = Some(skew_ms * 1_000_000);
        self
    }

    /// Give the sender a video stream that never delivers a packet.
    fn with_silent_video(self) -> Self {
        self.shared.flags.video_present.store(true, Relaxed);
        self
    }

    /// The sender starts sending its video `skew_ms` behind its audio.
    fn video_trails_by(&mut self, skew_ms: i64) {
        self.video_skew_ns = Some(skew_ms * 1_000_000);
    }

    /// The audio hold in force, in ms.
    fn hold_ms(&self) -> i32 {
        self.shared.conn.audio_hold_ms.load(Relaxed)
    }

    fn reanchors(&self) -> u64 {
        self.shared.lifetime.audio_offset_reanchors.load(Relaxed)
    }

    /// OBS time of the first chunk submitted, relative to the sim's start.
    fn first_audio_out_ms(&self) -> u64 {
        let first = self.audio_out.emitted.lock().first().map(|e| e.timestamp);
        (first.expect("audio primed") - 1_000_000_000) / 1_000_000
    }

    /// How early a video frame arriving now, stamped the sender's skew behind
    /// the newest audio, is in hand against its due time on the audio playout
    /// mapping: positive is on time. What the video thread measures for such
    /// a frame, and covers with a standing delay when it is negative.
    fn video_margin_now_ms(&self) -> i64 {
        let skew = self.video_skew_ns.expect("a sender with video");
        let pts = self.delivered.last().expect("audio delivered") - skew;
        let state = self.shared.audio_state();
        assert!(state.latest_obs_end_ts_ns != 0, "audio has not primed");
        let due = irl_core::video_time::map_through_playout(
            pts,
            state.latest_obs_end_ts_ns,
            state.latest_buffered_end_pts_ns,
        );
        (due as i64 - self.now_ns() as i64) / 1_000_000
    }

    fn now_ns(&self) -> u64 {
        self.clock.load(Relaxed)
    }

    fn fill_ms(&self) -> i32 {
        self.shared
            .audio_buf()
            .as_ref()
            .map_or(0, irl_core::AudioBuffer::fill_ms)
    }

    /// One decoded audio chunk of constant-valued PCM, as the decoder would
    /// hand it to the intake.
    fn decoded_chunk(pts_ns: i64, value: f32) -> ffmpeg::Frame {
        let mut frame = ffmpeg::Frame::new().unwrap();
        // SAFETY: setting the audio parameters before av_frame_get_buffer is
        // the documented allocation sequence; the buffer is then written
        // through its own data pointer for exactly nb_samples * channels
        // samples.
        unsafe {
            let raw = frame.as_mut_ptr();
            (*raw).format = ffmpeg::AVSampleFormat::AV_SAMPLE_FMT_FLT as core::ffi::c_int;
            (*raw).nb_samples = CHUNK_FRAMES;
            (*raw).sample_rate = RATE;
            ffmpeg::sys::av_channel_layout_default(&raw mut (*raw).ch_layout, CHANNELS);
            assert_eq!(ffmpeg::sys::av_frame_get_buffer(raw, 0), 0);
            (*raw).pts = pts_ns;
            // In the frame's time base, which here is nanoseconds — not a
            // sample count. PTS repair sizes its expected gap from this.
            (*raw).duration = CHUNK_NS as i64;

            let dst = (*raw).data[0];
            for i in 0..(CHUNK_FRAMES * CHANNELS) as usize {
                let bytes = value.to_le_bytes();
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(i * 4), 4);
            }
        }
        frame
    }

    /// Advance one chunk of wall clock. `link` says whether the sender's output
    /// reaches us; while it is `Down` the receiver is blocked, so nothing is
    /// ingested at all.
    fn tick(&mut self, link: Link) {
        // The sender produces at its own clock rate regardless of the link.
        self.chunk_debt += self.sender_rate;
        while self.chunk_debt >= 1.0 {
            self.chunk_debt -= 1.0;
            self.pending.push(self.next_pts_ns);
            self.next_pts_ns += CHUNK_NS as i64;
        }

        self.since_delivery += 1;
        if link == Link::Up && self.since_delivery >= self.burst_ticks {
            self.since_delivery = 0;
            for pts in std::mem::take(&mut self.pending) {
                let frame = Self::decoded_chunk(pts, 0.25);
                self.intake.handle_frame(
                    &self.shared,
                    &mut self.flags,
                    &frame,
                    ffmpeg::NS_TIME_BASE,
                );
                self.delivered.push(pts);
                if let Some(skew) = self.video_skew_ns {
                    hold::observe_video_packet(&self.shared, self.now_ns(), pts - skew);
                }
            }
        }

        // Sampled here, before the pump reads: this is the level the speed
        // controller sees and regulates. Sampling after the pump has taken
        // everything it wants measures the trough of the within-tick
        // oscillation instead, which is a whole chunk lower and says more
        // about where the sample was taken than about the cushion.
        self.fill_history.push(self.fill_ms());

        // The audio thread runs whatever the receiver is doing.
        while self.pump.pump_once() {}
        self.speed_history.push(self.shared.conn.current_speed());
        self.clock.fetch_add(CHUNK_NS, Relaxed);
    }

    fn run(&mut self, secs: f64, link: Link) {
        for _ in 0..((secs * 1_000_000_000.0) as u64 / CHUNK_NS) {
            self.tick(link);
        }
    }

    // ── Invariants ──

    /// Mean fill over the last `secs`, which is what the cushion actually is.
    /// The instantaneous level can only be one of two values a chunk apart, so
    /// a snapshot says less than the average does.
    fn mean_fill_ms(&self, secs: f64) -> f64 {
        let ticks = ((secs * 1_000_000_000.0) as u64 / CHUNK_NS) as usize;
        let tail = &self.fill_history[self.fill_history.len().saturating_sub(ticks)..];
        assert!(!tail.is_empty(), "no fill history");
        tail.iter().map(|&f| f as f64).sum::<f64>() / tail.len() as f64
    }

    /// Peak-to-trough playback speed over the last `secs`. What a listener
    /// hears as pitch modulation, and — because video due times are the audio
    /// playout offset — what a viewer sees as judder.
    fn speed_swing(&self, secs: f64) -> (f32, f32) {
        let ticks = ((secs * 1_000_000_000.0) as u64 / CHUNK_NS) as usize;
        let tail = &self.speed_history[self.speed_history.len().saturating_sub(ticks)..];
        let lo = tail.iter().copied().fold(f32::INFINITY, f32::min);
        let hi = tail.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        (lo, hi)
    }

    fn fill_swing(&self, secs: f64) -> (i32, i32) {
        let ticks = ((secs * 1_000_000_000.0) as u64 / CHUNK_NS) as usize;
        let tail = &self.fill_history[self.fill_history.len().saturating_sub(ticks)..];
        (
            tail.iter().copied().min().unwrap_or(0),
            tail.iter().copied().max().unwrap_or(0),
        )
    }

    /// The libobs contract: `ts[n+1] == ts[n] + frames/rate`, exactly. A gap
    /// under 70 ms is smoothed, 70 ms–2 s is zero-filled audibly, and over 2 s
    /// flushes everything OBS has queued for this source.
    ///
    /// The plugin is allowed to break it, but only where it says so: an output
    /// restart after the audio thread was starved, and the offset re-anchor
    /// that caps concealment-inflated latency. Both are counted, both cost one
    /// concealed splice, and both are the deliberate alternative to letting OBS
    /// add global buffering. So the invariant is not "never jumps" — it is
    /// **never jumps silently**.
    fn assert_clock_only_jumps_where_declared(&self) {
        let emitted = self.audio_out.emitted.lock();
        assert!(emitted.len() > 10, "nothing was emitted");

        let mut jumps = 0;
        for pair in emitted.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert_eq!(a.rate, RATE as u32, "the submitted rate must never change");
            let expected = a.timestamp + a.frames as u64 * 1_000_000_000 / a.rate as u64;
            if (b.timestamp as i64 - expected as i64).abs() > 1 {
                jumps += 1;
            }
        }

        let declared = self.shared.conn.audio_output_restarts.load(Relaxed)
            + self.shared.lifetime.audio_offset_reanchors.load(Relaxed);
        assert!(
            jumps <= declared,
            "{jumps} discontinuities against {declared} declared restarts"
        );
    }

    /// Content is never skipped once playback has primed. Backlog is bled off
    /// by playing faster instead, however deep it gets.
    ///
    /// The one sanctioned exception is the hidden trim, which runs only
    /// *before* priming — nothing has been audible yet, so startup backlog is
    /// free to drop. A batching upstream reaches the prime threshold in its
    /// first batch and hits that path, which is correct. So the invariant is
    /// not "nothing was skipped" but "everything skipped was inaudible".
    fn assert_no_audible_audio_dropped(&self) {
        let skipped = self.shared.conn.audible_skipped_chunks.load(Relaxed);
        assert_eq!(
            skipped, 0,
            "{skipped} audible chunks were skipped rather than played faster"
        );
    }

    /// The two invariants every scenario owes, whatever the link did.
    fn assert_healthy(&self) {
        self.assert_clock_only_jumps_where_declared();
        self.assert_no_audible_audio_dropped();
    }

    /// Video stamped the sender's skew behind the newest audio is in hand
    /// before it is due, by about the hold's margin: picture and sound agree.
    fn assert_in_lip_sync(&self) {
        let margin = self.video_margin_now_ms();
        assert!(
            (40..=200).contains(&margin),
            "video arriving now is {margin}ms early against its audio"
        );
    }

    fn underruns(&self) -> u64 {
        self.shared.conn.audio_underruns.load(Relaxed)
    }
}

// ── Scenarios ─────────────────────────────────────────────────

/// An upstream that hands over batches rather than a smooth stream — a remux
/// hop like MediaMTX, an RTMP relay — makes the buffer level sawtooth across
/// the whole speed ramp at the batch period.
///
/// A controller that regulates the instantaneous level chases that sawtooth and
/// modulates playback speed at the same period. Measured here before the level
/// was smoothed: 1.2% peak-to-peak at a 500ms batch and 3.3% at a 1s batch.
/// 1% is 17 cents, so that is plainly audible pitch wobble — and because video
/// due times are the frame PTS plus the audio playout offset, the same
/// modulation lands on video as judder that looks like the picture repeatedly
/// speeding up and catching up.
///
/// Regulating the smoothed level instead leaves the batch period alone while
/// still tracking anything that lasts: a stall's backlog persists for tens of
/// seconds, a batch for under one.
#[test]
fn a_batching_upstream_does_not_modulate_playback_speed() {
    // Batch period, and a target big enough to cover it — the configuration
    // that should simply work.
    for (batch_ticks, target_ms) in [(25u32, 500), (50, 500), (10, 120)] {
        let mut sim = Sim::new(target_ms);
        sim.burst_ticks = batch_ticks;
        sim.run(180.0, Link::Up);

        let (lo, hi) = sim.speed_swing(60.0);
        let swing = (hi - lo) * 100.0;
        assert!(
            swing < 0.5,
            "a {}ms batch at a {target_ms}ms target modulated playback {swing:.2}% (speed {lo:.4}..{hi:.4})",
            batch_ticks * 20,
        );
        sim.assert_healthy();
    }
}

/// The other half of that: a batch period longer than the whole cushion cannot
/// be smoothed away by any controller, because the buffer genuinely runs dry
/// between batches. It must still not be made *worse* than the arithmetic
/// requires — no skipped audio, no silent clock jumps. The fix for that case is
/// a Target Buffer above the batch period, not a cleverer loop.
#[test]
fn a_batch_longer_than_the_buffer_underruns_but_stays_honest() {
    let mut sim = Sim::new(120);
    sim.burst_ticks = 50; // 1s batches against a 120ms cushion
    sim.run(120.0, Link::Up);

    assert!(
        sim.underruns() > 0,
        "a 1s batch against a 120ms buffer has to underrun"
    );
    sim.assert_healthy();
}

#[test]
fn a_clean_link_holds_the_target_and_never_breaks_the_clock() {
    let mut sim = Sim::new(120);
    sim.run(60.0, Link::Up);

    sim.assert_healthy();
    assert_eq!(sim.underruns(), 0, "a clean link must not underrun");

    // The centred read alignment puts the two reachable levels half a chunk
    // either side of the target, so the *average* is the configured cushion
    // even though no single sample can be: measured, 110ms and 130ms with a
    // mean of 117ms, and stable for as long as the sim runs.
    let mean = sim.mean_fill_ms(30.0);
    assert!(
        (mean - 120.0).abs() <= 12.0,
        "held {mean:.1}ms on average against a 120ms target"
    );
}

#[test]
fn a_three_second_stall_conceals_then_recovers_without_skipping() {
    let mut sim = Sim::new(120);
    sim.run(20.0, Link::Up);
    let before = sim.audio_out.emitted.lock().len();

    // The link drops. The sender keeps producing; nothing reaches us, and the
    // receiver is blocked.
    sim.run(3.0, Link::Down);

    // Output must not stop: a source that goes quiet gets a tick of silence
    // plus a spliced restart from the OBS mixer, and one that falls behind the
    // mix window makes OBS add global buffering it never gives back.
    let during = sim.audio_out.emitted.lock().len();
    assert!(
        during > before,
        "the pump stopped emitting during the stall ({before} -> {during})"
    );
    assert!(sim.underruns() > 0, "the stall should have been concealed");

    // Everything the sender buffered lands at once, then the link is fine.
    sim.run(60.0, Link::Up);

    sim.assert_healthy();
    let fill = sim.fill_ms();
    assert!(
        fill < 120 + 60,
        "the backlog never drained: {fill}ms against a 120ms target"
    );
}

#[test]
fn repeated_short_dropouts_do_not_ratchet_latency() {
    let mut sim = Sim::new(200);
    sim.run(10.0, Link::Up);

    // A cell handoff every few seconds, twelve times over.
    for _ in 0..12 {
        sim.run(0.4, Link::Down);
        sim.run(4.0, Link::Up);
    }

    sim.assert_healthy();
    let fill = sim.fill_ms();
    assert!(
        fill < 200 + 100,
        "latency ratcheted up across dropouts: {fill}ms against a 200ms target"
    );
}

#[test]
fn a_sender_whose_clock_is_not_wall_clock_still_holds_the_target() {
    // The case the speed trim exists for: a sender 0.3% fast delivers 3ms of
    // extra audio every second, forever. A proportional-only loop parks
    // off-target and the latency parks with it.
    for rate in [1.003, 0.997] {
        let mut sim = Sim::new(120);
        sim.sender_rate = rate;
        sim.run(240.0, Link::Up);

        sim.assert_healthy();
        // Without the integral trim a proportional loop parks tens of
        // milliseconds off target here, permanently, and the latency parks
        // with it.
        let mean = sim.mean_fill_ms(60.0);
        assert!(
            (mean - 120.0).abs() <= 20.0,
            "sender at {rate}x parked the buffer at {mean:.1}ms"
        );
    }
}

#[test]
fn an_unwinnably_fast_sender_is_bounded_rather_than_unbounded() {
    // Faster than the catch-up ceiling can ever drain. Nothing fixes that
    // without skipping audio, which this design does not do — but the buffer
    // must stay bounded rather than growing without limit.
    let mut sim = Sim::new(120);
    sim.sender_rate = 1.20;
    sim.run(120.0, Link::Up);

    sim.assert_healthy();
    let fill = sim.fill_ms();
    assert!(
        fill <= consts::BLEED_PACE_FILL_MS * 4,
        "the buffer grew without bound: {fill}ms"
    );
}

// ── Video ─────────────────────────────────────────────────────

/// The property the packet-paced design turns on: video output does not depend
/// on the receiver thread running. The receiver spends a stall blocked in
/// `av_read_frame`, and that is exactly when video has to keep draining what it
/// already holds. If decode ever moves back onto the receiver, this stops.
#[test]
fn video_keeps_flowing_while_the_receiver_is_blocked() {
    let mut sim = Sim::new(120);

    // Queue a second of packets, as a healthy link would have.
    for i in 0..30 {
        common::push_packet(&sim.shared, i * 33_333_333, 4096);
    }
    assert_eq!(sim.shared.video.len(), 30);

    // The link drops: nothing is ingested and the receiver never runs again.
    sim.run(3.0, Link::Down);

    // The video thread is a separate thread with its own queue, so the packets
    // are still there to be decoded — the receiver being blocked has not
    // discarded or stalled them.
    assert_eq!(
        sim.shared.video.len(),
        30,
        "queued video was lost while the receiver was blocked"
    );
    assert!(sim.shared.video.span_ns() > 900_000_000);
}

/// Decoded-frame memory is bounded by the decode lead, not by Target Buffer.
/// This is what makes 4K at a deep buffer affordable: the latency is held as
/// compressed packets, and only a quarter second of it is ever decoded.
#[test]
fn decoded_memory_does_not_grow_with_the_target() {
    let deep = Sim::new(consts::BUFFER_TARGET_MAX_MS);

    // 8s of 1080p60 packets: what an 8s target actually holds.
    for i in 0..480 {
        common::push_packet(&deep.shared, i * 16_666_667, 16 * 1024);
    }

    // Compressed, that is single-digit megabytes. Decoded it would be ~1.5GB,
    // which is what the pacing queue used to be asked to hold.
    let queued = deep.shared.video.bytes();
    assert!(
        queued < 16 * 1024 * 1024,
        "{queued} bytes of packets for 8s of video"
    );
    assert!(
        deep.shared.video.span_ns() > 7_000_000_000,
        "the queue is not holding the configured latency"
    );
    assert_eq!(
        deep.shared.lifetime.video_queue_drops.load(Relaxed),
        0,
        "8s of 1080p60 must fit the packet queue without dropping"
    );
}

// ── Audio hold ────────────────────────────────────────────────

/// pocketSRT queues its audio about 300 ms ahead of its deadline, so the
/// audio of an instant reaches the plugin ~350 ms before the video of it,
/// against a default cushion of 120 ms plus the 80 ms output lead. Played as
/// it arrives, the sound ran that far ahead of the picture, and the only
/// remedies were a Target Buffer the user had to know to raise or a Sync
/// Offset by hand (#34). The hold measures the skew before audio starts and
/// starts audio late enough for the picture: nothing inserted, nothing
/// skipped, and video maps with an ordinary margin.
#[test]
fn audio_that_arrives_ahead_of_its_video_starts_late_enough_to_keep_lip_sync() {
    let mut sim = Sim::new(120).with_video_trailing_by(350);
    sim.run(40.0, Link::Up);

    // The uncovered part of the skew: 350 + 100 margin - 120 target - 80
    // lead, folded into the target the loop regulates.
    assert_eq!(sim.hold_ms(), 250);
    assert_eq!(sim.shared.hot.watermarks().target_ms, 370);

    let started_ms = sim.first_audio_out_ms();
    assert!(
        (550..1_000).contains(&started_ms),
        "audio started {started_ms}ms in against a 250ms hold"
    );
    sim.assert_healthy();
    assert_eq!(sim.underruns(), 0);
    let mean = sim.mean_fill_ms(20.0);
    assert!(
        (mean - 370.0).abs() <= 25.0,
        "held {mean:.1}ms on average against a 370ms effective target"
    );

    sim.assert_in_lip_sync();
}

/// The stabiliser stream from #33: video 1.65 s behind its audio.
#[test]
fn video_seconds_behind_its_audio_is_held_for_too() {
    let mut sim = Sim::new(120).with_video_trailing_by(1650);
    sim.run(40.0, Link::Up);

    assert_eq!(sim.hold_ms(), 1650 + 100 - 120 - 80);
    sim.assert_healthy();
    assert_eq!(sim.underruns(), 0);
    sim.assert_in_lip_sync();
}

/// A sender whose video is within what Target Buffer already covers needs no
/// hold, and starts as soon as it always did.
#[test]
fn a_sender_within_the_target_gets_no_hold() {
    let mut sim = Sim::new(120).with_video_trailing_by(50);
    sim.run(30.0, Link::Up);

    assert_eq!(sim.hold_ms(), 0);
    assert_eq!(sim.shared.hot.watermarks().target_ms, 120);
    let started_ms = sim.first_audio_out_ms();
    assert!(started_ms < 700, "audio started {started_ms}ms in");
    assert!(sim.video_margin_now_ms() > 0);
}

/// A connection that carries video but delivers none by the time audio could
/// prime waits a bounded time for a reading, then starts without one rather
/// than holding the sound forever.
#[test]
fn audio_starts_without_a_skew_reading_once_the_wait_runs_out() {
    let mut sim = Sim::new(120).with_silent_video();
    sim.run(5.0, Link::Up);

    assert_eq!(sim.hold_ms(), 0);
    let started_ms = sim.first_audio_out_ms();
    let wait_ms = consts::AUDIO_HOLD_PRIME_WAIT_MS;
    assert!(
        (wait_ms..wait_ms + 700).contains(&started_ms),
        "audio started {started_ms}ms in against a {wait_ms}ms wait"
    );
    sim.assert_healthy();
}

/// The sender starts sending its video 700 ms behind its audio halfway
/// through. Nothing can be done for the picture at once, so the video delay
/// covers it; after the raise window the hold rises to match, and the speed
/// controller builds it by playing slower until video is back on time. The
/// growth is the hold the user did not have to ask for, so it must not be
/// mistaken for concealment drift and thrown away by a re-anchor.
#[test]
fn video_that_falls_behind_mid_stream_is_caught_up_by_slowing_audio() {
    let mut sim = Sim::new(120).with_video_trailing_by(50);
    sim.run(20.0, Link::Up);
    assert_eq!(sim.hold_ms(), 0);

    sim.video_trails_by(700);
    sim.run(1.0, Link::Up);
    assert_eq!(sim.hold_ms(), 0, "a second is not a sustained skew");
    assert!(sim.video_margin_now_ms() < -300);

    sim.run(1.5, Link::Up);
    assert_eq!(sim.hold_ms(), 700 + 100 - 120 - 80);

    // Built at -2 %, 20 ms a second, easing off as the buffer nears its new
    // target: about a minute for 600 ms.
    sim.run(70.0, Link::Up);
    sim.assert_in_lip_sync();
    let built_ms = sim.shared.audio_state().hold_built_ns / 1_000_000;
    assert!(
        (590..=600).contains(&built_ms),
        "credited {built_ms}ms of a 600ms raise to the video thread"
    );
    assert_eq!(sim.reanchors(), 0, "the hold building was read as drift");
    sim.assert_healthy();
    assert_eq!(sim.underruns(), 0);
}

/// The regression #34 asked for: a sender whose video is steadily behind
/// its audio, with a short burst of later video every few seconds. The hold
/// covers the steady skew; the bursts are the video delay's to cover and do
/// not ratchet the latency up.
#[test]
fn recurring_bursts_of_late_video_do_not_ratchet_the_hold() {
    let mut sim = Sim::new(120).with_video_trailing_by(350);
    sim.run(10.0, Link::Up);
    assert_eq!(sim.hold_ms(), 250);

    for _ in 0..15 {
        sim.video_trails_by(650);
        sim.run(0.5, Link::Up);
        sim.video_trails_by(350);
        sim.run(3.5, Link::Up);
    }

    assert_eq!(sim.hold_ms(), 250);
    assert!(sim.video_margin_now_ms() > 0);
    sim.assert_healthy();
    assert_eq!(sim.underruns(), 0);
}

/// A sender that recovers gives the latency back: once a whole release
/// window of video needed less, the hold drops to what it needed and the
/// speed controller drains the surplus without skipping anything.
#[test]
fn the_hold_is_released_when_the_sender_recovers() {
    let mut sim = Sim::new(120).with_video_trailing_by(700);
    sim.run(20.0, Link::Up);
    assert_eq!(sim.hold_ms(), 600);

    sim.video_trails_by(50);
    sim.run(9.0, Link::Up);
    assert_eq!(sim.hold_ms(), 600, "released before a whole window");
    sim.run(2.0, Link::Up);
    assert_eq!(sim.hold_ms(), 0);
    assert_eq!(sim.shared.hot.watermarks().target_ms, 120);

    sim.run(60.0, Link::Up);
    let mean = sim.mean_fill_ms(10.0);
    assert!(
        (mean - 120.0).abs() <= 20.0,
        "held {mean:.1}ms on average after the release"
    );
    assert!(sim.video_margin_now_ms() > 0);
    sim.assert_healthy();
}

/// Low-latency mode keeps no cushion and has no speed control to build one
/// with, so the hold is sized once, before audio starts, and kept queued.
#[test]
fn low_latency_audio_holds_at_connection_start_only() {
    let mut sim = Sim::with_mode(120, true).with_video_trailing_by(350);
    sim.run(20.0, Link::Up);

    // The lead is a single 20 ms chunk, and there is no target.
    let hold = sim.hold_ms();
    assert_eq!(hold, 350 + 100 - 20);
    assert_eq!(sim.shared.hot.watermarks().target_ms, 120, "nothing folded");
    sim.assert_in_lip_sync();

    // A later skew is the video delay's, as before.
    sim.video_trails_by(900);
    sim.run(20.0, Link::Up);
    assert_eq!(sim.hold_ms(), hold);
}

/// Target Buffer is the user's, and the hold rides on top of it. An edit
/// mid-stream keeps the hold in force, and a deeper Target Buffer covers more
/// of the skew, so the readings give that much of the hold straight back:
/// what the stream needs does not change because the user asked for it.
#[test]
fn a_target_buffer_edit_composes_with_the_hold() {
    let mut sim = Sim::new(120).with_video_trailing_by(350);
    sim.run(12.0, Link::Up);
    assert_eq!(sim.hold_ms(), 250);

    let stream = sim.shared.cfg.clone();
    let edit = |target_ms| Config {
        stream: stream.clone(),
        hot: common::hot_values(target_ms),
        close_when_inactive: false,
    };

    let effective = edit(300).apply_hot(&sim.shared);
    assert_eq!(effective.target_ms, 300, "the user's own target comes back");
    assert_eq!(sim.shared.hot.watermarks().target_ms, 550);
    sim.run(0.1, Link::Up);
    assert_eq!(sim.hold_ms(), 350 + 100 - 300 - 80);
    assert_eq!(sim.shared.hot.watermarks().target_ms, 370);

    // A Target Buffer at the ceiling leaves no room for any hold.
    let effective = edit(consts::BUFFER_TARGET_MAX_MS).apply_hot(&sim.shared);
    assert_eq!(effective.target_ms, consts::BUFFER_TARGET_MAX_MS);
    assert_eq!(sim.hold_ms(), 0);
    assert_eq!(
        sim.shared.hot.watermarks().target_ms,
        consts::BUFFER_TARGET_MAX_MS
    );
    sim.assert_healthy();
}

/// A new connection may be a different sender, or the same one with its
/// stabiliser switched off: it measures its own hold, starting from Target
/// Buffer as the user set it.
#[test]
fn a_new_connection_starts_without_the_last_ones_hold() {
    let mut sim = Sim::new(120).with_video_trailing_by(350);
    sim.run(5.0, Link::Up);
    assert_eq!(sim.hold_ms(), 250);

    {
        let mut state = sim.shared.audio_state();
        hold::reset_connection(&sim.shared, &mut state);
    }
    assert_eq!(sim.hold_ms(), 0);
    assert_eq!(sim.shared.hot.watermarks().target_ms, 120);
}
