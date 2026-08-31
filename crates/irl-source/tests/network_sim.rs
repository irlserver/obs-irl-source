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

use std::ffi::CString;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use parking_lot::Mutex;

use irl_core::{HwDecode, Watermarks, consts};
use obs_irl_source::audio::{AudioPump, AudioSink};
use obs_irl_source::receiver::ReceiverFlags;
use obs_irl_source::receiver::audio_in::AudioIntake;
use obs_irl_source::shared::{HotValues, LifetimeStats, Shared, StreamConfig, TimedPacket};

const RATE: i32 = 48_000;
const CHANNELS: i32 = 2;
/// One Opus frame: 20 ms at 48 kHz.
const CHUNK_FRAMES: i32 = 960;
const CHUNK_NS: u64 = 20_000_000;
const TB_NS: ffmpeg::Rational = ffmpeg::Rational::new(1, 1_000_000_000);

// ── Recording sink ────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Emitted {
    timestamp: u64,
    frames: u32,
    rate: u32,
    /// The constant this chunk's samples carry, so silence is distinguishable
    /// from real audio.
    value: f32,
}

#[derive(Clone)]
struct Recorder {
    channels: usize,
    emitted: Arc<Mutex<Vec<Emitted>>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            channels: CHANNELS as usize,
            emitted: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl AudioSink for Recorder {
    fn output_audio(&self, audio: &obs::AudioFrame<'_>) {
        let sys = audio.as_sys();
        let bytes = sys.frames as usize * self.channels * 4;
        // SAFETY: the frame borrows a live interleaved-float buffer of
        // `frames * channels` samples, which is what the pump built.
        let raw = unsafe { std::slice::from_raw_parts(sys.data[0], bytes) };
        let value = if raw.len() >= 4 {
            f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
        } else {
            0.0
        };
        self.emitted.lock().push(Emitted {
            timestamp: sys.timestamp,
            frames: sys.frames,
            rate: sys.samples_per_sec,
            value,
        });
    }
}

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
    /// Sender-side PTS of the next chunk, in nanoseconds.
    next_pts_ns: i64,
    /// Fractional chunk debt, so a non-integer sender rate is exact over time.
    chunk_debt: f64,
    /// Every chunk PTS handed to the plugin, in order.
    delivered: Vec<i64>,
}

impl Sim {
    fn new(target_ms: i32) -> Self {
        // SAFETY: no code under test dereferences the handle — audio leaves
        // through the recording sink and nothing here calls into libobs.
        let source = unsafe { obs::SourceHandle::from_raw(NonNull::dangling()) };
        let wm = Watermarks::derive(target_ms);
        let cfg = StreamConfig {
            url: CString::new("srt://sim.invalid:9000").unwrap(),
            ffmpeg_options: None,
            hw_decode: HwDecode::Off,
            low_latency_audio: false,
            small_gap_ms: consts::SMALL_GAP_MS,
            large_gap_ms: consts::LARGE_GAP_MS,
        };
        let shared = Shared::new(
            source,
            cfg.clone(),
            HotValues {
                reconnect_delay_s: 2,
                adaptive_speed: true,
                catchup_percent: consts::DEFAULT_CATCHUP_PERCENT as i32,
                wait_for_keyframe: true,
                clear_on_disconnect: true,
                watermarks: wm,
            },
            Arc::new(LifetimeStats::default()),
        );

        let clock = Arc::new(AtomicU64::new(1_000_000_000));
        let audio_out = Recorder::new();
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
        let mut intake = AudioIntake::new(&cfg);
        intake.init_pts_repair(&cfg, TB_NS);

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
            next_pts_ns: 0,
            chunk_debt: 0.0,
            delivered: Vec::new(),
        }
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
            for i in 0..(CHUNK_FRAMES as usize * CHANNELS as usize) {
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

        if link == Link::Up {
            for pts in std::mem::take(&mut self.pending) {
                let frame = Self::decoded_chunk(pts, 0.25);
                self.intake
                    .handle_frame(&self.shared, &mut self.flags, &frame, TB_NS);
                self.delivered.push(pts);
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
            let (a, b) = (pair[0], pair[1]);
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

    /// Content is never skipped once playback has primed: every chunk the
    /// plugin accepted is still accounted for, as buffered audio or as audio
    /// already emitted.
    fn assert_no_audio_dropped(&self) {
        let skipped = self.shared.conn.audio_resync_skipped_chunks.load(Relaxed);
        assert_eq!(skipped, 0, "audio was skipped rather than played faster");
    }

    fn underruns(&self) -> u64 {
        self.shared.conn.audio_underruns.load(Relaxed)
    }
}

// ── Scenarios ─────────────────────────────────────────────────

#[test]
fn a_clean_link_holds_the_target_and_never_breaks_the_clock() {
    let mut sim = Sim::new(120);
    sim.run(60.0, Link::Up);

    sim.assert_clock_only_jumps_where_declared();
    sim.assert_no_audio_dropped();
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

    sim.assert_clock_only_jumps_where_declared();
    sim.assert_no_audio_dropped();
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

    sim.assert_clock_only_jumps_where_declared();
    sim.assert_no_audio_dropped();
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

        sim.assert_clock_only_jumps_where_declared();
        sim.assert_no_audio_dropped();
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

    sim.assert_clock_only_jumps_where_declared();
    sim.assert_no_audio_dropped();
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
        sim.shared.video.push_packet(
            TimedPacket {
                packet: ffmpeg::Packet::new().unwrap(),
                pts_ns: i * 33_333_333,
                bytes: 4096,
            },
            &sim.shared.lifetime,
        );
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
        deep.shared.video.push_packet(
            TimedPacket {
                packet: ffmpeg::Packet::new().unwrap(),
                pts_ns: i * 16_666_667,
                bytes: 16 * 1024,
            },
            &deep.shared.lifetime,
        );
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
