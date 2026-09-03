//! Video decode (the video half of `irl_handle_video_packet` /
//! `drain_video_frames`, moved off the receiver thread).
//!
//! It lives here rather than in `receiver/` because the receiver must not be
//! what decides *when* a packet is decoded. The stream's latency is held as
//! compressed packets, and decode happens only as the frames come due — but the
//! receiver spends most of its life blocked in `av_read_frame`, and a network
//! stall is exactly when it blocks longest and when video most needs to keep
//! draining what it already has.

use std::sync::atomic::Ordering::Relaxed;

use irl_core::consts;

use crate::shared::{Shared, VideoDecoder};
use crate::video::intake::{self, DecodeState};

/// How long the packet-level keyframe gate waits before giving up and feeding
/// the decoder whatever arrives (`receiver-decode.c`).
const VIDEO_PKT_GATE_TIMEOUT_US: u64 = 5_000_000;

fn should_log_warning(last_warning_us: &mut u64, now_us: u64) -> bool {
    if *last_warning_us != 0 && now_us - *last_warning_us < consts::DECODER_WARNING_INTERVAL_US {
        return false;
    }
    *last_warning_us = now_us;
    true
}

fn note_decode_error(shared: &Shared, state: &mut DecodeState, stage: &str) {
    state.decode_errors += 1;
    shared.video_flags.corrupted.store(true, Relaxed);
    if state.decode_errors < consts::DECODER_ERROR_BURST {
        return;
    }
    let now_us = ffmpeg::gettime_us() as u64;
    if should_log_warning(&mut state.last_warning_us, now_us) {
        irl_warn!(
            "Video decoder {stage}: corruption burst ({} consecutive errors), waiting for the next keyframe",
            state.decode_errors
        );
    }
    state.decode_errors = 0;
}

/// Whether the packet-level keyframe gate lets this packet through.
///
/// Feeding a decoder mid-GOP produces reference-miss error spam and decoder
/// churn for no picture, so on join the decoder is not fed at all until a key
/// packet arrives — with a timeout, because some senders never mark them.
fn gate_open(shared: &Shared, state: &mut DecodeState, is_key: bool) -> bool {
    if !shared.hot.wait_for_keyframe.load(Relaxed)
        || shared.video_flags.first_keyframe.load(Relaxed)
        || state.pkt_gate_open
    {
        return true;
    }
    if is_key {
        state.pkt_gate_open = true;
        return true;
    }
    let now_us = ffmpeg::gettime_us() as u64;
    if state.pkt_gate_start_us == 0 {
        state.pkt_gate_start_us = now_us;
    }
    if now_us - state.pkt_gate_start_us < VIDEO_PKT_GATE_TIMEOUT_US {
        return false;
    }
    state.pkt_gate_open = true;
    true
}

/// Everything the decoder has ready, appended to `out`.
fn drain(
    decoder: &mut VideoDecoder,
    scratch: &mut ffmpeg::Frame,
    shared: &Shared,
    state: &mut DecodeState,
    out: &mut Vec<ffmpeg::Frame>,
) {
    loop {
        match decoder.ctx.receive_frame(scratch) {
            Err(err) if err.is_eagain() || err.is_eof() => return,
            Err(_) => {
                note_decode_error(shared, state, "receive");
                return;
            }
            Ok(()) => {
                state.decode_errors = 0;
                if let Some(frame) = intake::handle_frame(
                    shared,
                    state,
                    scratch,
                    decoder.time_base,
                    decoder.codec_id,
                ) {
                    out.push(frame);
                }
                scratch.unref();
            }
        }
    }
}

/// Decode one packet, returning every frame it produced that should be paced.
pub fn decode_packet(
    decoder: &mut VideoDecoder,
    scratch: &mut ffmpeg::Frame,
    shared: &Shared,
    state: &mut DecodeState,
    packet: &ffmpeg::Packet,
    out: &mut Vec<ffmpeg::Frame>,
) {
    if !gate_open(shared, state, packet.is_key()) {
        return;
    }

    let mut result = decoder.ctx.send_packet(packet);
    if result.as_ref().is_err_and(ffmpeg::Error::is_eagain) {
        // The decoder refused the packet: it has output waiting and, on
        // fixed-pool hardware decoders, no free surface until we take it.
        // FFmpeg's contract is to read the output and resend the same packet —
        // falling through would discard it, and with a reference frame that
        // costs artifacts until the next keyframe rather than one dropped
        // frame.
        //
        // One retry, deliberately not a loop: a single drain frees every
        // surface the decoder was waiting on, and a decoder that returned
        // EAGAIN without producing anything would otherwise spin here.
        shared.lifetime.video_pkt_eagain.fetch_add(1, Relaxed);
        drain(decoder, scratch, shared, state, out);
        result = decoder.ctx.send_packet(packet);
        if result.as_ref().is_err_and(ffmpeg::Error::is_eagain) {
            shared.lifetime.video_pkt_dropped.fetch_add(1, Relaxed);
        }
    }

    match &result {
        Err(err) if !err.is_eagain() && !err.is_eof() => note_decode_error(shared, state, "send"),
        _ => state.decode_errors = 0,
    }

    drain(decoder, scratch, shared, state, out);
}
