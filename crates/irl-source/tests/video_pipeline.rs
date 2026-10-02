//! Video path: format/colour mapping, the copy counts of `to_sysmem`, plane
//! lending in `output_frame`, the swscale fallback, the receiver-side intake
//! gate and the pacing loop.
//!
//! No OBS is running under `cargo test`, so every frame goes to a recording
//! [`VideoSink`] instead of the source; the `SourceHandle` inside `Shared` is
//! a dangling pointer that nothing here dereferences.
//!
//! The plugin's modules are public (`pub mod shared/receiver/video`), so the
//! test drives the real types rather than a re-compilation.

#![allow(dead_code)]

mod common;

use obs_irl_source::{shared, video};

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use ffmpeg::AVPixelFormat as Pix;
use ffmpeg::sys::{AVColorRange, AVColorSpace, AVColorTransferCharacteristic};
use obs::{ColorRange, ColorSpace, VideoFormat};
use parking_lot::Mutex;

use shared::Shared;
use video::VideoSink;
use video::output;
use video::thread::VideoThread;

/// A 60fps canvas tick, what `thread_with` reports.
const TICK: u64 = 16_666_667;
/// One 30fps frame interval.
const FRAME: u64 = 33_333_333;

/* ── Harness ──────────────────────────────────────────────── */

/// One `obs_source_output_video` call, flattened.
#[derive(Debug, Clone)]
struct Emitted {
    width: u32,
    height: u32,
    format: obs::sys::video_format,
    timestamp: u64,
    full_range: bool,
    /// `(address, linesize)` per non-null plane, in plane order.
    planes: Vec<(usize, u32)>,
}

#[derive(Default)]
struct Recorder {
    frames: Mutex<Vec<Emitted>>,
    cleared: AtomicUsize,
}

impl Recorder {
    fn emitted(&self) -> Vec<Emitted> {
        self.frames.lock().clone()
    }

    fn only(&self) -> Emitted {
        let frames = self.emitted();
        assert_eq!(frames.len(), 1, "expected exactly one emitted frame");
        frames[0].clone()
    }
}

/// Local newtype: `VideoSink` and `Arc` are both foreign to this test crate.
struct RecorderSink(Arc<Recorder>);

impl VideoSink for RecorderSink {
    fn output_video(&self, frame: &obs::VideoFrame<'_>) {
        let raw = frame.as_sys();
        let planes = (0..8)
            .filter(|&i| !raw.data[i].is_null())
            .map(|i| (raw.data[i] as usize, raw.linesize[i]))
            .collect();
        self.0.frames.lock().push(Emitted {
            width: raw.width,
            height: raw.height,
            format: raw.format,
            timestamp: raw.timestamp,
            full_range: raw.full_range,
            planes,
        });
    }

    fn output_video_none(&self) {
        self.0.cleared.fetch_add(1, Relaxed);
    }
}

fn shared() -> Arc<Shared> {
    common::shared(
        common::stream_config(false),
        common::hot_values(irl_core::consts::DEFAULT_BUFFER_TARGET_MS as i32),
    )
}

/// The OBS clock the video thread reads, stepped by the test.
#[derive(Clone)]
struct Clock(Arc<AtomicU64>);

impl Clock {
    fn now(&self) -> u64 {
        self.0.load(Relaxed)
    }

    fn set(&self, now_ns: u64) {
        self.0.store(now_ns, Relaxed);
    }
}

fn thread_with(shared: Arc<Shared>) -> (VideoThread, Arc<Recorder>, Clock) {
    let recorder = Arc::new(Recorder::default());
    // Far enough from zero that the tests can stamp arrivals seconds in the
    // past.
    let clock = Clock(Arc::new(AtomicU64::new(100_000_000_000)));
    let reads = clock.clone();
    // No libobs is running, so the real canvas tick would fault; 60fps is
    // what a default OBS install reports.
    let thread = VideoThread::with_sink(shared, Box::new(RecorderSink(recorder.clone())))
        .with_canvas_tick(Box::new(|| Some(16_666_667)))
        .with_clock(Box::new(move || reads.now()));
    (thread, recorder, clock)
}

/// One pass of the video thread's loop with the clock at `now_ns`.
fn run_at(thread: &mut VideoThread, clock: &Clock, now_ns: u64) -> Duration {
    clock.set(now_ns);
    thread.run_once()
}

/// A software frame with real buffers.
fn sw_frame(fmt: Pix, width: i32, height: i32) -> ffmpeg::Frame {
    ffmpeg::Frame::alloc_video(fmt, width, height).expect("alloc")
}

/// Hand the pacing loop a small I420 frame stamped `pts`, decoded from a
/// packet that reached the video thread at `received_ns`.
fn feed(thread: &mut VideoThread, pts: i64, received_ns: u64) {
    let mut frame = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    frame.set_pts(pts);
    thread.pace_decoded(frame, received_ns);
}

/// `AV_FRAME_FLAG_KEY`, which the safe API deliberately has no setter for
/// (only decoders set it).
fn mark_keyframe(frame: &mut ffmpeg::Frame) {
    // SAFETY: `frame` owns a live AVFrame for the duration of the borrow.
    unsafe {
        (*frame.as_mut_ptr()).flags |= ffmpeg::sys::AV_FRAME_FLAG_KEY;
    }
}

/* ── Format and colour mapping ────────────────────────────── */

#[test]
fn pixel_formats_map_to_the_obs_table() {
    let table = [
        (Pix::AV_PIX_FMT_YUV420P, VideoFormat::I420),
        (Pix::AV_PIX_FMT_YUVJ420P, VideoFormat::I420),
        (Pix::AV_PIX_FMT_YUV420P10LE, VideoFormat::I010),
        (Pix::AV_PIX_FMT_NV12, VideoFormat::Nv12),
        (Pix::AV_PIX_FMT_P010LE, VideoFormat::P010),
        (Pix::AV_PIX_FMT_YUV422P, VideoFormat::I422),
        (Pix::AV_PIX_FMT_YUVJ422P, VideoFormat::I422),
        (Pix::AV_PIX_FMT_YUV444P, VideoFormat::I444),
        (Pix::AV_PIX_FMT_YUVJ444P, VideoFormat::I444),
        (Pix::AV_PIX_FMT_UYVY422, VideoFormat::Uyvy),
        (Pix::AV_PIX_FMT_YUYV422, VideoFormat::Yuy2),
        (Pix::AV_PIX_FMT_RGBA, VideoFormat::Rgba),
        (Pix::AV_PIX_FMT_BGRA, VideoFormat::Bgra),
    ];
    for (av, obs) in table {
        assert_eq!(output::avpixfmt_to_obs(av), obs, "{av:?}");
    }

    // Everything else has to go through swscale.
    for unmapped in [
        Pix::AV_PIX_FMT_YUV444P10LE,
        Pix::AV_PIX_FMT_YUV420P12LE,
        Pix::AV_PIX_FMT_GRAY8,
        Pix::AV_PIX_FMT_D3D11,
        Pix::AV_PIX_FMT_VAAPI,
        Pix::AV_PIX_FMT_CUDA,
        Pix::AV_PIX_FMT_NONE,
    ] {
        assert_eq!(
            output::avpixfmt_to_obs(unmapped),
            VideoFormat::None,
            "{unmapped:?}"
        );
    }
}

#[test]
fn colour_spaces_map_the_way_the_c_did() {
    let sdr = AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    let hlg = AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67;
    let pq = AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;

    let table = [
        (AVColorSpace::AVCOL_SPC_BT709, sdr, ColorSpace::Bt709),
        (AVColorSpace::AVCOL_SPC_SMPTE170M, sdr, ColorSpace::Bt601),
        (AVColorSpace::AVCOL_SPC_BT470BG, sdr, ColorSpace::Bt601),
        // BT.2020 splits on the transfer function, not the primaries.
        (AVColorSpace::AVCOL_SPC_BT2020_NCL, hlg, ColorSpace::Hlg2100),
        (AVColorSpace::AVCOL_SPC_BT2020_CL, hlg, ColorSpace::Hlg2100),
        (AVColorSpace::AVCOL_SPC_BT2020_NCL, pq, ColorSpace::Pq2100),
        // Unspecified and everything unknown fall back to BT.709.
        (AVColorSpace::AVCOL_SPC_UNSPECIFIED, sdr, ColorSpace::Bt709),
        (AVColorSpace::AVCOL_SPC_FCC, sdr, ColorSpace::Bt709),
    ];
    for (space, trc, obs) in table {
        assert_eq!(
            output::convert_color_space(space, trc),
            obs,
            "{space:?} / {trc:?}"
        );
    }

    for (range, obs) in [
        (AVColorRange::AVCOL_RANGE_JPEG, ColorRange::Full),
        (AVColorRange::AVCOL_RANGE_MPEG, ColorRange::Partial),
        (AVColorRange::AVCOL_RANGE_UNSPECIFIED, ColorRange::Partial),
    ] {
        assert_eq!(output::convert_color_range(range), obs, "{range:?}");
    }
}

/* ── to_sysmem ────────────────────────────────────────────── */

#[test]
fn sysmem_frames_are_referenced_not_copied() {
    let (mut thread, _recorder, _) = thread_with(shared());
    let frame = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);

    let out = thread.to_sysmem(&frame).expect("system-memory passthrough");

    for plane in 0..3 {
        assert_eq!(
            out.plane(plane).unwrap().as_ptr(),
            frame.plane(plane).unwrap().as_ptr(),
            "plane {plane} shares the decoder's buffer"
        );
        assert_eq!(out.plane_linesize(plane), frame.plane_linesize(plane));
    }
    assert_eq!(out.width(), 64);
    assert_eq!(out.height(), 32);
}

/* ── output_frame ─────────────────────────────────────────── */

#[test]
fn a_mappable_frame_lends_its_planes_to_libobs() {
    let (mut thread, recorder, _) = thread_with(shared());
    let frame = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);

    thread.output_frame(&frame, 1_234_567_890);

    let emitted = recorder.only();
    assert_eq!(emitted.format, VideoFormat::I420.to_sys());
    assert_eq!((emitted.width, emitted.height), (64, 32));
    assert_eq!(emitted.timestamp, 1_234_567_890);
    assert_eq!(emitted.planes.len(), 3, "Y, U and V");
    for plane in 0..3 {
        let (address, linesize) = emitted.planes[plane];
        assert_eq!(
            address,
            frame.plane(plane).unwrap().as_ptr() as usize,
            "plane {plane} is lent, not copied"
        );
        assert_eq!(linesize, frame.plane_linesize(plane) as u32);
    }
}

#[test]
fn an_unmappable_frame_arrives_as_nv12_from_the_scaler() {
    let (mut thread, recorder, _) = thread_with(shared());
    // 10-bit 4:4:4 is not in the OBS table, so it has to be converted.
    let frame = sw_frame(Pix::AV_PIX_FMT_YUV444P10LE, 64, 32);
    assert_eq!(output::avpixfmt_to_obs(frame.pix_fmt()), VideoFormat::None);

    thread.output_frame(&frame, 42);

    let emitted = recorder.only();
    assert_eq!(emitted.format, VideoFormat::Nv12.to_sys());
    assert_eq!((emitted.width, emitted.height), (64, 32));
    assert_eq!(emitted.timestamp, 42);
    assert_eq!(emitted.planes.len(), 2, "Y and interleaved UV");
    assert_eq!(emitted.planes[0].1, 64, "stride is the display width");
    assert_eq!(emitted.planes[1].1, 64);
    assert_eq!(
        emitted.planes[1].0 - emitted.planes[0].0,
        64 * 32,
        "UV follows Y inside the one scratch buffer"
    );
    for (address, _) in &emitted.planes {
        assert!(
            (0..3).all(|p| *address != frame.plane(p).unwrap().as_ptr() as usize),
            "the converted frame is not the source frame"
        );
    }

    // The scratch is reused, not reallocated, for the next frame of the same
    // geometry.
    let again = sw_frame(Pix::AV_PIX_FMT_YUV444P10LE, 64, 32);
    thread.output_frame(&again, 43);
    let frames = recorder.emitted();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].planes[0].0, frames[0].planes[0].0);
}

/* ── Intake (receiver thread) ─────────────────────────────── */

/// 90 kHz, the usual MPEG-TS/RTMP video time base.
const TB_90K: ffmpeg::Rational = ffmpeg::Rational::new(1, 90_000);
const H264: ffmpeg::AVCodecID = ffmpeg::AVCodecID::AV_CODEC_ID_H264;
const HEVC: ffmpeg::AVCodecID = ffmpeg::AVCodecID::AV_CODEC_ID_HEVC;

fn video_frame(pts: i64, key: bool) -> ffmpeg::Frame {
    let mut frame = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    frame.set_pts(pts);
    if key {
        mark_keyframe(&mut frame);
    }
    frame
}

/// Run the intake the way the video thread does: one decoded frame in, the
/// frame to pace out (or `None` when it is gated or held).
fn intake_frame(
    shared: &Shared,
    state: &mut video::DecodeState,
    frame: &ffmpeg::Frame,
    codec_id: ffmpeg::AVCodecID,
) -> Option<ffmpeg::Frame> {
    video::intake::handle_frame(shared, state, frame, TB_90K, codec_id)
}

#[test]
fn the_keyframe_gate_drops_until_the_first_key_frame() {
    let shared = shared();
    assert!(shared.hot.wait_for_keyframe.load(Relaxed));
    let mut state = video::DecodeState::default();

    for pts in [0, 3_000, 6_000] {
        let frame = video_frame(pts, false);
        assert!(
            intake_frame(&shared, &mut state, &frame, H264).is_none(),
            "nothing paced before the keyframe"
        );
    }
    assert_eq!(shared.conn.total_video_frames.load(Relaxed), 0);
    assert!(!shared.video_flags.first_keyframe.load(Relaxed));

    // 9000 ticks at 90 kHz is 100 ms.
    let frame = video_frame(9_000, true);
    let paced = intake_frame(&shared, &mut state, &frame, H264).expect("keyframe paced");

    assert!(shared.video_flags.first_keyframe.load(Relaxed));
    assert_eq!(shared.conn.total_video_frames.load(Relaxed), 1);
    assert_eq!((state.last_width, state.last_height), (64, 32));
    assert_eq!(paced.pts(), 100_000_000, "PTS rescaled to nanoseconds");
    // The paced frame holds its own reference; the decoder's is untouched.
    assert_eq!(frame.pts(), 9_000);
}

/// The packet queue is where the stream's latency lives, so its bounds are
/// what stop a sender from making the plugin allocate without limit. It is
/// bounded by media duration and by bytes, not by a frame count, because a
/// packet's size and its duration are unrelated.
#[test]
fn the_packet_queue_is_bounded_by_duration_and_bytes() {
    let shared = shared();
    let ms = |n: i64| n * 1_000_000;

    // Well past the duration ceiling: the oldest packets go.
    for i in 0..40 {
        common::push_packet(&shared, ms(i * 500), 1024);
    }
    assert!(
        shared.video.span_ns() <= ms(irl_core::consts::VIDEO_PACKET_QUEUE_MAX_MS),
        "span {}ms over the ceiling",
        shared.video.span_ns() / 1_000_000
    );
    assert!(shared.lifetime.video_queue_drops.load(Relaxed) > 0);
}

/// And by bytes, for a sender whose timestamps claim the queue is shallow: a
/// duration bound alone would let it allocate without limit.
#[test]
fn the_packet_queue_is_bounded_by_bytes_whatever_the_timestamps_say() {
    let big = shared();
    for _ in 0..40 {
        common::push_packet(&big, 0, 4 * 1024 * 1024);
    }
    assert!(
        big.video.bytes() <= irl_core::consts::VIDEO_PACKET_QUEUE_MAX_BYTES,
        "{} bytes queued",
        big.video.bytes()
    );
}

/// Packets are decoded by whichever decoder was installed before them, which
/// is what makes a reconnect mid-queue safe.
#[test]
fn the_channel_delivers_packets_after_the_decoder_that_owns_them() {
    let shared = shared();
    common::push_packet(&shared, 0, 1);
    assert_eq!(shared.video.len(), 1);

    assert!(matches!(
        shared.video.pop(),
        Some(shared::VideoMsg::Packet(_))
    ));
    assert_eq!(shared.video.len(), 0);
    assert!(shared.video.pop().is_none());
}

#[test]
fn hevc_frames_from_a_missing_reference_are_held_back() {
    let shared = shared();
    let mut state = video::DecodeState::default();

    let key = video_frame(0, true);
    assert!(intake_frame(&shared, &mut state, &key, HEVC).is_some());

    let mut corrupt = video_frame(3_000, false);
    unsafe {
        (*corrupt.as_mut_ptr()).flags |= ffmpeg::sys::AV_FRAME_FLAG_CORRUPT;
    }
    assert!(
        intake_frame(&shared, &mut state, &corrupt, HEVC).is_none(),
        "the damaged frame is not paced"
    );
    assert_eq!(shared.conn.video_corrupt_held.load(Relaxed), 1);
    assert!(state.hold_logged);

    // H.264 damage passes through instead, to preserve cadence.
    let mut damaged = video_frame(6_000, false);
    unsafe {
        (*damaged.as_mut_ptr()).flags |= ffmpeg::sys::AV_FRAME_FLAG_CORRUPT;
    }
    assert!(intake_frame(&shared, &mut state, &damaged, H264).is_some());
}

#[test]
fn a_resolution_change_re_anchors_the_video_clock() {
    let shared = shared();
    let mut state = video::DecodeState::default();

    let first = video_frame(0, true);
    assert!(intake_frame(&shared, &mut state, &first, H264).is_some());
    shared.conn.video_ts_init.store(true, Relaxed);

    let mut second = sw_frame(Pix::AV_PIX_FMT_YUV420P, 128, 64);
    second.set_pts(3_000);
    mark_keyframe(&mut second);
    assert!(intake_frame(&shared, &mut state, &second, H264).is_some());

    assert!(
        !shared.conn.video_ts_init.load(Relaxed),
        "the fallback clock re-anchors on a resolution change"
    );
    assert_eq!((state.last_width, state.last_height), (128, 64));
    assert_eq!(shared.conn.last_video_width.load(Relaxed), 128);
}

/* ── The pacing loop ──────────────────────────────────────── */

/// Frames go to libobs a couple of canvas ticks *before* they are due.
///
/// libobs is a scheduler too: `ready_async_frame()` advances its play head by
/// wall-clock deltas and takes the frame it has just passed, so a frame already
/// queued lands on a deterministic tick while one handed over at its due time
/// slips to the next — sometimes, depending on this thread's wakeup jitter. At
/// 30fps on a 60fps canvas that is every frame holding two ticks versus frames
/// alternating between one and three.
#[test]
fn a_frame_is_handed_over_a_lead_before_it_is_due() {
    let shared = shared();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    // Anchor the play head first: the very first frame out deliberately gets
    // no lead, so a second frame is needed to see one. The video-only fallback
    // schedules it at its arrival, so it is delayed by a tick first (see
    // `video_without_audio_is_delayed_by_a_tick_and_anchors_on_the_fallback`).
    let t0 = clock.now();
    feed(&mut thread, t0 as i64, t0);
    let now = t0 + TICK;
    run_at(&mut thread, &clock, now);
    assert_eq!(recorder.emitted().len(), 1, "anchor frame emitted");

    // A frame due 25ms out: inside two 60fps ticks (33.3ms) of the lead, so
    // it should go now even though its due time has not arrived. (Its due
    // time is its PTS on the fallback epoch plus the delay: `t0 + 25 ms +
    // 1 tick`, 25 ms from `now`.)
    feed(&mut thread, t0 as i64 + 25_000_000, t0);
    run_at(&mut thread, &clock, now);
    assert_eq!(
        recorder.emitted().len(),
        2,
        "a frame inside the delivery lead should have been handed over"
    );

    // One due far out still waits.
    feed(&mut thread, t0 as i64 + 500_000_000, now);
    let wait = run_at(&mut thread, &clock, now);
    assert_eq!(recorder.emitted().len(), 2, "not due, not delivered");
    assert!(!wait.is_zero(), "should sleep toward the lead");
}

/// A queued packet is only work if there is room for what it decodes.
///
/// Once the channel carries the whole configured latency as packets, the
/// pacing queue sitting at its decode lead with the head not yet due is the
/// *normal* steady state at any Target Buffer above that lead. Treating a
/// queued message as proof of work makes the video thread skip its sleep and
/// spin at full CPU for the entire connection — and with the decoder stalled
/// behind a queue it cannot drain.
#[test]
fn a_full_pacing_queue_does_not_keep_the_video_thread_awake() {
    let shared = shared();
    let (mut thread, _recorder, clock) = thread_with(shared.clone());

    // A backlog of packets, as any target above the decode lead produces.
    for i in 0..200 {
        common::push_packet(&shared, i * 33_333_333, 2048);
    }
    assert!(!shared.video.is_empty());

    // Fill the pacing queue past its decode lead with frames due far ahead.
    let now = clock.now();
    let mut pts = 0i64;
    while thread.pacing().has_room() {
        feed(&mut thread, now as i64 + 60_000_000_000 + pts, now);
        pts += 33_333_333;
    }
    assert!(!thread.pacing().has_room(), "queue should be at its lead");

    // Packets queued, no room, nothing due: there is nothing to do, so the
    // thread must sleep rather than spin.
    assert!(
        !shared.video.has_work(thread.pacing().has_room()),
        "the thread would spin: packets queued but no room to decode them"
    );

    // Room again means work again.
    assert!(shared.video.has_work(true));

    // A clear always wakes it, whatever the queue looks like.
    shared.video.request_clear();
    assert!(shared.video.has_work(false));
}

/// A decoder handover is not gated on pacing room: it produces nothing by
/// itself, and leaving it behind a full queue would strand a reconnect until
/// the previous connection's frames drained.
#[test]
fn a_decoder_handover_is_taken_even_with_no_pacing_room() {
    let shared = shared();
    common::push_packet(&shared, 0, 1);
    assert!(!shared.video.next_is_decoder(), "a packet is at the front");

    shared.video.request_clear();
    common::push_packet(&shared, 0, 1);
    assert!(!shared.video.next_is_decoder());
}

#[test]
fn a_queued_frame_is_transferred_paced_and_emitted() {
    let shared = shared();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let mut queued = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    // The receiver hands the queue nanosecond PTS.
    queued.set_pts(5_000_000_000);
    let source_plane = queued.plane(0).unwrap().as_ptr() as usize;
    let now = clock.now();
    thread.pace_decoded(queued, now);

    // With no audio mapping the video-only anchor puts the first frame at its
    // arrival, and a tick's allowance is added as the standing delay: one
    // cycle at that due time takes it all the way out.
    let wait = run_at(&mut thread, &clock, now + TICK);

    let emitted = recorder.only();
    assert_eq!(emitted.planes[0].0, source_plane, "still zero-copy");
    assert_eq!(thread.pacing().len(), 0);
    assert!(
        !wait.is_zero(),
        "nothing left to pace: sleep the full slice"
    );
    assert_eq!(shared.lifetime.pacing_peak.load(Relaxed), 1);
    assert_eq!(shared.lifetime.pacing_now.load(Relaxed), 0);
    assert_eq!(thread.pacing().overflows(), 0);
}

#[test]
fn a_future_frame_waits_instead_of_being_emitted() {
    let shared = shared();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let now = clock.now();
    let mut queued = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    // 200 ms into the future on the video-only anchor: the fallback anchors on
    // the first frame, so pace the *second* one forward.
    queued.set_pts(0);
    thread.pace_decoded(queued, now);
    let now = now + TICK;
    run_at(&mut thread, &clock, now);
    assert_eq!(recorder.emitted().len(), 1, "the anchor frame goes out");

    feed(&mut thread, 200_000_000, now);
    let wait = run_at(&mut thread, &clock, now);

    assert_eq!(recorder.emitted().len(), 1, "not due yet");
    assert_eq!(thread.pacing().len(), 1);
    assert_eq!(
        wait.as_millis() as u64,
        irl_core::consts::VIDEO_PACING_MAX_WAIT_MS,
        "sleeps until due, capped at the pacing slice"
    );
    assert_eq!(shared.lifetime.pacing_now.load(Relaxed), 1);
}

#[test]
fn a_clear_request_drops_the_queue_and_blanks_the_source() {
    let shared = shared();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let now = clock.now();
    feed(&mut thread, 0, now);
    let now = now + TICK;
    run_at(&mut thread, &clock, now);
    assert_eq!(recorder.emitted().len(), 1);

    feed(&mut thread, 10_000_000_000, now);
    run_at(&mut thread, &clock, now);
    assert_eq!(thread.pacing().len(), 1, "parked until its due time");

    shared.video.request_clear();
    let wait = run_at(&mut thread, &clock, now);

    assert_eq!(recorder.cleared.load(Relaxed), 1);
    assert!(
        wait.is_zero(),
        "the clear cycle goes round again immediately"
    );
    assert_eq!(thread.pacing().len(), 0, "paced frames go with the clear");
    assert_eq!(shared.video.len(), 0);
    assert_eq!(
        recorder.emitted().len(),
        1,
        "nothing repainted after a clear"
    );
}

#[test]
fn queued_frames_reschedule_onto_the_audio_playout_offset() {
    let shared = shared();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    // Audio that ends at stream PTS 10 s plays out at OBS time `now + 1 s`:
    // everything maps 1 s into the future minus 10 s of stream time.
    let now = clock.now();
    {
        let mut state = shared.audio_state();
        state.mapping.publish(now + 1_000_000_000, 10_000_000_000);
    }

    feed(&mut thread, 10_000_000_000, now);
    run_at(&mut thread, &clock, now);

    assert!(recorder.emitted().is_empty(), "due a second from now");
    let due = thread.pacing().next_due().expect("paced");
    assert_eq!(due, now + 1_000_000_000);

    // The audio side reclaims half of that latency; the queued frame must move
    // with it rather than trailing by the depth of the queue.
    {
        let mut state = shared.audio_state();
        state.mapping.publish(now + 500_000_000, 10_000_000_000);
    }
    run_at(&mut thread, &clock, now);
    assert_eq!(thread.pacing().next_due(), Some(now + 500_000_000));

    // Once the offset says "now", the frame has no margin left at all: a
    // tick's allowance is added as the standing delay, and it goes out at the
    // end of it.
    {
        let mut state = shared.audio_state();
        state.mapping.publish(now, 10_000_000_000);
    }
    run_at(&mut thread, &clock, now);
    assert!(recorder.emitted().is_empty());
    assert_eq!(thread.pacing().next_due(), Some(now + TICK));
    run_at(&mut thread, &clock, now + TICK);
    assert_eq!(recorder.only().timestamp, now + TICK);
}

#[test]
fn clearing_the_ts_init_mirror_re_anchors_the_fallback_clock() {
    let shared = shared();
    let (mut thread, _recorder, _) = thread_with(shared.clone());

    // No audio mapping, so the video-only anchor runs: the first frame anchors
    // the epoch at `now`.
    let mut first = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    first.set_pts(0);
    thread.due_time(&first);
    assert!(shared.conn.video_ts_init.load(Relaxed));

    // Ten seconds further on the same epoch is way past the +500 ms drift
    // window, so it caps at now + 200 ms.
    let mut second = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    second.set_pts(10_000_000_000);
    let capped = thread.due_time(&second);

    // A reconnect (or a resolution change) clears the mirror; the private
    // anchors must not survive it.
    shared.conn.video_ts_init.store(false, Relaxed);
    let re_anchored = thread.due_time(&second);

    assert!(
        re_anchored < capped,
        "re-anchored {re_anchored} should sit at ~now, before the capped {capped}"
    );
    assert!(shared.conn.video_ts_init.load(Relaxed));
}

#[test]
fn a_frame_is_due_where_the_audio_mapping_puts_it() {
    let shared = shared();
    let (mut thread, _recorder, clock) = thread_with(shared.clone());

    let now = clock.now();
    {
        let mut state = shared.audio_state();
        // A frame at stream PTS 0 maps two seconds into the future.
        state.mapping.publish(now + 2_000_000_000, 1);
    }

    let mut frame = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    frame.set_pts(0);
    let due = thread.due_time(&frame);

    assert!(
        due >= now + 1_900_000_000,
        "due {}ms ahead",
        (due as i64 - now as i64) / 1_000_000
    );
}

#[test]
fn a_disabled_keyframe_gate_passes_non_key_frames() {
    let shared = shared();
    shared.hot.wait_for_keyframe.store(false, Relaxed);
    let mut state = video::DecodeState::default();

    let frame = video_frame(0, false);
    assert!(
        intake_frame(&shared, &mut state, &frame, H264).is_some(),
        "non-key frame paced with the gate off"
    );
    assert!(
        !shared.video_flags.first_keyframe.load(Relaxed),
        "first-keyframe bookkeeping still waits for a real key frame"
    );

    let frame = video_frame(9_000, true);
    assert!(intake_frame(&shared, &mut state, &frame, H264).is_some());
    assert!(shared.video_flags.first_keyframe.load(Relaxed));
}

/* ── Anchoring libobs's play head to the audio playout ────── */

/// Audio publishes where video belongs: its chunk ending at stream PTS
/// `buffered_end_pts_ns` plays out at OBS time `obs_end_ts_ns`.
fn publish_mapping(shared: &Shared, obs_end_ts_ns: u64, buffered_end_pts_ns: i64) {
    let mut state = shared.audio_state();
    state.mapping.publish(obs_end_ts_ns, buffered_end_pts_ns);
}

/// Anchor the play head on a frame at stream PTS 10 s that arrives at the
/// returned OBS time and maps 100 ms out: a comfortable margin.
fn anchor_with_margin(shared: &Shared, thread: &mut VideoThread, clock: &Clock) -> u64 {
    let t0 = clock.now();
    publish_mapping(shared, t0 + 100_000_000, 10_000_000_000);
    feed(thread, 10_000_000_000, t0);
    run_at(thread, clock, t0 + 100_000_000);
    t0
}

fn shared_with_audio() -> Arc<Shared> {
    let shared = shared();
    shared.flags.audio_present.store(true, Relaxed);
    // By the time the first keyframe decodes the audio warm-up has drained, so
    // the fallback would put that frame just one Target Buffer out — ~100 ms
    // before the mapping audio is about to publish will want it.
    shared.audio_state().startup_warmup_remaining_ms = 0;
    shared
}

/// libobs anchors its play head to the *arrival* of the first frame and never
/// moves it, so a frame handed over on the video-only fallback fixes the
/// connection's lip sync at whatever the fallback got wrong. With an audio
/// stream present, nothing goes out until audio has said where video belongs.
#[test]
fn video_waits_for_the_audio_mapping_before_anchoring_the_play_head() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let now = clock.now();
    feed(&mut thread, 10_000_000_000, now);

    // Well past the fallback due time (+120 ms): still held.
    let wait = run_at(&mut thread, &clock, now + 300_000_000);
    assert!(
        recorder.emitted().is_empty(),
        "no mapping yet, nothing may anchor"
    );
    assert!(!wait.is_zero(), "holding must not spin the thread");

    // Audio primes: the chunk ending at stream PTS 10 s plays at +400 ms.
    publish_mapping(&shared, now + 400_000_000, 10_000_000_000);
    run_at(&mut thread, &clock, now + 350_000_000);
    assert!(recorder.emitted().is_empty(), "due at +400 ms, not before");
    run_at(&mut thread, &clock, now + 400_000_000);
    assert_eq!(recorder.only().timestamp, now + 400_000_000);
}

/// The mapping can also land frames in the past — the ones whose audio the
/// warm-up discarded. Handing those over would anchor the play head late by
/// however stale the first one was; they are dropped instead, and the first
/// frame that is on time anchors. Their arrival margins are the same as the
/// live frames' (60 ms here, more than a lead), so the backlog raises no
/// video delay either.
#[test]
fn frames_already_past_due_when_the_mapping_arrives_do_not_anchor_the_play_head() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    // 25 fps: 40 ms apart, so no two frames fall inside one 60fps tick,
    // arriving in real time over the 160 ms before the mapping.
    let now = clock.now();
    for i in 0..4 {
        feed(
            &mut thread,
            10_000_000_000 + i * 40_000_000,
            now - 160_000_000 + i as u64 * 40_000_000,
        );
    }
    run_at(&mut thread, &clock, now);
    assert!(recorder.emitted().is_empty());
    assert_eq!(thread.pacing().len(), 4);

    // Audio primes so that the first three frames map 100, 60 and 20 ms into
    // the past — all past a canvas tick — and the fourth lands 20 ms out.
    publish_mapping(&shared, now - 100_000_000, 10_000_000_000);
    run_at(&mut thread, &clock, now);
    assert!(
        recorder.emitted().is_empty(),
        "the stale frames must not go out in place of the on-time one"
    );
    assert_eq!(thread.pacing().len(), 1, "three stale frames dropped");
    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), 0);

    run_at(&mut thread, &clock, now + 20_000_000);
    assert_eq!(recorder.only().timestamp, now + 20_000_000);
}

/// A frame late by less than a canvas tick still anchors: libobs quantises
/// display to its ticks, and a coarse timer can oversleep by most of one.
#[test]
fn a_frame_late_by_under_a_canvas_tick_still_anchors() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let now = clock.now();
    feed(&mut thread, 10_000_000_000, now - 50_000_000);
    publish_mapping(&shared, now, 10_000_000_000);

    run_at(&mut thread, &clock, now + 10_000_000);
    assert_eq!(
        recorder.only().timestamp,
        now,
        "10 ms late is within a 60fps tick"
    );
}

/// A stream that advertises audio but never primes it cannot hold video
/// forever: past the expected prime the fallback anchors.
#[test]
fn video_stops_waiting_for_audio_that_never_primes() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let now = clock.now();
    feed(&mut thread, 10_000_000_000, now);
    run_at(&mut thread, &clock, now);
    assert!(recorder.emitted().is_empty());

    // warm-up 150 + target 120 + lead 80 + the audio hold's wait for a skew
    // reading 2000 + margin 1000.
    let give_up = now + 3_350_000_000;
    run_at(&mut thread, &clock, give_up - 1_000_000);
    assert!(recorder.emitted().is_empty(), "still inside the wait");

    // Past it: the held frame is stale by more than a tick and is dropped,
    // and a fresh frame goes out on the fallback at its own due time.
    run_at(&mut thread, &clock, give_up + 1_000_000);
    assert!(recorder.emitted().is_empty());
    assert_eq!(
        thread.pacing().len(),
        0,
        "the stale frame was dropped, not anchored"
    );

    feed(&mut thread, 10_000_000_000 + 1_400_000_000, clock.now());
    let due = thread.pacing().next_due().expect("paced on the fallback");
    run_at(&mut thread, &clock, due);
    assert_eq!(recorder.only().timestamp, due);
}

/// Without an audio stream there is nothing to wait for and nothing to be in
/// sync with: the video-only fallback anchors as soon as the first frame can
/// be on time. The fallback schedules that frame at its arrival, which leaves
/// no time to decode it, so a canvas tick is added as the standing video delay
/// and the frame goes out one tick after it arrived.
#[test]
fn video_without_audio_is_delayed_by_a_tick_and_anchors_on_the_fallback() {
    let shared = shared();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let t0 = clock.now();
    feed(&mut thread, t0 as i64, t0);
    run_at(&mut thread, &clock, t0);
    assert!(
        recorder.emitted().is_empty(),
        "no audio to wait for, but not decodable in time either"
    );
    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), TICK);
    let due = thread.pacing().next_due().expect("paced");
    assert_eq!(due, t0 + TICK);

    run_at(&mut thread, &clock, due);
    assert_eq!(recorder.only().timestamp, due);
}

/* ── The standing video delay ─────────────────────────────── */

/// Frames of real-time arrival history before video counts as live and may
/// size the delay: the lookback, in whole frames.
const LIVE_FRAMES: u64 = (irl_core::consts::VIDEO_LIVE_LOOKBACK_MS * 1_000_000).div_ceil(FRAME);

/// Give the thread a lookback of live history ending one frame before a
/// frame with `next_pts_ns` arrives at `next_arrival_ns`: the frames a
/// connection that has been delivering in real time would already have
/// handed over. They are queued like any others, and go as stale.
fn live_history(thread: &mut VideoThread, next_arrival_ns: u64, next_pts_ns: i64) {
    for k in (1..=LIVE_FRAMES).rev() {
        feed(
            thread,
            next_pts_ns - (k * FRAME) as i64,
            next_arrival_ns - k * FRAME,
        );
    }
}

/// The mapping can also put *every* frame in the past: a sender whose video
/// leaves the encoder later than the audio of the same instant by more than
/// Target Buffer covers. Dropping stale frames until an on-time one turns up
/// would drop such a stream forever, and anchoring on a late frame would play
/// the whole connection unpaced — each frame handed over on arrival, and
/// dropped by libobs whenever two arrive inside one canvas tick, which is the
/// "low fps" such a stream shows. The shortfall is measured instead and added
/// to the schedule as a standing delay, sized so that frames are in hand a
/// tick before they are due again, and the picture is on time from the first
/// frame. The canvas rate sets the tick and therefore the rounding, and at
/// 30fps also caps the delivery lead, so both common rates are swept.
#[test]
fn video_that_trails_its_audio_is_delayed_into_pacing_not_dropped() {
    for canvas_fps in [30u64, 60] {
        let tick = 1_000_000_000 / canvas_fps;
        let shared = shared_with_audio();
        let (thread, recorder, clock) = thread_with(shared.clone());
        let mut thread = thread.with_canvas_tick(Box::new(move || Some(tick)));

        // Audio has primed such that a video frame arriving now maps 150 ms
        // into the past, and every later frame likewise.
        let t0 = clock.now();
        let late = 150_000_000;
        publish_mapping(&shared, t0 - late, 10_000_000_000);

        for i in 0..30u64 {
            let arrival = t0 + i * FRAME;
            feed(&mut thread, 10_000_000_000 + (i * FRAME) as i64, arrival);
            run_at(&mut thread, &clock, arrival);
        }

        // 150 ms of lateness plus a tick of decode allowance, in whole ticks:
        // 10 of 16.7 ms at 60fps, 6 of 33.3 ms at 30fps.
        let delay = (late + tick).div_ceil(tick) * tick;
        assert_eq!(
            shared.conn.video_delay_ns.load(Relaxed),
            delay,
            "canvas {canvas_fps}fps"
        );
        // The first lookback of frames went as stale while the thread waited
        // to see that this video is live, not catching up; every frame from
        // the one that sized the delay is shown.
        let emitted = recorder.emitted();
        assert_eq!(
            emitted.len() as u64,
            30 - LIVE_FRAMES,
            "canvas {canvas_fps}fps: every frame shown once live"
        );
        for (i, frame) in emitted.iter().enumerate() {
            let arrival = t0 + (i as u64 + LIVE_FRAMES) * FRAME;
            assert_eq!(
                frame.timestamp,
                arrival - late + delay,
                "canvas {canvas_fps}fps, frame {i}"
            );
        }
        assert_eq!(thread.pacing().len(), 0);
    }
}

/// The probe buffers about a second of the stream and the receiver then hands
/// it over in one burst, every packet stamped with the same arrival time. The
/// older frames in that burst look late by up to the probe span against their
/// mapped due times when they were merely buffered, and the one that happens
/// to be on time has its few milliseconds of margin from buffering too. None
/// of them may size the delay; only the newest frame in hand is a live
/// arrival, and here it is not the one that anchors.
#[test]
fn a_stale_startup_burst_does_not_raise_the_delay() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    // Twenty 30fps frames from the probe, all pushed at `t0`, mapping from
    // 593 ms in the past up to one 40 ms out, with the one before it 6.7 ms
    // out: on time, but only just.
    let t0 = clock.now();
    let last_due = t0 + 40_000_000;
    publish_mapping(&shared, last_due, 10_000_000_000 + 19 * FRAME as i64);
    for i in 0..20i64 {
        feed(&mut thread, 10_000_000_000 + i * FRAME as i64, t0);
    }
    run_at(&mut thread, &clock, t0);

    assert_eq!(thread.pacing().len(), 2, "eighteen stale frames dropped");
    assert_eq!(
        shared.conn.video_delay_ns.load(Relaxed),
        0,
        "buffered frames are not evidence of a late sender"
    );
    run_at(&mut thread, &clock, last_due - FRAME);
    let emitted = recorder.emitted();
    assert_eq!(
        emitted[0].timestamp,
        last_due - FRAME,
        "anchored on the on-time frame"
    );
    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), 0);
}

/// When every frame in hand is late, the newest one is the measurement: its
/// arrival is a live one, and the frames behind it in the queue only say how
/// long the wait for the mapping was.
#[test]
fn a_late_stream_is_measured_on_its_newest_frame() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    // Frames that arrived in real time over the last lookback, each mapping
    // 150 ms before its arrival, the newest one now.
    let t0 = clock.now();
    let late = 150_000_000;
    publish_mapping(&shared, t0 - late, 10_000_000_000);
    live_history(&mut thread, t0, 10_000_000_000);
    feed(&mut thread, 10_000_000_000, t0);
    run_at(&mut thread, &clock, t0);

    // 150 ms plus a tick, in ticks, from the newest frame; the older ones
    // were stale even after that and went.
    let delay = 10 * TICK;
    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), delay);
    assert_eq!(thread.pacing().len(), 1);
    run_at(&mut thread, &clock, t0 - late + delay);
    assert_eq!(recorder.only().timestamp, t0 - late + delay);
}

/// A sender whose skew sits right at the edge of what Target Buffer covers
/// gets some frames in hand before they are due and some after. The anchor
/// frame may well be one of the former, and from then on every frame that
/// arrives past due is handed over late, unpaced: the same dropped-frame
/// judder in libobs. Lateness that recurs across a window raises the delay to
/// cover it. A raise moves the picture, so it happens once, not per frame.
#[test]
fn recurring_lateness_after_the_anchor_raises_the_delay_once() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    // A frame arriving now maps 100 ms out: a comfortable margin to anchor on.
    let t0 = anchor_with_margin(&shared, &mut thread, &clock);
    assert_eq!(recorder.emitted().len(), 1, "anchored with a full margin");
    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), 0);

    // From here every frame arrives 5 ms after it was due, 105 ms later
    // relative to its due time than the anchor frame did.
    let arrival_of = |i: u64| t0 + 105_000_000 + i * FRAME;
    let mut delay_seen = Vec::new();
    for i in 1..=80u64 {
        feed(
            &mut thread,
            10_000_000_000 + (i * FRAME) as i64,
            arrival_of(i),
        );
        run_at(&mut thread, &clock, arrival_of(i));
        delay_seen.push(shared.conn.video_delay_ns.load(Relaxed));
    }

    // The window opened with frame 1 and ran its course a second later, at
    // frame 32: raised then, by one tick, and never again for the same
    // lateness.
    assert_eq!(delay_seen[30], 0, "frame 31: still inside the window");
    assert_eq!(delay_seen[31], TICK, "frame 32: raised");
    assert!(delay_seen[31..].iter().all(|&d| d == TICK), "raised once");

    let emitted = recorder.emitted();
    assert_eq!(emitted.len(), 81, "no frame dropped across the raise");
    for (i, frame) in emitted.iter().enumerate().skip(1) {
        let i = i as u64;
        let due = arrival_of(i) - 5_000_000;
        let expected = if i < 32 { due } else { due + TICK };
        assert_eq!(frame.timestamp, expected, "frame {i}");
    }
}

/// A frame in hand with anything to spare is on time, however little that is:
/// the delivery lead is an allowance for the pacing timer oversleeping, and a
/// frame handed over on arrival never sleeps. Demanding the lead of every
/// frame would delay a healthy stream by the lead for nothing.
#[test]
fn frames_handed_over_short_of_the_lead_but_before_due_do_not_raise_the_delay() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let t0 = anchor_with_margin(&shared, &mut thread, &clock);
    assert_eq!(recorder.emitted().len(), 1);

    // Two seconds of frames with 3 ms in hand: a tenth of the lead.
    let arrival_of = |i: u64| t0 + 97_000_000 + i * FRAME;
    for i in 1..=60u64 {
        feed(
            &mut thread,
            10_000_000_000 + (i * FRAME) as i64,
            arrival_of(i),
        );
        run_at(&mut thread, &clock, arrival_of(i));
    }

    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), 0);
    assert_eq!(recorder.emitted().len(), 61);
}

/// A few late frames in a row are a scheduling hiccup on the host, not a
/// sender skew: they go out late, and the delay stays.
#[test]
fn a_single_late_burst_after_the_anchor_does_not_raise_the_delay() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let t0 = anchor_with_margin(&shared, &mut thread, &clock);
    assert_eq!(recorder.emitted().len(), 1);

    let due_of = |i: u64| t0 + 100_000_000 + i * FRAME;
    // Six frames 20 ms late, inside 200 ms ...
    for i in 1..=6u64 {
        feed(
            &mut thread,
            10_000_000_000 + (i * FRAME) as i64,
            due_of(i) + 20_000_000,
        );
        run_at(&mut thread, &clock, due_of(i) + 20_000_000);
    }
    // ... then a second and a half of frames with their full margin.
    for i in 7..=50u64 {
        feed(
            &mut thread,
            10_000_000_000 + (i * FRAME) as i64,
            due_of(i) - 100_000_000,
        );
        run_at(&mut thread, &clock, due_of(i) - 100_000_000);
    }
    run_at(&mut thread, &clock, due_of(50));

    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), 0);
    assert_eq!(
        recorder.emitted().len(),
        51,
        "late frames go out late, not dropped"
    );
}

/// The delay was sized for one connection's sender. A clear — disconnect,
/// hide, restart — forgets it along with the play head, and the next
/// connection is measured afresh.
#[test]
fn a_clear_forgets_the_video_delay() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    let t0 = clock.now();
    publish_mapping(&shared, t0 - 150_000_000, 10_000_000_000);
    live_history(&mut thread, t0, 10_000_000_000);
    feed(&mut thread, 10_000_000_000, t0);
    run_at(&mut thread, &clock, t0);
    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), 10 * TICK);

    shared.video.request_clear();
    let t1 = t0 + 5_000_000_000;
    run_at(&mut thread, &clock, t1);
    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), 0);

    // The next connection's audio primes with room to spare: no delay.
    publish_mapping(&shared, t1 + 200_000_000, 20_000_000_000);
    feed(&mut thread, 20_000_000_000, t1);
    run_at(&mut thread, &clock, t1 + 200_000_000);
    assert_eq!(
        recorder.emitted().last().map(|f| f.timestamp),
        Some(t1 + 200_000_000)
    );
    assert_eq!(shared.conn.video_delay_ns.load(Relaxed), 0);
}

/// A sender can be later than the delay ceiling covers: a phone whose video
/// pipeline runs a stabiliser has sent video 1.6 s behind its audio (#33).
/// No delay makes such a stream on time, so the stale test cannot be allowed
/// to run its course — it would wait for an on-time frame forever, dropping
/// every frame meanwhile while the audio played on, which is a source with
/// sound and no picture. The delay stops at its ceiling, the newest frame in
/// hand anchors as it is, and the connection plays unpaced from there: every
/// frame handed over on arrival, none dropped here.
#[test]
fn video_later_than_the_delay_ceiling_anchors_and_plays_unpaced() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());
    let max = irl_core::consts::VIDEO_DELAY_MAX_MS * 1_000_000;

    // A frame arriving now maps six seconds into the past, past the ceiling,
    // and every later one too.
    let t0 = clock.now();
    let late = 6_000_000_000;
    publish_mapping(&shared, t0 - late, 10_000_000_000);

    for i in 0..60u64 {
        let arrival = t0 + i * FRAME;
        feed(&mut thread, 10_000_000_000 + (i * FRAME) as i64, arrival);
        run_at(&mut thread, &clock, arrival);
        assert_eq!(
            thread.pacing().len(),
            0,
            "frame {i} neither held nor dropped"
        );
    }

    assert_eq!(
        shared.conn.video_delay_ns.load(Relaxed),
        max,
        "the delay stops at its ceiling"
    );
    // Every frame from the one that showed the video to be live, not
    // catching up, is shown; the lookback before it went as stale.
    let emitted = recorder.emitted();
    assert_eq!(emitted.len() as u64, 60 - LIVE_FRAMES);
    for (i, frame) in emitted.iter().enumerate() {
        let arrival = t0 + (i as u64 + LIVE_FRAMES) * FRAME;
        assert_eq!(
            frame.timestamp,
            arrival - late + max,
            "frame {i}: the schedule carries the ceiling, no more"
        );
    }
}

/// The delay before the anchor is sized from one frame, and that frame can
/// still lie in a way the live check cannot see: a sender that is late for
/// its first seconds and then is not (an encoder warming up, a stabiliser
/// switched off), arriving in real time throughout. A delay that never shrank
/// would hold on-time video that far behind its audio for the whole
/// connection. Once a whole window of frames has needed less, the delay ramps
/// back down at the Catch-Up Speed: video plays slightly fast, nothing jumps
/// and nothing is dropped.
#[test]
fn a_delay_set_by_a_bad_first_frame_ramps_back_without_a_jump() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    // The first frame in hand maps 1.3 s into the past.
    let t0 = clock.now();
    publish_mapping(&shared, t0 - 1_300_000_000, 10_000_000_000);
    live_history(&mut thread, t0, 10_000_000_000);
    feed(&mut thread, 10_000_000_000, t0);
    run_at(&mut thread, &clock, t0);
    let set = shared.conn.video_delay_ns.load(Relaxed);
    assert_eq!(set, (1_300_000_000 + TICK).div_ceil(TICK) * TICK);

    // The stream then settles: every frame from here arrives 100 ms before
    // its undelayed due time, for a minute.
    let frames = 60 * 30u64;
    let pts_of = |i: u64| 10_000_000_000 + 1_400_000_000 + (i * FRAME) as i64;
    let mut delays = Vec::new();
    for i in 1..=frames {
        let arrival = t0 + i * FRAME;
        feed(&mut thread, pts_of(i), arrival);
        run_at(&mut thread, &clock, arrival);
        delays.push(shared.conn.video_delay_ns.load(Relaxed));
    }
    for k in 1..=60u64 {
        run_at(&mut thread, &clock, t0 + (frames + k) * FRAME);
    }

    // Untouched for the ten-second window, then down to what frames need,
    // gradually: 5% takes back 1.3 s in about 26 s. 100 ms in hand covers the
    // tick of decode and the tick of headroom, so nothing is left.
    assert_eq!(delays[8 * 30], set, "inside the window the delay stands");
    assert_eq!(*delays.last().unwrap(), 0, "settled on what frames need");
    let halfway = delays[(10 + 13) * 30];
    assert!(
        halfway > set / 3 && halfway < set * 2 / 3,
        "13 s into the ramp the delay is {halfway}ns of {set}ns"
    );
    assert!(
        delays.windows(2).all(|w| w[1] <= w[0]),
        "the delay only ever comes down here"
    );

    // Every frame shown, in order, and no jump: consecutive timestamps are a
    // frame apart, or 5% less while the ramp runs.
    let emitted = recorder.emitted();
    assert_eq!(emitted.len() as u64, frames + 1, "no frame dropped");
    for pair in emitted[1..].windows(2) {
        let step = pair[1].timestamp - pair[0].timestamp;
        assert!(
            (FRAME * 9 / 10..=FRAME + 1).contains(&step),
            "frames {step}ns apart"
        );
    }
}

/// Once the audio hold rises for video that trails its audio, the speed
/// controller builds it by playing slower, and the audio playout offset (with
/// every due time mapped through it) grows until video is on time again. The
/// delay covered that same lateness in the meantime, and it is the floor less
/// the offset, so each millisecond of growth is one it no longer carries. Due
/// times stay exactly where they were, so the picture neither jumps nor
/// changes speed while the lip-sync error the delay stood for drains to
/// nothing.
#[test]
fn the_video_delay_hands_itself_back_as_the_audio_hold_builds() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    // Every frame maps 300 ms into the past.
    let t0 = clock.now();
    let late = 300_000_000;
    publish_mapping(&shared, t0 - late, 10_000_000_000);
    live_history(&mut thread, t0, 10_000_000_000);
    feed(&mut thread, 10_000_000_000, t0);
    run_at(&mut thread, &clock, t0);
    let set = shared.conn.video_delay_ns.load(Relaxed);
    assert_eq!(set, (late + TICK).div_ceil(TICK) * TICK);

    // The hold builds at -2 %: the offset grows by 2 % of each frame.
    let growth_per_frame = FRAME / 50;
    let frames = 30 * 30u64;
    let mut delays = Vec::new();
    for i in 1..=frames {
        let grown = i * growth_per_frame;
        publish_mapping(&shared, t0 - late + grown, 10_000_000_000);
        let arrival = t0 + i * FRAME;
        feed(&mut thread, 10_000_000_000 + (i * FRAME) as i64, arrival);
        run_at(&mut thread, &clock, arrival);
        delays.push(shared.conn.video_delay_ns.load(Relaxed));
    }
    // By now video is in hand early; let what is queued go out.
    for k in 1..=30u64 {
        run_at(&mut thread, &clock, t0 + (frames + k) * FRAME);
    }

    assert!(
        delays.windows(2).all(|w| w[1] <= w[0]),
        "the delay only ever comes down"
    );
    assert_eq!(*delays.last().unwrap(), 0, "the hold took all of it");

    // Every frame shown. While the delay hands itself back the frames go out
    // exactly a frame apart: nothing on screen moves. After that they follow
    // the playout as it keeps growing, a frame plus the growth apart, give or
    // take the cycle a frame queued early happens to go out on.
    let emitted = recorder.emitted();
    assert_eq!(emitted.len() as u64, frames + 1, "no frame dropped");
    for (n, pair) in emitted.windows(2).enumerate() {
        let step = pair[1].timestamp - pair[0].timestamp;
        let handing_back = delays[n] > 0;
        let max_step = if handing_back {
            FRAME + 1
        } else {
            FRAME + 2 * growth_per_frame + 1
        };
        assert!(
            (FRAME - 1..=max_step).contains(&step),
            "frames {n} and {} are {step}ns apart",
            n + 1
        );
    }
}

/// A relay hands a new subscriber video from its last keyframe next to live
/// audio, so the connection opens with video a second behind its audio, and
/// that video then arrives faster than real time until it has caught up. A
/// frame measured during the catch-up reads as a late sender on a stream
/// with no skew at all. The network cannot cause this (both streams share
/// one mux), so it is not a poor-link problem and must not be answered like
/// one: video sizes the delay only once it is live, and this stream anchors
/// in sync with none.
#[test]
fn video_catching_up_to_live_at_connection_start_sets_no_delay() {
    let shared = shared_with_audio();
    let (mut thread, recorder, clock) = thread_with(shared.clone());

    // 39 frames, 1.3 s of video, arrive at four times real time; from then
    // on frames arrive live, 100 ms before they are due.
    let t0 = clock.now();
    let caught_up = 39u64;
    let live_from = t0 + (caught_up - 1) * FRAME / 4;
    let arrival_of = |i: u64| {
        if i < caught_up {
            t0 + i * FRAME / 4
        } else {
            live_from + (i + 1 - caught_up) * FRAME
        }
    };
    let pts_of = |i: u64| 10_000_000_000 + (i * FRAME) as i64;
    // Frame `caught_up` is due 100 ms after it arrives.
    publish_mapping(
        &shared,
        arrival_of(caught_up) + 100_000_000,
        pts_of(caught_up),
    );

    for i in 0..caught_up + 90 {
        feed(&mut thread, pts_of(i), arrival_of(i));
        run_at(&mut thread, &clock, arrival_of(i));
        assert_eq!(
            shared.conn.video_delay_ns.load(Relaxed),
            0,
            "frame {i}: a catch-up is not a late sender"
        );
    }

    // The first frame of the catch-up was 850 ms past due and the early ones
    // went as stale; the play head anchored on the first frame that was on
    // time, at its real due time, and every frame after it was shown.
    let emitted = recorder.emitted();
    let first = emitted.first().expect("anchored");
    let base = arrival_of(caught_up) as i64 + 100_000_000;
    let first_index =
        ((first.timestamp as i64 - base).div_euclid(FRAME as i64) + caught_up as i64) as u64;
    assert!(
        (30..caught_up).contains(&first_index),
        "anchored on frame {first_index}"
    );
    for (k, frame) in emitted.iter().enumerate() {
        let i = first_index + k as u64;
        assert_eq!(
            frame.timestamp as i64,
            base + (i as i64 - caught_up as i64) * FRAME as i64,
            "frame {i} shown at its undelayed due time"
        );
    }
    assert!(emitted.len() >= 85, "{} frames shown", emitted.len());
}
