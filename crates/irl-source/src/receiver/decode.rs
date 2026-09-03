//! Packet → decoder plumbing (port of `src/receiver-decode.c`). W2-A.

use std::sync::atomic::Ordering::Relaxed;

use ffmpeg::{CodecContext, Frame, Rational};
use irl_core::consts;

use crate::audio;
use crate::receiver::audio_in::AudioIntake;
use crate::receiver::{Receiver, ReceiverFlags};
use crate::shared::{Shared, TimedPacket};

/// Nanosecond time base packet PTS is rescaled into for the video queue's
/// duration bound.
const NS_TIME_BASE: Rational = Rational::new(1, 1_000_000_000);

fn should_log_decoder_warning(last_warning_time_us: &mut u64, now_us: u64) -> bool {
    if *last_warning_time_us != 0
        && now_us - *last_warning_time_us < consts::DECODER_WARNING_INTERVAL_US
    {
        return false;
    }
    *last_warning_time_us = now_us;
    true
}

fn should_flush_decoder(last_flush_time_us: &mut u64, now_us: u64) -> bool {
    if *last_flush_time_us != 0 && now_us - *last_flush_time_us < consts::DECODER_FLUSH_COOLDOWN_US
    {
        return false;
    }
    *last_flush_time_us = now_us;
    true
}

/// `reinit_audio_pts_repair`: the repair state machine restarts on the same
/// time base after a decoder flush.
fn reinit_audio_pts_repair(audio_in: &mut AudioIntake, shared: &Shared, audio_tb: Rational) {
    if let Some(repair) = audio_in.pts_repair() {
        repair.reset();
    }
    audio_in.init_pts_repair(&shared.cfg, audio_tb);
}

/// Drain everything the audio decoder has ready.
fn drain_audio_frames(
    dec: &mut CodecContext,
    frame: &mut Frame,
    shared: &Shared,
    flags: &mut ReceiverFlags,
    audio_in: &mut AudioIntake,
    audio_tb: Rational,
) {
    loop {
        match dec.receive_frame(frame) {
            Err(err) if err.is_eagain() || err.is_eof() => return,
            Err(_) => {
                flags.audio_decode_errors += 1;
                if flags.audio_decode_errors >= consts::DECODER_ERROR_BURST {
                    let now_us = ffmpeg::gettime_us() as u64;
                    let do_flush =
                        should_flush_decoder(&mut flags.audio_last_decoder_flush_time_us, now_us);
                    if should_log_decoder_warning(
                        &mut flags.audio_last_decoder_warning_time_us,
                        now_us,
                    ) {
                        irl_warn!(
                            "Audio decoder receive: corruption burst ({} consecutive errors){}",
                            flags.audio_decode_errors,
                            if do_flush {
                                ", resetting audio state"
                            } else {
                                ", reset cooldown active"
                            }
                        );
                    }
                    if do_flush {
                        dec.flush();
                        shared.conn.audio_decoder_flushes.fetch_add(1, Relaxed);
                        shared.conn.audio_quality_events.fetch_add(1, Relaxed);
                        {
                            let mut state = shared.audio_state();
                            if let Some(buf) = shared.audio_buf().as_mut() {
                                buf.flush();
                            }
                            audio::reset_audio_timing_state(shared, &mut state);
                            audio::mark_audio_recovery(&mut state, now_us, 2_500_000);
                        }
                        reinit_audio_pts_repair(audio_in, shared, audio_tb);
                    }
                    flags.audio_decode_errors = 0;
                }
                return;
            }
            Ok(()) => {
                flags.audio_decode_errors = 0;
                audio_in.handle_frame(shared, flags, frame, audio_tb);
                frame.unref();
            }
        }
    }
}

/// The video decoder is never flushed on a corruption burst, unlike the audio
/// one. `avcodec_flush_buffers` empties the reference picture buffer and clears
/// the decoder's recovery state, and neither the H.264 nor the HEVC decoder can
/// produce a real picture again until the next IDR/CRA: h264dec paints every
/// frame gray until it sees a recovery point, and the HEVC decoder synthesizes
/// each missing reference as a flat mid-gray frame that every later P-frame is
/// predicted from. So the flush turned "a few damaged frames" into a whole GOP
/// of gray — one to two seconds at the keyframe intervals IRL encoders use — on
/// exactly the lossy streams it was meant to help. A decoder error on a live
/// stream is a property of the packet, not of the decoder's state; the next
/// intact packet decodes fine without any reset, and the reference chain heals
/// at the next keyframe either way. The burst is still counted and logged so it
/// shows up in diagnostics.
impl Receiver {
    /// `irl_handle_audio_packet`.
    pub(super) fn handle_audio_packet(&mut self) {
        let audio_tb = self.audio_tb;
        let Self {
            shared,
            audio_dec,
            frame,
            flags,
            audio_in,
            pkt,
            ..
        } = self;
        let Some(dec) = audio_dec.as_mut() else {
            return;
        };

        let mut result = dec.send_packet(pkt);
        if result.as_ref().is_err_and(ffmpeg::Error::is_eagain) {
            // The decoder did not take the packet. FFmpeg's contract is to
            // read output and resend the same packet; returning here would
            // silently discard it.
            shared.lifetime.audio_pkt_eagain.fetch_add(1, Relaxed);
            drain_audio_frames(dec, frame, shared, flags, audio_in, audio_tb);
            result = dec.send_packet(pkt);
            if result.as_ref().is_err_and(ffmpeg::Error::is_eagain) {
                shared.lifetime.audio_pkt_dropped.fetch_add(1, Relaxed);
            }
        }

        match &result {
            Err(err) if !err.is_eagain() && !err.is_eof() => {
                flags.audio_decode_errors += 1;
                if flags.audio_decode_errors >= consts::DECODER_ERROR_BURST {
                    let now_us = ffmpeg::gettime_us() as u64;
                    let do_flush =
                        should_flush_decoder(&mut flags.audio_last_decoder_flush_time_us, now_us);
                    if should_log_decoder_warning(
                        &mut flags.audio_last_decoder_warning_time_us,
                        now_us,
                    ) {
                        irl_warn!(
                            "Audio decoder: corruption burst ({} consecutive errors){}",
                            flags.audio_decode_errors,
                            if do_flush {
                                ", flushing"
                            } else {
                                ", suppressing repeated flush"
                            }
                        );
                    }
                    if do_flush {
                        dec.flush();
                        shared.conn.audio_decoder_flushes.fetch_add(1, Relaxed);
                        shared.conn.audio_quality_events.fetch_add(1, Relaxed);
                    }
                    flags.audio_decode_errors = 0;
                }
            }
            _ => flags.audio_decode_errors = 0,
        }

        drain_audio_frames(dec, frame, shared, flags, audio_in, audio_tb);
    }

    /// The video half of the read loop: hand the packet to the video thread.
    ///
    /// Nothing is decoded here. The stream's latency is held as compressed
    /// packets and decoded just before display, which is what makes a deep
    /// Target Buffer affordable at 4K — and it has to be the video thread that
    /// decodes, because this one spends a stall blocked in `av_read_frame`.
    pub(super) fn push_video_packet(&mut self) {
        // Only used to bound the queue by media duration; output timing comes
        // from the decoded frame's own PTS, after repair.
        let pts_ns = self
            .pkt
            .pts_or_dts()
            .map_or(0, |pts| ffmpeg::rescale_q(pts, self.video_tb, NS_TIME_BASE));
        let bytes = self.pkt.size().max(0) as usize;

        match self.pkt.new_ref() {
            Ok(packet) => self.shared.video.push_packet(
                TimedPacket {
                    packet,
                    pts_ns,
                    bytes,
                },
                &self.shared.lifetime,
            ),
            Err(err) => {
                self.shared.lifetime.video_pkt_dropped.fetch_add(1, Relaxed);
                irl_warn!("Could not reference a video packet ({err}); frame dropped");
            }
        }
    }
}
