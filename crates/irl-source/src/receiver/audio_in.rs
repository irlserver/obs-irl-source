//! Decoded audio intake on the receiver thread (port of
//! `irl_handle_audio_frame`, `receiver-audio.c:906-1127`). W2-B.

use std::sync::atomic::Ordering::Relaxed;

use ffmpeg::{AVSampleFormat, Rational, Resampler};
use irl_core::{LastSample, PtsAction, PtsRepair, consts, dsp, timing};

use crate::receiver::ReceiverFlags;
use crate::shared::{Shared, StreamConfig};

/// Bytes one interleaved float sample occupies (`sizeof(float)`).
const BYTES_PER_SAMPLE: i32 = 4;

/// Nanosecond time base.
const NS_TB: Rational = Rational::new(1, 1_000_000_000);

/// Receiver-thread audio state: the input resampler, its scratch buffer, the
/// PTS repair state machine and the last-sample memory used for silence
/// shaping.
pub struct AudioIntake {
    swr: Option<Resampler>,
    scratch: Vec<u8>,
    pts: Option<PtsRepair>,
    last_sample: LastSample,
    small_gap_ms: i32,
    large_gap_ms: i32,
    /// Byte ⇄ float view for the silence shaping and the post-silence fade;
    /// see the note on `audio::pump::FloatEdit` (this crate forbids the
    /// unsafe cast, and both edits are off the steady-state path).
    float: Vec<f32>,
}

impl AudioIntake {
    /// Fresh intake for a run; PTS-repair thresholds come from `cfg`.
    pub fn new(cfg: &StreamConfig) -> Self {
        Self {
            swr: None,
            scratch: Vec::new(),
            pts: None,
            last_sample: LastSample::default(),
            small_gap_ms: cfg.small_gap_ms,
            large_gap_ms: cfg.large_gap_ms,
            float: Vec::new(),
        }
    }

    /// Per-connection reset (`irl_prepare_new_connection` for the audio
    /// fields): drops the resampler, resets PTS repair.
    pub fn reset(&mut self) {
        self.swr = None;
        if let Some(pts) = self.pts.as_mut() {
            pts.reset();
        }
        self.last_sample = LastSample::default();
    }

    /// (Re)initialise PTS repair for the audio stream's time base
    /// (`pts_repair_init` at decoder open and after a decoder flush).
    pub fn init_pts_repair(&mut self, cfg: &StreamConfig, tb: ffmpeg::Rational) {
        self.small_gap_ms = cfg.small_gap_ms;
        self.large_gap_ms = cfg.large_gap_ms;
        self.pts = Some(PtsRepair::new(
            cfg.small_gap_ms,
            cfg.large_gap_ms,
            tb.num,
            tb.den,
        ));
    }

    /// The PTS repair state (the decode path calls `reset` on it after a
    /// decoder flush).
    pub fn pts_repair(&mut self) -> Option<&mut PtsRepair> {
        self.pts.as_mut()
    }

    /// One decoded frame: format change handling, PTS repair, warm-up
    /// discard, silence insertion, resample to interleaved f32, pre-keyframe
    /// discard, write to the jitter buffer, publish
    /// `decoded_frame_samples` / `latest_audio_stream_pts_ns`.
    pub fn handle_frame(
        &mut self,
        shared: &Shared,
        flags: &mut ReceiverFlags,
        frame: &ffmpeg::Frame,
        tb: ffmpeg::Rational,
    ) {
        let out_channels = frame.channels();
        let out_rate = frame.sample_rate();

        if !self.ensure_buffer_format(shared, out_rate, out_channels) {
            return;
        }

        // ── PTS ──
        let Some((verdict, pts_tb, duration)) = self.evaluate_pts(frame, out_rate, tb) else {
            return;
        };
        let mut inserted_silence = false;

        // ── Startup warm-up ──
        let frame_ms = audio_frame_duration_ms(frame.nb_samples(), out_rate);
        {
            let mut state = shared.audio_state();
            if state.startup_warmup_remaining_ms > 0 {
                state.startup_warmup_remaining_ms -= frame_ms;
                if state.startup_warmup_remaining_ms < 0 {
                    state.startup_warmup_remaining_ms = 0;
                }
                return;
            }
        }

        // ── PTS repair dispatch ──
        match verdict.action {
            PtsAction::Silence if verdict.silence_ms > 0 => {
                if self.insert_silence(shared, verdict.corrected_pts, verdict.silence_ms, pts_tb) {
                    shared.conn.silence_insertions.fetch_add(1, Relaxed);
                    shared.conn.audio_quality_events.fetch_add(1, Relaxed);
                    inserted_silence = true;
                }
            }
            PtsAction::Reset => {
                let mut state = shared.audio_state();
                if let Some(buf) = shared.audio_buf().as_mut() {
                    buf.flush();
                }
                crate::audio::reset_stream_timing_state(shared, &mut state);
                crate::audio::mark_audio_recovery(
                    &mut state,
                    ffmpeg::gettime_us() as u64,
                    consts::AUDIO_RECOVERY_HOLD_US,
                );
                shared.conn.audio_quality_events.fetch_add(1, Relaxed);
                drop(state);
                // The receiver-owned half of `irl_reset_stream_timing_state`,
                // which lives in `ReceiverFlags` here.
                flags.video_prev_pts_ns = 0;
                flags.video_decode_errors = 0;
                flags.video_last_decoder_warning_time_us = 0;
                flags.video_corrupted = false;
                flags.video_skip_logged = false;
                flags.video_hold_logged = false;
                flags.audio_decode_errors = 0;
                flags.audio_last_decoder_flush_time_us = 0;
                flags.audio_last_decoder_warning_time_us = 0;
            }
            _ => {}
        }

        if verdict.action != PtsAction::Pass {
            let gap_ms = verdict.gap_ms;
            shared.conn.pts_last_gap_ms.store(gap_ms, Relaxed);
            shared.conn.pts_max_gap_ms.fetch_max(gap_ms, Relaxed);

            let frame_sized_normalization = verdict.action == PtsAction::Interpolate
                && frame_ms > 0
                && gap_ms <= frame_ms + 2;
            if frame_sized_normalization {
                shared.conn.pts_normalizations.fetch_add(1, Relaxed);
            } else {
                shared.conn.pts_repairs.fetch_add(1, Relaxed);
                if verdict.action == PtsAction::Interpolate {
                    shared.conn.pts_interpolations.fetch_add(1, Relaxed);
                }
            }
            if verdict.action == PtsAction::Reset {
                shared.conn.pts_resets.fetch_add(1, Relaxed);
            }
        }

        // ── Convert to interleaved float ──
        let mut out_samples = frame.nb_samples();
        let mut in_scratch = false;

        if frame.sample_format() != AVSampleFormat::AV_SAMPLE_FMT_FLT {
            match self.resample(frame, out_rate, out_channels, duration, pts_tb) {
                Some(samples) => {
                    out_samples = samples;
                    in_scratch = true;
                }
                None => return,
            }
        } else if inserted_silence {
            // The C faded the decoder's own buffer in place; a decoded frame
            // is borrowed immutably here, so the (rare) fade path copies it
            // into the scratch first.
            let Some(bytes) = frame.interleaved_f32_bytes() else {
                return;
            };
            self.scratch.clear();
            self.scratch.extend_from_slice(bytes);
            in_scratch = true;
        }

        let data_bytes =
            out_samples as usize * out_channels as usize * BYTES_PER_SAMPLE as usize;
        if data_bytes == 0 {
            return;
        }

        if inserted_silence {
            let channels = out_channels as usize;
            edit_floats(
                &mut self.float,
                &mut self.scratch[..data_bytes],
                |pcm| dsp::apply_fade_in(pcm, channels, out_rate),
            );
        }

        if shared.hot.wait_for_keyframe.load(Relaxed)
            && flags.has_video_stream
            && !flags.first_keyframe_received
        {
            return;
        }

        let data: &[u8] = if in_scratch {
            &self.scratch[..data_bytes]
        } else {
            match frame.interleaved_f32_bytes() {
                Some(bytes) => &bytes[..data_bytes.min(bytes.len())],
                None => return,
            }
        };

        let frame_pts_ns = ffmpeg::rescale_q(verdict.corrected_pts, pts_tb, NS_TB);
        if let Some(buf) = shared.audio_buf().as_mut() {
            buf.write_pts(data, frame_pts_ns);
        }
        remember_last_sample(&mut self.last_sample, data, out_channels as usize);

        let mut state = shared.audio_state();
        state.latest_audio_stream_pts_ns = frame_pts_ns;
        state.decoded_frame_samples = out_samples;
    }

    /// The PTS half of `irl_handle_audio_frame`: the frame's timestamp (or an
    /// extrapolated one), its duration, and the repair verdict. `None` drops
    /// the frame.
    fn evaluate_pts(
        &mut self,
        frame: &ffmpeg::Frame,
        out_rate: i32,
        tb: Rational,
    ) -> Option<(irl_core::Verdict, Rational, i64)> {
        let pts = self.pts.as_mut()?;

        let input_pts = match frame.best_effort_pts() {
            Some(value) => value,
            None => match pts.last() {
                Some((last_pts, last_duration)) => last_pts + last_duration,
                None => {
                    irl_warn!("Dropping audio frame without valid PTS");
                    return None;
                }
            },
        };

        let mut duration = frame.duration();
        if duration <= 0 && out_rate > 0 && frame.nb_samples() > 0 {
            duration =
                ffmpeg::rescale_q(frame.nb_samples() as i64, Rational::new(1, out_rate), tb);
        }
        if duration <= 0 {
            duration = 1;
        }

        let verdict = pts.evaluate(input_pts, duration);
        let (tb_num, tb_den) = pts.time_base();
        Some((verdict, Rational::new(tb_num, tb_den), duration))
    }

    /// The format-change half of `irl_handle_audio_frame`: (re)build the
    /// jitter buffer and restart the output clock. Returns false when the
    /// buffer could not be configured (the frame is then dropped).
    fn ensure_buffer_format(&mut self, shared: &Shared, out_rate: i32, out_channels: i32) -> bool {
        let matches = shared
            .audio_buf()
            .as_ref()
            .is_some_and(|b| b.sample_rate() == out_rate && b.channels() == out_channels);
        if matches {
            return true;
        }

        let watermarks = shared.hot.watermarks();
        let mut state = shared.audio_state();
        let reconfigured = {
            let mut guard = shared.audio_buf();
            match guard.as_mut() {
                Some(buf) => buf.reconfigure(out_rate, out_channels, BYTES_PER_SAMPLE),
                None => {
                    let buf = irl_core::AudioBuffer::new(
                        out_rate,
                        out_channels,
                        BYTES_PER_SAMPLE,
                        watermarks.target_ms,
                        watermarks.min_ms,
                        watermarks.max_ms,
                    );
                    let ok = buf.is_some();
                    *guard = buf;
                    ok
                }
            }
        };

        state.primed = false;
        state.anchor_ns = 0;
        state.samples = 0;
        state.out_last.forget();
        state.latest_buffered_end_pts_ns = 0;
        state.latest_audio_stream_pts_ns = 0;
        state.latest_obs_end_ts_ns = 0;
        state.startup_warmup_remaining_ms = consts::STARTUP_AUDIO_WARMUP_MS;
        reconfigured
    }

    /// The `PTS_ACTION_SILENCE` branch: shaped silence, timestamped to end
    /// where the repaired frame begins.
    fn insert_silence(
        &mut self,
        shared: &Shared,
        corrected_pts: i64,
        silence_ms: i32,
        pts_tb: Rational,
    ) -> bool {
        let (silence_bytes, channels, rate) = {
            let guard = shared.audio_buf();
            match guard.as_ref() {
                Some(buf) => (
                    buf.ms_to_bytes(silence_ms),
                    buf.channels() as usize,
                    buf.sample_rate(),
                ),
                None => return false,
            }
        };
        if silence_bytes == 0 {
            return false;
        }

        if self.scratch.len() < silence_bytes {
            self.scratch.resize(silence_bytes, 0);
        }
        let last = self.last_sample;
        edit_floats(
            &mut self.float,
            &mut self.scratch[..silence_bytes],
            |pcm| dsp::shape_silence_from_last(pcm, channels, rate, &last),
        );

        let mut silence_pts_ns = ffmpeg::rescale_q(corrected_pts, pts_tb, NS_TB)
            - silence_ms as i64 * 1_000_000;
        if silence_pts_ns < 0 {
            silence_pts_ns = 0;
        }

        match shared.audio_buf().as_mut() {
            Some(buf) => {
                buf.write_pts(&self.scratch[..silence_bytes], silence_pts_ns);
                true
            }
            None => false,
        }
    }

    /// Non-float decoder output: convert to interleaved float with the
    /// bounded soft compensation. Returns the frames written to the scratch.
    fn resample(
        &mut self,
        frame: &ffmpeg::Frame,
        out_rate: i32,
        out_channels: i32,
        duration: i64,
        pts_tb: Rational,
    ) -> Option<i32> {
        if self.swr.as_ref().is_none_or(|swr| !swr.matches(frame)) {
            self.swr = Resampler::to_interleaved_f32(frame, out_channels, out_rate).ok();
        }
        let swr = self.swr.as_mut()?;

        let expected = timing::expected_samples(
            duration,
            pts_tb.num,
            pts_tb.den,
            out_rate,
            frame.nb_samples(),
        );
        let soft_comp_samples = timing::soft_compensation_samples(expected, frame.nb_samples());
        if soft_comp_samples != 0 {
            let _ = swr.set_compensation(soft_comp_samples, frame.nb_samples());
        }

        let max_out = swr.out_samples(frame.nb_samples()) + soft_comp_samples.abs() + 32;
        let need = max_out as usize * out_channels as usize * BYTES_PER_SAMPLE as usize;
        if self.scratch.len() < need {
            self.scratch.resize(need, 0);
        }

        let out_samples = swr
            .convert_from_frame(&mut self.scratch[..need], max_out, frame)
            .ok()?;
        if out_samples <= 0 { None } else { Some(out_samples) }
    }
}

/// `audio_frame_duration_ms`.
fn audio_frame_duration_ms(samples: i32, sample_rate: i32) -> i32 {
    if samples <= 0 || sample_rate <= 0 {
        return 0;
    }
    let ms = samples as i64 * 1000 / sample_rate as i64;
    if ms <= 0 { 1 } else { ms as i32 }
}

/// `remember_last_sample` over an interleaved-float byte buffer.
fn remember_last_sample(last: &mut LastSample, samples: &[u8], channels: usize) {
    let mut values = [0.0f32; consts::AUDIO_MAX_CHANNELS];
    let take = channels.min(consts::AUDIO_MAX_CHANNELS);
    let frame_bytes = channels * BYTES_PER_SAMPLE as usize;
    if channels == 0 || samples.len() < frame_bytes {
        last.remember(&[], channels);
        return;
    }

    let start = samples.len() - samples.len() % frame_bytes - frame_bytes;
    for (ch, value) in values[..take].iter_mut().enumerate() {
        let off = start + ch * BYTES_PER_SAMPLE as usize;
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&samples[off..off + 4]);
        *value = f32::from_le_bytes(raw);
    }
    last.remember(&values[..take], channels);
}

/// Run an `irl_core::dsp` edit over an interleaved-float byte buffer through a
/// reusable float scratch (this crate forbids the unsafe view cast).
fn edit_floats(scratch: &mut Vec<f32>, bytes: &mut [u8], edit: impl FnOnce(&mut [f32])) {
    scratch.clear();
    scratch.extend(bytes.chunks_exact(4).map(|chunk| {
        let mut raw = [0u8; 4];
        raw.copy_from_slice(chunk);
        f32::from_le_bytes(raw)
    }));
    edit(scratch);
    for (dst, value) in bytes.chunks_exact_mut(4).zip(scratch.iter()) {
        dst.copy_from_slice(&value.to_le_bytes());
    }
}
