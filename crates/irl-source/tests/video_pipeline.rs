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

use obs_irl_source::{receiver, shared, video};

use std::ffi::CString;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use ffmpeg::AVPixelFormat as Pix;
use ffmpeg::sys::{AVColorRange, AVColorSpace, AVColorTransferCharacteristic};
use obs::{ColorRange, ColorSpace, VideoFormat};
use parking_lot::Mutex;

use shared::{HotValues, LifetimeStats, Shared, StreamConfig};
use video::VideoSink;
use video::output;
use video::thread::VideoThread;

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

/// Local newtype: `VideoSink` and `Arc` are both foreign to this test crate
/// now that the plugin modules come from the library.
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

fn stream_config(low_latency_audio: bool) -> StreamConfig {
    StreamConfig {
        url: CString::new("srt://127.0.0.1:9999").unwrap(),
        ffmpeg_options: None,
        hw_decode: irl_core::HwDecode::Auto,
        low_latency_audio,
        small_gap_ms: irl_core::consts::SMALL_GAP_MS,
        large_gap_ms: irl_core::consts::LARGE_GAP_MS,
    }
}

fn hot_values() -> HotValues {
    HotValues {
        reconnect_delay_s: irl_core::consts::DEFAULT_RECONNECT_DELAY_S as i32,
        adaptive_speed: true,
        wait_for_keyframe: true,
        clear_on_disconnect: true,
        watermarks: irl_core::Watermarks::derive(irl_core::consts::DEFAULT_BUFFER_TARGET_MS as i32),
    }
}

fn shared() -> Arc<Shared> {
    // Never dereferenced: every output in these tests goes to the recorder.
    let source = unsafe { obs::SourceHandle::from_raw(NonNull::dangling()) };
    Shared::new(
        source,
        stream_config(false),
        hot_values(),
        Arc::new(LifetimeStats::default()),
    )
}

fn thread_with(shared: Arc<Shared>) -> (VideoThread, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let thread = VideoThread::with_sink(shared, Box::new(RecorderSink(recorder.clone())));
    (thread, recorder)
}

/// A software frame with real buffers.
fn sw_frame(fmt: Pix, width: i32, height: i32) -> ffmpeg::Frame {
    ffmpeg::Frame::alloc_video(fmt, width, height).expect("alloc")
}

/// `AV_FRAME_FLAG_KEY`, which the safe API deliberately has no setter for
/// (only decoders set it).
fn mark_keyframe(frame: &mut ffmpeg::Frame) {
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

    assert_eq!(
        output::convert_color_space(AVColorSpace::AVCOL_SPC_BT709, sdr),
        ColorSpace::Bt709
    );
    assert_eq!(
        output::convert_color_space(AVColorSpace::AVCOL_SPC_SMPTE170M, sdr),
        ColorSpace::Bt601
    );
    assert_eq!(
        output::convert_color_space(AVColorSpace::AVCOL_SPC_BT470BG, sdr),
        ColorSpace::Bt601
    );
    // BT.2020 splits on the transfer function, not the primaries.
    assert_eq!(
        output::convert_color_space(AVColorSpace::AVCOL_SPC_BT2020_NCL, hlg),
        ColorSpace::Hlg2100
    );
    assert_eq!(
        output::convert_color_space(AVColorSpace::AVCOL_SPC_BT2020_CL, hlg),
        ColorSpace::Hlg2100
    );
    assert_eq!(
        output::convert_color_space(
            AVColorSpace::AVCOL_SPC_BT2020_NCL,
            AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
        ),
        ColorSpace::Pq2100
    );
    // Unspecified and everything unknown fall back to BT.709.
    assert_eq!(
        output::convert_color_space(AVColorSpace::AVCOL_SPC_UNSPECIFIED, sdr),
        ColorSpace::Bt709
    );
    assert_eq!(
        output::convert_color_space(AVColorSpace::AVCOL_SPC_FCC, sdr),
        ColorSpace::Bt709
    );

    assert_eq!(
        output::convert_color_range(AVColorRange::AVCOL_RANGE_JPEG),
        ColorRange::Full
    );
    assert_eq!(
        output::convert_color_range(AVColorRange::AVCOL_RANGE_MPEG),
        ColorRange::Partial
    );
    assert_eq!(
        output::convert_color_range(AVColorRange::AVCOL_RANGE_UNSPECIFIED),
        ColorRange::Partial
    );
}

/* ── to_sysmem ────────────────────────────────────────────── */

#[test]
fn sysmem_frames_are_referenced_not_copied() {
    let (mut thread, _recorder) = thread_with(shared());
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
    let (mut thread, recorder) = thread_with(shared());
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
    let (mut thread, recorder) = thread_with(shared());
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

fn video_frame(pts: i64, key: bool) -> ffmpeg::Frame {
    let mut frame = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    frame.set_pts(pts);
    if key {
        mark_keyframe(&mut frame);
    }
    frame
}

#[test]
fn the_keyframe_gate_drops_until_the_first_key_frame() {
    let shared = shared();
    assert!(shared.hot.wait_for_keyframe.load(Relaxed));
    let mut flags = receiver::ReceiverFlags::default();
    let mut intake = video::VideoIntake::new();

    for pts in [0, 3_000, 6_000] {
        let frame = video_frame(pts, false);
        intake.handle_frame(
            &shared,
            &mut flags,
            &frame,
            TB_90K,
            ffmpeg::AVCodecID::AV_CODEC_ID_H264,
        );
    }
    assert_eq!(shared.video.len(), 0, "nothing queued before the keyframe");
    assert_eq!(shared.conn.total_video_frames.load(Relaxed), 0);
    assert!(!flags.first_keyframe_received);

    // 9000 ticks at 90 kHz is 100 ms.
    let frame = video_frame(9_000, true);
    intake.handle_frame(
        &shared,
        &mut flags,
        &frame,
        TB_90K,
        ffmpeg::AVCodecID::AV_CODEC_ID_H264,
    );

    assert!(flags.first_keyframe_received);
    assert_eq!(shared.video.len(), 1);
    assert_eq!(shared.conn.total_video_frames.load(Relaxed), 1);
    assert_eq!((flags.last_video_width, flags.last_video_height), (64, 32));

    let queued = shared
        .video
        .pop_in_flight(&shared.lifetime)
        .expect("queued frame");
    assert_eq!(
        queued.frame().unwrap().pts(),
        100_000_000,
        "PTS rescaled to nanoseconds"
    );
    // The queue holds its own reference; the decoder's frame is untouched.
    assert_eq!(frame.pts(), 9_000);
}

#[test]
fn a_full_queue_drops_the_oldest_frame() {
    let shared = shared();
    let mut flags = receiver::ReceiverFlags::default();
    let mut intake = video::VideoIntake::new();

    for index in 0..5 {
        let frame = video_frame(index * 3_000, true);
        intake.handle_frame(
            &shared,
            &mut flags,
            &frame,
            TB_90K,
            ffmpeg::AVCodecID::AV_CODEC_ID_H264,
        );
    }

    assert_eq!(shared.video.len(), irl_core::consts::VIDEO_QUEUE_SIZE);
    assert_eq!(shared.lifetime.video_queue_drops.load(Relaxed), 1);
    assert_eq!(shared.conn.total_video_frames.load(Relaxed), 5);

    // The survivor at the head is the second frame pushed (33.33 ms).
    let head = shared
        .video
        .pop_in_flight(&shared.lifetime)
        .expect("queued frame");
    assert_eq!(head.frame().unwrap().pts(), 33_333_333);
}

#[test]
fn hevc_frames_from_a_missing_reference_are_held_back() {
    let shared = shared();
    let mut flags = receiver::ReceiverFlags::default();
    let mut intake = video::VideoIntake::new();

    let key = video_frame(0, true);
    intake.handle_frame(
        &shared,
        &mut flags,
        &key,
        TB_90K,
        ffmpeg::AVCodecID::AV_CODEC_ID_HEVC,
    );
    assert_eq!(shared.video.len(), 1);

    let mut corrupt = video_frame(3_000, false);
    unsafe {
        (*corrupt.as_mut_ptr()).flags |= ffmpeg::sys::AV_FRAME_FLAG_CORRUPT;
    }
    intake.handle_frame(
        &shared,
        &mut flags,
        &corrupt,
        TB_90K,
        ffmpeg::AVCodecID::AV_CODEC_ID_HEVC,
    );

    assert_eq!(shared.video.len(), 1, "the damaged frame is not queued");
    assert_eq!(shared.conn.video_corrupt_held.load(Relaxed), 1);
    assert_eq!(shared.conn.video_corrupt_frames.load(Relaxed), 1);
    assert!(flags.video_hold_logged);

    // H.264 damage passes through instead, to preserve cadence.
    let mut damaged = video_frame(6_000, false);
    unsafe {
        (*damaged.as_mut_ptr()).flags |= ffmpeg::sys::AV_FRAME_FLAG_CORRUPT;
    }
    intake.handle_frame(
        &shared,
        &mut flags,
        &damaged,
        TB_90K,
        ffmpeg::AVCodecID::AV_CODEC_ID_H264,
    );
    assert_eq!(shared.video.len(), 2);
}

#[test]
fn a_resolution_change_re_anchors_the_video_clock() {
    let shared = shared();
    let mut flags = receiver::ReceiverFlags::default();
    let mut intake = video::VideoIntake::new();

    let first = video_frame(0, true);
    intake.handle_frame(
        &shared,
        &mut flags,
        &first,
        TB_90K,
        ffmpeg::AVCodecID::AV_CODEC_ID_H264,
    );
    shared.conn.video_ts_init.store(true, Relaxed);

    let mut second = ffmpeg::Frame::alloc_video(Pix::AV_PIX_FMT_YUV420P, 128, 64).unwrap();
    second.set_pts(3_000);
    mark_keyframe(&mut second);
    intake.handle_frame(
        &shared,
        &mut flags,
        &second,
        TB_90K,
        ffmpeg::AVCodecID::AV_CODEC_ID_H264,
    );

    assert!(!shared.conn.video_ts_init.load(Relaxed));
    assert_eq!(shared.conn.last_video_width.load(Relaxed), 128);
    assert_eq!(shared.conn.last_video_height.load(Relaxed), 64);
}

/* ── The pacing loop ──────────────────────────────────────── */

#[test]
fn a_queued_frame_is_transferred_paced_and_emitted() {
    let shared = shared();
    let (mut thread, recorder) = thread_with(shared.clone());

    let mut queued = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    // The receiver hands the queue nanosecond PTS.
    queued.set_pts(5_000_000_000);
    let source_plane = queued.plane(0).unwrap().as_ptr() as usize;
    shared.video.push(queued, &shared.lifetime);

    // With no audio mapping the video-only anchor puts the first frame at
    // `now`, so one cycle takes it all the way out.
    let wait = thread.run_once(obs::time::gettime_ns());

    let emitted = recorder.only();
    assert_eq!(emitted.planes[0].0, source_plane, "still zero-copy");
    assert_eq!(shared.video.len(), 0);
    assert_eq!(thread.paced_len(), 0);
    assert!(
        !wait.is_zero(),
        "nothing left to pace: sleep the full slice"
    );
    assert_eq!(shared.lifetime.pacing_peak.load(Relaxed), 1);
    assert_eq!(shared.lifetime.pacing_now.load(Relaxed), 0);
    assert_eq!(thread.pacing_overflows(), 0);
}

#[test]
fn a_future_frame_waits_instead_of_being_emitted() {
    let shared = shared();
    let (mut thread, recorder) = thread_with(shared.clone());

    let now = obs::time::gettime_ns();
    let mut queued = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    // 200 ms into the future on the video-only anchor: the fallback anchors on
    // the first frame, so pace the *second* one forward.
    queued.set_pts(0);
    shared.video.push(queued, &shared.lifetime);
    thread.run_once(now);
    assert_eq!(recorder.emitted().len(), 1, "the anchor frame goes out");

    let mut later = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    later.set_pts(200_000_000);
    shared.video.push(later, &shared.lifetime);
    let wait = thread.run_once(obs::time::gettime_ns());

    assert_eq!(recorder.emitted().len(), 1, "not due yet");
    assert_eq!(thread.paced_len(), 1);
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
    let (mut thread, recorder) = thread_with(shared.clone());

    let mut queued = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    queued.set_pts(0);
    shared.video.push(queued, &shared.lifetime);
    thread.run_once(obs::time::gettime_ns());
    assert_eq!(recorder.emitted().len(), 1);

    let mut pending = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    pending.set_pts(10_000_000_000);
    shared.video.push(pending, &shared.lifetime);
    thread.run_once(obs::time::gettime_ns());
    assert_eq!(thread.paced_len(), 1, "parked until its due time");

    shared.video.request_clear();
    let wait = thread.run_once(obs::time::gettime_ns());

    assert_eq!(recorder.cleared.load(Relaxed), 1);
    assert!(
        wait.is_zero(),
        "the clear cycle goes round again immediately"
    );
    assert_eq!(thread.paced_len(), 0, "paced frames go with the clear");
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
    let (mut thread, recorder) = thread_with(shared.clone());

    // Audio that ends at stream PTS 10 s plays out at OBS time `now + 1 s`:
    // everything maps 1 s into the future minus 10 s of stream time.
    let now = obs::time::gettime_ns();
    {
        let mut state = shared.audio_state();
        state.latest_obs_end_ts_ns = now + 1_000_000_000;
        state.latest_buffered_end_pts_ns = 10_000_000_000;
    }

    let mut queued = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    queued.set_pts(10_000_000_000);
    shared.video.push(queued, &shared.lifetime);
    thread.run_once(now);

    assert!(recorder.emitted().is_empty(), "due a second from now");
    let due = thread.next_due_ns().expect("paced");
    assert_eq!(due, now + 1_000_000_000);

    // The audio side reclaims half of that latency; the queued frame must move
    // with it rather than trailing by the depth of the queue.
    {
        let mut state = shared.audio_state();
        state.latest_obs_end_ts_ns = now + 500_000_000;
    }
    thread.run_once(now);
    assert_eq!(thread.next_due_ns(), Some(now + 500_000_000));

    // Once the offset says "now", it goes out.
    {
        let mut state = shared.audio_state();
        state.latest_obs_end_ts_ns = now;
    }
    thread.run_once(now);
    assert_eq!(recorder.emitted().len(), 1);
    assert_eq!(recorder.only().timestamp, now);
}

#[test]
fn clearing_the_ts_init_mirror_re_anchors_the_fallback_clock() {
    let shared = shared();
    let (mut thread, _recorder) = thread_with(shared.clone());

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
    assert_eq!(shared.conn.video_pts_base.load(Relaxed), 10_000_000_000);
}

#[test]
fn the_lead_stats_follow_the_mapping() {
    let shared = shared();
    let (mut thread, _recorder) = thread_with(shared.clone());

    let now = obs::time::gettime_ns();
    {
        let mut state = shared.audio_state();
        // A frame at stream PTS 0 maps two seconds into the future.
        state.latest_obs_end_ts_ns = now + 2_000_000_000;
        state.latest_buffered_end_pts_ns = 1;
    }

    let mut frame = sw_frame(Pix::AV_PIX_FMT_YUV420P, 64, 32);
    frame.set_pts(0);
    let due = thread.due_time(&frame);

    assert!(due >= now + 1_900_000_000);
    let lead = shared.conn.video_lead_ns.load(Relaxed);
    assert!(lead > 1_900_000_000, "lead {lead}");
    assert_eq!(shared.lifetime.video_lead_peak_ns.load(Relaxed), lead);
    // 2 s is past 120 ms of target buffer plus the 400 ms floor.
    assert_eq!(shared.lifetime.video_lead_excess.load(Relaxed), 1);
}
