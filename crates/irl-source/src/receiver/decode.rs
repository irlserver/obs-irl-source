//! Packet routing: audio packets into the audio decoder, video packets onto
//! the video channel.

use std::sync::atomic::Ordering::Relaxed;

use ffmpeg::{CodecContext, Frame, Rational};
use irl_core::{consts, timing};

use crate::audio;
use crate::receiver::audio_in::AudioIntake;
use crate::receiver::{Receiver, ReceiverFlags};
use crate::shared::{Shared, TimedPacket};

/// Count one audio decode error. At a burst, log it (rate limited) and flush
/// the decoder unless a flush is cooling down; returns when it flushed.
fn audio_error_burst(
    dec: &mut CodecContext,
    flags: &mut ReceiverFlags,
    stage: &str,
    flushing: &str,
    cooling: &str,
) -> Option<u64> {
    flags.audio_decode_errors += 1;
    if flags.audio_decode_errors < consts::DECODER_ERROR_BURST {
        return None;
    }
    let now_us = ffmpeg::gettime_us() as u64;
    let do_flush = timing::throttle(
        &mut flags.audio_last_decoder_flush_time_us,
        now_us,
        consts::DECODER_FLUSH_COOLDOWN_US,
    );
    if timing::throttle(
        &mut flags.audio_last_decoder_warning_time_us,
        now_us,
        consts::DECODER_WARNING_INTERVAL_US,
    ) {
        irl_warn!(
            "Audio decoder{stage}: corruption burst ({} consecutive errors){}",
            flags.audio_decode_errors,
            if do_flush { flushing } else { cooling }
        );
    }
    flags.audio_decode_errors = 0;
    if !do_flush {
        return None;
    }
    dec.flush();
    Some(now_us)
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
                if let Some(now_us) = audio_error_burst(
                    dec,
                    flags,
                    " receive",
                    ", resetting audio state",
                    ", reset cooldown active",
                ) {
                    {
                        let mut state = shared.audio_state();
                        if let Some(buf) = shared.audio_buf().as_mut() {
                            buf.flush();
                        }
                        audio::reset_audio_timing_state(&mut state);
                        audio::mark_audio_recovery(
                            &mut state,
                            now_us,
                            consts::AUDIO_RESET_RECOVERY_HOLD_US,
                        );
                    }
                    audio_in.init_pts_repair(audio_tb);
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
/// one. `avcodec_flush_buffers` empties the reference picture buffer, and
/// neither the H.264 nor the HEVC decoder produces a real picture again until
/// the next IDR/CRA (h264dec paints gray until a recovery point; the HEVC
/// decoder synthesizes each missing reference as flat mid-gray). A flush
/// would turn a few damaged frames into one to two seconds of gray. The next
/// intact packet decodes fine without a reset, and the reference chain heals
/// at the next keyframe either way.
impl Receiver {
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
                audio_error_burst(dec, flags, "", ", flushing", ", suppressing repeated flush");
            }
            _ => flags.audio_decode_errors = 0,
        }

        drain_audio_frames(dec, frame, shared, flags, audio_in, audio_tb);
    }

    /// The video half of the read loop: hand the packet to the video thread.
    ///
    /// Nothing is decoded here; see [`crate::shared::VideoChannel`] for why.
    pub(super) fn push_video_packet(&mut self) {
        // Only used to bound the queue by media duration; output timing comes
        // from the decoded frame's own PTS, after repair.
        let pts_ns = self.pkt.pts_or_dts().map_or(0, |pts| {
            ffmpeg::rescale_q(pts, self.video_tb, ffmpeg::NS_TIME_BASE)
        });
        let bytes = self.pkt.size().max(0) as usize;
        let received_ns = obs::time::gettime_ns();
        let dts_ns = self
            .pkt
            .dts_or_pts()
            .map(|dts| ffmpeg::rescale_q(dts, self.video_tb, ffmpeg::NS_TIME_BASE));
        note_video_arrival(&self.shared, received_ns, pts_ns, dts_ns);

        match self.pkt.new_ref() {
            Ok(packet) => self.shared.video.push_packet(
                TimedPacket {
                    packet,
                    pts_ns,
                    bytes,
                    received_ns,
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

/// What the receiver reads off a video packet as it arrives, before queueing
/// it: its PTS for the skew stat, and its decode timestamp for the audio
/// hold, which reads the sender's skew against the newest audio decoded
/// before it, in mux order.
pub fn note_video_arrival(shared: &Shared, received_ns: u64, pts_ns: i64, dts_ns: Option<i64>) {
    shared.conn.video_arrival_pts_ns.store(pts_ns, Relaxed);
    if let Some(dts_ns) = dts_ns {
        audio::hold::observe_video_packet(shared, received_ns, dts_ns);
    }
}
