//! End-to-end A/V sync simulation: the receiver's audio intake and video
//! arrival bookkeeping, the audio pump and the video thread, all driven on one
//! virtual clock against a synthetic sender.
//!
//! What it measures that nothing else in the suite does is the offset between
//! picture and sound as libobs is handed them. Both ends carry their own
//! truth, so neither is read through the playout mapping the video thread
//! schedules by:
//!
//! - every audio sample carries its own index on the sender's timeline
//!   ([`encode_sample`]), so the OBS time at which content PTS `p` plays is
//!   read off the submitted PCM, through speed changes, concealment and
//!   re-anchors;
//! - every video frame carries its PTS in its first luma bytes, so the frame
//!   libobs received is known whatever timestamp it was given.
//!
//! The offset of a frame is when libobs shows it (its timestamp, or its
//! hand-over if that was later) minus when the audio of the same instant
//! plays. Positive is picture behind sound.
//!
//! **What it does not cover.** The demuxer and the decoder, as in
//! `network_sim.rs`. Video packets go through the real channel and the video
//! thread's real intake loop, and a stub turns each packet into its frame.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use parking_lot::Mutex;

use irl_core::consts;
use irl_core::sync::{SyncStatus, VideoCodec};
use irl_core::{SyncSnapshot, Timecode};
use obs_irl_source::audio::{AudioPump, AudioSink};
use obs_irl_source::config::Config;
use obs_irl_source::receiver::ReceiverFlags;
use obs_irl_source::receiver::audio_in::AudioIntake;
use obs_irl_source::receiver::decode::note_video_arrival;
use obs_irl_source::shared::{Shared, TimedPacket};
use obs_irl_source::source::av_skew_ms;
use obs_irl_source::sync::control::{Context, PacketInfo, SyncStage};
use obs_irl_source::video::VideoSink;
use obs_irl_source::video::thread::VideoThread;

use common::{CHANNELS, RATE};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

/// OBS clock when the connection opens. Far enough from zero for a relay's
/// backlog, captured before it, to have a capture time.
const START_NS: u64 = 10 * SEC;
/// The sender's first audio PTS. Not zero: the playout mapping treats a zero
/// PTS as "no PTS".
const FIRST_PTS_NS: i64 = 10_000_000_000;
/// An AAC frame.
const AUDIO_FRAMES: i64 = 1024;
/// Network latency both streams share, unless a scenario sets its own; it
/// moves nothing relative to the other, but keeps arrival after capture.
const NET_NS: u64 = 20 * MS;
/// The sender's frame count at `START_NS` on the UTC timeline its timecodes
/// carry: a hundred days in, so the timecode wraps neither hour nor day near
/// the run, and a whole number of seconds at every frame rate used here.
const UTC_FRAMES_AT_START: i64 = 100 * 86_400 * 60;

/// Lip sync people do not notice: ITU-R BT.1359 puts the detectability
/// threshold at 45 ms of sound ahead of picture, which is the direction every
/// error here runs in. The bar for "in sync" wherever the design promises it.
const SYNC_TOLERANCE_NS: i64 = 45 * MS as i64;

/// Exact PTS of sample `n` on the sender's timeline.
fn sample_pts_ns(n: i64) -> i64 {
    n * 1_000_000_000 / i64::from(RATE)
}

fn secs(ns: u64) -> f64 {
    ns as f64 / SEC as f64
}

/// A video lag that moves linearly between `(seconds, ms)` points and holds
/// its ends.
fn lag_profile(points: &'static [(f64, f64)]) -> impl Fn(u64) -> u64 {
    move |capture_ns| {
        let t = secs(capture_ns);
        let ms = match points.iter().position(|&(at, _)| at > t) {
            Some(0) => points[0].1,
            Some(i) => {
                let ((t0, a), (t1, b)) = (points[i - 1], points[i]);
                a + (b - a) * (t - t0) / (t1 - t0)
            }
            None => points.last().map_or(0.0, |p| p.1),
        };
        (ms * MS as f64) as u64
    }
}

// ── The sender ────────────────────────────────────────────────

/// Which streams an outage touches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Streams {
    Both,
    Video,
}

/// An outage of the link. A stall holds everything that would have arrived
/// inside the window and hands it over at the end, as a burst; a loss drops
/// it.
#[derive(Clone, Copy, Debug)]
struct Stall {
    at_ns: u64,
    len_ns: u64,
    streams: Streams,
    lost: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Media {
    /// One decoded audio frame starting at sample index `n`.
    Audio(i64),
    /// One video packet stamped `pts_ns`.
    Video(i64),
}

#[derive(Clone, Copy, Debug)]
struct Arrival {
    at_ns: u64,
    capture_ns: u64,
    media: Media,
}

/// A change made to the running source.
enum Event {
    /// The user edits Target Buffer.
    TargetBuffer(i32),
}

/// One scenario: what the sender does, and the source's settings.
struct Scenario {
    secs: u64,
    target_ms: i32,
    low_latency: bool,
    audio: bool,
    fps: u64,
    canvas_fps: u64,
    /// How far behind its audio the sender's video reaches the plugin, by
    /// capture time since the connection opened.
    video_lag: Box<dyn Fn(u64) -> u64>,
    stalls: Vec<Stall>,
    /// A relay replaying video from its last keyframe: how far behind live
    /// the replay starts, and how many times faster than real time it runs.
    relay_backlog: Option<(u64, u64)>,
    events: Vec<(u64, Event)>,
    net_ns: u64,
    /// Timecode sync on, at this offset.
    sync_offset_ms: Option<i32>,
    /// Captured between these times after the open, frames carry no
    /// timecode.
    timecode_gap: Option<(u64, u64)>,
}

impl Scenario {
    /// A clean link at the default Target Buffer: 30 fps on a 60 fps canvas.
    fn new(secs: u64) -> Self {
        Self {
            secs,
            target_ms: consts::DEFAULT_BUFFER_TARGET_MS as i32,
            low_latency: false,
            audio: true,
            fps: 30,
            canvas_fps: 60,
            video_lag: Box::new(|_| 0),
            stalls: Vec::new(),
            relay_backlog: None,
            events: Vec::new(),
            net_ns: NET_NS,
            sync_offset_ms: None,
            timecode_gap: None,
        }
    }

    fn network_latency(mut self, ms: u64) -> Self {
        self.net_ns = ms * MS;
        self
    }

    /// The sender stamps SEI timecodes and the source is synced at
    /// `offset_ms`, against an NTP reference that agrees with the sender's.
    fn timecode_sync(mut self, offset_ms: i32) -> Self {
        self.sync_offset_ms = Some(offset_ms);
        self
    }

    /// The sender stops stamping timecodes for a while.
    fn timecode_gap(mut self, at_s: f64, len_ms: u64) -> Self {
        let at = (at_s * SEC as f64) as u64;
        self.timecode_gap = Some((at, at + len_ms * MS));
        self
    }

    fn fps(mut self, fps: u64) -> Self {
        self.fps = fps;
        self
    }

    fn low_latency(mut self) -> Self {
        self.low_latency = true;
        self
    }

    fn video_only(mut self) -> Self {
        self.audio = false;
        self
    }

    fn video_lag(mut self, lag: impl Fn(u64) -> u64 + 'static) -> Self {
        self.video_lag = Box::new(lag);
        self
    }

    fn stall(mut self, at_s: f64, len_ms: u64, streams: Streams) -> Self {
        self.stalls.push(Stall {
            at_ns: (at_s * SEC as f64) as u64,
            len_ns: len_ms * MS,
            streams,
            lost: false,
        });
        self
    }

    fn loss(mut self, at_s: f64, len_ms: u64) -> Self {
        self.stalls.push(Stall {
            at_ns: (at_s * SEC as f64) as u64,
            len_ns: len_ms * MS,
            streams: Streams::Both,
            lost: true,
        });
        self
    }

    fn relay_backlog(mut self, backlog_ms: u64, speedup: u64) -> Self {
        self.relay_backlog = Some((backlog_ms * MS, speedup));
        self
    }

    fn at(mut self, at_s: f64, event: Event) -> Self {
        self.events.push(((at_s * SEC as f64) as u64, event));
        self
    }

    fn frame_ns(&self) -> u64 {
        SEC / self.fps
    }

    fn canvas_tick_ns(&self) -> u64 {
        SEC / self.canvas_fps
    }

    /// When a packet that would reach the plugin at `at_ns` actually does
    /// once the outages have had it, or `None` if one dropped it.
    fn through_stalls(&self, at_ns: u64, video: bool) -> Option<u64> {
        let mut at = at_ns;
        for stall in &self.stalls {
            let applies = video || stall.streams == Streams::Both;
            let start = START_NS + stall.at_ns;
            if applies && (start..start + stall.len_ns).contains(&at) {
                if stall.lost {
                    return None;
                }
                at = start + stall.len_ns;
            }
        }
        Some(at)
    }

    /// Every packet the sender's link delivers, in arrival order: mux order,
    /// so a burst lands in capture order.
    fn arrivals(&self) -> Vec<Arrival> {
        let end_ns = self.secs * SEC;
        let mut out = Vec::new();

        if self.audio {
            let first = FIRST_PTS_NS * i64::from(RATE) / 1_000_000_000;
            let mut last_at = 0;
            for n in (first..).step_by(AUDIO_FRAMES as usize) {
                let capture = (sample_pts_ns(n) - FIRST_PTS_NS) as u64;
                if capture >= end_ns {
                    break;
                }
                let Some(at) = self.through_stalls(START_NS + capture + self.net_ns, false) else {
                    continue;
                };
                last_at = at.max(last_at);
                out.push(Arrival {
                    at_ns: last_at,
                    capture_ns: START_NS + capture,
                    media: Media::Audio(n),
                });
            }
        }

        let frame = self.frame_ns();
        let backlog = self.relay_backlog.map_or(0, |(ns, _)| ns.div_ceil(frame));
        let mut last_at = 0;
        for k in 0.. {
            let j = k as i64 - backlog as i64;
            let offset = j * frame as i64;
            if offset >= end_ns as i64 {
                break;
            }
            let capture = (START_NS as i64 + offset) as u64;
            let mut at = capture + self.net_ns + (self.video_lag)(capture.saturating_sub(START_NS));
            if let Some((_, speedup)) = self.relay_backlog {
                at = at.max(START_NS + self.net_ns + k * frame / speedup);
            }
            let Some(at) = self.through_stalls(at, true) else {
                continue;
            };
            last_at = at.max(last_at);
            out.push(Arrival {
                at_ns: last_at,
                capture_ns: capture,
                media: Media::Video(FIRST_PTS_NS + offset),
            });
        }

        out.sort_by_key(|a| (a.at_ns, a.capture_ns));
        out
    }

    fn run(self) -> Report {
        Sim::new(&self).run(&self)
    }
}

// ── What libobs is handed ─────────────────────────────────────

/// The span of the fine half of the audio's timeline code; see
/// [`encode_sample`].
const FINE_SPAN: i64 = 65_536;

/// The timeline code every audio sample carries: which sample of the
/// sender's timeline it is, in two channels. Not as one number, because the
/// speed resampler's gain strays from 1 by about 1e-5 from one filter phase
/// to the next, which on a sample index in the millions moves it by tens of
/// samples. So channel 1 carries the index modulo [`FINE_SPAN`], centred,
/// where that error is under a sample, and channel 0 the index in units of
/// the span, which says which span the fine one is in.
fn encode_sample(n: i64) -> [f32; 2] {
    [
        (n as f64 / FINE_SPAN as f64) as f32,
        (n.rem_euclid(FINE_SPAN) - FINE_SPAN / 2) as f32,
    ]
}

/// The sample index [`encode_sample`] wrote, from what came out of the pump.
fn decode_sample(coarse: f32, fine: f32) -> f64 {
    let fine = f64::from(fine) + (FINE_SPAN / 2) as f64;
    let spans = ((f64::from(coarse) * FINE_SPAN as f64 - fine) / FINE_SPAN as f64).round();
    spans * FINE_SPAN as f64 + fine
}

/// Points per audio submission at which its content is read.
const PROBE_POINTS: usize = 9;

/// One audio submission: when it plays and which content it carries.
#[derive(Clone, Copy, Debug)]
struct Submission {
    timestamp: u64,
    frames: u32,
    rate: u32,
    /// The timeline code at evenly spaced output samples, `(index, sample)`:
    /// sample indices on the sender's timeline wherever the chunk carries
    /// real, unfaded audio.
    points: [(u32, f64); PROBE_POINTS],
}

impl Submission {
    fn first(&self) -> f64 {
        self.points[0].1
    }

    fn last(&self) -> f64 {
        self.points[PROBE_POINTS - 1].1
    }

    /// Whether the chunk is real audio played straight: no fade, no
    /// concealment. At any speed the controller uses, every stretch of it
    /// advances by roughly one content sample per output sample; faded or
    /// concealed audio is nowhere near that, and neither is a stretch across
    /// the fine code's wrap.
    fn is_straight(&self) -> bool {
        self.frames >= PROBE_POINTS as u32
            && self.points.windows(2).all(|w| {
                let slope = (w[1].1 - w[0].1) / f64::from(w[1].0 - w[0].0);
                (0.9..1.1).contains(&slope)
            })
    }

    /// OBS time at which content sample `sample` plays, if this chunk carries
    /// it, interpolated between the probe points around it.
    fn plays_at(&self, sample: f64) -> Option<f64> {
        // Between two chunks the content line steps by about a sample.
        if sample < self.first() - 2.0 || sample > self.last() {
            return None;
        }
        let i = self
            .points
            .partition_point(|p| p.1 < sample)
            .clamp(1, PROBE_POINTS - 1);
        let ((i0, v0), (i1, v1)) = (self.points[i - 1], self.points[i]);
        let at = f64::from(i0) + (sample - v0).max(0.0) / (v1 - v0) * f64::from(i1 - i0);
        Some(self.timestamp as f64 + at * 1e9 / f64::from(self.rate))
    }
}

#[derive(Clone, Default)]
struct AudioProbe(Arc<Mutex<Vec<Submission>>>);

impl AudioSink for AudioProbe {
    fn output_audio(&self, audio: &obs::AudioFrame<'_>) {
        let sys = audio.as_sys();
        let frames = sys.frames as usize;
        // SAFETY: the frame borrows a live interleaved-float buffer of
        // `frames * CHANNELS` samples, which is what the pump built.
        let pcm = unsafe {
            std::slice::from_raw_parts(sys.data[0].cast::<f32>(), frames * CHANNELS as usize)
        };
        let mut points = [(0, 0.0); PROBE_POINTS];
        for (k, point) in points.iter_mut().enumerate() {
            let i = k * frames.saturating_sub(1) / (PROBE_POINTS - 1);
            let at = i * CHANNELS as usize;
            *point = (i as u32, decode_sample(pcm[at], pcm[at + 1]));
        }
        self.0.lock().push(Submission {
            timestamp: sys.timestamp,
            frames: sys.frames,
            rate: sys.samples_per_sec,
            points,
        });
    }
}

/// One frame libobs received.
#[derive(Clone, Copy, Debug)]
struct Shown {
    pts_ns: i64,
    timestamp: u64,
    handed_ns: u64,
    /// How late the frame that anchored libobs's play head was handed over
    /// against its own timestamp (negative: early). libobs shows that frame
    /// on arrival and advances its play head by wall clock from there, so
    /// every frame after it is shown that much off its timestamp.
    anchor_error_ns: i64,
    /// Whether this is the frame that anchored the play head.
    anchor: bool,
}

impl Shown {
    /// When libobs shows it: at its timestamp as the anchored play head
    /// reaches it, or on arrival if it came later than that.
    fn displayed_ns(&self) -> u64 {
        (self.timestamp as i64 + self.anchor_error_ns).max(self.handed_ns as i64) as u64
    }

    fn late_ns(&self) -> i64 {
        self.handed_ns as i64 - self.timestamp as i64
    }
}

#[derive(Default)]
struct VideoOut {
    shown: Vec<Shown>,
    /// `None` while libobs's play head is unanchored: at the start, and after
    /// a clear.
    anchor_error_ns: Option<i64>,
}

#[derive(Clone)]
struct VideoProbe {
    out: Arc<Mutex<VideoOut>>,
    clock: Arc<AtomicU64>,
}

impl VideoSink for VideoProbe {
    fn output_video(&self, frame: &obs::VideoFrame<'_>) {
        let raw = frame.as_sys();
        // SAFETY: the plane is the stub's I420 luma, lent as it is and at
        // least 64 bytes wide; the first eight carry the frame's PTS.
        let pts = unsafe { std::ptr::read_unaligned(raw.data[0].cast::<i64>()) };
        let handed_ns = self.clock.load(Relaxed);
        let mut out = self.out.lock();
        let anchor = out.anchor_error_ns.is_none();
        let anchor_error_ns = *out
            .anchor_error_ns
            .get_or_insert(handed_ns as i64 - raw.timestamp as i64);
        out.shown.push(Shown {
            pts_ns: pts,
            timestamp: raw.timestamp,
            handed_ns,
            anchor_error_ns,
            anchor,
        });
    }

    fn output_video_none(&self) {
        self.out.lock().anchor_error_ns = None;
    }
}

/// The decoder stand-in: a small I420 keyframe carrying the packet's PTS in
/// its first luma bytes.
fn stub_decode(packet: &TimedPacket) -> Option<ffmpeg::Frame> {
    let mut frame = ffmpeg::Frame::alloc_video(ffmpeg::AVPixelFormat::AV_PIX_FMT_YUV420P, 64, 32)
        .expect("alloc");
    frame.set_pts(packet.pts_ns);
    // SAFETY: a freshly allocated 64x32 I420 frame owns a luma plane of at
    // least 64 * 32 bytes, and its flags are a plain field.
    unsafe {
        let raw = frame.as_mut_ptr();
        (*raw).flags |= ffmpeg::sys::AV_FRAME_FLAG_KEY;
        std::ptr::write_unaligned((*raw).data[0].cast::<i64>(), packet.pts_ns);
    }
    Some(frame)
}

// ── The simulated plugin ──────────────────────────────────────

/// The stats as a stats proc would read them.
#[derive(Clone, Copy, Debug)]
struct StatSample {
    at_ns: u64,
    video_delay_ms: u64,
    audio_hold_ms: i32,
    av_skew_ms: i64,
    fill_ms: i32,
    target_ms: i32,
}

struct Sim {
    clock: Arc<AtomicU64>,
    shared: Arc<Shared>,
    intake: AudioIntake,
    flags: ReceiverFlags,
    pump: AudioPump,
    video: VideoThread,
    audio_out: AudioProbe,
    video_out: Arc<Mutex<VideoOut>>,
    sync: Option<SimSync>,
}

/// Timecode sync in front of the intake, as the receiver runs it: every
/// packet the link delivers goes through the stage, and held packets reach
/// the intake when the stage releases them.
struct SimSync {
    stage: SyncStage<Media>,
    offset_ms: i32,
    fps: u64,
    gap: Option<(u64, u64)>,
}

impl SimSync {
    /// UTC per the NTP reference at OBS time `now_ns`. The sender's clock and
    /// this one agree exactly, so the frame captured at OBS time `c` carries
    /// the timecode of UTC `c + utc_offset_ns`.
    fn utc_now_ns(&self, now_ns: u64) -> i64 {
        now_ns as i64 + UTC_FRAMES_AT_START * SEC as i64 / self.fps as i64 - START_NS as i64
    }

    /// Whether the sender stamps the frame with `pts_ns` at all.
    fn stamps(&self, pts_ns: i64) -> bool {
        let capture = (pts_ns - FIRST_PTS_NS) as u64;
        self.gap
            .is_none_or(|(from, to)| !(from..to).contains(&capture))
    }

    /// The timecode the sender stamps on the frame with `pts_ns`.
    fn timecode(&self, pts_ns: i64) -> Timecode {
        let frame = (pts_ns - FIRST_PTS_NS) / (SEC / self.fps) as i64;
        let total = UTC_FRAMES_AT_START + frame;
        let secs = total / self.fps as i64;
        Timecode {
            hours: (secs / 3600 % 24) as u8,
            minutes: (secs / 60 % 60) as u8,
            seconds: (secs % 60) as u8,
            n_frames: (total % self.fps as i64) as u16,
        }
    }
}

impl Sim {
    fn new(scenario: &Scenario) -> Self {
        let mut hot = common::hot_values(scenario.target_ms);
        hot.sync_enabled = scenario.sync_offset_ms.is_some();
        let shared = common::shared(common::stream_config(scenario.low_latency), hot);
        shared.flags.audio_present.store(scenario.audio, Relaxed);
        shared.flags.video_present.store(true, Relaxed);

        let clock = Arc::new(AtomicU64::new(START_NS));
        let audio_out = AudioProbe::default();
        let pump = {
            let ns = Arc::clone(&clock);
            let us = Arc::clone(&clock);
            AudioPump::with_sink(Arc::clone(&shared), Box::new(audio_out.clone()))
                .with_clock(Box::new(move || ns.load(Relaxed)))
                .with_us_clock(Box::new(move || us.load(Relaxed) / 1000))
        };

        let video_out = Arc::new(Mutex::new(VideoOut::default()));
        let sink = VideoProbe {
            out: Arc::clone(&video_out),
            clock: Arc::clone(&clock),
        };
        let tick = scenario.canvas_tick_ns();
        let video = {
            let ns = Arc::clone(&clock);
            VideoThread::with_sink(Arc::clone(&shared), Box::new(sink))
                .with_clock(Box::new(move || ns.load(Relaxed)))
                .with_canvas_tick(Box::new(move || Some(tick)))
                .with_decode_stub(Box::new(stub_decode))
        };

        let mut intake = AudioIntake::default();
        intake.init_pts_repair(ffmpeg::NS_TIME_BASE);

        let sync = scenario.sync_offset_ms.map(|offset_ms| SimSync {
            stage: SyncStage::new(Arc::clone(&shared), 0),
            offset_ms,
            fps: scenario.fps,
            gap: scenario.timecode_gap,
        });

        Self {
            clock,
            shared,
            intake,
            flags: ReceiverFlags {
                has_audio_stream: scenario.audio,
                has_video_stream: true,
                ..Default::default()
            },
            pump,
            video,
            audio_out,
            video_out,
            sync,
        }
    }

    fn now(&self) -> u64 {
        self.clock.load(Relaxed)
    }

    /// The receiver thread reading one packet off the link.
    fn receive(&mut self, media: Media) {
        let now = self.now();
        let Some(sync) = self.sync.as_mut() else {
            self.dispatch(media);
            return;
        };
        let info = match media {
            Media::Audio(n) => PacketInfo {
                stream: 1,
                dts_ns: Some(sample_pts_ns(n)),
                timecode: None,
                pts_ns: None,
                bytes: 400,
            },
            Media::Video(pts) => PacketInfo {
                stream: 0,
                dts_ns: Some(pts),
                timecode: sync.stamps(pts).then(|| sync.timecode(pts)),
                pts_ns: Some(pts),
                bytes: 4096,
            },
        };
        let ctx = Context {
            now_ns: now,
            utc_now_ns: Some(sync.utc_now_ns(now)),
            enabled: true,
            offset_ms: sync.offset_ms,
            offset_generation: 0,
            video_codec: VideoCodec::Hevc,
        };
        if !sync.stage.intercept(&ctx, &info, || Some(media)) {
            self.dispatch(media);
        }
    }

    /// Hand the intake every held packet that has come due.
    fn release_due(&mut self) {
        let now = self.now();
        while let Some(media) = self.sync.as_mut().and_then(|s| s.stage.pop_due(now)) {
            self.dispatch(media);
        }
    }

    /// A packet reaching the decoders: audio is decoded into the intake,
    /// video queued for the video thread.
    fn dispatch(&mut self, media: Media) {
        let now = self.now();
        match media {
            Media::Audio(n) => {
                let pts = sample_pts_ns(n);
                let duration = sample_pts_ns(n + AUDIO_FRAMES) - pts;
                let frame = common::decoded_audio(pts, duration, AUDIO_FRAMES as i32, |i, c| {
                    encode_sample(n + i as i64)[c]
                });
                self.intake.handle_frame(
                    &self.shared,
                    &mut self.flags,
                    &frame,
                    ffmpeg::NS_TIME_BASE,
                );
            }
            Media::Video(pts) => {
                note_video_arrival(&self.shared, now, pts, Some(pts));
                self.shared.video.push_packet(
                    TimedPacket {
                        packet: ffmpeg::Packet::new().unwrap(),
                        pts_ns: pts,
                        bytes: 4096,
                        received_ns: now,
                    },
                    &self.shared.lifetime,
                );
            }
        }
    }

    fn apply(&mut self, event: &Event) {
        match *event {
            Event::TargetBuffer(target_ms) => {
                let edit = Config {
                    stream: self.shared.cfg.clone(),
                    hot: common::hot_values(target_ms),
                    close_when_inactive: false,
                };
                edit.apply_hot(&self.shared);
            }
        }
    }

    fn stats(&self) -> StatSample {
        let state = self.shared.audio_state();
        StatSample {
            at_ns: self.now(),
            video_delay_ms: self.shared.conn.video_delay_ns.load(Relaxed) / MS,
            audio_hold_ms: self.shared.conn.audio_hold_ms.load(Relaxed),
            av_skew_ms: av_skew_ms(&self.shared, &state),
            fill_ms: self
                .shared
                .audio_buf()
                .as_ref()
                .map_or(0, irl_core::AudioBuffer::fill_ms),
            target_ms: self.shared.hot.watermarks().target_ms,
        }
    }

    /// Run the three threads against the sender, each woken when it would
    /// be: the receiver as packets arrive, the audio thread when its pump
    /// says, the video thread when its pacing sleep ends, a packet lands with
    /// room to decode it, or audio publishes its first mapping.
    fn run(mut self, scenario: &Scenario) -> Report {
        let arrivals = scenario.arrivals();
        let end_ns = START_NS + scenario.secs * SEC;
        let mut next_arrival = 0;
        let mut next_event = 0;
        let mut pump_wake = START_NS;
        let mut video_wake = START_NS;
        let mut next_sample = START_NS;
        let mut mapped = false;
        let mut stats = Vec::new();

        while self.now() < end_ns {
            let now = self.now();

            while let Some((at, event)) = scenario.events.get(next_event) {
                if START_NS + at > now {
                    break;
                }
                self.apply(event);
                next_event += 1;
            }

            while let Some(arrival) = arrivals.get(next_arrival) {
                if arrival.at_ns > now {
                    break;
                }
                self.receive(arrival.media);
                next_arrival += 1;
            }
            self.release_due();

            if now >= pump_wake {
                while self.pump.pump_once() {}
                pump_wake = now + u64::from(self.pump.idle_sleep_ms()) * MS;
            }

            let published = self.shared.audio_state().mapping.is_published();
            let woken = published && !mapped;
            mapped = published;
            let has_work = |sim: &Self| sim.shared.video.has_work(sim.video.pacing().has_room());
            if now >= video_wake || woken || has_work(&self) {
                let mut spins = 0;
                loop {
                    let wait = self.video.run_once();
                    if !wait.is_zero() && !has_work(&self) {
                        video_wake = now + wait.as_nanos() as u64;
                        break;
                    }
                    spins += 1;
                    assert!(spins < 1000, "the video thread spins at {}s", secs(now));
                }
            }

            if now >= next_sample {
                stats.push(self.stats());
                next_sample = now + 100 * MS;
            }

            let mut next = pump_wake.min(video_wake).min(next_sample).min(end_ns);
            if let Some(arrival) = arrivals.get(next_arrival) {
                next = next.min(arrival.at_ns);
            }
            if let Some((at, _)) = scenario.events.get(next_event) {
                next = next.min(START_NS + at);
            }
            if let Some(release) = self.sync.as_ref().and_then(|s| s.stage.next_release_ns()) {
                next = next.min(release);
            }
            self.clock.store(next.max(now + 1), Relaxed);
        }

        let video_sent = arrivals
            .iter()
            .filter(|a| matches!(a.media, Media::Video(_)))
            .count();
        // Only real audio played straight carries a readable timeline; fades
        // and concealment are left out, so what remains is in content order.
        let played = self
            .audio_out
            .0
            .lock()
            .iter()
            .filter(|s| s.is_straight())
            .copied()
            .collect();
        Report {
            submissions: self.audio_out.0.lock().clone(),
            played,
            shown: self.video_out.lock().shown.clone(),
            stats,
            video_sent,
            tick_ns: scenario.canvas_tick_ns(),
            frame_ns: scenario.frame_ns(),
            sync_offset_ns: scenario
                .sync_offset_ms
                .map(|ms| u64::from(ms.unsigned_abs()) * MS),
            sync: self.sync.as_ref().map(|s| s.stage.snapshot()),
            underruns: self.shared.conn.audio_underruns.load(Relaxed),
            restarts: self.shared.conn.audio_output_restarts.load(Relaxed),
            reanchors: self.shared.lifetime.audio_offset_reanchors.load(Relaxed),
            audible_skipped: self.shared.conn.audible_skipped_chunks.load(Relaxed),
        }
    }
}

// ── What the run shows ────────────────────────────────────────

struct Report {
    submissions: Vec<Submission>,
    /// The submissions of real audio played straight.
    played: Vec<Submission>,
    shown: Vec<Shown>,
    stats: Vec<StatSample>,
    video_sent: usize,
    tick_ns: u64,
    frame_ns: u64,
    /// The sync offset the scenario ran at.
    sync_offset_ns: Option<u64>,
    /// What the sync stage reported at the end.
    sync: Option<SyncSnapshot>,
    underruns: u64,
    restarts: u64,
    reanchors: u64,
    audible_skipped: u64,
}

/// What the frames shown inside one stretch of the run measured.
#[derive(Debug)]
struct Window {
    from_s: f64,
    to_s: f64,
    frames: usize,
    /// Frames whose audio could not be located (it played faded, concealed,
    /// or not yet).
    unmeasured: usize,
    min_offset_ms: f64,
    max_offset_ms: f64,
    mean_offset_ms: f64,
    late: usize,
    max_late_ms: f64,
}

impl Report {
    /// When the audio of content PTS `pts_ns` plays, read off the PCM.
    fn audio_plays_at(&self, pts_ns: i64) -> Option<f64> {
        let sample = pts_ns as f64 * f64::from(RATE) / 1e9;
        let i = self.played.partition_point(|s| s.last() < sample);
        self.played.get(i)?.plays_at(sample)
    }

    /// Picture minus sound for one frame, in ms; positive is picture behind.
    fn offset_ms(&self, frame: &Shown) -> Option<f64> {
        let audio = self.audio_plays_at(frame.pts_ns)?;
        Some((frame.displayed_ns() as f64 - audio) / 1e6)
    }

    /// The frames handed over between `from_s` and `to_s` after the open.
    fn window(&self, from_s: f64, to_s: f64) -> Window {
        let from = START_NS + (from_s * SEC as f64) as u64;
        let to = START_NS + (to_s * SEC as f64) as u64;
        let frames: Vec<&Shown> = self
            .shown
            .iter()
            .filter(|f| (from..to).contains(&f.handed_ns))
            .collect();
        // A frame whose audio had not played by the end has nothing to be
        // measured against yet.
        let played_to = self.played.last().map_or(0.0, Submission::last);
        let frames: Vec<&Shown> = frames
            .into_iter()
            .filter(|f| (f.pts_ns as f64 * f64::from(RATE) / 1e9) < played_to)
            .collect();
        let offsets: Vec<f64> = frames.iter().filter_map(|f| self.offset_ms(f)).collect();
        // The anchoring frame may go up to a canvas tick late by design (see
        // `VideoThread::settle_anchor_candidate`); what that costs shows up
        // in every later frame's offset instead.
        let late: Vec<i64> = frames
            .iter()
            .filter(|f| !f.anchor)
            .map(|f| f.late_ns())
            .filter(|&l| l > consts::VIDEO_PACING_SLACK_NS)
            .collect();
        Window {
            from_s,
            to_s,
            frames: frames.len(),
            unmeasured: frames.len() - offsets.len(),
            min_offset_ms: offsets.iter().copied().fold(f64::INFINITY, f64::min),
            max_offset_ms: offsets.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            mean_offset_ms: offsets.iter().sum::<f64>() / offsets.len().max(1) as f64,
            late: late.len(),
            max_late_ms: late.iter().copied().max().unwrap_or(0) as f64 / 1e6,
        }
    }

    /// When each frame shown in the window reached the screen, against when
    /// timecode sync says it should: its capture time plus the offset. In ms,
    /// `(min, max)`; positive is late.
    fn sync_error_ms(&self, from_s: f64, to_s: f64) -> (f64, f64) {
        let offset = self.sync_offset_ns.expect("a synced scenario");
        let from = START_NS + (from_s * SEC as f64) as u64;
        let to = START_NS + (to_s * SEC as f64) as u64;
        self.shown
            .iter()
            .filter(|f| (from..to).contains(&f.handed_ns))
            .map(|f| {
                let capture = START_NS as i64 + (f.pts_ns - FIRST_PTS_NS);
                (f.displayed_ns() as i64 - (capture + offset as i64)) as f64 / 1e6
            })
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), e| {
                (lo.min(e), hi.max(e))
            })
    }

    /// Every frame shown in the window reached the screen within
    /// `tolerance_ns` of its capture time plus the sync offset.
    fn assert_on_offset(&self, from_s: f64, to_s: f64, tolerance_ns: i64) {
        let (lo, hi) = self.sync_error_ms(from_s, to_s);
        let tol = tolerance_ns as f64 / 1e6;
        assert!(lo.is_finite(), "no frames shown in {from_s}-{to_s}s");
        assert!(
            lo >= -tol && hi <= tol,
            "{lo:.1}..{hi:.1}ms off capture + offset in {from_s}-{to_s}s, against {tol:.1}ms"
        );
    }

    /// When the frame with `pts_ns` reached the screen, if it was shown.
    fn displayed_at(&self, pts_ns: i64) -> Option<u64> {
        self.shown
            .iter()
            .find(|f| f.pts_ns == pts_ns)
            .map(Shown::displayed_ns)
    }

    fn last_stats(&self) -> StatSample {
        *self.stats.last().expect("sampled")
    }

    /// One line per scenario for the record, with `--nocapture`.
    fn print(&self, name: &str, steady: &Window) {
        let s = self.last_stats();
        println!(
            "{name}: offset {:.1}..{:.1}ms (mean {:.1}) late {} (max {:.1}ms) over {:.0}-{:.0}s; \
             anchor error {:.1}ms, shown {}/{} video_delay={}ms audio_hold={}ms av_skew={}ms \
             reanchors={} restarts={}",
            steady.min_offset_ms,
            steady.max_offset_ms,
            steady.mean_offset_ms,
            steady.late,
            steady.max_late_ms,
            steady.from_s,
            steady.to_s,
            self.shown.first().map_or(0, |f| f.anchor_error_ns) as f64 / 1e6,
            self.shown.len(),
            self.video_sent,
            s.video_delay_ms,
            s.audio_hold_ms,
            s.av_skew_ms,
            self.reanchors,
            self.restarts,
        );
    }

    // ── Promises ──

    /// Picture and sound agree within `tolerance_ns` for every frame shown in
    /// the window, and every one of them could be measured.
    fn assert_in_sync(&self, w: &Window, tolerance_ns: i64) {
        let tol = tolerance_ns as f64 / 1e6;
        assert!(w.frames > 0, "no frames shown in {w:?}");
        assert!(
            w.unmeasured * 20 <= w.frames,
            "the audio of most frames could not be located: {w:?}"
        );
        assert!(
            w.min_offset_ms >= -tol && w.max_offset_ms <= tol,
            "out of sync by more than {tol:.1}ms: {w:?}"
        );
    }

    /// No frame in the window reached libobs after its own timestamp.
    fn assert_paced(&self, w: &Window) {
        assert_eq!(w.late, 0, "frames handed over late: {w:?}");
    }

    /// The libobs audio contract: timestamps contiguous except across the
    /// restarts and re-anchors the plugin declares and counts.
    fn assert_clock_only_jumps_where_declared(&self) {
        let mut jumps = 0;
        for pair in self.submissions.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert_eq!(a.rate, RATE as u32, "the submitted rate must never change");
            let expected = a.timestamp + u64::from(a.frames) * SEC / u64::from(a.rate);
            if (b.timestamp as i64 - expected as i64).abs() > 1 {
                jumps += 1;
            }
        }
        let declared = self.restarts + self.reanchors;
        assert!(
            jumps <= declared,
            "{jumps} discontinuities against {declared} declared"
        );
    }

    /// What every scenario owes whatever the link did.
    fn assert_healthy(&self) {
        self.assert_clock_only_jumps_where_declared();
        assert_eq!(self.audible_skipped, 0, "audible audio was skipped");
    }

    /// Every frame sent from `from_s` on was shown: nothing dropped once the
    /// play head anchored.
    fn assert_nothing_dropped_after(&self, from_s: f64) {
        let from = START_NS + (from_s * SEC as f64) as u64;
        let first = self
            .shown
            .iter()
            .find(|f| f.handed_ns >= from)
            .expect("frames shown")
            .pts_ns;
        let shown: Vec<i64> = self
            .shown
            .iter()
            .filter(|f| f.pts_ns >= first)
            .map(|f| f.pts_ns)
            .collect();
        let gaps = shown
            .windows(2)
            .filter(|p| p[1] - p[0] > self.frame_ns as i64 + 1)
            .count();
        assert_eq!(gaps, 0, "frames dropped after {from_s}s");
    }

    /// Picture and sound settled into sync between `from_s` and `to_s`:
    /// every frame within the tolerance and handed over in time.
    fn assert_settled(&self, from_s: f64, to_s: f64) -> Window {
        let w = self.window(from_s, to_s);
        self.assert_in_sync(&w, SYNC_TOLERANCE_NS);
        self.assert_paced(&w);
        w
    }

    /// Sync in `later` is where it was in `earlier`: whatever happened in
    /// between left no lasting offset.
    fn assert_no_drift(&self, earlier: &Window, later: &Window) {
        let drift = later.mean_offset_ms - earlier.mean_offset_ms;
        assert!(
            drift.abs() <= 2.0,
            "sync drifted {drift:+.1}ms: {earlier:?} then {later:?}"
        );
    }

    /// The audio latency came back to what the settings and the hold ask
    /// for: the buffer at its target.
    fn assert_latency_settled(&self) {
        let s = self.last_stats();
        assert!(
            (s.fill_ms - s.target_ms).abs() <= 30,
            "the buffer holds {}ms against a {}ms target",
            s.fill_ms,
            s.target_ms
        );
    }

    /// The video delay the stats reported, over the samples from `from_s` on.
    fn video_delay_ms(&self, from_s: f64) -> std::ops::RangeInclusive<u64> {
        let from = START_NS + (from_s * SEC as f64) as u64;
        let delays = self
            .stats
            .iter()
            .filter(|s| s.at_ns >= from)
            .map(|s| s.video_delay_ms);
        let (lo, hi) = delays.fold((u64::MAX, 0), |(lo, hi), d| (lo.min(d), hi.max(d)));
        lo..=hi
    }

    /// The stats sample taken at or just before `at_s` after the open.
    fn stats_at(&self, at_s: f64) -> StatSample {
        let at = START_NS + (at_s * SEC as f64) as u64;
        let i = self.stats.partition_point(|s| s.at_ns <= at);
        self.stats[i.saturating_sub(1)]
    }

    /// Seconds after `from_s` until picture and sound are back in sync for
    /// good: the hand-over time of the first frame after which every frame
    /// shown up to `to_s` is measurable, within [`SYNC_TOLERANCE_NS`] and on
    /// time.
    fn resynced_after(&self, from_s: f64, to_s: f64) -> f64 {
        let from = START_NS + (from_s * SEC as f64) as u64;
        let to = START_NS + (to_s * SEC as f64) as u64;
        let tol = SYNC_TOLERANCE_NS as f64 / 1e6;
        let frames: Vec<&Shown> = self
            .shown
            .iter()
            .filter(|f| (from..to).contains(&f.handed_ns))
            .collect();
        let last_bad = frames.iter().rposition(|f| {
            let off = self.offset_ms(f);
            off.is_none_or(|o| o.abs() > tol) || f.late_ns() > consts::VIDEO_PACING_SLACK_NS
        });
        let synced_ns = match last_bad {
            None => from,
            Some(i) => frames.get(i + 1).map_or(to, |f| f.handed_ns),
        };
        (synced_ns - from) as f64 / SEC as f64
    }
}

// ── Scenarios ─────────────────────────────────────────────────
//
// A sender's video can trail its audio in the mux by its encoder's habits
// (pocketSRT queues audio early, a stabiliser holds video back). Each
// scenario states what the plugin promises for it, and how soon.

/// Nothing wrong with the link or the sender: picture and sound agree within
/// a canvas tick from the first frame shown, every frame is handed over ahead
/// of its timestamp, and no delay or hold is ever needed. The tick is what
/// the anchoring frame may be late by (see `VideoThread::
/// settle_anchor_candidate`), and libobs keeps that error for the connection.
#[test]
fn a_clean_link_is_in_sync_from_the_first_frame() {
    for fps in [30, 60] {
        let report = Scenario::new(30).fps(fps).run();
        let all = report.window(0.0, 30.0);
        report.print(&format!("clean {fps}fps"), &all);

        report.assert_in_sync(&all, report.tick_ns as i64);
        report.assert_paced(&all);
        report.assert_nothing_dropped_after(0.0);
        report.assert_healthy();
        assert_eq!(report.video_delay_ms(0.0), 0..=0, "{fps}fps");
        assert_eq!(report.last_stats().audio_hold_ms, 0);
    }
}

/// pocketSRT queues its audio 300 ms and more ahead of its video, and its
/// video also stalls on its own for a couple of hundred milliseconds every
/// few seconds (#34). The hold covers the steady skew from the first frame.
/// The frames a stall holds back cannot be shown in time and go out late;
/// the picture is back in sync once the burst is through, and it must be in
/// the same sync after the last stall as after the first.
#[test]
fn audio_ahead_of_its_video_with_recurring_video_stalls_stays_in_sync() {
    for lag_ms in [350, 700] {
        let mut scenario = Scenario::new(70).video_lag(move |_| lag_ms * MS);
        for k in 0..15 {
            scenario = scenario.stall(10.0 + 4.0 * f64::from(k), 200, Streams::Video);
        }
        let report = scenario.run();
        let between_stalls = |k: u32| {
            let at = 10.0 + 4.0 * f64::from(k);
            report.assert_settled(at + 1.0, at + 4.0)
        };

        let before = report.assert_settled(0.0, 10.0);
        let first = between_stalls(0);
        for k in 1..13 {
            between_stalls(k);
        }
        let last = between_stalls(13);
        report.print(&format!("pocketSRT {lag_ms}ms"), &last);

        report.assert_no_drift(&before, &last);
        report.assert_no_drift(&first, &last);
        let hold = report.last_stats().audio_hold_ms;
        let expected = lag_ms as i32 + consts::AUDIO_HOLD_MARGIN_MS
            - consts::DEFAULT_BUFFER_TARGET_MS as i32
            - consts::AUDIO_OUT_LEAD_MS;
        assert!(
            (hold - expected).abs() <= 3,
            "{lag_ms}ms: hold {hold}ms against {expected}ms"
        );
        assert!(
            report.stats.iter().all(|s| s.audio_hold_ms <= hold),
            "{lag_ms}ms: the stalls ratcheted the hold"
        );
        assert_eq!(report.video_delay_ms(0.0), 0..=0, "{lag_ms}ms");
        report.assert_healthy();
    }
}

/// The stabiliser stream from #33: video 1.65 s behind its audio, for good.
/// The hold is sized before audio starts, so the picture is in sync from the
/// first frame and never needs a delay.
#[test]
fn video_seconds_behind_its_audio_is_in_sync_from_the_first_frame() {
    let report = Scenario::new(40).video_lag(|_| 1650 * MS).run();
    let all = report.assert_settled(0.0, 40.0);
    report.print("stabiliser", &all);

    report.assert_nothing_dropped_after(0.0);
    assert_eq!(report.video_delay_ms(0.0), 0..=0);
    let hold = report.last_stats().audio_hold_ms;
    assert!(
        (hold - (1650 + consts::AUDIO_HOLD_MARGIN_MS - 120 - consts::AUDIO_OUT_LEAD_MS)).abs() <= 3,
        "hold {hold}ms"
    );
    report.assert_latency_settled();
    report.assert_healthy();
}

const SKEW_RISES_TO_600_MS: &[(f64, f64)] = &[(20.0, 0.0), (23.0, 600.0)];

/// Video falls 600 ms behind its audio over three seconds, stays there, and
/// recovers. While it falls the delay covers what it can and frames go out
/// late; after the raise window the hold rises and is built at -2 %, which
/// for 600 ms is half a minute, and the delay hands itself back as it builds.
/// After the recovery the hold is released and the latency drained. In sync
/// before, once built, and after.
#[test]
fn a_skew_that_rises_and_recovers_is_followed_both_ways() {
    let report = Scenario::new(120)
        .video_lag(lag_profile(&[
            (20.0, 0.0),
            (23.0, 600.0),
            (60.0, 600.0),
            (63.0, 0.0),
        ]))
        .run();

    let before = report.assert_settled(0.0, 20.0);
    report.assert_settled(55.0, 60.0);
    let after = report.assert_settled(80.0, 120.0);
    report.print("skew 0-600-0", &after);

    assert_eq!(report.window(28.0, 120.0).late, 0, "late once sized");
    report.assert_no_drift(&before, &after);
    // The release keeps anything within its hysteresis.
    assert!(report.last_stats().audio_hold_ms <= consts::AUDIO_HOLD_RELAX_MIN_MS);
    assert_eq!(report.video_delay_ms(80.0), 0..=0);
    report.assert_latency_settled();
    report.assert_nothing_dropped_after(28.0);
    report.assert_healthy();
}

/// Both streams stall for three seconds while the hold for a new 600 ms skew
/// is being built, with the picture still delayed behind the sound. Audio
/// conceals, which grows the playout by the stall, and the backlog lands at
/// once and drains at the Catch-Up Speed. The delay is the video floor less
/// the playout, so the stall itself takes it away: in sync within a second
/// of the stall ending, and back where the connection started.
#[test]
fn a_full_stall_during_a_hold_build_recovers_sync() {
    let report = Scenario::new(120)
        .video_lag(lag_profile(SKEW_RISES_TO_600_MS))
        .stall(30.0, 3000, Streams::Both)
        .run();

    let at_stall = report.stats_at(29.9);
    assert!(
        at_stall.video_delay_ms > 0,
        "the delay had gone before the stall: {at_stall:?}"
    );
    let before = report.assert_settled(0.0, 20.0);
    let resynced_s = report.resynced_after(33.0, 120.0);
    assert!(
        resynced_s <= 1.0,
        "in sync {resynced_s:.2}s after the stall"
    );
    report.assert_settled(34.0, 120.0);
    let after = report.window(100.0, 120.0);
    report.print("stall during build", &after);
    println!("stall during build: in sync {resynced_s:.2}s after the stall");

    report.assert_no_drift(&before, &after);
    assert_eq!(report.video_delay_ms(34.0), 0..=0);
    report.assert_latency_settled();
    report.assert_healthy();
}

/// A loss while the delay stands (a hold build in progress) leaves audio
/// latency inflated by its concealment, which the pump re-anchors away once
/// the buffer is back at its target, under a video floor sized for the skew. Video
/// follows the playout down only as far as the floor: in sync within a few
/// seconds of the loss, with no lasting offset.
#[test]
fn a_reanchor_while_the_video_delay_stands_recovers_sync() {
    let report = Scenario::new(120)
        .video_lag(lag_profile(SKEW_RISES_TO_600_MS))
        .loss(26.0, 1500)
        .run();

    let at_loss = report.stats_at(25.9);
    assert!(
        at_loss.video_delay_ms > 0,
        "the delay had gone before the loss: {at_loss:?}"
    );
    assert_eq!(report.reanchors, 1);

    let before = report.assert_settled(0.0, 20.0);
    let resynced_s = report.resynced_after(27.5, 120.0);
    assert!(resynced_s <= 3.0, "in sync {resynced_s:.2}s after the loss");
    let after = report.assert_settled(30.0, 120.0);
    report.print("re-anchor under delay", &after);
    println!("re-anchor under delay: in sync {resynced_s:.2}s after the loss");

    report.assert_no_drift(&before, &after);
    assert_eq!(report.video_delay_ms(28.0), 0..=0);
    report.assert_latency_settled();
    report.assert_healthy();
}

/// The user raises Target Buffer to 500 ms while the hold for a 600 ms skew is
/// being built. The hold gives back what the deeper buffer now covers, and
/// the effective target is the same skew plus margin as before.
#[test]
fn a_target_buffer_edit_during_a_hold_build_recomposes_the_hold() {
    let report = target_edit_during_build();

    let s = report.last_stats();
    assert!(
        (s.audio_hold_ms - (600 + consts::AUDIO_HOLD_MARGIN_MS - 500 - consts::AUDIO_OUT_LEAD_MS))
            .abs()
            <= 25,
        "hold {}ms",
        s.audio_hold_ms
    );
    assert_eq!(s.target_ms, 500 + s.audio_hold_ms);
    assert_eq!(report.window(30.0, 120.0).late, 0);
    report.assert_latency_settled();
    report.assert_healthy();
}

/// The same edit must also end in sync: the effective target is unchanged,
/// so the playout still grows past the video floor, and the delay, which is
/// the floor less the playout, goes with it. Within the anchoring frame's
/// tick of where the connection started.
#[test]
fn a_target_buffer_edit_during_a_hold_build_ends_in_sync() {
    let report = target_edit_during_build();
    let after = report.assert_settled(90.0, 120.0);
    report.print("target edit during build", &after);
    report.assert_in_sync(&after, (report.tick_ns + 2 * MS) as i64);
    assert_eq!(report.video_delay_ms(90.0), 0..=0);
}

fn target_edit_during_build() -> Report {
    Scenario::new(120)
        .video_lag(lag_profile(SKEW_RISES_TO_600_MS))
        .at(28.0, Event::TargetBuffer(500))
        .run()
}

/// Low Latency Audio has no target and no speed control: the hold is sized
/// before audio starts and kept queued on top of the single chunk of lead.
#[test]
fn low_latency_audio_with_trailing_video_is_in_sync_from_the_first_frame() {
    let report = Scenario::new(40)
        .low_latency()
        .video_lag(|_| 350 * MS)
        .run();
    let all = report.assert_settled(0.0, 40.0);
    report.print("low latency", &all);

    let hold = report.last_stats().audio_hold_ms;
    let lead_ms = (sample_pts_ns(AUDIO_FRAMES) / 1_000_000) as i32;
    assert!(
        (hold - (350 + consts::AUDIO_HOLD_MARGIN_MS - lead_ms)).abs() <= 3,
        "hold {hold}ms"
    );
    assert_eq!(report.video_delay_ms(0.0), 0..=0);
    report.assert_nothing_dropped_after(0.0);
    report.assert_healthy();
}

/// Without audio there is nothing to be in sync with: the video-only clock
/// schedules each frame at its arrival, a tick of delay leaves room to
/// decode it, and every frame goes out paced, a frame apart.
#[test]
fn video_without_audio_is_paced_a_tick_behind_its_arrival() {
    let report = Scenario::new(20).video_only().run();
    let all = report.window(0.0, 20.0);
    report.print("video only", &all);

    let tick_ms = report.tick_ns / MS;
    assert_eq!(report.video_delay_ms(0.5), tick_ms..=tick_ms);
    report.assert_paced(&all);
    report.assert_nothing_dropped_after(0.0);
    for pair in report.shown.windows(2) {
        let step = pair[1].timestamp - pair[0].timestamp;
        assert!(step.abs_diff(report.frame_ns) <= 1, "frames {step}ns apart");
    }
}

/// A relay hands a new subscriber video from its last keyframe, a second
/// behind live audio, and catches up at four times real time. That is not a
/// late sender: no delay, no hold, and the picture is in sync from the first
/// frame shown.
#[test]
fn a_relay_catching_up_at_connect_sets_no_delay_and_ends_in_sync() {
    let report = Scenario::new(40).relay_backlog(1000, 4).run();
    let all = report.assert_settled(0.0, 40.0);
    report.print("relay catch-up", &all);

    assert_eq!(report.video_delay_ms(0.0), 0..=0);
    assert_eq!(report.last_stats().audio_hold_ms, 0);
    report.assert_nothing_dropped_after(0.0);
    report.assert_latency_settled();
    report.assert_healthy();
}

// ── Timecode sync ─────────────────────────────────────────────

// Every synced frame is shown at its capture time plus the offset, on the
// NTP clock. The sender's clock and the reference agree exactly here, so what
// is measured is the plugin's own error. The timecode carries capture time to
// a frame, and libobs shows a frame on a canvas tick, so a frame is about the
// floor.

/// From the first frame shown, within the lip sync tolerance of where sync
/// says it belongs, and within a frame once the loop has had time to settle;
/// lip sync and the audio clock exactly as without sync.
#[test]
fn timecode_sync_shows_each_frame_at_its_capture_time_plus_the_offset() {
    for (latency_ms, offset_ms) in [(300, 2000), (1200, 2000), (300, 5000)] {
        let report = Scenario::new(60)
            .network_latency(latency_ms)
            .timecode_sync(offset_ms)
            .run();
        let all = report.window(0.0, 60.0);
        report.print(
            &format!("sync {latency_ms}ms link, {offset_ms}ms offset"),
            &all,
        );

        report.assert_in_sync(&all, report.tick_ns as i64);
        report.assert_paced(&all);
        // The longer the offset, the later the first frame: the line holds
        // all of it before anything plays.
        let first_s = secs(report.shown[0].handed_ns - START_NS);
        report.assert_on_offset(0.0, first_s + 10.0, SYNC_TOLERANCE_NS);
        report.assert_on_offset(first_s + 10.0, 60.0, report.frame_ns as i64);
        report.assert_healthy();
        assert_eq!(report.underruns, 0, "sync never starves the audio");
        assert_eq!(report.sync.expect("synced").status, SyncStatus::Locked);
    }
}

/// The point of the feature: two feeds on links a second apart show the same
/// captured instant at the same time.
#[test]
fn two_feeds_on_different_links_show_the_same_instant() {
    let near = Scenario::new(40)
        .network_latency(300)
        .timecode_sync(2000)
        .run();
    let far = Scenario::new(40)
        .network_latency(1300)
        .timecode_sync(2000)
        .run();

    let mut compared = 0;
    for frame in near
        .shown
        .iter()
        .filter(|f| f.handed_ns > START_NS + 10 * SEC)
    {
        if let Some(at) = far.displayed_at(frame.pts_ns) {
            let apart_ms = (frame.displayed_ns() as i64 - at as i64) as f64 / 1e6;
            assert!(
                apart_ms.abs() <= (near.tick_ns * 2) as f64 / 1e6,
                "frame {} shown {apart_ms:.1}ms apart",
                frame.pts_ns
            );
            compared += 1;
        }
    }
    assert!(compared > 500, "only {compared} frames shown by both");
}

/// Video 700 ms behind its audio: the audio hold and sync stack, the hold
/// keeping lip sync and sync shrinking its delay line by what the hold adds.
#[test]
fn timecode_sync_with_video_behind_its_audio_keeps_both_promises() {
    let report = Scenario::new(60)
        .network_latency(300)
        .video_lag(|_| 700 * MS)
        .timecode_sync(3000)
        .run();
    let settled = report.assert_settled(0.0, 60.0);
    report.print("sync with trailing video", &settled);

    report.assert_on_offset(0.0, 60.0, SYNC_TOLERANCE_NS);
    report.assert_healthy();
    assert_eq!(report.underruns, 0);
    assert!(
        report.last_stats().audio_hold_ms > 500,
        "the hold is in force"
    );
}

/// A relay replaying from its last keyframe delivers seconds of old video at
/// connect. Read as latency it would seed the hold and the peak wrong; the
/// controller waits for video to arrive live before it takes either.
#[test]
fn timecode_sync_ignores_a_relay_catching_up_at_connect() {
    let report = Scenario::new(60)
        .network_latency(300)
        .relay_backlog(2000, 4)
        .timecode_sync(3000)
        .run();
    let settled = report.assert_settled(5.0, 60.0);
    report.print("sync behind a relay", &settled);

    report.assert_on_offset(20.0, 60.0, SYNC_TOLERANCE_NS);
    report.assert_healthy();
    assert_eq!(report.underruns, 0);
    let peak = report.sync.expect("synced").latency_peak_ms;
    assert!(
        (300..=400).contains(&peak),
        "the latency peak {peak}ms counts the replay"
    );
}

/// The delay line releases each packet at its place on its stream's
/// timeline, so a stall shorter than the hold is covered by what the line
/// already has: nothing underruns, nothing is late, and presentation does not
/// move. Without sync the same stall underruns or freezes the picture.
#[test]
fn a_stall_shorter_than_the_hold_costs_nothing() {
    for streams in [Streams::Both, Streams::Video] {
        let stall = |s: Scenario| s.network_latency(300).stall(20.0, 1500, streams);
        let synced = stall(Scenario::new(40).timecode_sync(3000)).run();
        let settled = synced.assert_settled(15.0, 40.0);
        synced.print(&format!("sync through a {streams:?} stall"), &settled);

        synced.assert_on_offset(15.0, 40.0, synced.frame_ns as i64);
        synced.assert_healthy();
        assert_eq!(synced.underruns, 0, "{streams:?}");

        let plain = stall(Scenario::new(40)).run();
        let w = plain.window(20.0, 25.0);
        assert!(
            plain.underruns > 0 || w.late > 0,
            "the same {streams:?} stall costs something without sync"
        );
    }
}

/// A stall longer than the hold runs the line dry, and what is left of it
/// reaches the jitter buffer. It must still cost less than without sync.
#[test]
fn a_stall_longer_than_the_hold_costs_less_than_without_sync() {
    let stall = |s: Scenario| s.network_latency(300).stall(20.0, 3500, Streams::Both);
    let synced = stall(Scenario::new(40).timecode_sync(3000)).run();
    let plain = stall(Scenario::new(40)).run();

    synced.assert_healthy();
    assert!(
        synced.underruns < plain.underruns,
        "{} concealed chunks with sync, {} without",
        synced.underruns,
        plain.underruns
    );
}

/// Without audio there is no mapping to measure against, and the fallback
/// anchor is fixed at the first frame. The seed places the stream open loop,
/// with no bias, since there is no anchor to absorb one.
#[test]
fn timecode_sync_places_a_stream_without_audio() {
    let report = Scenario::new(40)
        .network_latency(300)
        .video_only()
        .timecode_sync(3000)
        .run();

    // Frames shown before the controller engaged are not synced yet.
    report.assert_on_offset(5.0, 40.0, report.tick_ns as i64);
    assert_eq!(report.sync.expect("synced").status, SyncStatus::Locked);
}

/// Low Latency Audio has no speed controller to absorb a hold change and no
/// jitter buffer to anchor in, so sync stays out of it.
#[test]
fn timecode_sync_leaves_low_latency_audio_alone() {
    let report = Scenario::new(30)
        .network_latency(300)
        .low_latency()
        .timecode_sync(3000)
        .run();

    report.assert_healthy();
    let snap = report.sync.expect("synced");
    assert_eq!(snap.added_ms, 0);
    assert_eq!(snap.status, SyncStatus::Off);
}

/// A sender that stops stamping for a few seconds: the feed keeps its place
/// while there is nothing to measure, and the loop resumes when timecodes
/// return, without engaging again.
#[test]
fn timecodes_that_drop_out_and_return_do_not_move_the_feed() {
    let report = Scenario::new(60)
        .network_latency(300)
        .timecode_sync(3000)
        .timecode_gap(20.0, 6000)
        .run();
    let settled = report.assert_settled(15.0, 60.0);
    report.print("sync through a timecode dropout", &settled);

    report.assert_on_offset(15.0, 60.0, report.frame_ns as i64);
    report.assert_healthy();
    assert_eq!(report.underruns, 0);
}
