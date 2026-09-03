//! Audio core tests: the output pump's clock contract, concealment and speed
//! compensation, plus the receiver-side intake.
//!
//! No libobs runs under `cargo test`, so the pump emits into a recording sink
//! and reads a test-owned clock instead of `os_gettime_ns`. The `SourceHandle`
//! in `Shared` is never dereferenced by these paths.

use std::ffi::CString;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use parking_lot::Mutex;

use irl_core::{AudioBuffer, HwDecode, Watermarks, consts};
use obs_irl_source::audio::{AudioPump, AudioSink};
use obs_irl_source::receiver::ReceiverFlags;
use obs_irl_source::receiver::audio_in::AudioIntake;
use obs_irl_source::shared::{HotValues, LifetimeStats, Shared, StreamConfig};

const RATE: i32 = 48_000;
const CHANNELS: i32 = 2;
/// One Opus frame at 48 kHz: 20 ms.
const CHUNK_FRAMES: usize = 960;
const CHUNK_NS: u64 = 20_000_000;

// ── Harness ───────────────────────────────────────────────────

struct Recorded {
    timestamp: u64,
    frames: u32,
    rate: u32,
    samples: Vec<f32>,
}

/// Sink that keeps every submission instead of handing it to libobs.
#[derive(Clone)]
struct Recorder {
    channels: usize,
    emitted: Arc<Mutex<Vec<Recorded>>>,
}

impl Recorder {
    fn new(channels: usize) -> Self {
        Self {
            channels,
            emitted: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn len(&self) -> usize {
        self.emitted.lock().len()
    }
}

impl AudioSink for Recorder {
    fn output_audio(&self, audio: &obs::AudioFrame<'_>) {
        let sys = audio.as_sys();
        let bytes = sys.frames as usize * self.channels * 4;
        // SAFETY: the frame borrows a live interleaved-float buffer of
        // `frames * channels` samples, which is what the pump built.
        let raw = unsafe { std::slice::from_raw_parts(sys.data[0], bytes) };
        self.emitted.lock().push(Recorded {
            timestamp: sys.timestamp,
            frames: sys.frames,
            rate: sys.samples_per_sec,
            samples: raw
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        });
    }
}

fn stream_config(low_latency: bool) -> StreamConfig {
    StreamConfig {
        url: CString::new("srt://127.0.0.1:9000").unwrap(),
        ffmpeg_options: None,
        hw_decode: HwDecode::Auto,
        low_latency_audio: low_latency,
        small_gap_ms: consts::SMALL_GAP_MS,
        large_gap_ms: consts::LARGE_GAP_MS,
    }
}

fn make_shared(low_latency: bool, adaptive: bool) -> Arc<Shared> {
    // SAFETY: no code under test dereferences the handle — audio leaves
    // through the recording sink, and nothing here calls into libobs.
    let source = unsafe { obs::SourceHandle::from_raw(NonNull::dangling()) };
    let hot = HotValues {
        reconnect_delay_s: 2,
        adaptive_speed: adaptive,
        catchup_percent: consts::DEFAULT_CATCHUP_PERCENT as i32,
        wait_for_keyframe: true,
        clear_on_disconnect: true,
        watermarks: Watermarks {
            target_ms: 120,
            min_ms: 60,
            max_ms: 320,
        },
    };
    Shared::new(
        source,
        stream_config(low_latency),
        hot,
        Arc::new(LifetimeStats::default()),
    )
}

/// A configured jitter buffer and a pump wired to `clock` and `recorder`.
fn make_pump(shared: &Arc<Shared>, clock: &Arc<AtomicU64>, recorder: &Recorder) -> AudioPump {
    *shared.audio_buf() = AudioBuffer::new(RATE, CHANNELS, 4, 120, 60, 320);
    shared.audio_state().decoded_frame_samples = CHUNK_FRAMES as i32;

    let clock = Arc::clone(clock);
    AudioPump::with_sink(Arc::clone(shared), Box::new(recorder.clone()))
        .with_clock(Box::new(move || clock.load(Relaxed)))
}

/// Append one 20 ms chunk of constant-valued PCM at `pts_ns`.
fn write_chunk(shared: &Shared, pts_ns: i64, value: f32) {
    let mut bytes = Vec::with_capacity(CHUNK_FRAMES * CHANNELS as usize * 4);
    for _ in 0..CHUNK_FRAMES * CHANNELS as usize {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    shared
        .audio_buf()
        .as_mut()
        .unwrap()
        .write_pts(&bytes, pts_ns);
}

fn fill_ms(shared: &Shared) -> i32 {
    shared.audio_buf().as_ref().map_or(0, |b| b.fill_ms())
}

/// `anchor + round(samples * 1e9 / rate)`: the pure sample-counter clock the
/// pump derives every timestamp from.
fn expected_ts(anchor: u64, samples: u64, rate: u64) -> u64 {
    anchor + (samples * 1_000_000_000 + rate / 2) / rate
}

// ── Tests ─────────────────────────────────────────────────────

/// OBS smooths under 70 ms, zero-fills 70 ms..2 s and flushes beyond that, so
/// the one property the output clock may never break is `ts[n+1] = ts[n] +
/// frames/rate` — here over 10 000 consecutive chunks of a healthy stream.
#[test]
fn output_timestamps_are_contiguous_over_ten_thousand_chunks() {
    let shared = make_shared(false, false);
    let clock = Arc::new(AtomicU64::new(1_000_000_000));
    let recorder = Recorder::new(CHANNELS as usize);
    let mut pump = make_pump(&shared, &clock, &recorder);

    let mut pts_ns = 0i64;
    // Prime: target (120 ms) + lead (80 ms) must be queued before playback
    // starts, so hand it 300 ms.
    for _ in 0..15 {
        write_chunk(&shared, pts_ns, 0.25);
        pts_ns += CHUNK_NS as i64;
    }

    let mut guard = 0;
    while recorder.len() < 10_000 {
        while pump.pump_once() {}
        clock.fetch_add(CHUNK_NS, Relaxed);
        write_chunk(&shared, pts_ns, 0.25);
        pts_ns += CHUNK_NS as i64;

        guard += 1;
        assert!(
            guard < 20_000,
            "pump stalled after {} chunks",
            recorder.len()
        );
    }

    let emitted = recorder.emitted.lock();
    assert!(emitted.len() >= 10_000);

    let anchor = emitted[0].timestamp;
    let mut samples = 0u64;
    for (i, chunk) in emitted.iter().enumerate() {
        assert_eq!(chunk.rate, RATE as u32, "rate changed at chunk {i}");
        assert_eq!(
            chunk.timestamp,
            expected_ts(anchor, samples, RATE as u64),
            "timestamp discontinuity at chunk {i}"
        );
        samples += chunk.frames as u64;
    }

    // A fed stream must never conceal, and the clock must never restart.
    assert_eq!(shared.conn.audio_underruns.load(Relaxed), 0);
    assert_eq!(shared.conn.audio_output_restarts.load(Relaxed), 0);
    assert_eq!(
        shared.conn.total_audio_frames.load(Relaxed),
        emitted.len() as u64
    );
}

/// A dry buffer must still produce a chunk every cycle (a silent OBS tick
/// costs a time-shifted splice), shaped so the dropout decays out of the last
/// real sample instead of clicking.
#[test]
fn underrun_conceals_with_shaped_silence() {
    let shared = make_shared(false, false);
    let clock = Arc::new(AtomicU64::new(1_000_000_000));
    let recorder = Recorder::new(CHANNELS as usize);
    let mut pump = make_pump(&shared, &clock, &recorder);

    let mut pts_ns = 0i64;
    for _ in 0..15 {
        write_chunk(&shared, pts_ns, 0.5);
        pts_ns += CHUNK_NS as i64;
    }

    // Feed nothing more: the buffer drains and the pump has to conceal. The
    // run stops short of the re-anchor margin (400 ms of concealment), so the
    // clock line below must be contiguous end to end.
    for _ in 0..25 {
        while pump.pump_once() {}
        clock.fetch_add(CHUNK_NS, Relaxed);
    }

    assert!(
        shared.conn.audio_underruns.load(Relaxed) >= 2,
        "expected repeated underruns, got {}",
        shared.conn.audio_underruns.load(Relaxed)
    );
    assert_eq!(fill_ms(&shared), 0);
    assert_eq!(shared.lifetime.audio_offset_reanchors.load(Relaxed), 0);

    let emitted = recorder.emitted.lock();
    let last = emitted.last().expect("emitted something");
    assert_eq!(last.frames, CHUNK_FRAMES as u32);
    assert!(
        last.samples.iter().all(|s| *s == 0.0),
        "a later concealment chunk must be pure silence"
    );

    // The first concealment chunk decays out of the last real sample (0.5)
    // over AUDIO_CONCEAL_FADE_MS and is silent afterwards.
    let first_conceal = emitted
        .iter()
        .find(|c| c.samples[0] != 0.0 && c.samples[c.samples.len() - 1] == 0.0)
        .expect("a decaying concealment chunk");
    assert!(first_conceal.samples[0] > 0.0 && first_conceal.samples[0] < 0.5);
    let fade_frames = RATE as usize * consts::AUDIO_CONCEAL_FADE_MS as usize / 1000;
    assert!(
        first_conceal.samples[fade_frames * CHANNELS as usize..]
            .iter()
            .all(|s| *s == 0.0)
    );

    // The clock line stays contiguous across the concealment splice.
    let anchor = emitted[0].timestamp;
    let mut samples = 0u64;
    for chunk in emitted.iter() {
        assert_eq!(chunk.timestamp, expected_ts(anchor, samples, RATE as u64));
        samples += chunk.frames as u64;
    }
}

/// Backlog is drained by playing faster, never by skipping: at full authority
/// (+5 %) a chunk read from the buffer must leave the plugin shorter than it
/// arrived, with the sample rate submitted to OBS unchanged.
#[test]
fn speed_compensation_emits_fewer_frames_than_it_reads() {
    let shared = make_shared(false, true);
    let clock = Arc::new(AtomicU64::new(1_000_000_000));
    let recorder = Recorder::new(CHANNELS as usize);
    let mut pump = make_pump(&shared, &clock, &recorder);

    let mut pts_ns = 0i64;
    for _ in 0..12 {
        write_chunk(&shared, pts_ns, 0.1);
        pts_ns += CHUNK_NS as i64;
    }

    // Hold the buffer well above buffer_max (320 ms) so the controller runs
    // at full drain authority.
    for _ in 0..600 {
        while fill_ms(&shared) < 450 {
            write_chunk(&shared, pts_ns, 0.1);
            pts_ns += CHUNK_NS as i64;
        }
        while pump.pump_once() {}
        clock.fetch_add(CHUNK_NS, Relaxed);
    }

    let speed = shared.conn.current_speed();
    let max_speed = irl_core::catchup_speed_max(consts::DEFAULT_CATCHUP_PERCENT as i32);
    assert!(
        speed >= max_speed - 0.001,
        "speed should ramp to the catch-up ceiling, got {speed}"
    );

    let emitted = recorder.emitted.lock();
    let last = emitted.last().unwrap();
    assert_eq!(
        last.rate, RATE as u32,
        "the submitted rate must never change"
    );
    assert!(
        (last.frames as usize) < CHUNK_FRAMES,
        "at +5% a {CHUNK_FRAMES}-frame chunk must come out shorter, got {}",
        last.frames
    );
    // ~960 / 1.05.
    assert!(last.frames >= 900, "compensation overshot: {}", last.frames);
}

/// The Catch-Up Speed setting is the drain ceiling, applied live: the same
/// backlog drains at whatever the slider says, not at a compiled-in +5 %.
#[test]
fn the_catchup_setting_bounds_the_drain_end_to_end() {
    let shared = make_shared(false, true);
    shared
        .hot
        .catchup_percent
        .store(consts::CATCHUP_PERCENT_MIN, Relaxed);

    let clock = Arc::new(AtomicU64::new(1_000_000_000));
    let recorder = Recorder::new(CHANNELS as usize);
    let mut pump = make_pump(&shared, &clock, &recorder);

    let mut pts_ns = 0i64;
    for _ in 0..12 {
        write_chunk(&shared, pts_ns, 0.1);
        pts_ns += CHUNK_NS as i64;
    }

    // Same runaway backlog as the test above, so the only difference is the
    // setting.
    for _ in 0..600 {
        while fill_ms(&shared) < 450 {
            write_chunk(&shared, pts_ns, 0.1);
            pts_ns += CHUNK_NS as i64;
        }
        while pump.pump_once() {}
        clock.fetch_add(CHUNK_NS, Relaxed);
    }

    let speed = shared.conn.current_speed();
    let min_ceiling = irl_core::catchup_speed_max(consts::CATCHUP_PERCENT_MIN);
    assert!(
        (speed - min_ceiling).abs() < 0.001,
        "speed should pin at the 2% ceiling, got {speed}"
    );

    // ~960 / 1.02, so measurably longer than the ~914 the +5 % ceiling gives.
    let emitted = recorder.emitted.lock();
    let last = emitted.last().unwrap();
    assert!(
        (last.frames as usize) < CHUNK_FRAMES && last.frames > 930,
        "at +2% a {CHUNK_FRAMES}-frame chunk should come out around 941, got {}",
        last.frames
    );
}

/// The pump sleeps to the deadline its own output clock gives it, rather than
/// polling: once primed and queued ahead, "nothing to do" comes with how long
/// nothing is to be done for.
#[test]
fn an_idle_pump_reports_when_it_next_has_work() {
    let shared = make_shared(false, true);
    let clock = Arc::new(AtomicU64::new(1_000_000_000));
    let recorder = Recorder::new(CHANNELS as usize);
    let mut pump = make_pump(&shared, &clock, &recorder);

    // Nothing buffered yet: the wake condition is a write from another
    // thread, which no deadline here can predict.
    assert!(!pump.pump_once());
    assert_eq!(pump.idle_sleep_ms(), consts::AUDIO_PUMP_SLEEP_MS);

    let mut pts_ns = 0i64;
    for _ in 0..20 {
        write_chunk(&shared, pts_ns, 0.1);
        pts_ns += CHUNK_NS as i64;
    }
    while pump.pump_once() {}

    // Primed and queued ahead: it now knows when the lead runs down, and that
    // is further away than the 1ms poll it used to spend.
    assert!(shared.audio_state().primed);
    let hint = pump.idle_sleep_ms();
    assert!(
        hint > consts::AUDIO_PUMP_SLEEP_MS && hint <= consts::AUDIO_PUMP_MAX_SLEEP_MS,
        "expected a real deadline, got {hint}ms"
    );

    // The hint truncates to whole milliseconds, so it lands at or just before
    // the deadline and never past it: sleeping it must not overshoot the
    // moment the chunk was due, and one more millisecond must reach it.
    clock.fetch_add(hint as u64 * 1_000_000, Relaxed);
    let early = pump.pump_once();
    clock.fetch_add(1_000_000, Relaxed);
    assert!(
        early || pump.pump_once(),
        "still nothing emitted a millisecond past the reported deadline"
    );
}

/// Low-latency mode emits no concealment, so an empty input cannot advance the
/// sample counter. The output clock then sits still while wall clock moves, and
/// the stall check used to read that as a stalled audio thread — restarting,
/// re-anchoring, waiting one lead and tripping again, roughly every 150ms for
/// as long as the source stayed quiet.
#[test]
fn a_quiet_low_latency_input_suspends_the_clock_instead_of_restart_looping() {
    let shared = make_shared(true, true); // low latency
    let clock = Arc::new(AtomicU64::new(1_000_000_000));
    let recorder = Recorder::new(CHANNELS as usize);
    let mut pump = make_pump(&shared, &clock, &recorder);

    // Prime on real audio.
    let mut pts_ns = 0i64;
    for _ in 0..6 {
        write_chunk(&shared, pts_ns, 0.1);
        pts_ns += CHUNK_NS as i64;
    }
    while pump.pump_once() {}
    assert!(shared.audio_state().primed, "never primed");

    // The input goes quiet. Wall clock runs well past the stall threshold.
    for _ in 0..40 {
        clock.fetch_add(50_000_000, Relaxed);
        while pump.pump_once() {}
    }

    assert_eq!(
        shared.conn.audio_output_restarts.load(Relaxed),
        0,
        "a quiet low-latency source must not restart the output clock"
    );
    assert!(
        !shared.audio_state().primed,
        "the clock should be stood down, not left running"
    );

    // Real audio returns: the normal prime path establishes one new clock.
    for _ in 0..6 {
        write_chunk(&shared, pts_ns, 0.2);
        pts_ns += CHUNK_NS as i64;
    }
    while pump.pump_once() {}
    assert!(
        shared.audio_state().primed,
        "did not re-prime on real audio"
    );
    assert_eq!(shared.conn.audio_output_restarts.load(Relaxed), 0);
}

/// AAC decodes 1024 frames at a time, which does not divide a 120ms target
/// (5760 frames). Reads and writes are both whole chunks, so before the read
/// alignment the residual could only be a multiple of 1024 and the loop had to
/// straddle the target at 106ms or 128ms — up to a whole chunk of cushion the
/// user configured and never got.
///
/// The existing tests all use 960-frame chunks, where 120ms *is* on the grid,
/// so none of them could see this.
#[test]
fn the_buffer_settles_on_the_configured_target_with_aac_chunks() {
    const AAC_FRAMES: usize = 1024;
    const AAC_NS: u64 = AAC_FRAMES as u64 * 1_000_000_000 / RATE as u64;

    let shared = make_shared(false, true);
    let clock = Arc::new(AtomicU64::new(1_000_000_000));
    let recorder = Recorder::new(CHANNELS as usize);

    *shared.audio_buf() = AudioBuffer::new(RATE, CHANNELS, 4, 120, 60, 320);
    shared.audio_state().decoded_frame_samples = AAC_FRAMES as i32;
    let mut pump = {
        let ns = Arc::clone(&clock);
        let us = Arc::clone(&clock);
        AudioPump::with_sink(Arc::clone(&shared), Box::new(recorder.clone()))
            .with_clock(Box::new(move || ns.load(Relaxed)))
            // The same virtual time, in the FFmpeg domain: the speed trim
            // integrates against this, and it has to advance with the buffer
            // or the loop under test never closes.
            .with_us_clock(Box::new(move || us.load(Relaxed) / 1000))
    };

    let mut pts_ns = 0i64;
    let write = |shared: &Shared, pts: &mut i64| {
        let mut bytes = Vec::with_capacity(AAC_FRAMES * CHANNELS as usize * 4);
        for _ in 0..AAC_FRAMES * CHANNELS as usize {
            bytes.extend_from_slice(&0.1f32.to_le_bytes());
        }
        shared.audio_buf().as_mut().unwrap().write_pts(&bytes, *pts);
        *pts += AAC_NS as i64;
    };

    // Prime: the threshold is target + lead = 200ms, so ~10 chunks.
    for _ in 0..10 {
        write(&shared, &mut pts_ns);
    }

    // Then hold the sender at exactly real time: one chunk in per chunk of
    // wall clock. Whatever the level settles on is the loop's own choice.
    //
    // Sampled before the pump reads, which is the level the speed controller
    // sees and regulates. The level swings a whole chunk within every cycle,
    // so sampling after the pump has taken what it wants measures the trough
    // and says more about the sampling point than about the cushion.
    let mut levels = Vec::new();
    for _ in 0..3000 {
        write(&shared, &mut pts_ns);
        levels.push(fill_ms(&shared));
        while pump.pump_once() {}
        clock.fetch_add(AAC_NS, Relaxed);
    }

    assert!(shared.audio_state().primed, "never primed");
    let tail = &levels[levels.len() - 1000..];
    let mean = tail.iter().map(|&f| f as f64).sum::<f64>() / tail.len() as f64;
    let chunk_ms = (AAC_NS / 1_000_000) as f64;
    // Centring puts the two reachable levels half a chunk either side of the
    // target, so the average is the configured cushion even though no single
    // sample can be.
    assert!(
        (mean - 120.0).abs() <= chunk_ms / 2.0 + 1.0,
        "held {mean:.1}ms on average, further than half a {chunk_ms}ms chunk from the 120ms target"
    );
}

/// The receiver-side intake: warm-up discard, then float passthrough into the
/// jitter buffer with the published chunk size and stream PTS.
#[test]
fn intake_discards_warmup_then_buffers_decoded_audio() {
    let shared = make_shared(false, true);
    let cfg = stream_config(false);
    let tb = ffmpeg::Rational::new(1, RATE);

    let mut intake = AudioIntake::new(&cfg);
    intake.init_pts_repair(&cfg, tb);
    let mut flags = ReceiverFlags::default();

    let frames = 20;
    for i in 0..frames {
        let frame = audio_frame(i * CHUNK_FRAMES as i64, 0.75);
        intake.handle_frame(&shared, &mut flags, &frame, tb);
    }

    // The first IRL_STARTUP_AUDIO_WARMUP_MS (150 ms) is decoder warm-up and
    // gets discarded — a whole 20 ms chunk at a time, so eight of them; the
    // rest lands in the buffer.
    let warmup_chunks = (consts::STARTUP_AUDIO_WARMUP_MS as usize).div_ceil(20);
    let expected_chunks = frames as usize - warmup_chunks;
    let buffered_ms = fill_ms(&shared);
    assert_eq!(buffered_ms, (expected_chunks * 20) as i32);

    let state = shared.audio_state();
    assert_eq!(state.decoded_frame_samples, CHUNK_FRAMES as i32);
    assert_eq!(
        state.latest_audio_stream_pts_ns,
        (frames - 1) * CHUNK_NS as i64
    );
    assert_eq!(shared.conn.pts_repairs.load(Relaxed), 0);
    assert_eq!(shared.conn.silence_insertions.load(Relaxed), 0);

    let buf = shared.audio_buf();
    let buf = buf.as_ref().unwrap();
    assert_eq!(buf.sample_rate(), RATE);
    assert_eq!(buf.channels(), CHANNELS);
}

/// One interleaved-float audio frame, as an AAC/Opus decoder would hand it
/// over (`AV_SAMPLE_FMT_FLT`, `duration` in the 1/rate time base).
fn audio_frame(pts: i64, value: f32) -> ffmpeg::Frame {
    let mut frame = ffmpeg::Frame::new().unwrap();
    // SAFETY: setting the audio parameters before av_frame_get_buffer is the
    // documented allocation sequence; the buffer is then written through its
    // own data pointer for exactly nb_samples * channels samples.
    unsafe {
        let raw = frame.as_mut_ptr();
        (*raw).format = ffmpeg::AVSampleFormat::AV_SAMPLE_FMT_FLT as core::ffi::c_int;
        (*raw).nb_samples = CHUNK_FRAMES as i32;
        (*raw).sample_rate = RATE;
        ffmpeg::sys::av_channel_layout_default(&raw mut (*raw).ch_layout, CHANNELS);
        assert_eq!(ffmpeg::sys::av_frame_get_buffer(raw, 0), 0);
        (*raw).pts = pts;
        (*raw).duration = CHUNK_FRAMES as i64;

        let dst = (*raw).data[0];
        for i in 0..CHUNK_FRAMES * CHANNELS as usize {
            let bytes = value.to_le_bytes();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(i * 4), 4);
        }
    }
    frame
}
