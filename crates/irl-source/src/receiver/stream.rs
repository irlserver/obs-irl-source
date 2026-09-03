//! Stream open/close, reconnection, stats line (port of `src/receiver-stream.c`). W2-A.

use std::ffi::CString;
use std::sync::atomic::Ordering::Relaxed;

use ffmpeg::{AVPixelFormat, Codec, CodecContext, HwDeviceContext, MediaType, StreamRef};
use irl_core::{HwDecode, consts};

use crate::audio;
use crate::receiver::{Receiver, probe};
use crate::shared::{AudioState, Shared, VideoDecoder};

/// `nvdec_get_format`. Installed only for forced NVDEC, where a software
/// fallback is exactly what must not happen.
fn nvdec_get_format(codec: &Codec, offered: &[AVPixelFormat]) -> AVPixelFormat {
    let picked = probe::pick_cuda_format(codec, offered);
    if picked == AVPixelFormat::AV_PIX_FMT_NONE {
        irl_error!("NVDEC requested, but the decoder offered no CUDA hardware format");
    }
    picked
}

/// Open one decoder for `stream` (port of `open_decoder`).
///
/// `hw_device` is the connection's shared device slot: the video decoder
/// creates it, and the software-fallback path releases it. `using_hw_decode`
/// records whether a hardware device actually made it onto an open decoder.
fn open_decoder(
    stream: &StreamRef<'_>,
    hw_decode: HwDecode,
    hw_device: &mut Option<HwDeviceContext>,
    using_hw_decode: &mut bool,
) -> Option<CodecContext> {
    let is_video = stream.media_type() == MediaType::Video;
    let try_hw = is_video && hw_decode != HwDecode::Off;
    let force_nvdec = is_video && hw_decode == HwDecode::Nvdec;
    let device_types = if force_nvdec {
        probe::NVDEC_DEVICE_TYPES
    } else {
        probe::HW_DEVICE_TYPES
    };

    let codec = Codec::find_decoder(stream.codec_id())?;
    let mut builder = ffmpeg::CodecBuilder::from_stream(codec, stream)
        .ok()?
        .pkt_timebase(stream.time_base());

    builder = if stream.media_type() == MediaType::Audio {
        builder.threads(1, 0)
    } else {
        // Low-delay decode: don't hold frames for B-frame reordering. IRL
        // encoders essentially never emit B-frames, so that buffer is pure
        // latency.
        //
        // Frame threading adds thread_count-1 frames of pipeline latency on
        // software decode, so cap it instead of letting FFmpeg use every core.
        // The thread type is FFmpeg's own default (frame + slice); only the
        // count is capped. Hardware decode ignores both.
        //
        // error_concealment keeps FFmpeg's guess_mvs+deblock default and adds
        // favor_inter, which patches damaged macroblocks from the previous
        // frame instead of guessing spatially. flags2 |= FAST adds the
        // spec-noncompliant software speedups for the machines where hardware
        // decode fell back to software.
        //
        // extra_hw_frames covers every surface this plugin can pin at once:
        // the queue, the one the video thread is transferring, and the one
        // just returned by receive_frame.
        builder
            .flag_low_delay()
            .threads(
                consts::VIDEO_DECODER_THREADS,
                ffmpeg::sys::FF_THREAD_FRAME | ffmpeg::sys::FF_THREAD_SLICE,
            )
            .error_concealment(ffmpeg::sys::FF_EC_FAVOR_INTER)
            .flag2_fast()
            .extra_hw_frames(consts::VIDEO_EXTRA_HW_FRAMES)
    };

    if try_hw {
        if hw_device.is_none() {
            *hw_device = HwDeviceContext::probe(device_types, &mut |kind, err| {
                irl_info!(
                    "Hardware device {} unavailable: {err}",
                    ffmpeg::hwdevice_type_name(kind)
                );
            });
            if let Some(device) = hw_device.as_ref() {
                irl_info!(
                    "Using hardware device: {}",
                    ffmpeg::hwdevice_type_name(device.kind())
                );
            }
        }
        if force_nvdec && hw_device.is_none() {
            irl_error!("NVDEC was selected, but no CUDA device is available");
            return None;
        }
        if let Some(device) = hw_device.as_ref() {
            builder = builder.hw_device(device);
            if force_nvdec {
                builder = builder.get_format(nvdec_get_format);
            }
        }
    }

    let had_hw_device = builder.has_hw_device();
    match builder.open() {
        Ok(ctx) => {
            if ctx.has_hw_device() {
                *using_hw_decode = true;
            }
            Some(ctx)
        }
        Err(err) => {
            if had_hw_device && !force_nvdec {
                *hw_device = None;
                irl_info!("Hardware decode failed, falling back to software");
                return open_decoder(stream, HwDecode::Off, hw_device, using_hw_decode);
            }
            if had_hw_device {
                *hw_device = None;
            }
            if force_nvdec {
                irl_error!("NVDEC decoder failed ({err}); software fallback is disabled");
            } else {
                irl_warn!("Decoder failed to open: {err}");
            }
            None
        }
    }
}

/// Fade the buffered audio out over `IRL_FADE_DURATION_MS` and submit it, so a
/// disconnect ends on silence rather than a click.
///
/// The caller holds `audio_state`: the timestamp claim advances the shared
/// output clock that the audio pump also uses.
fn fade_out_buffered_audio(shared: &Shared, state: &mut AudioState) {
    let mut guard = shared.audio_buf();
    let Some(buf) = guard.as_mut() else { return };

    let buffered_ms = buf.fill_ms();
    if buffered_ms <= 0 || !state.primed {
        return;
    }

    let buffered_bytes = buf.ms_to_bytes(buffered_ms);
    let mut fade_bytes = buf.ms_to_bytes(consts::FADE_DURATION_MS);
    if fade_bytes > buffered_bytes {
        fade_bytes = buffered_bytes;
    }
    if fade_bytes == 0 {
        return;
    }

    let mut fade_buf = vec![0u8; fade_bytes];
    let got = buf.read_with_fade_out(&mut fade_buf);
    if got == 0 {
        return;
    }

    let frame_size = buf.frame_size();
    if frame_size == 0 {
        return;
    }
    let frames = (got / frame_size) as u32;
    let channels = buf.channels() as u32;
    let sample_rate = buf.sample_rate() as u32;
    drop(guard);

    let timestamp = audio::output_claim(state, frames, sample_rate);
    shared.source.output_audio(&obs::AudioFrame::interleaved(
        &fade_buf[..got],
        frames,
        obs::SpeakerLayout::from_channels(channels),
        sample_rate,
        obs::AudioFormat::Float,
        timestamp,
    ));
}

impl Receiver {
    /// One `avformat_open_input` + probe + decoder-open pass.
    pub(super) fn open_stream_attempt(&mut self, fast_probe: bool) -> bool {
        let url = self.shared.cfg.url.clone();
        let url_str = url.to_string_lossy().into_owned();

        let mut opts = ffmpeg::Dictionary::new();
        for (key, value) in irl_core::url_opts::demuxer_options(
            &url_str,
            self.shared.cfg.ffmpeg_options.as_deref(),
            consts::NETWORK_BUFFER_MB,
            fast_probe,
        ) {
            let (Ok(key), Ok(value)) = (CString::new(key.as_ref()), CString::new(value.as_ref()))
            else {
                continue;
            };
            let _ = opts.set(&key, &value);
        }

        // Whether this URL waits to be called decides whether the stall
        // deadline applies before a connection exists. Latched per attempt,
        // because a settings edit can change the URL.
        self.shared
            .interrupt
            .set_awaits_caller(irl_core::url_awaits_caller(&url.to_string_lossy()));

        crate::log::log_input_url("Connecting to", &url);

        // Unrecognised options are dropped without a word, as `av_dict_free`
        // does in the C: FFmpeg option names differ per protocol, so the
        // table above deliberately sets keys most inputs ignore.
        let fmt = match ffmpeg::FormatContext::open(&url, opts, self.shared.interrupt.clone()) {
            Ok((fmt, _unrecognised)) => fmt,
            Err(err) => {
                irl_warn!("Failed to open input: {err}");
                return false;
            }
        };
        self.fmt = Some(fmt);

        irl_info!("Input opened, probing streams...");

        let probed = match self.fmt.as_mut() {
            Some(fmt) => fmt.find_stream_info(),
            None => return false,
        };
        if probed.is_err() {
            irl_warn!("Failed to find stream info");
            self.fmt = None;
            return false;
        }

        self.audio_stream_idx = -1;
        self.shared.flags.audio_present.store(false, Relaxed);
        self.video_stream_idx = -1;
        self.flags.has_audio_stream = false;
        self.flags.has_video_stream = false;

        self.select_streams();

        if self.video_stream_idx < 0 && self.audio_stream_idx < 0 {
            irl_warn!("No usable audio or video streams found");
            self.close_ffmpeg();
            return false;
        }

        irl_info!(
            "Stream opened (video={}, audio={})",
            self.video_stream_idx,
            self.audio_stream_idx
        );

        if self.audio_stream_idx >= 0 {
            let tb = self.audio_tb;
            let Self {
                audio_in, shared, ..
            } = self;
            audio_in.init_pts_repair(&shared.cfg, tb);
        }

        true
    }

    /// Pick the first video and the first audio stream that open a decoder,
    /// caching everything the read loop needs from them.
    fn select_streams(&mut self) {
        let hw_decode = self.shared.cfg.hw_decode;
        let Self {
            shared,
            fmt,
            audio_dec,
            hw_device,
            audio_stream_idx,
            video_stream_idx,
            audio_tb,
            video_tb,
            using_hw_decode,
            flags,
            ..
        } = self;
        let Some(fmt) = fmt.as_ref() else { return };

        for stream in fmt.streams() {
            let index = stream.index();
            let codec_id = stream.codec_id();
            match stream.media_type() {
                MediaType::Video if *video_stream_idx < 0 => {
                    match open_decoder(&stream, hw_decode, hw_device, using_hw_decode) {
                        Some(dec) => {
                            *video_stream_idx = index as i32;
                            flags.has_video_stream = true;
                            *video_tb = stream.time_base();
                            // This reports the requested decode path; the
                            // first-keyframe log reports the ground truth from
                            // the actual decoded frame.
                            let hw_attached = dec.has_hw_device();
                            let path = match hw_device.as_ref() {
                                Some(device) if hw_attached => {
                                    ffmpeg::hwdevice_type_name(device.kind())
                                }
                                _ => "SW",
                            };
                            let (width, height) = stream.dimensions();
                            irl_info!(
                                "Video stream {index}: {} {width}x{height} ({path} requested, using_hw={})",
                                ffmpeg::codec_name(codec_id),
                                i32::from(*using_hw_decode)
                            );
                            // The video thread owns the decoder from here:
                            // it decides when each packet is decoded, and this
                            // thread spends a stall blocked in av_read_frame.
                            // Ordering in the channel is what binds the
                            // decoder to the packets that follow it.
                            shared.video.install_decoder(VideoDecoder {
                                ctx: dec,
                                time_base: stream.time_base(),
                                codec_id,
                            });
                        }
                        None => irl_warn!(
                            "Failed to open video decoder for stream {index} ({})",
                            ffmpeg::codec_name(codec_id)
                        ),
                    }
                }
                MediaType::Audio if *audio_stream_idx < 0 => {
                    match open_decoder(&stream, HwDecode::Off, hw_device, using_hw_decode) {
                        Some(dec) => {
                            *audio_stream_idx = index as i32;
                            flags.has_audio_stream = true;
                            shared.flags.audio_present.store(true, Relaxed);
                            *audio_tb = stream.time_base();
                            let (sample_rate, channels) = stream.audio_params();
                            irl_info!(
                                "Audio stream {index}: {} {sample_rate}Hz {channels}ch",
                                ffmpeg::codec_name(codec_id)
                            );
                            *audio_dec = Some(dec);
                        }
                        None => {
                            irl_warn!("Failed to open audio decoder for stream {index}");
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// `irl_open_stream`: fast probe when the previous session on this thread
    /// showed what the stream carries, full probe otherwise.
    ///
    /// The short probe can miss a stream some encoders advertise late, so a
    /// result thinner than the previous session is thrown away and re-probed
    /// in full rather than trusted.
    pub(super) fn open_stream(&mut self) -> bool {
        let fast = self.prev_had_video || self.prev_had_audio;
        if fast && self.open_stream_attempt(true) {
            let missing = (self.prev_had_video && self.video_stream_idx < 0)
                || (self.prev_had_audio && self.audio_stream_idx < 0);
            if !missing {
                self.prev_had_video = self.video_stream_idx >= 0;
                self.prev_had_audio = self.audio_stream_idx >= 0;
                return true;
            }
            irl_info!(
                "Fast probe missed a stream the previous session had (video={}, audio={}), re-probing in full",
                self.video_stream_idx,
                self.audio_stream_idx
            );
            self.close_ffmpeg();
        }

        if !self.open_stream_attempt(false) {
            return false;
        }

        self.prev_had_video = self.video_stream_idx >= 0;
        self.prev_had_audio = self.audio_stream_idx >= 0;
        true
    }

    /// `irl_close_ffmpeg`. The hardware device goes with the connection it was
    /// created for; the software scaler belongs to the video thread and is not
    /// touched here.
    pub(super) fn close_ffmpeg(&mut self) {
        self.audio_dec = None;
        // The video decoder belongs to the video thread; it drops it on the
        // next `install_decoder` or when the run ends. Its own reference to the
        // hardware device (taken by `avcodec_open2`) keeps that alive past the
        // line below.
        self.hw_device = None;
        self.fmt = None;

        self.audio_stream_idx = -1;
        self.shared.flags.audio_present.store(false, Relaxed);
        self.video_stream_idx = -1;
        self.using_hw_decode = false;

        self.audio_in.reset();
        self.flags.reset();
    }

    /// `irl_prepare_new_connection`.
    pub(super) fn prepare_new_connection(&mut self) {
        self.shared.flags.reconnecting.store(false, Relaxed);
        self.shared.video_flags.first_keyframe.store(false, Relaxed);
        self.shared.video_flags.corrupted.store(false, Relaxed);
        self.shared.conn.video_ts_init.store(false, Relaxed);

        let mut state = self.shared.audio_state();
        state.fade_in_pending = true;
        state.fade_in_frames_remaining = 0;
        state.startup_warmup_remaining_ms = consts::STARTUP_AUDIO_WARMUP_MS;
    }

    /// `irl_wait_for_reconnect`. Returns whether the run is still active.
    pub(super) fn wait_for_reconnect(&mut self) -> bool {
        self.shared.flags.reconnecting.store(true, Relaxed);
        self.shared.lifetime.reconnect_count.fetch_add(1, Relaxed);
        // Sampled once: a delay edited mid-wait should apply to the next
        // attempt, not stretch or truncate the one already counting down.
        let delay_s = self.shared.hot.reconnect_delay_s.load(Relaxed);
        irl_info!("Reconnecting in {delay_s}s...");
        probe::reconnect_sleep(delay_s, &self.shared.flags.thread_active);
        self.shared.flags.reconnecting.store(false, Relaxed);
        self.shared.is_active()
    }

    /// `irl_handle_stream_read_error`: log, tear the connection down, blank
    /// the source, fade the buffered audio out and reset the per-connection
    /// counters. The read loop reconnects immediately afterwards.
    pub(super) fn handle_stream_read_error(&mut self, err: ffmpeg::Error) {
        let shared = self.shared.clone();
        shared.flags.reconnecting.store(true, Relaxed);
        irl_warn!(
            "Stream read error: {err} (video_frames={}, audio_frames={})",
            shared.conn.total_video_frames.load(Relaxed),
            shared.conn.total_audio_frames.load(Relaxed)
        );

        self.close_ffmpeg();

        // Blank the source instead of leaving the last decoded frame frozen
        // on screen, matching what OBS's own media source does on media end
        // (its clear_on_media_end, likewise on by default). The audio fade-out
        // below is the same idea for the other half of the stream. The video
        // thread performs the actual clear.
        if shared.hot.clear_on_disconnect.load(Relaxed) {
            shared.video.request_clear();
        }

        {
            let mut state = shared.audio_state();
            fade_out_buffered_audio(&shared, &mut state);
            if let Some(buf) = shared.audio_buf().as_mut() {
                buf.flush();
            }
            audio::reset_stream_timing_state(&shared, &mut state);
            audio::mark_audio_recovery(&mut state, ffmpeg::gettime_us() as u64, 2_500_000);
            state.fade_in_pending = true;
            shared.conn.video_corrupt_frames.store(0, Relaxed);
            shared.conn.video_corrupt_held.store(0, Relaxed);
        }

        let conn = &shared.conn;
        conn.set_current_speed(1.0);
        conn.audio_output_restarts.store(0, Relaxed);
        conn.audio_underruns.store(0, Relaxed);
        conn.audio_resync_skipped_chunks.store(0, Relaxed);
        conn.audio_hidden_trimmed_chunks.store(0, Relaxed);
        conn.audio_quality_events.store(0, Relaxed);
        conn.audio_decoder_flushes.store(0, Relaxed);
        conn.pts_repairs.store(0, Relaxed);
        conn.pts_normalizations.store(0, Relaxed);
        conn.pts_interpolations.store(0, Relaxed);
        conn.pts_resets.store(0, Relaxed);
        conn.pts_last_gap_ms.store(0, Relaxed);
        conn.pts_max_gap_ms.store(0, Relaxed);
        conn.silence_insertions.store(0, Relaxed);
        conn.total_audio_frames.store(0, Relaxed);
        conn.total_video_frames.store(0, Relaxed);
        self.last_stats_time = 0;
    }

    /// The periodic diagnostics line, every 30 seconds of read-loop time.
    pub(super) fn log_receiver_stats(&mut self) {
        let now = obs::time::gettime_ns();
        if now - self.last_stats_time <= consts::STATS_LOG_INTERVAL_NS {
            return;
        }
        self.last_stats_time = now;

        let shared = &self.shared;
        let conn = &shared.conn;
        let lifetime = &shared.lifetime;

        // Drift of the audio->OBS playout offset from its primed baseline.
        // Stays near 0 when healthy; a climbing value is concealment inflating
        // the video lip-sync mapping. Computed inside the lock rather than
        // from separate reads: its three inputs are only meaningful against
        // each other, and the audio thread updates them together.
        //
        // The rest of what other threads write are atomics, so unlike the C
        // they need no lock at all — and the stats line is the last place the
        // audio_state / video queue lock edge should be introduced.
        let av_drift_ms = {
            let state = shared.audio_state();
            if state.offset_baseline_set
                && state.latest_obs_end_ts_ns != 0
                && state.latest_buffered_end_pts_ns > 0
            {
                (state.latest_obs_end_ts_ns as i64
                    - state.latest_buffered_end_pts_ns
                    - state.offset_baseline_ns)
                    / 1_000_000
            } else {
                0
            }
        };

        let video_frame_interval_ns = conn.video_frame_interval_ns.load(Relaxed);
        let buffer_fill_ms = self.audio_fill_ms();

        irl_info!(
            "Stats: video={} audio={} \
             buf={}ms peak={}ms target={}ms speed={:.3} ctrl={} pts_repairs={} \
             norm={} interp={} silence={} resets={} \
             last_gap={}ms max_gap={}ms underruns={} resync_skips={} \
             hidden_trims={} quality_events={} \
             audio_flushes={} corrupt={} held={} vq_drops={} \
             obs_lead={}ms chunk={}@{} \
             stream_chunk={}ms obs_chunk={}ms \
             restarts={} av_drift={}ms reanchors={} \
             vlead={}ms peak={}ms excess={} vfps={:.1} \
             pktq={}/{}({}KB,{}ms) paced={}/{}({}MB) early={} eagain={}/{} pktdrop={}/{} res={}x{}",
            conn.total_video_frames.load(Relaxed),
            conn.total_audio_frames.load(Relaxed),
            buffer_fill_ms,
            lifetime.audio_fill_peak_ms.load(Relaxed),
            shared.hot.watermarks().target_ms,
            f64::from(conn.current_speed()),
            if shared.hot.adaptive_speed.load(Relaxed) {
                "on"
            } else {
                "off"
            },
            conn.pts_repairs.load(Relaxed),
            conn.pts_normalizations.load(Relaxed),
            conn.pts_interpolations.load(Relaxed),
            conn.silence_insertions.load(Relaxed),
            conn.pts_resets.load(Relaxed),
            conn.pts_last_gap_ms.load(Relaxed),
            conn.pts_max_gap_ms.load(Relaxed),
            conn.audio_underruns.load(Relaxed),
            conn.audio_resync_skipped_chunks.load(Relaxed),
            conn.audio_hidden_trimmed_chunks.load(Relaxed),
            conn.audio_quality_events.load(Relaxed),
            conn.audio_decoder_flushes.load(Relaxed),
            conn.video_corrupt_frames.load(Relaxed),
            conn.video_corrupt_held.load(Relaxed),
            lifetime.video_queue_drops.load(Relaxed),
            conn.last_obs_lead_ns.load(Relaxed) / 1_000_000,
            conn.last_frames_out.load(Relaxed),
            conn.last_samples_per_sec.load(Relaxed),
            conn.last_chunk_stream_ns.load(Relaxed) / 1_000_000,
            conn.last_chunk_obs_ns.load(Relaxed) / 1_000_000,
            conn.audio_output_restarts.load(Relaxed),
            av_drift_ms,
            lifetime.audio_offset_reanchors.load(Relaxed),
            conn.video_lead_ns.load(Relaxed) / 1_000_000,
            lifetime.video_lead_peak_ns.load(Relaxed) / 1_000_000,
            lifetime.video_lead_excess.load(Relaxed),
            if video_frame_interval_ns > 0 {
                1_000_000_000.0 / video_frame_interval_ns as f64
            } else {
                0.0
            },
            // The compressed video queue: where the stream's latency is
            // actually held. Its duration should track the Target Buffer, and
            // its size is what a deep buffer costs at this bitrate — the
            // decoded side is bounded by VIDEO_DECODE_LEAD_MS whatever this
            // says.
            shared.video.len(),
            lifetime.video_queue_peak.load(Relaxed),
            shared.video.bytes() / 1024,
            shared.video.span_ns() / 1_000_000,
            lifetime.pacing_now.load(Relaxed),
            lifetime.pacing_peak.load(Relaxed),
            lifetime.pacing_bytes.load(Relaxed) / (1024 * 1024),
            lifetime.pacing_overflows.load(Relaxed),
            lifetime.video_pkt_eagain.load(Relaxed),
            lifetime.audio_pkt_eagain.load(Relaxed),
            lifetime.video_pkt_dropped.load(Relaxed),
            lifetime.audio_pkt_dropped.load(Relaxed),
            conn.last_video_width.load(Relaxed),
            conn.last_video_height.load(Relaxed)
        );
    }
}
