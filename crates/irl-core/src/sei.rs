//! HEVC `time_code` SEI extraction (ITU-T H.265 D.2.27), port of the C
//! branch's `sync-sei.c`.
//!
//! Parsed here rather than taken from FFmpeg's `AV_FRAME_DATA_S12M_TIMECODE`
//! side data, which the HEVC decoder does produce for this payload. That side
//! data is packed through `av_timecode_get_smpte(avctx->framerate, ...)`, which
//! squeezes the wire format's 9-bit `n_frames` into SMPTE 12M's two-digit frame
//! field (`ff % 40`) and only handles rates above 30fps when
//! `avctx->framerate` is populated. Live MPEG-TS routinely leaves the frame
//! rate unset — the same reason the video thread measures the frame interval
//! itself — so a 60fps sender's frame 45 would silently arrive as 5. Reading
//! the SEI directly keeps the full field and needs no frame rate at all.
//!
//! HEVC only, deliberately. Moblin — the only encoder in the IRL ecosystem
//! currently emitting these — has its H.264 path disabled at the source, so
//! there is nothing to read on that codec. H.264's `pic_timing` SEI is also not
//! parseable in isolation: its leading `cpb_removal_delay` /
//! `dpb_output_delay` fields are present only when the SPS VUI says so, so a
//! correct reader has to track sequence parameter sets. Adding that for a
//! payload no encoder sends would be untestable code, so this returns `None`
//! for H.264 and the source reports "no timecode" — which is the honest
//! answer.

use core::fmt;

/// `prefix_sei_nut`.
const HEVC_NAL_PREFIX_SEI_NUT: u8 = 39;
/// `time_code` SEI payload type.
const HEVC_SEI_TYPE_TIME_CODE: usize = 136;

/// Cap on the unescaped SEI payload buffered per NAL. `time_code` is a handful
/// of bytes; anything larger is another payload type entirely (HDR metadata,
/// user data) and is skipped without copying.
const SEI_SCRATCH_BYTES: usize = 256;

/// One SMPTE-style wall-clock stamp as carried in an HEVC `time_code` SEI.
///
/// Only the sub-hour fields are used for alignment: the sender writes local
/// calendar components, so `hours` differs by whole hours between senders in
/// different time zones and is display-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timecode {
    /// `hours_value`, 0..=23.
    pub hours: u8,
    /// `minutes_value`, 0..=59.
    pub minutes: u8,
    /// `seconds_value`, 0..=59.
    pub seconds: u8,
    /// `n_frames`: the frame index within its second, a 9-bit field.
    pub n_frames: u16,
}

impl fmt::Display for Timecode {
    /// `HH:MM:SS:FF`, the form the stats and the websocket vendor report.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}:{:02}",
            self.hours, self.minutes, self.seconds, self.n_frames
        )
    }
}

// ── Bit reader ───────────────────────────────────────────────

struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl BitReader<'_> {
    fn read(&mut self, count: usize) -> Option<u32> {
        if count == 0 || count > 32 || self.bit_pos + count > self.data.len() * 8 {
            return None;
        }
        let mut value = 0u32;
        for i in 0..count {
            let byte = (self.bit_pos + i) / 8;
            let bit = 7 - ((self.bit_pos + i) % 8);
            value = (value << 1) | u32::from((self.data[byte] >> bit) & 1);
        }
        self.bit_pos += count;
        Some(value)
    }
}

// ── Annex B / RBSP helpers ───────────────────────────────────

/// Strip emulation prevention bytes (`00 00 03` → `00 00`) from an escaped NAL
/// payload into `dst`, stopping once `dst` is full. Returns the bytes written.
///
/// Truncating rather than failing is deliberate: an SEI NAL may hold several
/// messages, and a `time_code` that fits within the scratch is still readable
/// even if something large follows it. A message that ends up straddling the
/// cut is rejected by the payload-size check in [`scan_sei_rbsp`].
fn rbsp_unescape(src: &[u8], dst: &mut [u8]) -> usize {
    let mut out = 0;
    let mut zeros = 0;
    for &b in src {
        if out >= dst.len() {
            break;
        }
        if zeros >= 2 && b == 0x03 {
            // The escape byte itself is dropped, and the zero run restarts:
            // 00 00 03 00 00 03 is two escapes.
            zeros = 0;
            continue;
        }
        dst[out] = b;
        out += 1;
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Locate the next Annex B start code at or after `from`. Returns the index of
/// the first byte after it.
fn next_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some(i + 3);
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some(i + 4);
            }
        }
        i += 1;
    }
    None
}

// ── time_code payload ────────────────────────────────────────

/// H.265 D.2.27 `time_code( payloadSize )`. Only the first clock timestamp is
/// decoded: the spec allows up to three, and a sender that means "now" writes
/// one (Moblin hardcodes `num_clock_ts = 1`).
fn parse_time_code(payload: &[u8]) -> Option<Timecode> {
    let mut br = BitReader {
        data: payload,
        bit_pos: 0,
    };

    // num_clock_ts
    if br.read(2)? < 1 {
        return None;
    }
    // clock_timestamp_flag[0]
    if br.read(1)? == 0 {
        return None;
    }
    // units_field_based_flag(1) + counting_type(5)
    br.read(6)?;
    let full_timestamp_flag = br.read(1)?;
    // discontinuity_flag(1) + cnt_dropped_flag(1)
    br.read(2)?;
    let n_frames = br.read(9)?;

    // Without full_timestamp_flag the fields are individually optional and a
    // sender may omit hours or minutes entirely, which cannot be aligned
    // against a wall clock. Treat it as no timecode rather than guessing.
    if full_timestamp_flag == 0 {
        return None;
    }

    let seconds = br.read(6)?;
    let minutes = br.read(6)?;
    let hours = br.read(5)?;
    if seconds > 59 || minutes > 59 || hours > 23 {
        return None;
    }

    Some(Timecode {
        hours: hours as u8,
        minutes: minutes as u8,
        seconds: seconds as u8,
        n_frames: n_frames as u16,
    })
}

// ── SEI message walk ─────────────────────────────────────────

/// Walk the `sei_message()` list in one unescaped prefix SEI RBSP.
fn scan_sei_rbsp(rbsp: &[u8]) -> Option<Timecode> {
    let size = rbsp.len();
    let mut pos = 0;

    while pos < size {
        // ff_byte runs, both fields. A 0xFF with nothing after it is a
        // truncated message, not a valid extension.
        let mut payload_type = 0usize;
        while pos < size && rbsp[pos] == 0xFF {
            payload_type += 255;
            pos += 1;
        }
        if pos >= size {
            return None;
        }
        payload_type += usize::from(rbsp[pos]);
        pos += 1;

        let mut payload_size = 0usize;
        while pos < size && rbsp[pos] == 0xFF {
            payload_size += 255;
            pos += 1;
        }
        if pos >= size {
            return None;
        }
        payload_size += usize::from(rbsp[pos]);
        pos += 1;

        if payload_size > size - pos {
            return None;
        }

        if payload_type == HEVC_SEI_TYPE_TIME_CODE
            && let Some(tc) = parse_time_code(&rbsp[pos..pos + payload_size])
        {
            return Some(tc);
        }

        pos += payload_size;
    }

    None
}

// ── Entry point ──────────────────────────────────────────────

/// Scan one Annex B access unit for an HEVC `time_code` SEI (`prefix_sei_nut`,
/// payload type 136) and decode the first clock timestamp in it.
///
/// `data` is the packet payload as delivered by the MPEG-TS demuxer. Returns
/// `None` when the packet carries no usable timecode, which is the common
/// case: only the access units the encoder chose to stamp have one.
pub fn find_timecode(data: &[u8]) -> Option<Timecode> {
    if data.len() < 4 {
        return None;
    }

    let mut nal_start = next_start_code(data, 0)?;
    let mut rbsp = [0u8; SEI_SCRATCH_BYTES];

    while nal_start < data.len() {
        let next_nal = next_start_code(data, nal_start);
        // The start code the scan found is preceded by its own two or three
        // leading zero bytes, which are not part of this NAL. Trimming them
        // is unnecessary here: parsing stops at the declared payload size
        // long before reaching them.
        let nal_end = next_nal.unwrap_or(data.len());

        if nal_end > nal_start + 2 {
            let nal_type = (data[nal_start] >> 1) & 0x3F;
            if nal_type == HEVC_NAL_PREFIX_SEI_NUT {
                // Skip the two-byte HEVC NAL header.
                let payload = &data[nal_start + 2..nal_end];
                let rbsp_size = rbsp_unescape(payload, &mut rbsp);
                if rbsp_size > 0
                    && let Some(tc) = scan_sei_rbsp(&rbsp[..rbsp_size])
                {
                    return Some(tc);
                }
            }
        }

        match next_nal {
            Some(next) => nal_start = next,
            None => break,
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pack bits MSB-first into bytes, padding the tail with zeros.
    struct BitWriter {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bits: 0,
            }
        }

        fn put(&mut self, value: u32, count: usize) {
            for i in (0..count).rev() {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = (value >> i) & 1;
                let byte = self.bytes.last_mut().unwrap();
                *byte |= (bit as u8) << (7 - (self.bits % 8));
                self.bits += 1;
            }
        }

        fn finish(self) -> Vec<u8> {
            self.bytes
        }
    }

    /// The `time_code` payload for one full clock timestamp.
    fn time_code_payload(tc: Timecode, full: bool) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(1, 2); // num_clock_ts
        w.put(1, 1); // clock_timestamp_flag[0]
        w.put(0, 1); // units_field_based_flag
        w.put(0, 5); // counting_type
        w.put(u32::from(full), 1); // full_timestamp_flag
        w.put(0, 1); // discontinuity_flag
        w.put(0, 1); // cnt_dropped_flag
        w.put(u32::from(tc.n_frames), 9);
        if full {
            w.put(u32::from(tc.seconds), 6);
            w.put(u32::from(tc.minutes), 6);
            w.put(u32::from(tc.hours), 5);
        }
        w.put(0, 5); // time_offset_length
        w.finish()
    }

    /// An `sei_message()` with the given type and payload, escaped as an RBSP
    /// would be on the wire (emulation prevention applied).
    fn sei_message(payload_type: usize, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut t = payload_type;
        while t >= 255 {
            out.push(0xFF);
            t -= 255;
        }
        out.push(t as u8);
        let mut s = payload.len();
        while s >= 255 {
            out.push(0xFF);
            s -= 255;
        }
        out.push(s as u8);
        out.extend_from_slice(payload);
        out
    }

    fn escape(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut zeros = 0;
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    /// A prefix SEI NAL, with start code and the two-byte HEVC header.
    fn sei_nal(messages: &[u8]) -> Vec<u8> {
        let mut nal = vec![0, 0, 0, 1, HEVC_NAL_PREFIX_SEI_NUT << 1, 1];
        nal.extend(escape(messages));
        nal.push(0x80); // rbsp_trailing_bits
        nal
    }

    fn vcl_nal() -> Vec<u8> {
        // IDR_W_RADL (type 19) with a bit of slice data.
        vec![0, 0, 1, 19 << 1, 1, 0xAF, 0x12, 0x34]
    }

    const TC: Timecode = Timecode {
        hours: 13,
        minutes: 47,
        seconds: 5,
        n_frames: 45,
    };

    #[test]
    fn finds_a_full_time_code_in_a_prefix_sei() {
        let mut au = sei_nal(&sei_message(136, &time_code_payload(TC, true)));
        au.extend(vcl_nal());
        assert_eq!(find_timecode(&au), Some(TC));
        assert_eq!(TC.to_string(), "13:47:05:45");
    }

    #[test]
    fn keeps_the_full_nine_bit_frame_index() {
        // 60fps senders pass two digits; the field itself allows 511.
        let tc = Timecode {
            n_frames: 300,
            ..TC
        };
        let au = sei_nal(&sei_message(136, &time_code_payload(tc, true)));
        assert_eq!(find_timecode(&au).map(|t| t.n_frames), Some(300));
    }

    #[test]
    fn skips_other_sei_messages_and_three_byte_start_codes() {
        let mut messages = sei_message(1, &[0xAA; 7]); // pic_timing-ish filler
        messages.extend(sei_message(5, &[0xBB; 100])); // user_data_unregistered
        messages.extend(sei_message(136, &time_code_payload(TC, true)));
        let mut au = vec![0, 0, 1, 35 << 1, 1, 0x50]; // AUD first
        au.extend(sei_nal(&messages));
        au.extend(vcl_nal());
        assert_eq!(find_timecode(&au), Some(TC));
    }

    #[test]
    fn unescapes_emulation_prevention_bytes() {
        // A payload whose bit pattern contains 00 00 0x, so the escape is
        // exercised: seconds=0, minutes=0, hours=0, n_frames=0 yields runs
        // of zero bytes.
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            n_frames: 0,
        };
        let msg = sei_message(136, &time_code_payload(tc, true));
        let nal = sei_nal(&msg);
        // The escaped NAL really does differ from the raw message.
        assert!(nal.len() > msg.len() + 6);
        assert_eq!(find_timecode(&nal), Some(tc));
        let raw = super::rbsp_unescape(&[0, 0, 3, 0, 0, 3, 1], &mut [0u8; 8]);
        assert_eq!(raw, 5);
    }

    #[test]
    fn rejects_partial_timestamps_and_out_of_range_fields() {
        let partial = sei_nal(&sei_message(136, &time_code_payload(TC, false)));
        assert_eq!(find_timecode(&partial), None);

        let bad = Timecode { seconds: 61, ..TC };
        let au = sei_nal(&sei_message(136, &time_code_payload(bad, true)));
        assert_eq!(find_timecode(&au), None);
    }

    #[test]
    fn ignores_h264_and_junk() {
        // An H.264 SEI NAL (type 6) is a single-byte header whose low bits do
        // not decode to prefix_sei_nut, so nothing is read from it.
        let mut h264 = vec![0, 0, 0, 1, 0x06];
        h264.extend(sei_message(1, &[1, 2, 3]));
        h264.extend([0x80, 0, 0, 1, 0x65, 0x88, 0x84]);
        assert_eq!(find_timecode(&h264), None);

        assert_eq!(find_timecode(&[]), None);
        assert_eq!(find_timecode(&[0, 0, 1]), None);
        assert_eq!(find_timecode(&[0xFF; 64]), None);
        // A truncated SEI: type present, size byte missing.
        let truncated = vec![0, 0, 1, HEVC_NAL_PREFIX_SEI_NUT << 1, 1, 0x88];
        assert_eq!(find_timecode(&truncated), None);
        // A declared payload longer than the NAL.
        let overlong = vec![0, 0, 1, HEVC_NAL_PREFIX_SEI_NUT << 1, 1, 0x88, 0x40, 0x11];
        assert_eq!(find_timecode(&overlong), None);
    }

    #[test]
    fn a_time_code_past_the_scratch_cap_is_not_read() {
        // A large message first pushes the time_code past the 256-byte
        // scratch; it is cut off rather than mis-parsed.
        let mut messages = sei_message(5, &[0x11; 400]);
        messages.extend(sei_message(136, &time_code_payload(TC, true)));
        assert_eq!(find_timecode(&sei_nal(&messages)), None);
    }
}
