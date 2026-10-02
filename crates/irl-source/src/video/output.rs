//! HW transfer, format mapping, OBS output.
//!
//! Everything here belongs to the video thread, so it is written as methods on
//! [`VideoThread`], plus the pure mapping helpers, which stay free functions so
//! tests can table them.

use std::sync::OnceLock;
use std::sync::atomic::Ordering::Relaxed;

use ffmpeg::sys::{AVColorRange, AVColorSpace, AVColorTransferCharacteristic};
use ffmpeg::{AVPixelFormat, Frame, FramePool, Scaler};
use irl_core::{consts, timing, video_time};
use obs::{ColorRange, ColorSpace, VideoFormat, VideoFrame};

use crate::video::thread::VideoThread;

/// `AV_NUM_DATA_POINTERS` and libobs's `MAX_AV_PLANES`; both are 8.
const MAX_PLANES: usize = 8;

/* ── swscale backend selection ────────────────────────────── */

/// `IRL_SWS_UNSTABLE`, read and logged once: on when set, non-empty and not
/// starting with `0`.
///
/// FFmpeg 9.0's op-chain backends are reachable only through the dynamic API
/// (`sws_alloc_context` with no `sws_init_context`, driven by
/// `sws_scale_frame`), which is what [`Scaler`] uses; `SWS_UNSTABLE` makes it
/// prefer them. Every format OBS makes us convert is subsampled, which the op
/// chain declines in 9.0, so the legacy pass runs anyway and the flag defaults
/// off.
fn sws_unstable() -> bool {
    static UNSTABLE: OnceLock<bool> = OnceLock::new();
    *UNSTABLE.get_or_init(|| {
        let on = std::env::var_os("IRL_SWS_UNSTABLE")
            .map(|value| {
                let value = value.to_string_lossy().into_owned();
                !value.is_empty() && !value.starts_with('0')
            })
            .unwrap_or(false);
        irl_info!(
            "swscale backend: {}",
            if on {
                "experimental (IRL_SWS_UNSTABLE=1)"
            } else {
                "legacy"
            }
        );
        on
    })
}

/* ── Format mapping ───────────────────────────────────────── */

pub fn avpixfmt_to_obs(fmt: AVPixelFormat) -> VideoFormat {
    use AVPixelFormat as F;
    match fmt {
        F::AV_PIX_FMT_YUV420P | F::AV_PIX_FMT_YUVJ420P => VideoFormat::I420,
        F::AV_PIX_FMT_YUV420P10LE => VideoFormat::I010,
        F::AV_PIX_FMT_NV12 => VideoFormat::Nv12,
        F::AV_PIX_FMT_P010LE => VideoFormat::P010,
        F::AV_PIX_FMT_YUV422P | F::AV_PIX_FMT_YUVJ422P => VideoFormat::I422,
        F::AV_PIX_FMT_YUV444P | F::AV_PIX_FMT_YUVJ444P => VideoFormat::I444,
        F::AV_PIX_FMT_UYVY422 => VideoFormat::Uyvy,
        F::AV_PIX_FMT_YUYV422 => VideoFormat::Yuy2,
        F::AV_PIX_FMT_RGBA => VideoFormat::Rgba,
        F::AV_PIX_FMT_BGRA => VideoFormat::Bgra,
        _ => VideoFormat::None,
    }
}

/// BT.2020 splits into HLG and PQ on the transfer function alone.
pub fn convert_color_space(cs: AVColorSpace, trc: AVColorTransferCharacteristic) -> ColorSpace {
    match cs {
        AVColorSpace::AVCOL_SPC_BT709 => ColorSpace::Bt709,
        AVColorSpace::AVCOL_SPC_SMPTE170M | AVColorSpace::AVCOL_SPC_BT470BG => ColorSpace::Bt601,
        AVColorSpace::AVCOL_SPC_BT2020_NCL | AVColorSpace::AVCOL_SPC_BT2020_CL => {
            if trc == AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67 {
                ColorSpace::Hlg2100
            } else {
                ColorSpace::Pq2100
            }
        }
        _ => ColorSpace::Bt709,
    }
}

pub fn convert_color_range(range: AVColorRange) -> ColorRange {
    if range == AVColorRange::AVCOL_RANGE_JPEG {
        ColorRange::Full
    } else {
        ColorRange::Partial
    }
}

impl VideoThread {
    /* ── Transfer to system memory ────────────────────────── */

    /// A system-memory frame comes back as a new reference (no copy). A
    /// hardware frame is copied out exactly once, into a buffer recycled
    /// through [`FramePool`]: `av_hwframe_map` would keep the decoder surface
    /// pinned for the whole output lead and exhaust the pool within a few
    /// frames, and letting `av_hwframe_transfer_data` allocate costs a
    /// frame-sized malloc/free per frame (page zeroing plus thousands of soft
    /// faults per 4K frame).
    // The `&mut self` is the transfer pool and its broken latch, not a
    // conversion of the receiver.
    #[allow(clippy::wrong_self_convention)]
    pub fn to_sysmem(&mut self, frame: &Frame) -> Option<Frame> {
        if !frame.is_hw() {
            // Already system memory: take a reference so the pacing queue owns
            // its entries uniformly.
            return frame.new_ref().ok();
        }

        let mut out = self.pooled_transfer(frame);
        if out.is_none() {
            out = ffmpeg::hwframe_transfer_new(frame).ok();
        }
        let mut out = out?;
        out.copy_video_props_from(frame);
        Some(out)
    }

    /// The pooled half of the transfer. `None` means "let FFmpeg allocate the
    /// destination instead".
    fn pooled_transfer(&mut self, frame: &Frame) -> Option<Frame> {
        if self.xfer_pool_broken {
            return None;
        }
        let sw_fmt = frame.hw_sw_format()?;
        let (width, height) = (frame.width(), frame.height());

        if !self
            .xfer_pool
            .as_ref()
            .is_some_and(|pool| pool.matches(sw_fmt, width, height))
        {
            self.xfer_pool = None;
            let (pool, size) = FramePool::new(sw_fmt, width, height).ok()?;
            let (padded_w, padded_h) = pool.dimensions();
            irl_info!(
                "Transfer buffer pool: {}x{} fmt={}, {:.1} MB/frame",
                padded_w,
                padded_h,
                sw_fmt as i32,
                size as f64 / (1024.0 * 1024.0)
            );
            self.xfer_pool = Some(pool);
        }

        let mut dst = self.xfer_pool.as_ref()?.acquire().ok()?;
        if ffmpeg::hwframe_transfer_into(&mut dst, frame).is_err() {
            // This backend refuses a caller-allocated destination; stop
            // offering one.
            self.xfer_pool_broken = true;
            self.xfer_pool = None;
            irl_warn!("Pooled hw frame transfer rejected; falling back to per-frame allocation");
            return None;
        }
        // Undo the 16-alignment padding: OBS must see the display size, not
        // the surface size.
        dst.set_display_size(width, height);
        Some(dst)
    }

    /* ── Timestamp mapping ────────────────────────────────── */

    /// The OBS timestamp a freshly transferred frame is scheduled for.
    ///
    /// While audio is playing, queued audio is the master playout clock: video
    /// PTS maps through the same stream-PTS → OBS-clock offset as the latest
    /// chunk handed to OBS, which keeps lip sync stable across buffering and
    /// speed changes. Without that mapping it falls back to the video-only
    /// wall-clock anchor.
    pub fn due_time(&mut self, frame: &Frame) -> u64 {
        // `frame.pts()` is already in nanoseconds: intake rescales it with the
        // time base captured at stream open, because the video thread must not
        // touch the format context (it can be freed mid-reconnect).
        let pts_ns = frame.pts();
        let now = obs::time::gettime_ns();

        let (mapping, startup_warmup_ms) = {
            let state = self.shared.audio_state();
            (state.playout_mapping(), state.startup_warmup_remaining_ms)
        };
        let frame_interval_ns = self.shared.conn.video_frame_interval_ns.load(Relaxed);

        // No audio-stream test: a published mapping already implies the pump
        // handed OBS a real chunk, so it implies the audio stream.
        // Both schedules carry the standing video delay; see
        // `VideoThread::settle_anchor_candidate`.
        let delay_ns = self.delay.delay_ns();

        if let Some(mapped) = mapping.map(pts_ns) {
            let mapped = mapped.saturating_add(delay_ns);
            self.record_lead(mapped as i64, now, frame_interval_ns);
            return mapped;
        }

        // `conn.video_ts_init` is the authority here: it is cleared on a
        // resolution change and on every new connection
        // (`prepare_new_connection` / `reset_stream_timing_state`), so a
        // cleared flag means "re-anchor now" even though the private anchors
        // below still hold the previous connection's epoch. This is the only
        // place that sets it.
        if !self.ts_init || !self.shared.conn.video_ts_init.load(Relaxed) {
            self.sys_base = now;
            self.pts_base = pts_ns;
            self.ts_init = true;
            self.shared.conn.video_ts_init.store(true, Relaxed);
        }

        let mut computed = video_time::fallback_anchor(pts_ns, self.pts_base, self.sys_base, now)
            .saturating_add(delay_ns);

        // Startup fallback before the audio playout mapping exists. The
        // mapping cannot stand in for "there is audio" here, so this reads the
        // flag the receiver publishes.
        if self.shared.flags.audio_present.load(Relaxed) {
            let mut audio_lead_ns = 0;
            if !mapping.has_output() {
                audio_lead_ns = startup_warmup_ms as i64 * 1_000_000;
                if !self.shared.cfg.low_latency_audio {
                    audio_lead_ns += self.shared.hot.watermarks().target_ms as i64 * 1_000_000;
                }
            }
            if audio_lead_ns > 0 {
                computed += audio_lead_ns as u64;
            }
        }

        self.record_lead(computed as i64, now, frame_interval_ns);
        computed
    }

    /// Whether audio has published a playout mapping at all — the same test
    /// [`Self::due_time`] makes, without the hold [`Self::playout_offset`]
    /// applies after one goes away. The anchor logic needs the live answer:
    /// a held offset belongs to audio that has stopped, and anchoring on it
    /// would be anchoring on a guess.
    pub fn mapping_published(&self) -> bool {
        self.shared.audio_state().playout_mapping().is_published()
    }

    /// The current stream-PTS → OBS-clock offset, for re-deriving the due time
    /// of frames already queued.
    ///
    /// Free of the side effects in [`Self::due_time`]: the lead warning and the
    /// fallback anchor belong to a frame arriving, and running them for every
    /// queued frame would report the queue rather than the stream.
    /// `None` when there is no audio to slave to, or when the mapping has been
    /// gone long enough that holding it would be a guess; the caller then
    /// keeps the due times the frames arrived with. The standing video delay
    /// rides on the offset, so a reschedule keeps it.
    pub fn playout_offset(&mut self) -> Option<i64> {
        let offset_ns = self.shared.audio_state().playout_mapping().offset_ns();
        let now = obs::time::gettime_ns();

        if let Some(offset_ns) = offset_ns {
            self.playout_offset_ns = offset_ns;
            self.playout_offset_time_ns = now;
        } else if self.playout_offset_time_ns == 0
            || now.saturating_sub(self.playout_offset_time_ns) > consts::VIDEO_OFFSET_HOLD_NS
        {
            return None;
        }

        Some(self.playout_offset_ns + self.delay.delay_ns() as i64)
    }

    /// Warn when the mapping placed this frame further ahead of wall clock
    /// than libobs can queue.
    ///
    /// The lead is never clamped. What libobs queues is the lead's *growth*
    /// since its play head last anchored, not its size, so a large but steady
    /// lead queues nothing and clamping it would only shift video ahead of
    /// audio.
    pub fn record_lead(&mut self, ts: i64, now: u64, frame_interval_ns: i64) {
        let lead_ns = ts - now as i64;
        let frame_interval_ns = if frame_interval_ns <= 0 {
            consts::VIDEO_INTERVAL_DEFAULT_NS
        } else {
            frame_interval_ns
        };
        let queue_safe_ns = self.shared.hot.watermarks().target_ms as i64 * 1_000_000
            + video_time::queue_safe_ns(frame_interval_ns);
        if lead_ns <= queue_safe_ns {
            return;
        }

        // Only a risk while the lead is still climbing — a steady lead of any
        // size is free — so this is a "watch this" line, not a fault.
        if timing::throttle(
            &mut self.lead_warn_time_ns,
            now,
            consts::VIDEO_LEAD_WARN_INTERVAL_NS,
        ) {
            irl_info!(
                "Video lead {}ms is beyond what OBS can queue ({}ms at {:.0}fps); harmless while it holds steady, but a rise of that size would make OBS drop queued video",
                lead_ns / 1_000_000,
                queue_safe_ns / 1_000_000,
                1_000_000_000.0 / frame_interval_ns as f64
            );
        }
    }

    /* ── Output ───────────────────────────────────────────── */

    /// Hand a system-memory frame to OBS with the timestamp pacing scheduled
    /// it for.
    ///
    /// A directly supported format lends its planes to libobs, which copies
    /// them inside `obs_source_output_video`, so this path makes no copy of its
    /// own. Anything else (an unmapped pixel format, or the bottom-up layout
    /// OBS's async path cannot express, which `Frame::plane` reports as no
    /// plane) goes through swscale into the persistent NV12 scratch.
    ///
    /// Returns whether the frame reached libobs. A conversion that fails
    /// submits nothing, and the caller needs to know: libobs's play head is
    /// only anchored by a frame it actually received.
    pub fn output_frame(&mut self, frame: &Frame, timestamp: u64) -> bool {
        let (width, height) = (frame.width(), frame.height());
        let colors = frame.colorimetry();
        let cs = convert_color_space(colors.colorspace, colors.color_trc);
        let range = convert_color_range(colors.color_range);

        let obs_fmt = avpixfmt_to_obs(frame.pix_fmt());
        let planes = frame.plane_count().min(MAX_PLANES);
        let mut lent: [Option<(&[u8], u32)>; MAX_PLANES] = [None; MAX_PLANES];
        let mut direct = obs_fmt != VideoFormat::None && planes > 0;
        if direct {
            for (index, slot) in lent.iter_mut().enumerate().take(planes) {
                match frame.plane(index) {
                    Some(data) => *slot = Some((data, frame.plane_linesize(index) as u32)),
                    None => {
                        direct = false;
                        break;
                    }
                }
            }
        }

        if direct {
            let mut obs_frame = VideoFrame::new(width as u32, height as u32, obs_fmt)
                .timestamp(timestamp)
                .colorimetry(cs, range);
            for (index, plane) in lent.iter().enumerate() {
                if let Some((data, linesize)) = *plane {
                    obs_frame = obs_frame.plane(index, data, linesize);
                }
            }
            self.sink.output_video(&obs_frame);
            return true;
        }

        self.output_frame_via_nv12(frame, timestamp, cs, range)
    }

    /// The swscale half of [`Self::output_frame`].
    fn output_frame_via_nv12(
        &mut self,
        frame: &Frame,
        timestamp: u64,
        cs: ColorSpace,
        range: ColorRange,
    ) -> bool {
        let (width, height) = (frame.width(), frame.height());
        if width <= 0 || height <= 0 {
            return false;
        }
        let pix_fmt = frame.pix_fmt();

        if self.sws_src != Some((width, height, pix_fmt)) {
            irl_info!(
                "Converting pixel format {} to NV12 via swscale ({}x{})",
                pix_fmt as i32,
                width,
                height
            );
            self.sws_src = Some((width, height, pix_fmt));
        }

        // Stride is the display width. The chroma plane has `ceil(height / 2)`
        // rows, which is what `scale_into_nv12` validates.
        let stride = width as usize;
        let y_size = stride * height as usize;
        let uv_size = stride * (height as usize).div_ceil(2);
        let need = y_size + uv_size;
        if self.nv12_scratch.len() < need {
            // Grown, never shrunk: a resolution change downwards keeps the
            // buffer rather than churning the allocator.
            self.nv12_scratch.resize(need, 0);
        }

        let scaler = match self
            .scaler
            .take()
            .map_or_else(|| Scaler::new(sws_unstable()), Ok)
        {
            Ok(scaler) => self.scaler.insert(scaler),
            Err(err) => {
                irl_warn!("swscale conversion failed: {err}");
                return false;
            }
        };
        let (y, uv) = self.nv12_scratch.split_at_mut(y_size);
        if let Err(err) = scaler.scale_into_nv12(frame, y, uv, width) {
            irl_warn!("swscale conversion failed: {err}");
            return false;
        }

        let obs_frame = VideoFrame::new(width as u32, height as u32, VideoFormat::Nv12)
            .timestamp(timestamp)
            .colorimetry(cs, range)
            .plane(0, &self.nv12_scratch[..y_size], width as u32)
            .plane(1, &self.nv12_scratch[y_size..need], width as u32);
        self.sink.output_video(&obs_frame);
        true
    }
}
