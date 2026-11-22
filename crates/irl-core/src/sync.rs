//! Timecode sync: absolute-time presentation control. Designed by Spillmaker
//! for the C plugin's `nal-timecodes` branch.
//!
//! Every synced source follows one rule: the frame stamped T is presented at
//! NTP time T + offset. Sources do not negotiate; the alignment falls out of
//! sharing a clock. Two OBS instances anywhere, on the same NTP reference and
//! offset, composite the same captured moment at the same instant, which is
//! why the offset is one global number rather than a per-source setting.
//!
//! There is one actuator, a delay line of *encoded* packets in front of the
//! decoders ([`DelayLine`]), and one loop driving it ([`SyncController`]).
//! Holding compressed packets is a requirement, not an optimisation: a second
//! of a 6Mbit/s feed is under a megabyte compressed and ~190MB decoded at
//! 1080p60.
//!
//! Nothing in the audio pipeline changes. Delaying the input shifts the whole
//! PTS-to-OBS mapping with it, so the jitter buffer, the speed controller and
//! the video pacing simply see the stream arrive later. Every rate below is
//! set under the speed controller's -2%/+5% authority for that reason.
//!
//! Every input (clocks, NTP reference, playout mapping, settings) is passed in
//! per packet, so the whole loop runs on a virtual clock in tests. The plugin
//! crate owns the packets, the locks and the log lines.

use std::collections::VecDeque;

use crate::arrival::ArrivalFloor;
use crate::consts;
use crate::playout::PlayoutMapping;
use crate::sei::Timecode;

const NS_PER_MS: i64 = 1_000_000;
/// Latency is read modulo a quarter hour: every time zone is a whole number
/// of quarter hours from UTC.
const NS_PER_FOLD: i64 = 15 * 60 * 1_000_000_000;

// ── Status ───────────────────────────────────────────────────

/// Per-source sync status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncStatus {
    /// Sync off globally, this source not opted in, or no stream arriving.
    /// Not an alarm state: a disconnected feed is normal before going live.
    #[default]
    Off,
    /// Opted in, but no SEI timecode in the stream or no NTP reference on
    /// this machine. The most common reason sync "does not work", and not a
    /// failure of the sync itself.
    NoTimecode,
    /// The source stopped publishing status. Set by the registry, never by
    /// the controller.
    Stale,
    /// The feed arrives later than the offset, so its frames are past their
    /// slot on arrival. Only a larger offset or a better uplink fixes this.
    TooSlow,
    /// Converging. Expected at startup and after an offset change.
    Acquiring,
    /// Aligned, within a frame or two.
    Locked,
}

impl SyncStatus {
    /// The wire name (`sync_status`, the websocket `status`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::NoTimecode => "no_timecode",
            Self::Stale => "stale",
            Self::TooSlow => "too_slow",
            Self::Acquiring => "acquiring",
            Self::Locked => "locked",
        }
    }
}

/// Why a source that wants to sync has no timecode to align against.
///
/// "No timecode" alone sends people hunting in the wrong place; the cause is
/// almost always one of three things, each with a different fix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TcReason {
    /// Timecodes are arriving.
    #[default]
    Ok,
    /// This machine has no NTP reference.
    NoClock,
    /// The stream is not H.265. Moblin only writes the SEI on HEVC, and the
    /// `time_code` SEI this reads is HEVC-specific.
    Codec,
    /// HEVC, but no `time_code` SEI is arriving: Timecodes off on the sender,
    /// no NTP pool set there, or a transport that drops the SEI.
    Absent,
}

impl TcReason {
    /// The wire name (`timecode_reason`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NoClock => "no_clock",
            Self::Codec => "codec",
            Self::Absent => "absent",
        }
    }
}

/// What one source publishes for the dock and the websocket vendor.
///
/// The three latency figures answer different questions and stay separate:
/// `latency_ms` is how stale the freshest data is (sender and network only),
/// `added_ms` is the hold this plugin applies to reach the target, and
/// `error_ms` is how far presentation lands from it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncSnapshot {
    /// Current status.
    pub status: SyncStatus,
    /// Only meaningful while `status` is [`SyncStatus::NoTimecode`].
    pub tc_reason: TcReason,
    /// The sender's most recent timecode, if one is on display.
    pub timecode: Option<Timecode>,
    /// How stale the freshest received frame is, by its timecode against NTP.
    pub latency_ms: i64,
    /// Rolling 60s maximum of `latency_ms`. Instantaneous latency reads fine
    /// right up until a bitrate dip, so an offset is chosen against this.
    pub latency_peak_ms: i64,
    /// Extra hold applied to reach the target presentation time.
    pub added_ms: i64,
    /// How far presentation lands from the target, averaged over about a
    /// second. Near zero when locked.
    pub error_ms: i64,
    /// `latency_peak_ms` rounded up to a whole second. Not padded: a source
    /// locks whenever the offset is at or above its arrival latency, so a
    /// padded figure reads as a bug the first time someone locks below it.
    pub required_offset_ms: i64,
}

/// A source that has stopped publishing is stale, whatever it last said,
/// unless that was Off.
pub fn apply_staleness(snap: &mut SyncSnapshot, updated_ns: u64, now_ns: u64) {
    if snap.status == SyncStatus::Off {
        return;
    }
    if updated_ns != 0 && now_ns.wrapping_sub(updated_ns) < consts::SYNC_PUBLISH_STALE_NS {
        return;
    }
    snap.status = SyncStatus::Stale;
    snap.error_ms = 0;
}

/// The smallest offset the control can be set to that still clears
/// `peak_ms`: rounded up to [`consts::SYNC_OFFSET_STEP_MS`].
pub fn required_offset_ms(peak_ms: i64) -> i64 {
    if peak_ms <= 0 {
        return 0;
    }
    let step = i64::from(consts::SYNC_OFFSET_STEP_MS);
    (peak_ms + step - 1) / step * step
}

/// Clamp an offset into the settable range.
pub fn clamp_offset_ms(offset_ms: i32) -> i32 {
    offset_ms.clamp(consts::SYNC_MIN_OFFSET_MS, consts::SYNC_MAX_OFFSET_MS)
}

// ── Timecode to latency ──────────────────────────────────────

/// How old the content stamped `tc` is, from its timecode against NTP.
///
/// Compared modulo a quarter hour, reduced to the nearest representative. A
/// sender may write local calendar components, and every time zone (India's
/// +5:30 and Nepal's +5:45 included) is a whole number of quarter hours from
/// UTC; the timecode carries no date either, so it wraps at midnight. A real
/// arrival latency is seconds, never minutes, so the fold absorbs both.
pub fn timecode_latency_ns(utc_now_ns: i64, tc: &Timecode, frame_interval_ns: i64) -> i64 {
    let now_in_fold = utc_now_ns.rem_euclid(NS_PER_FOLD);
    let tc_in_fold = (i64::from(tc.minutes) * 60 + i64::from(tc.seconds)) * 1_000_000_000
        + i64::from(tc.n_frames) * frame_interval_ns;

    let diff = (now_in_fold - tc_in_fold).rem_euclid(NS_PER_FOLD);
    if diff >= NS_PER_FOLD / 2 {
        diff - NS_PER_FOLD
    } else {
        diff
    }
}

// ── Playout anchor ───────────────────────────────────────────

/// Fold one raw playout-offset reading into the smoothed `present_bias`.
///
/// Where content should play for the stream to land on its offset, expressed
/// as the audio playout offset it implies (`obs_ts - pts`). Smoothed, because
/// a single reading inherits the timecode's one-frame grid (33ms at 30fps,
/// most of the lock tolerance) and the anchor is a one-shot placement. Eight
/// samples, a third of a second at 30fps, average the grid away without
/// lagging an offset change the loop would have to unpick.
pub fn fold_present_bias(current: Option<i64>, raw_ns: i64) -> i64 {
    match current {
        Some(bias) => bias + (raw_ns - bias) / consts::SYNC_BIAS_SMOOTHING_DIV,
        None => raw_ns,
    }
}

/// Where the audio holding `pts_ns` has to start playing, in the OBS clock,
/// for the stream to land on its configured offset: `(anchor_ns, wait_ns)`.
///
/// `None` with no bias yet (no timecodes, no NTP reference, sync off), or when
/// the placement is already in the past or further out than the jitter buffer
/// should hold. The delay line takes over the wait so the pre-roll does not
/// land in the jitter buffer.
pub fn playout_anchor(
    pts_ns: i64,
    present_bias_ns: Option<i64>,
    now_ns: u64,
) -> Option<(u64, i64)> {
    if pts_ns <= 0 {
        return None;
    }
    let bias_ns = present_bias_ns?;
    // The playout offset the loop spends its life steering toward, applied
    // here in one step because nothing is playing yet to disturb.
    let placed_ns = pts_ns + bias_ns;
    let wait_ns = placed_ns - now_ns as i64;
    if wait_ns <= 0 || wait_ns > consts::SYNC_ANCHOR_MAX_WAIT_NS {
        return None;
    }
    Some((placed_ns as u64, wait_ns))
}

// ── Delay line ───────────────────────────────────────────────

/// Encoded packets held in front of the decoders until their release time.
///
/// Storage, not steering: moving the hold moves content between this line and
/// the jitter buffer, and the total delay is unchanged. Only the speed
/// controller draining or building that buffer presents anything earlier or
/// later.
///
/// One queue per stream. A decoder must see its own stream in order, but
/// audio and video go to different decoders, and released on the media
/// timeline ([`ReleaseClock`]) video that a stall delayed is due before audio
/// that arrived with it.
pub struct DelayLine<T> {
    streams: Vec<Stream<T>>,
    len: usize,
    bytes: usize,
    /// Breaks ties between streams in arrival order.
    seq: u64,
    /// Packets the line could not take. The read loop applies backpressure
    /// before the ceilings, so a non-zero count is reported, not absorbed.
    overflows: u64,
    max_packets: usize,
    max_bytes: usize,
}

struct Stream<T> {
    key: u32,
    entries: VecDeque<Entry<T>>,
    /// Release times are clamped non-decreasing within a stream: the hold
    /// moves while packets are queued, and a decoder must never see them
    /// reordered.
    last_release_ns: u64,
}

struct Entry<T> {
    item: T,
    bytes: usize,
    release_ns: u64,
    seq: u64,
}

impl<T> DelayLine<T> {
    /// A line bounded by `max_packets` entries and `max_bytes` of payload.
    pub fn new(max_packets: usize, max_bytes: usize) -> Self {
        Self {
            streams: Vec::new(),
            len: 0,
            bytes: 0,
            seq: 0,
            overflows: 0,
            max_packets,
            max_bytes,
        }
    }

    /// Either ceiling reached.
    pub fn is_full(&self) -> bool {
        self.len >= self.max_packets || self.bytes >= self.max_bytes
    }

    /// Nothing queued.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Entries queued.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Payload bytes queued.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Packets refused so far this connection.
    pub fn overflows(&self) -> u64 {
        self.overflows
    }

    /// Count one refused packet; returns the count *before* this one, so the
    /// caller can warn on the first.
    pub fn note_overflow(&mut self) -> u64 {
        let before = self.overflows;
        self.overflows += 1;
        before
    }

    /// Queue `item` of stream `stream` for release at `release_ns`, clamped so
    /// releases within the stream never overtake each other. Returns `false`,
    /// dropping `item`, when the line is at a ceiling.
    pub fn push(&mut self, stream: u32, item: T, bytes: usize, release_ns: u64) -> bool {
        if self.is_full() {
            return false;
        }
        let index = match self.streams.iter().position(|s| s.key == stream) {
            Some(index) => index,
            None => {
                self.streams.push(Stream {
                    key: stream,
                    entries: VecDeque::new(),
                    last_release_ns: 0,
                });
                self.streams.len() - 1
            }
        };
        let lane = &mut self.streams[index];
        let release_ns = release_ns.max(lane.last_release_ns);
        lane.last_release_ns = release_ns;
        lane.entries.push_back(Entry {
            item,
            bytes,
            release_ns,
            seq: self.seq,
        });
        self.seq += 1;
        self.len += 1;
        self.bytes += bytes;
        true
    }

    /// The stream whose head comes due first, ties in arrival order.
    fn next_stream(&mut self) -> Option<&mut Stream<T>> {
        self.streams
            .iter_mut()
            .filter(|s| !s.entries.is_empty())
            .min_by_key(|s| s.entries.front().map(|e| (e.release_ns, e.seq)))
    }

    /// The earliest due head entry, if its moment has come.
    pub fn pop_due(&mut self, now_ns: u64) -> Option<T> {
        let lane = self.next_stream()?;
        if lane.entries.front()?.release_ns > now_ns {
            return None;
        }
        let entry = lane.entries.pop_front()?;
        self.len -= 1;
        self.bytes -= entry.bytes;
        Some(entry.item)
    }

    /// When the next packet comes due, if anything is queued.
    pub fn next_release_ns(&self) -> Option<u64> {
        self.streams
            .iter()
            .filter_map(|s| s.entries.front().map(|e| e.release_ns))
            .min()
    }

    /// Move every queued release time later by `shift_ns`.
    ///
    /// Only ever later: earlier would make the whole line due at once, which
    /// [`consts::SYNC_RELEASE_RATE`] exists to prevent. Later opens a gap, safe
    /// only while something downstream holds enough to cover it (the pre-roll
    /// takeover at prime).
    pub fn shift_later(&mut self, shift_ns: i64) {
        let shift = shift_ns.max(0) as u64;
        for lane in &mut self.streams {
            for entry in &mut lane.entries {
                entry.release_ns += shift;
            }
            lane.last_release_ns += shift;
        }
    }

    /// Drop everything and re-arm the one-shot overflow warning.
    pub fn clear(&mut self) {
        self.streams.clear();
        self.len = 0;
        self.bytes = 0;
        self.overflows = 0;
    }
}

// ── Release clock ────────────────────────────────────────────

/// When a held packet is released: at its place on its stream's timeline, a
/// hold after the earliest any packet of that stream has arrived relative to
/// its DTS.
///
/// Releasing a hold after each packet's own arrival passes every gap on the
/// link straight through, a hold later. Releasing on the timeline gives a
/// packet that a gap delayed the slot it would have had, so a gap shorter
/// than the hold never reaches the decoders. A packet that arrives after its
/// slot goes at once.
///
/// The floor is `arrival − DTS` at its lowest, per stream, and each stream is
/// anchored on its own. A sender whose video trails its audio keeps that skew
/// through the line, so the audio hold downstream reads the same skew it
/// would without sync; what the line takes out is each stream's jitter. A
/// higher reading is a packet the network delayed. A stream whose DTS goes back by more than
/// [`consts::SYNC_JUMP_TOLERANCE_NS`] restarted its timestamps (a sender
/// restart without a reconnect), since a gap delays packets but never
/// reorders them, and the floors start over. A jump forward reads as a new
/// floor on its own.
#[derive(Debug, Default)]
pub struct ReleaseClock {
    floors: Vec<Floor>,
}

#[derive(Debug, Clone, Copy)]
struct Floor {
    stream: u32,
    floor_ns: i64,
    last_dts_ns: i64,
}

impl ReleaseClock {
    /// When a packet of `stream` with `dts_ns` arriving at `arrival_ns` is
    /// released, under `hold_ns`. A packet without a DTS is released a hold
    /// after it arrives.
    pub fn release_ns(
        &mut self,
        stream: u32,
        arrival_ns: u64,
        dts_ns: Option<i64>,
        hold_ns: i64,
    ) -> u64 {
        let hold_ns = hold_ns.max(0);
        let Some(dts_ns) = dts_ns else {
            return arrival_ns + hold_ns as u64;
        };
        let lateness_ns = arrival_ns as i64 - dts_ns;

        let fresh = Floor {
            stream,
            floor_ns: lateness_ns,
            last_dts_ns: dts_ns,
        };
        match self.floors.iter().position(|f| f.stream == stream) {
            None => self.floors.push(fresh),
            Some(index) => {
                if dts_ns < self.floors[index].last_dts_ns - consts::SYNC_JUMP_TOLERANCE_NS {
                    self.floors.clear();
                    self.floors.push(fresh);
                } else {
                    let floor = &mut self.floors[index];
                    floor.floor_ns = floor.floor_ns.min(lateness_ns);
                    floor.last_dts_ns = dts_ns;
                }
            }
        }

        let floor_ns = self
            .floors
            .iter()
            .find(|f| f.stream == stream)
            .map_or(lateness_ns, |f| f.floor_ns);
        let slot_ns = dts_ns + floor_ns + hold_ns;
        slot_ns.max(arrival_ns as i64) as u64
    }

    /// Forget the timeline, for a new connection.
    pub fn reset(&mut self) {
        self.floors.clear();
    }
}

// ── Rolling latency peak ─────────────────────────────────────

/// Rolling peak of arrival latency, bucketed so it ages out: 12 buckets of
/// 5s. The peak, not the instantaneous value, is what an offset has to be
/// chosen against.
#[derive(Debug, Clone)]
pub struct PeakTracker {
    buckets: [i64; consts::SYNC_PEAK_BUCKETS],
    bucket: usize,
    bucket_start_ns: u64,
}

impl Default for PeakTracker {
    fn default() -> Self {
        Self {
            buckets: [i64::MIN; consts::SYNC_PEAK_BUCKETS],
            bucket: 0,
            bucket_start_ns: 0,
        }
    }
}

impl PeakTracker {
    /// Fold one latency reading in at `now_ns`.
    pub fn record(&mut self, latency_ns: i64, now_ns: u64) {
        if self.bucket_start_ns == 0 {
            self.bucket_start_ns = now_ns;
        }
        while now_ns.wrapping_sub(self.bucket_start_ns) >= consts::SYNC_PEAK_BUCKET_NS {
            self.bucket = (self.bucket + 1) % consts::SYNC_PEAK_BUCKETS;
            self.buckets[self.bucket] = i64::MIN;
            self.bucket_start_ns += consts::SYNC_PEAK_BUCKET_NS;
        }
        if latency_ns > self.buckets[self.bucket] {
            self.buckets[self.bucket] = latency_ns;
        }
    }

    /// The peak over the window, or 0 when nothing has been recorded.
    pub fn value(&self) -> i64 {
        let peak = self.buckets.iter().copied().max().unwrap_or(i64::MIN);
        if peak == i64::MIN { 0 } else { peak }
    }
}

// ── Error filter ─────────────────────────────────────────────

/// Trimmed mean of the presentation error over the last second.
///
/// One reading carries about a frame of noise: it is the *predicted*
/// presentation time of one access unit, from an audio playout mapping that
/// steps as audio is submitted, against a timecode with frame resolution. The
/// noise is zero-mean and the true error moves far slower, so filtering lets
/// the loop correct on a reading that is not a frame of dither and keeps the
/// operator's number steady while the alignment is.
///
/// A trimmed mean rather than a median: both reject a corrupt timecode, but a
/// median *returns* one of the samples, so the loop parks on the nearest
/// quantisation step instead of on zero. Averaging what is left after the
/// tails are cut lands between the steps.
///
/// Bounded by time, not sample count, so 25fps and 240fps behave the same.
#[derive(Debug, Clone)]
pub struct ErrorFilter {
    window: [i64; consts::SYNC_ERROR_SAMPLES],
    time: [u64; consts::SYNC_ERROR_SAMPLES],
    head: usize,
    count: usize,
}

impl Default for ErrorFilter {
    fn default() -> Self {
        Self {
            window: [0; consts::SYNC_ERROR_SAMPLES],
            time: [0; consts::SYNC_ERROR_SAMPLES],
            head: 0,
            count: 0,
        }
    }
}

impl ErrorFilter {
    /// Samples currently in the window.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Empty the window.
    pub fn reset(&mut self) {
        self.head = 0;
        self.count = 0;
    }

    /// Fold one reading in and return the filtered window value.
    pub fn push(&mut self, error_ns: i64, now_ns: u64) -> i64 {
        const N: usize = consts::SYNC_ERROR_SAMPLES;

        if self.count == N {
            // Full before the window expired: a rate above what the interval
            // bounds accept, or timecodes on every field of an interlaced
            // feed. Dropping the oldest keeps the window to the most recent
            // second's worth either way.
            self.head = (self.head + 1) % N;
            self.count -= 1;
        }

        let tail = (self.head + self.count) % N;
        self.window[tail] = error_ns;
        self.time[tail] = now_ns;
        self.count += 1;

        // Age out anything past the window. Never empties: the sample just
        // pushed is by definition current.
        while self.count > 1
            && now_ns.wrapping_sub(self.time[self.head]) > consts::SYNC_ERROR_WINDOW_NS
        {
            self.head = (self.head + 1) % N;
            self.count -= 1;
        }

        let mut sorted = [0i64; N];
        for (i, slot) in sorted.iter_mut().enumerate().take(self.count) {
            *slot = self.window[(self.head + i) % N];
        }
        let sorted = &mut sorted[..self.count];
        sorted.sort_unstable();

        // A tenth off each end loses a corrupt timecode without losing the
        // distribution's shape. Below ten samples a tenth rounds to nothing,
        // so trim one instead, or one bad sample in three on a feed that has
        // just started passes straight through. At three samples this is the
        // median.
        let mut trim = self.count / 10;
        if trim == 0 && self.count >= 3 {
            trim = 1;
        }
        let kept = self.count - 2 * trim;
        let sum: i64 = sorted[trim..trim + kept].iter().sum();
        sum / kept as i64
    }
}

// ── Frame rate ───────────────────────────────────────────────

/// The frame rate learned from the timecodes themselves, so converting
/// `n_frames` to time never assumes one.
///
/// Not from the decoder: `n_frames` is an index within its timecode second, so
/// the highest index a second carries is its frame count, and converting with
/// it is self-consistent by construction. It is also on the receiver thread at
/// the first complete second, while the decoder's measurement is cross-thread
/// and only exists once frames are past the keyframe gate.
///
/// Taken from agreement between seconds, so neither a lossy second nor a
/// corrupt SEI can push a wrong figure into every later latency reading.
#[derive(Debug, Clone, Default)]
pub struct FpsLearner {
    interval_ns: i64,
    recent: [u16; consts::SYNC_FPS_SECONDS],
    next: usize,
    count: usize,
    /// The timecode second being accumulated, and the highest frame index
    /// seen in it so far.
    max_frames: u16,
    second: u8,
    have_second: bool,
}

impl FpsLearner {
    /// The learned interval, once a complete second has been seen and agreed.
    pub fn interval_ns(&self) -> Option<i64> {
        (self.interval_ns > 0).then_some(self.interval_ns)
    }

    /// Fold one timecode in. Returns the new frames-per-second whenever the
    /// learned rate changes, for the log.
    pub fn observe(&mut self, tc: &Timecode) -> Option<u32> {
        if self.have_second && tc.seconds == self.second {
            if tc.n_frames > self.max_frames {
                self.max_frames = tc.n_frames;
            }
            return None;
        }

        // A second just ended, so its highest index is one full frame count.
        // A second joined partway counts too: indices ascend within a second,
        // so joining late still sees its tail, where the maximum is.
        let learned = if self.have_second {
            self.record(self.max_frames)
        } else {
            None
        };

        self.second = tc.seconds;
        self.max_frames = tc.n_frames;
        self.have_second = true;
        learned
    }

    fn record(&mut self, max_frames: u16) -> Option<u32> {
        self.recent[self.next] = max_frames;
        self.next = (self.next + 1) % consts::SYNC_FPS_SECONDS;
        if self.count < consts::SYNC_FPS_SECONDS {
            self.count += 1;
        }

        // The count the recent seconds agree on, not the highest. A maximum
        // survives loss, which only shortens a second, but a corrupt SEI's
        // n_frames is as likely high as low and a maximum adopts it outright:
        // a 30fps sender read as 41fps puts a quarter-second sawtooth into
        // every latency reading until it ages out. Requiring two seconds to
        // agree rejects both, because loss and corruption produce one-off
        // values. Ties go to the larger count, since between two plausible
        // readings the short one is the damaged one.
        let mut agreed: u16 = 0;
        let mut best_votes = 0;
        for i in 0..self.count {
            let candidate = self.recent[i];
            let votes = self.recent[..self.count]
                .iter()
                .filter(|&&v| v == candidate)
                .count();
            if votes > best_votes || (votes == best_votes && candidate > agreed) {
                best_votes = votes;
                agreed = candidate;
            }
        }

        if best_votes < 2 {
            return None;
        }

        // Indices are zero-based, so the count is one more than the highest.
        let frames = u32::from(agreed) + 1;
        let interval_ns = 1_000_000_000 / i64::from(frames);

        // Outside any real stream's rate: a corrupt SEI, or a sender counting
        // something other than frames. Keep what was already learned.
        if !(consts::VIDEO_INTERVAL_MIN_NS..=consts::VIDEO_INTERVAL_MAX_NS).contains(&interval_ns) {
            return None;
        }

        if self.interval_ns != interval_ns {
            self.interval_ns = interval_ns;
            return Some(frames);
        }
        None
    }
}

// ── Controller inputs ────────────────────────────────────────

/// The video stream's codec, for diagnosing a missing timecode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// No video stream open.
    None,
    /// H.265, the one codec that can carry the SEI.
    Hevc,
    /// Anything else, with FFmpeg's name for the log line.
    Other(&'static str),
}

/// The audio playout mapping as it stands, for predicting when a packet with
/// a given PTS reaches the screen.
///
/// The audio playout offset is authoritative: the video path puts every frame
/// through it, and it already accounts for what the delay line holds, because
/// delaying the input shifts the mapping with it. The prediction is where
/// presentation will settle, not where it is: the speed controller takes the
/// jitter buffer back toward its target on its own, and a loop that also
/// steered out that excess would fight it and wind up.
#[derive(Debug, Clone, Copy, Default)]
pub struct PresentationMap {
    /// The latest chunk the audio pump handed to OBS.
    pub audio: PlayoutMapping,
    /// How much of the jitter buffer's distance from its target the speed
    /// controller will take out on its own (negative: build up).
    pub audio_excess_ns: i64,
}

impl PresentationMap {
    /// When content with `pts_ns` will reach the screen once the speed
    /// controller has settled, in the OBS clock. `None` until audio plays.
    pub fn predict(&self, pts_ns: i64) -> Option<i64> {
        self.audio
            .offset_ns()
            .map(|offset_ns| pts_ns + offset_ns - self.audio_excess_ns)
    }
}

/// Everything the controller needs to know about one packet and the world
/// around it.
pub struct Observation<'a> {
    /// OBS clock.
    pub now_ns: u64,
    /// UTC per the NTP reference, or `None` while unsynced.
    pub utc_now_ns: Option<i64>,
    /// Whether the controller may act: sync on globally and this source
    /// opted in. Reading the timecodes does not depend on it.
    pub wanted: bool,
    /// The shared offset.
    pub offset_ms: i32,
    /// Bumped whenever the offset changes, so the hold steps instead of
    /// slewing.
    pub offset_generation: u32,
    /// How long content takes from leaving the delay line to playing, with
    /// the jitter buffer at its target: the part of the offset the seed
    /// leaves to the pipeline.
    pub nominal_pipeline_ns: i64,
    /// Whether the connection carries audio. Without it there is no playout
    /// mapping to anchor or measure against.
    pub has_audio: bool,
    /// The video thread's measured frame interval (0 when unknown), the
    /// fallback before the first complete timecode second.
    pub decoder_interval_ns: i64,
    /// The video stream's codec.
    pub video_codec: VideoCodec,
    /// The timecode this packet carried, if it was a video packet with one.
    pub timecode: Option<Timecode>,
    /// The packet's PTS in nanoseconds (video packets with a PTS only).
    pub pkt_pts_ns: Option<i64>,
    /// The playout mapping, read lazily: it is only needed for a controlled,
    /// timecoded packet with a PTS, and reading it takes a lock.
    pub presentation: &'a mut dyn FnMut() -> PresentationMap,
    /// Pre-roll the audio output handed back at prime, to be absorbed by the
    /// delay line; taken once.
    pub anchor_defer_ns: Option<i64>,
}

/// What the controller decided, for the plugin to act on.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Things worth a log line, in order.
    pub events: Vec<SyncEvent>,
    /// Shift every queued release time later by this much (the pre-roll
    /// takeover). Zero when nothing was taken over.
    pub shift_delay_ns: i64,
    /// A raw playout-offset reading to fold into `present_bias` with
    /// [`fold_present_bias`].
    pub bias_sample_ns: Option<i64>,
    /// Whether the audio output must not prime yet.
    pub prime_hold: bool,
    /// Drop the folded playout bias: sync has stopped placing this source,
    /// and a later restart of the output clock must not use a stale one.
    pub forget_bias: bool,
    /// A snapshot to publish to the registry (about 5Hz).
    pub publish: Option<SyncSnapshot>,
}

/// A log-worthy transition, rendered as a log line by the plugin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyncEvent {
    /// The missing-timecode diagnosis changed (or timecodes were detected).
    TcReason {
        /// The new reason.
        reason: TcReason,
        /// The codec's name when the reason is [`TcReason::Codec`].
        codec: Option<&'static str>,
    },
    /// The learned frame rate changed.
    FpsLearned(u32),
    /// The hold was seeded.
    Engaged {
        /// Frames per second the conversion uses.
        fps: f64,
        /// `"timecodes"` or `"decoder"`.
        origin: &'static str,
        /// Arrival latency at engage.
        latency_ms: i64,
        /// The offset engaged against.
        offset_ms: i32,
        /// The seeded hold.
        hold_ms: i64,
        /// The deliberate over-hold inside it.
        bias_ms: i64,
    },
    /// The first correction after engage: how far the seed missed.
    SeedMissed {
        /// Positive means the source started late.
        error_ms: i64,
    },
    /// The feed cannot reach the offset (after the enter hysteresis).
    TooSlow {
        /// Arrival latency.
        latency_ms: i64,
        /// The offset.
        offset_ms: i32,
        /// The offset that would clear the peak.
        required_ms: i64,
    },
    /// The feed is back within the offset (after the leave hysteresis).
    BackInReach,
    /// A timecode read as a latency no network has, so it was ignored.
    Implausible {
        /// The latency it read as.
        latency_ms: i64,
    },
    /// The delay line took over the audio output's pre-roll.
    PreRollTaken {
        /// The pre-roll absorbed.
        defer_ms: i64,
        /// The hold afterwards.
        hold_ms: i64,
    },
}

// ── Controller ───────────────────────────────────────────────

/// The per-source control loop. Receiver-thread state in the plugin; here it
/// is plain data driven by [`SyncController::observe`].
///
/// Two mechanisms, and it matters which does what. **The anchor places the
/// stream, once**, when the audio output primes ([`playout_anchor`]). **The
/// loop only holds it there**: after placement the residual is real drift,
/// tens of milliseconds over minutes, which the speed controller absorbs
/// inaudibly.
#[derive(Debug)]
pub struct SyncController {
    status: SyncStatus,
    tc_reason: TcReason,
    engaged: bool,
    locked: bool,
    tc: Option<Timecode>,
    /// How long packets are held in front of the decoders. The single
    /// actuator: everything else about sync is measurement.
    hold_ns: i64,
    latency_ns: i64,
    error_ns: i64,
    error: ErrorFilter,
    /// Cleared at engage, set when the first correction after it is made, so
    /// how far the seed missed is logged once rather than every packet.
    seed_reported: bool,
    /// Offset the current hold was computed against, plus the generation
    /// counter that says the user has changed it since.
    applied_offset_ms: i32,
    offset_generation: u32,
    last_tc_ns: u64,
    last_adjust_ns: u64,
    /// No further correction until this instant: the error signal lags a hold
    /// behind the hold, so the loop has to wait for its own last change to
    /// land or it winds itself up.
    settle_until_ns: u64,
    publish_ns: u64,
    /// Set on the delay line's first release: the point the pipeline is
    /// flowing again and the audio output can safely prime.
    released_once: bool,
    prime_deadline_ns: u64,
    too_slow_since_ns: u64,
    in_reach_since_ns: u64,
    fps: FpsLearner,
    peak: PeakTracker,
    /// Whether video arrives at its live pace yet. A relay's keyframe replay
    /// or the probe backlog reads seconds of latency that no live packet
    /// has, and seeding the hold or the peak from it would be wrong for as
    /// long as each lasts.
    live: ArrivalFloor,
    /// An implausible latency was logged and none plausible has come since.
    implausible_reported: bool,
}

impl SyncController {
    /// A fresh controller for a connection, aligned to the current offset
    /// generation so the first packet does not read a stale generation as an
    /// offset change.
    pub fn new(offset_generation: u32) -> Self {
        Self {
            status: SyncStatus::Off,
            tc_reason: TcReason::Ok,
            engaged: false,
            locked: false,
            tc: None,
            hold_ns: 0,
            latency_ns: 0,
            error_ns: 0,
            error: ErrorFilter::default(),
            seed_reported: false,
            applied_offset_ms: 0,
            offset_generation,
            last_tc_ns: 0,
            last_adjust_ns: 0,
            settle_until_ns: 0,
            publish_ns: 0,
            released_once: false,
            prime_deadline_ns: 0,
            too_slow_since_ns: 0,
            in_reach_since_ns: 0,
            fps: FpsLearner::default(),
            peak: PeakTracker::default(),
            live: ArrivalFloor::default(),
            implausible_reported: false,
        }
    }

    /// The current hold, in nanoseconds.
    pub fn hold_ns(&self) -> i64 {
        self.hold_ns
    }

    /// Whether a fresh packet has to join the delay line: once anything is
    /// queued, everything must queue behind it or the decoder would see
    /// packets out of order.
    pub fn should_hold(&self, delay_len: usize) -> bool {
        self.hold_ns > 0 || delay_len > 0
    }

    /// The delay line released a packet: the pipeline is flowing again, which
    /// is the condition the priming gate is really waiting on.
    pub fn note_released(&mut self) {
        self.released_once = true;
    }

    /// What this source publishes right now.
    pub fn snapshot(&self) -> SyncSnapshot {
        let peak_ms = self.peak.value() / NS_PER_MS;
        SyncSnapshot {
            status: self.status,
            tc_reason: self.tc_reason,
            timecode: self.tc,
            latency_ms: self.latency_ns / NS_PER_MS,
            latency_peak_ms: peak_ms,
            added_ms: self.hold_ns / NS_PER_MS,
            error_ms: self.error_ns / NS_PER_MS,
            required_offset_ms: required_offset_ms(peak_ms),
        }
    }

    /// Fold one freshly read packet in: take over any pending pre-roll,
    /// extract and measure its timecode, move the hold, and classify.
    pub fn observe(&mut self, mut obs: Observation<'_>) -> Outcome {
        let mut out = Outcome::default();
        let now = obs.now_ns;

        if let Some(pts_ns) = obs.pkt_pts_ns {
            self.live.note(now, pts_ns);
        }

        // Take over the pre-roll the audio output sits out before it starts.
        // Left alone it lands in the jitter buffer, which then starts playback
        // several hundred milliseconds above target, and the speed controller
        // drains that excess at +5%, pulling the playout earlier and undoing
        // the placement. Pausing the input for the same window leaves the
        // buffer where it was. Safe only because nothing is playing yet: the
        // gap this opens is one the output is already waiting through.
        if let Some(defer_ns) = obs.anchor_defer_ns
            && defer_ns > 0
            && obs.wanted
        {
            self.hold_ns += defer_ns;
            out.shift_delay_ns = defer_ns;
            // The queued packets moved with the hold, so this change is
            // already at the line's output rather than a hold away from it.
            // The gate only has to cover the rest of the pipeline.
            self.settle_until_ns = now + consts::SYNC_SETTLE_MARGIN_NS as u64;
            out.events.push(SyncEvent::PreRollTaken {
                defer_ms: defer_ns / NS_PER_MS,
                hold_ms: self.hold_ns / NS_PER_MS,
            });
        }

        // First packet of the stream: start the clock the gate runs against.
        // Here rather than at reset because reset has no `now`, and nothing
        // can prime before a packet has been read anyway.
        if self.prime_deadline_ns == 0 {
            self.prime_deadline_ns = now + consts::SYNC_PRIME_WAIT_NS;
        }

        // Reading the timecodes does not depend on `control`: a switched-off
        // source still reports what it receives, which is the only way to
        // tell a sender that is not stamping from one that is but has sync
        // switched off here.
        let control = obs.wanted;

        if !control && self.status != SyncStatus::Off {
            self.status = SyncStatus::Off;
            self.tc_reason = TcReason::Ok;
            self.error_ns = 0;
            self.locked = false;
        }

        if let Some(tc) = obs.timecode {
            self.observe_timecode(&tc, &mut obs, control, &mut out);
        }

        // No timecode for a while: either the sender never sends them or it
        // stopped. The hold stays where it is. The last placement is the best
        // estimate there is, and the line keeps releasing on the timeline, so
        // a feed whose timecodes come back is still where it was and the loop
        // just resumes. Giving the hold back would move a running feed, and
        // engaging again would step it back and starve the pipeline.
        if self.last_tc_ns == 0 || now.wrapping_sub(self.last_tc_ns) > consts::SYNC_TC_STALE_NS {
            if control && self.status != SyncStatus::NoTimecode {
                self.status = SyncStatus::NoTimecode;
                self.error_ns = 0;
                self.locked = false;
            }
            if control {
                let (reason, codec) =
                    diagnose_missing_timecode(obs.utc_now_ns.is_some(), obs.video_codec);
                self.set_tc_reason(reason, codec, &mut out);
            }
            // The stamp goes too: a frozen timecode is worse than none.
            self.tc = None;
        }

        // Switched off: the hold is handed back gradually, not dropped in one
        // go, and engaged=false so switching on again seeds from scratch.
        if !control {
            self.engaged = false;
            self.release_hold(now);
            out.forget_bias = true;
        }

        out.prime_hold = self.prime_hold(now, control);

        // ~5Hz is plenty for a dock and keeps this off the per-packet path.
        if now.wrapping_sub(self.publish_ns) >= consts::SYNC_PUBLISH_INTERVAL_NS {
            self.publish_ns = now;
            out.publish = Some(self.snapshot());
        }

        out
    }

    /// Said once per transition, so a stream that simply never carries
    /// timecodes explains itself in the log without repeating every packet.
    fn set_tc_reason(&mut self, reason: TcReason, codec: Option<&'static str>, out: &mut Outcome) {
        if self.tc_reason == reason {
            return;
        }
        self.tc_reason = reason;
        out.events.push(SyncEvent::TcReason { reason, codec });
    }

    /// Give back unwanted hold at a rate the pipeline downstream can absorb.
    fn release_hold(&mut self, now: u64) {
        if self.hold_ns <= 0 {
            self.hold_ns = 0;
            self.last_adjust_ns = now;
            return;
        }
        let elapsed_ns = self.elapsed_since_adjust(now);
        self.last_adjust_ns = now;
        let step_ns = (elapsed_ns as f64 * consts::SYNC_RELEASE_RATE) as i64;
        self.hold_ns -= step_ns;
        if self.hold_ns < 0 {
            self.hold_ns = 0;
        }
    }

    fn elapsed_since_adjust(&self, now: u64) -> i64 {
        if self.last_adjust_ns != 0 {
            now.wrapping_sub(self.last_adjust_ns) as i64
        } else {
            0
        }
    }

    /// Whether the audio thread should still be holding off priming.
    ///
    /// Engaging is not the finish line: seeding the hold stops the line for
    /// the length of the hold, so priming the instant it engages starves the
    /// pipeline exactly as priming before it would. The gate waits for the
    /// line to flow again (the first release). A source already arriving late
    /// enough needs no hold, so its line is flowing the moment it engages.
    ///
    /// Released once the line flows, the source turns out to have nothing to
    /// align against, sync is off, or the deadline passes.
    fn prime_hold(&self, now: u64, wanted: bool) -> bool {
        let flowing = self.released_once || (self.engaged && self.hold_ns <= 0);
        wanted && !flowing && self.status != SyncStatus::NoTimecode && now < self.prime_deadline_ns
    }

    /// Fold one timecoded video packet in.
    ///
    /// With `control` clear this measures and reports but changes nothing:
    /// the stamp, the latency and the peak are what the operator looks at to
    /// decide whether to switch sync on. The status stays Off.
    fn observe_timecode(
        &mut self,
        tc: &Timecode,
        obs: &mut Observation<'_>,
        control: bool,
        out: &mut Outcome,
    ) {
        let now = obs.now_ns;

        if let Some(fps) = self.fps.observe(tc) {
            out.events.push(SyncEvent::FpsLearned(fps));
        }

        // No default interval: an assumed rate puts n_frames × (assumed −
        // actual) of error into every latency reading, seeded into the hold
        // and kept in the peak long after the real rate is known. Before the
        // first complete second the decoder's cadence will do, since it is
        // also measured.
        let interval = match self.fps.interval_ns() {
            Some(ns) => Some((ns, "timecodes")),
            None => (consts::VIDEO_INTERVAL_MIN_NS..=consts::VIDEO_INTERVAL_MAX_NS)
                .contains(&obs.decoder_interval_ns)
                .then_some((obs.decoder_interval_ns, "decoder")),
        };
        let Some((frame_interval_ns, interval_origin)) = interval else {
            // No frame rate yet, so n_frames cannot become time. Keep the
            // stamp for the dock and feed the staleness clock, but measure
            // nothing: a guess would seed the hold wrong and sit in the peak
            // for a minute.
            self.tc = Some(*tc);
            self.last_tc_ns = now;
            if !control {
                return;
            }
            self.set_tc_reason(TcReason::Ok, None, out);
            self.status = SyncStatus::Acquiring;
            return;
        };

        let Some(utc_now_ns) = obs.utc_now_ns else {
            if !control {
                return;
            }
            self.status = SyncStatus::NoTimecode;
            self.set_tc_reason(TcReason::NoClock, None, out);
            return;
        };
        let latency_ns = timecode_latency_ns(utc_now_ns, tc, frame_interval_ns);
        // A corrupt SEI or a sender clock that is not on NTP reads minutes
        // off. Taken as latency it would seed a hold of that size, so it is
        // not taken at all: the source goes stale like one without
        // timecodes, and the log says why.
        if !(consts::SYNC_LATENCY_MIN_NS..=consts::SYNC_LATENCY_MAX_NS).contains(&latency_ns) {
            if !self.implausible_reported {
                self.implausible_reported = true;
                out.events.push(SyncEvent::Implausible {
                    latency_ms: latency_ns / NS_PER_MS,
                });
            }
            return;
        }
        self.implausible_reported = false;

        self.tc = Some(*tc);
        self.last_tc_ns = now;
        self.latency_ns = latency_ns;
        let live = self.live.live(now);
        if live {
            self.peak.record(latency_ns, now);
        }

        if !control {
            return;
        }

        self.set_tc_reason(TcReason::Ok, None, out);
        if !self.engaged && !live {
            self.status = SyncStatus::Acquiring;
            return;
        }

        let prev_hold_ns = self.hold_ns;
        let elapsed_ns = self.elapsed_since_adjust(now);

        let offset_ns = i64::from(obs.offset_ms) * NS_PER_MS;
        let needed_ns = offset_ns - latency_ns;
        let reachable = needed_ns >= 0;

        // Seed the hold so the pipeline primes already delayed, instead of
        // priming undelayed and then taking a multi-second step that would
        // starve the decoder. The seed errs early: the anchor at prime can
        // delay the start for free, but nothing can start it earlier, so a
        // late seed stays late until the loop drains it.
        if !self.engaged {
            let bias_ns = if obs.has_audio {
                consts::SYNC_SEED_BIAS_NS
            } else {
                0
            };
            self.hold_ns = (needed_ns - obs.nominal_pipeline_ns - bias_ns).max(0);
            self.seed_reported = false;

            // Every reading before the first packets leave the line describes
            // a pipeline that is not running yet.
            self.settle_until_ns = now + self.hold_ns as u64 + consts::SYNC_SETTLE_MARGIN_NS as u64;
            self.engaged = true;
            self.locked = false;
            self.applied_offset_ms = obs.offset_ms;
            self.offset_generation = obs.offset_generation;
            self.error.reset();

            // The line goes quiet for the length of the hold before its first
            // release, so the priming gate gets that much longer.
            self.prime_deadline_ns = now + self.hold_ns as u64 + consts::SYNC_PRIME_WAIT_NS;

            out.events.push(SyncEvent::Engaged {
                fps: 1_000_000_000.0 / frame_interval_ns as f64,
                origin: interval_origin,
                latency_ms: latency_ns / NS_PER_MS,
                offset_ms: obs.offset_ms,
                hold_ms: self.hold_ns / NS_PER_MS,
                bias_ms: consts::SYNC_SEED_BIAS_NS / NS_PER_MS,
            });
        }

        // An offset change steps by the delta rather than slewing: the user
        // asked for it and expects a hitch, and slewing seconds at the drift
        // rate would take a minute.
        if obs.offset_generation != self.offset_generation {
            let delta_ns =
                (i64::from(obs.offset_ms) - i64::from(self.applied_offset_ms)) * NS_PER_MS;
            self.hold_ns += delta_ns;
            self.applied_offset_ms = obs.offset_ms;
            self.offset_generation = obs.offset_generation;
            self.locked = false;
            // Every sample in the window describes the old target.
            self.error.reset();
        }

        // `now` cancels out (target = now + offset − (now − capture) =
        // capture + offset), so this carries no arrival jitter, only the
        // timecode's frame quantisation.
        let target_ns = now as i64 + needed_ns;

        if let Some(pts_ns) = obs.pkt_pts_ns {
            // The playout offset this implies, for the audio output to anchor
            // on when it primes.
            out.bias_sample_ns = Some(target_ns - pts_ns);

            // Without audio, video goes out about as it is released, from a
            // fallback anchor fixed at the first frame that the hold cannot
            // move. The seed places it open loop, and nothing is steered.
            let predicted_ns = if obs.has_audio {
                (obs.presentation)().predict(pts_ns)
            } else {
                Some(now as i64 + self.hold_ns + obs.nominal_pipeline_ns)
            };
            if let Some(presentation_ns) = predicted_ns {
                self.error_ns = self.error.push(presentation_ns - target_ns, now);

                // Wait for the last correction to reach the measurement. The
                // loop's dead time is its own actuator: the error comes from
                // the audio playout offset, which describes audio *released* a
                // hold ago, so a hold change shows up roughly a hold later.
                // Correcting per packet on a stale reading is positive
                // feedback dressed as negative.
                if obs.has_audio && now >= self.settle_until_ns {
                    // Positive error means late, so hold less. Unlocked, the
                    // correction applies outright: nothing is aligned yet, and
                    // the gate makes a full step safe. Locked, it is rate
                    // limited to keep the playback trim inside the speed
                    // controller's inaudible band.
                    let mut step_ns = -self.error_ns;
                    if self.locked {
                        let limit_ns = (elapsed_ns as f64 * consts::SYNC_SLEW_RATE) as i64;
                        step_ns = step_ns.clamp(-limit_ns, limit_ns);
                    }
                    self.hold_ns += step_ns;
                    self.last_adjust_ns = now;

                    // The first reading after engage is how far the seed
                    // missed; its sign says whether the bias does its job.
                    if !self.seed_reported {
                        self.seed_reported = true;
                        out.events.push(SyncEvent::SeedMissed {
                            error_ms: self.error_ns / NS_PER_MS,
                        });
                    }
                }
            }
        }

        // Ceiling from first principles: the hold is measured from the lowest
        // latency the release clock has seen, so it never needs more than
        // the offset minus the lowest plausible latency. Plus headroom for
        // the two deliberate overshoots (the seed bias and the largest
        // pre-roll the anchor may hand back). Not the current latency: a
        // packet the network delayed reads high, and clipping the hold on it
        // would move presentation for the very jitter the hold absorbs.
        let max_hold_ns = offset_ns - consts::SYNC_LATENCY_MIN_NS
            + consts::SYNC_SEED_BIAS_NS
            + consts::SYNC_ANCHOR_MAX_WAIT_NS;
        if self.hold_ns > max_hold_ns {
            self.hold_ns = max_hold_ns;
        }
        if self.hold_ns < 0 {
            self.hold_ns = 0;
        }

        // The invariant that keeps every path above safe, applied last so the
        // ceiling cannot bypass it: the hold may grow freely (packets only
        // wait longer) but never shrinks faster than the pipeline absorbs. A
        // large enough cut makes the whole line due at once and overruns the
        // buffers downstream; capped, it is an ordinary drain.
        let floor_ns = prev_hold_ns - (elapsed_ns as f64 * consts::SYNC_RELEASE_RATE) as i64;
        if self.hold_ns < floor_ns {
            self.hold_ns = floor_ns;
        }
        if self.hold_ns < 0 {
            self.hold_ns = 0;
        }

        // A hold change takes the larger of the old and new holds to work
        // through the line (queued packets keep their release times), plus
        // the rest of the pipeline.
        if self.hold_ns != prev_hold_ns {
            let propagation_ns = self.hold_ns.max(prev_hold_ns);
            self.settle_until_ns =
                now + propagation_ns as u64 + consts::SYNC_SETTLE_MARGIN_NS as u64;
        }

        self.update_status(now, reachable, obs.offset_ms, out);
    }

    /// Classify, with the alarm hysteresis applied.
    ///
    /// Bonded cellular crosses the threshold for a second or two in every
    /// bitrate dip and recovers on its own. Alarming on that trains the
    /// operator to ignore the warning. Transient excursions still show in the
    /// peak column.
    fn update_status(&mut self, now: u64, reachable: bool, offset_ms: i32, out: &mut Outcome) {
        if reachable {
            self.too_slow_since_ns = 0;
            if self.in_reach_since_ns == 0 {
                self.in_reach_since_ns = now;
            }
        } else {
            self.in_reach_since_ns = 0;
            if self.too_slow_since_ns == 0 {
                self.too_slow_since_ns = now;
            }
        }

        let mut alarmed = self.status == SyncStatus::TooSlow;

        if !alarmed
            && !reachable
            && now.wrapping_sub(self.too_slow_since_ns) >= consts::SYNC_ALARM_ENTER_NS
        {
            alarmed = true;
            out.events.push(SyncEvent::TooSlow {
                latency_ms: self.latency_ns / NS_PER_MS,
                offset_ms,
                required_ms: required_offset_ms(self.peak.value() / NS_PER_MS),
            });
        } else if alarmed
            && reachable
            && now.wrapping_sub(self.in_reach_since_ns) >= consts::SYNC_ALARM_LEAVE_NS
        {
            alarmed = false;
            out.events.push(SyncEvent::BackInReach);
        }

        if alarmed {
            self.status = SyncStatus::TooSlow;
            return;
        }

        // Nothing measured is not measured zero. Until the line's first
        // release error_ns is the 0 it was reset to, inside the lock
        // tolerance, and would report Locked while the line fills.
        if self.error.count() == 0 {
            self.locked = false;
            self.status = SyncStatus::Acquiring;
            return;
        }

        let err = self.error_ns.abs();
        if self.locked {
            if err > consts::SYNC_UNLOCK_TOLERANCE_NS {
                self.locked = false;
            }
        } else if err <= consts::SYNC_LOCK_TOLERANCE_NS {
            self.locked = true;
        }

        self.status = if self.locked {
            SyncStatus::Locked
        } else {
            SyncStatus::Acquiring
        };
    }
}

/// Ordered by what the operator can act on soonest: a missing clock is local,
/// a wrong codec is one sender setting, and "absent" is everything else about
/// the sender's configuration.
fn diagnose_missing_timecode(
    have_clock: bool,
    codec: VideoCodec,
) -> (TcReason, Option<&'static str>) {
    if !have_clock {
        return (TcReason::NoClock, None);
    }
    if let VideoCodec::Other(name) = codec {
        return (TcReason::Codec, Some(name));
    }
    (TcReason::Absent, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = NS_PER_MS;
    const S: i64 = 1_000_000_000;
    const FRAME_30: i64 = S / 30;

    /// A world the controller is driven through: a sender stamping 30fps
    /// timecodes at a fixed arrival latency, with a virtual OBS clock and a
    /// virtual NTP reference.
    struct World {
        ctl: SyncController,
        now_ns: u64,
        /// UTC at OBS clock zero.
        utc_epoch_ns: i64,
        /// How long content takes to reach us.
        latency_ns: i64,
        clock_synced: bool,
        wanted: bool,
        offset_ms: i32,
        generation: u32,
        target_ms: i32,
        codec: VideoCodec,
        stamp: bool,
        /// The playout mapping the pipeline would report: content plays
        /// `pipeline_ns` after the delay line releases it. The nominal one by
        /// default, so the only error after engage is the seed bias.
        pipeline_ns: i64,
        mapping_valid: bool,
        /// Frames sent so far; the PTS is derived from it.
        frame: i64,
        defer_ns: Option<i64>,
        events: Vec<SyncEvent>,
        last: Outcome,
    }

    impl World {
        fn new() -> Self {
            Self {
                ctl: SyncController::new(7),
                now_ns: 100 * S as u64,
                // 13:47:05.000 UTC on some day; only the hour matters.
                utc_epoch_ns: (13 * 3600 + 47 * 60 + 5) * S,
                latency_ns: 1500 * MS,
                clock_synced: true,
                wanted: true,
                offset_ms: 5000,
                generation: 7,
                target_ms: 120,
                codec: VideoCodec::Hevc,
                stamp: true,
                pipeline_ns: 120 * MS,
                mapping_valid: false,
                frame: 0,
                defer_ns: None,
                events: Vec::new(),
                last: Outcome::default(),
            }
        }

        fn utc_now(&self) -> i64 {
            self.utc_epoch_ns + self.now_ns as i64 - 100 * S
        }

        /// The timecode of the frame captured `latency_ns` ago.
        fn timecode_now(&self) -> Timecode {
            let capture = self.utc_now() - self.latency_ns;
            let in_day = capture.rem_euclid(24 * 3600 * S);
            let secs = in_day / S;
            // floor(fraction × 30), so a second never carries a 31st index:
            // the packet spacing below is 1e9/30 truncated, which drifts
            // 10ns/s against the second grid and would otherwise put an
            // extra frame into one second in a hundred million.
            let frames = ((in_day % S) * 30 / S) as u16;
            Timecode {
                hours: (secs / 3600) as u8,
                minutes: ((secs % 3600) / 60) as u8,
                seconds: (secs % 60) as u8,
                n_frames: frames,
            }
        }

        /// One video packet.
        fn packet(&mut self) -> &Outcome {
            let tc = self.stamp.then(|| self.timecode_now());
            // PTS from 1, so the mapping's `buffered_end > 0` test passes on
            // the first packet too.
            let pts_ns = (self.frame + 1) * FRAME_30;
            self.frame += 1;
            // The content arriving now is released after the hold and plays
            // a pipeline later, so the mapping's stream→OBS offset is
            // (now + hold + pipeline) − pts. The mapping reflects the current
            // hold at once, which is a faster pipeline than the real one (the
            // settle gate exists for the lag); the tests below do not depend
            // on that lag.
            let presentation_ns = self.now_ns as i64 + self.ctl.hold_ns() + self.pipeline_ns;
            let valid = self.mapping_valid;
            let mut presentation = move || {
                let mut audio = PlayoutMapping::default();
                if valid {
                    audio.publish(presentation_ns as u64, pts_ns);
                }
                PresentationMap {
                    audio,
                    audio_excess_ns: 0,
                }
            };
            let outcome = self.ctl.observe(Observation {
                now_ns: self.now_ns,
                utc_now_ns: self.clock_synced.then(|| self.utc_now()),
                wanted: self.wanted,
                offset_ms: self.offset_ms,
                offset_generation: self.generation,
                nominal_pipeline_ns: i64::from(self.target_ms) * NS_PER_MS,
                has_audio: true,
                decoder_interval_ns: 0,
                video_codec: self.codec,
                timecode: tc,
                pkt_pts_ns: Some(pts_ns),
                presentation: &mut presentation,
                anchor_defer_ns: self.defer_ns.take(),
            });
            self.events.extend(outcome.events.iter().copied());
            self.last = outcome;
            self.now_ns += FRAME_30 as u64;
            &self.last
        }

        fn run(&mut self, secs: f64) {
            for _ in 0..(secs * 30.0) as usize {
                self.packet();
            }
        }

        fn status(&self) -> SyncStatus {
            self.ctl.snapshot().status
        }

        fn has_event(&self, f: impl Fn(&SyncEvent) -> bool) -> bool {
            self.events.iter().any(f)
        }
    }

    // ── Arithmetic ──

    #[test]
    fn latency_folds_time_zones_and_midnight_out() {
        let tc = Timecode {
            hours: 23,
            minutes: 59,
            seconds: 58,
            n_frames: 15,
        };
        // Sender stamped 23:59:58.500 local; here it is 00:00:00.700 UTC the
        // next day, i.e. 2.2s later, in a different zone.
        // (15 frames at the truncated 1/30s interval are 5ns short of half a
        // second; the sub-microsecond tail is that.)
        let utc = 700 * MS;
        let latency = timecode_latency_ns(utc, &tc, FRAME_30);
        assert!((latency - 2200 * MS).abs() < 1000, "{latency}");
        // A sender on +5:30 stamps the same instant half an hour off any
        // whole-hour zone; the quarter-hour fold takes it out.
        let india = Timecode { minutes: 29, ..tc };
        let latency = timecode_latency_ns(utc, &india, FRAME_30);
        assert!((latency - 2200 * MS).abs() < 1000, "{latency}");
        // The same stamp seen 7 minutes early folds negative rather than to
        // 8 minutes.
        let utc_early = (59 * 60 + 58) * S + 500 * MS - 7 * 60 * S;
        let early = timecode_latency_ns(utc_early, &tc, FRAME_30);
        assert!((early + 7 * 60 * S).abs() < 1000, "{early}");
    }

    #[test]
    fn required_offset_rounds_up_to_whole_seconds() {
        assert_eq!(required_offset_ms(0), 0);
        assert_eq!(required_offset_ms(-50), 0);
        assert_eq!(required_offset_ms(1), 1000);
        assert_eq!(required_offset_ms(1000), 1000);
        assert_eq!(required_offset_ms(6123), 7000);
        assert_eq!(clamp_offset_ms(-5), 0);
        assert_eq!(clamp_offset_ms(31_000), 30_000);
        assert_eq!(clamp_offset_ms(6000), 6000);
    }

    #[test]
    fn staleness_marks_everything_but_off() {
        let mut snap = SyncSnapshot {
            status: SyncStatus::Locked,
            error_ms: 12,
            ..Default::default()
        };
        apply_staleness(&mut snap, 10 * S as u64, 12 * S as u64);
        assert_eq!(snap.status, SyncStatus::Locked);
        apply_staleness(&mut snap, 10 * S as u64, 13 * S as u64);
        assert_eq!((snap.status, snap.error_ms), (SyncStatus::Stale, 0));
        let mut never = SyncSnapshot {
            status: SyncStatus::Acquiring,
            ..Default::default()
        };
        apply_staleness(&mut never, 0, 1);
        assert_eq!(never.status, SyncStatus::Stale);
        let mut off = SyncSnapshot::default();
        apply_staleness(&mut off, 0, 99 * S as u64);
        assert_eq!(off.status, SyncStatus::Off);
    }

    #[test]
    fn playout_anchor_places_within_the_wait_window() {
        assert_eq!(playout_anchor(0, Some(5 * S), 1), None);
        assert_eq!(playout_anchor(10 * S, None, 1), None);
        // Placement 400ms ahead: taken, with the wait handed back.
        assert_eq!(
            playout_anchor(10 * S, Some(2 * S), (12 * S - 400 * MS) as u64),
            Some((12 * S as u64, 400 * MS))
        );
        // In the past, or further out than the buffer should hold: declined.
        assert_eq!(playout_anchor(10 * S, Some(2 * S), 12 * S as u64), None);
        assert_eq!(
            playout_anchor(10 * S, Some(2 * S), (12 * S - 1600 * MS) as u64),
            None
        );
        // The bias smoothing takes the first sample whole and then 1/8 steps.
        assert_eq!(fold_present_bias(None, 800), 800);
        assert_eq!(fold_present_bias(Some(800), 0), 700);
    }

    // ── Components ──

    #[test]
    fn delay_line_keeps_releases_ordered_and_bounded() {
        let mut line: DelayLine<u32> = DelayLine::new(3, 1000);
        assert!(line.push(0, 1, 100, 50));
        // A shrinking hold would make this due earlier than its predecessor;
        // it is clamped onto it.
        assert!(line.push(0, 2, 100, 40));
        assert!(line.push(0, 3, 100, 60));
        assert!(line.is_full());
        assert!(!line.push(0, 4, 100, 70));
        assert_eq!((line.len(), line.bytes()), (3, 300));

        assert_eq!(line.pop_due(49), None);
        assert_eq!(line.pop_due(50), Some(1));
        assert_eq!(line.pop_due(50), Some(2));
        assert_eq!(line.pop_due(50), None);
        line.shift_later(10);
        assert_eq!(line.pop_due(69), None);
        assert_eq!(line.pop_due(70), Some(3));
        assert!(line.is_empty());
        // The byte ceiling binds on its own.
        assert!(line.push(0, 5, 1000, 80));
        assert!(line.is_full());
        assert_eq!(line.note_overflow(), 0);
        assert_eq!(line.note_overflow(), 1);
        line.clear();
        assert_eq!((line.overflows(), line.bytes(), line.len()), (0, 0, 0));
        // After a clear the ordering clamp starts over.
        assert!(line.push(0, 6, 1, 5));
        assert_eq!(line.pop_due(5), Some(6));
    }

    #[test]
    fn delay_line_orders_each_stream_but_not_across_them() {
        let mut line: DelayLine<&str> = DelayLine::new(10, 1000);
        // Audio arrives first and is due at 100; video that a stall delayed
        // is due earlier, and goes first.
        assert!(line.push(1, "audio", 1, 100));
        assert!(line.push(0, "video", 1, 80));
        // Within a stream the clamp still holds.
        assert!(line.push(0, "video 2", 1, 70));
        assert_eq!(line.next_release_ns(), Some(80));
        assert_eq!(line.pop_due(100), Some("video"));
        assert_eq!(line.pop_due(100), Some("video 2"));
        assert_eq!(line.pop_due(100), Some("audio"));
        assert!(line.is_empty());
        line.clear();
        // Ties go in arrival order.
        assert!(line.push(1, "first", 1, 5));
        assert!(line.push(0, "second", 1, 5));
        assert_eq!(line.pop_due(5), Some("first"));
    }

    #[test]
    fn the_release_clock_keeps_each_packet_on_its_timeline_slot() {
        let mut clock = ReleaseClock::default();
        let hold = S;
        // A packet every 20ms, 300ms after its DTS: released a hold later.
        assert_eq!(
            clock.release_ns(1, 10 * S as u64, Some(10 * S - 300 * MS), hold),
            11 * S as u64
        );
        let on_time = |n: i64| (10 * S + n * 20 * MS) as u64;
        assert_eq!(
            clock.release_ns(1, on_time(1), Some(10 * S - 300 * MS + 20 * MS), hold),
            on_time(1) + S as u64
        );
        // A 600ms gap: the packet after it keeps the slot it would have had.
        let dts = 10 * S - 300 * MS + 40 * MS;
        assert_eq!(
            clock.release_ns(1, on_time(2) + 600 * MS as u64, Some(dts), hold),
            on_time(2) + S as u64
        );
        // Later than the hold: released on arrival.
        let dts = dts + 20 * MS;
        let late = on_time(3) + 1500 * MS as u64;
        assert_eq!(clock.release_ns(1, late, Some(dts), hold), late);
        // Video that trails its audio by 400ms keeps the skew: its own floor
        // anchors it.
        let video_at = on_time(4) + 400 * MS as u64;
        assert_eq!(
            clock.release_ns(0, video_at, Some(dts + 20 * MS), hold),
            video_at + S as u64
        );
        // Without a DTS, a hold after arrival.
        assert_eq!(
            clock.release_ns(1, 50 * S as u64, None, hold),
            51 * S as u64
        );
    }

    #[test]
    fn the_release_clock_starts_over_when_timestamps_jump_back() {
        let mut clock = ReleaseClock::default();
        let hold = S;
        let at = |n: u64| 10 * S as u64 + n * 20 * MS as u64;
        clock.release_ns(1, at(0), Some(100 * S), hold);
        clock.release_ns(1, at(1), Some(100 * S + 20 * MS), hold);
        // The sender restarts its clock: DTS falls by 60s. A gap never makes
        // DTS go back, so the timeline is taken afresh and the packet keeps
        // its hold.
        assert_eq!(
            clock.release_ns(1, at(2), Some(40 * S), hold),
            at(2) + S as u64
        );
        assert_eq!(
            clock.release_ns(1, at(3), Some(40 * S + 20 * MS), hold),
            at(3) + S as u64
        );
        // A jump forward is a new floor on its own.
        assert_eq!(
            clock.release_ns(1, at(4), Some(500 * S), hold),
            at(4) + S as u64
        );
    }

    #[test]
    fn peak_tracker_ages_out_after_a_minute() {
        let mut peak = PeakTracker::default();
        assert_eq!(peak.value(), 0);
        peak.record(900 * MS, 0);
        peak.record(300 * MS, 1);
        assert_eq!(peak.value(), 900 * MS);
        // 55s later the 900 is still in the window …
        peak.record(300 * MS, 55 * S as u64);
        assert_eq!(peak.value(), 900 * MS);
        // … and a minute after it was recorded it is gone.
        peak.record(300 * MS, 61 * S as u64);
        assert_eq!(peak.value(), 300 * MS);
    }

    #[test]
    fn error_filter_trims_outliers_and_bounds_by_time() {
        let mut f = ErrorFilter::default();
        assert_eq!(f.push(10, 0), 10);
        assert_eq!(f.push(30, 1), 20);
        // Three samples: trim one each side, i.e. the median.
        assert_eq!(f.push(-1000, 2), 10);
        // Ten steady readings with one corrupt one among them.
        let mut g = ErrorFilter::default();
        for i in 0..9 {
            g.push(20 + i % 3, i as u64 * 10 * MS as u64);
        }
        let filtered = g.push(5_000_000, 100 * MS as u64);
        assert!((20..=22).contains(&filtered), "got {filtered}");
        // Samples older than a second fall out; the newest always stays.
        let mut h = ErrorFilter::default();
        h.push(100, 0);
        h.push(100, 500 * MS as u64);
        assert_eq!(h.push(0, (1500 * MS + 1) as u64), 0);
        assert_eq!(h.count(), 1);
    }

    #[test]
    fn fps_is_learned_by_agreement_not_by_maximum() {
        let mut fps = FpsLearner::default();
        let tc = |seconds: u8, n_frames: u16| Timecode {
            hours: 0,
            minutes: 0,
            seconds,
            n_frames,
        };
        // Joined mid-second: the tail of second 0 still reaches 29.
        for n in [20, 25, 29] {
            assert_eq!(fps.observe(&tc(0, n)), None);
        }
        // Second 1 completes second 0 (one vote for 30); not enough.
        assert_eq!(fps.observe(&tc(1, 0)), None);
        assert_eq!(fps.interval_ns(), None);
        // A corrupt stamp in second 1 reads 41 frames.
        fps.observe(&tc(1, 40));
        // Second 2 completes second 1 (30 vs 41: no agreement yet).
        assert_eq!(fps.observe(&tc(2, 0)), None);
        assert_eq!(fps.interval_ns(), None);
        fps.observe(&tc(2, 29));
        // Second 3 completes second 2: 30 now has two votes and wins.
        assert_eq!(fps.observe(&tc(3, 0)), Some(30));
        assert_eq!(fps.interval_ns(), Some(S / 30));
        // Steady seconds report nothing new.
        fps.observe(&tc(3, 29));
        assert_eq!(fps.observe(&tc(4, 0)), None);
        // Two agreeing but impossible seconds are rejected.
        let mut bad = FpsLearner::default();
        bad.observe(&tc(0, 400));
        bad.observe(&tc(1, 400));
        assert_eq!(bad.observe(&tc(2, 0)), None);
        assert_eq!(bad.interval_ns(), None);
    }

    #[test]
    fn presentation_settles_net_of_the_buffer_excess() {
        let mut audio = PlayoutMapping::default();
        audio.publish(50 * S as u64, 20 * S);
        let map = PresentationMap {
            audio,
            audio_excess_ns: 0,
        };
        assert_eq!(map.predict(21 * S), Some(51 * S));
        // A buffer 30ms above target drains on its own, and presentation
        // settles that much earlier.
        let draining = PresentationMap {
            audio_excess_ns: 30 * MS,
            ..map
        };
        assert_eq!(draining.predict(21 * S), Some(51 * S - 30 * MS));
        assert_eq!(PresentationMap::default().predict(S), None);
    }

    // ── The loop ──

    #[test]
    fn a_stream_without_timecodes_says_why() {
        let mut w = World::new();
        w.stamp = false;
        w.packet();
        assert_eq!(w.status(), SyncStatus::NoTimecode);
        assert!(w.has_event(|e| matches!(
            e,
            SyncEvent::TcReason {
                reason: TcReason::Absent,
                ..
            }
        )));
        assert!(
            !w.last.prime_hold,
            "a source with nothing to align against must not hold priming"
        );

        let mut h264 = World::new();
        h264.stamp = false;
        h264.codec = VideoCodec::Other("h264");
        h264.packet();
        assert_eq!(h264.ctl.snapshot().tc_reason, TcReason::Codec);
        assert!(h264.has_event(|e| *e
            == SyncEvent::TcReason {
                reason: TcReason::Codec,
                codec: Some("h264")
            }));

        let mut no_clock = World::new();
        no_clock.clock_synced = false;
        // Until the rate is learned (two complete seconds) the stamp is held
        // and the status is Acquiring; from then on there is nothing to
        // compare against.
        no_clock.run(1.0);
        assert_eq!(no_clock.status(), SyncStatus::Acquiring);
        no_clock.run(1.0);
        assert_eq!(no_clock.status(), SyncStatus::NoTimecode);
        assert_eq!(no_clock.ctl.snapshot().tc_reason, TcReason::NoClock);
        // The stamp is still shown while the rate is being learned, then
        // dropped once the stale clock catches up with the unfed last_tc.
        no_clock.run(3.5);
        assert_eq!(no_clock.ctl.snapshot().timecode, None);
    }

    #[test]
    fn a_switched_off_source_measures_but_does_not_act() {
        let mut w = World::new();
        w.wanted = false;
        w.run(5.0);
        let snap = w.ctl.snapshot();
        assert_eq!(snap.status, SyncStatus::Off);
        assert!(snap.timecode.is_some());
        assert!(
            (snap.latency_ms - 1500).abs() <= 40,
            "latency {}",
            snap.latency_ms
        );
        assert!(snap.latency_peak_ms >= snap.latency_ms);
        assert_eq!(snap.added_ms, 0);
        assert_eq!(w.ctl.hold_ns(), 0);
        assert!(!w.last.prime_hold);
        // The rate is still learned (it is part of what is on display); the
        // controller itself does nothing.
        assert!(w.has_event(|e| *e == SyncEvent::FpsLearned(30)));
        assert!(
            !w.has_event(|e| !matches!(e, SyncEvent::FpsLearned(_))),
            "{:?}",
            w.events
        );
    }

    #[test]
    fn engage_seeds_the_hold_and_gates_priming_until_the_line_flows() {
        let mut w = World::new();
        // Before the rate is known nothing is measured; the status is
        // Acquiring and priming is held.
        w.packet();
        assert_eq!(w.status(), SyncStatus::Acquiring);
        assert!(w.last.prime_hold);
        assert_eq!(w.ctl.hold_ns(), 0);

        w.run(3.0);
        assert!(w.has_event(|e| *e == SyncEvent::FpsLearned(30)));
        let engaged = w
            .events
            .iter()
            .find_map(|e| match e {
                SyncEvent::Engaged {
                    fps,
                    origin,
                    latency_ms,
                    offset_ms,
                    hold_ms,
                    bias_ms,
                } => Some((*fps, *origin, *latency_ms, *offset_ms, *hold_ms, *bias_ms)),
                _ => None,
            })
            .expect("engaged");
        assert_eq!(engaged.1, "timecodes");
        assert!((engaged.0 - 30.0).abs() < 0.01);
        assert_eq!(engaged.3, 5000);
        assert_eq!(engaged.5, 250);
        // hold = offset − latency − pipeline − bias, within a frame.
        let expected_hold = 5000 - engaged.2 - 120 - 250;
        assert!(
            (engaged.4 - expected_hold).abs() <= 1,
            "{} vs {expected_hold}",
            engaged.4
        );
        assert!((w.ctl.hold_ns() / MS - expected_hold).abs() <= 1);
        assert!(w.ctl.should_hold(0));

        // Priming stays held until the line releases something …
        assert!(w.last.prime_hold);
        w.ctl.note_released();
        w.packet();
        assert!(!w.last.prime_hold);
        // … and the status is Acquiring until an error has been measured.
        assert_eq!(w.status(), SyncStatus::Acquiring);
    }

    #[test]
    fn the_prime_gate_gives_up_at_its_deadline() {
        let mut w = World::new();
        w.run(3.0);
        assert!(w.last.prime_hold);
        // hold + 3s after engage with no release: the backstop lets audio go.
        let hold_s = w.ctl.hold_ns() as f64 / S as f64;
        w.run(hold_s + 3.2);
        assert!(!w.last.prime_hold);
    }

    #[test]
    fn a_feed_slower_than_the_offset_needs_no_hold_and_alarms_after_hysteresis() {
        let mut w = World::new();
        w.latency_ns = 6 * S;
        w.run(3.0);
        assert_eq!(w.ctl.hold_ns(), 0);
        // Flowing the moment it engaged: nothing queued, so no prime hold.
        assert!(!w.last.prime_hold);
        assert_eq!(w.status(), SyncStatus::Acquiring);
        w.run(4.5);
        assert_eq!(w.status(), SyncStatus::TooSlow);
        let alarm = w.events.iter().find_map(|e| match e {
            SyncEvent::TooSlow {
                latency_ms,
                offset_ms,
                required_ms,
            } => Some((*latency_ms, *offset_ms, *required_ms)),
            _ => None,
        });
        let (latency, offset, required) = alarm.expect("alarm");
        // The timecode floors to the frame, so the measured latency reads up
        // to a frame *older* than the true 6s, and the peak rounds up to 7s.
        assert!((6000..=6034).contains(&latency), "latency {latency}");
        assert_eq!(offset, 5000);
        assert_eq!(required, 7000);
        assert_eq!(w.ctl.snapshot().required_offset_ms, 7000);

        // Back in reach: the alarm clears only after 8s.
        w.latency_ns = 1500 * MS;
        w.run(5.0);
        assert_eq!(w.status(), SyncStatus::TooSlow);
        w.run(4.0);
        assert_ne!(w.status(), SyncStatus::TooSlow);
        assert!(w.has_event(|e| *e == SyncEvent::BackInReach));
    }

    #[test]
    fn the_loop_corrects_once_per_settle_interval_and_locks() {
        let mut w = World::new();
        w.run(3.0);
        let hold_after_engage = w.ctl.hold_ns();
        // The pipeline starts reporting where things actually land: content
        // arrives after the hold and plays a pipeline later, and this
        // simulated pipeline is exactly the nominal one, so the only error is
        // the deliberate seed bias (early by 250ms).
        w.mapping_valid = true;
        w.ctl.note_released();
        // Inside the settle window nothing moves even though an error is now
        // measured; the status is Acquiring because the error is a quarter
        // second.
        w.run(1.0);
        assert_eq!(w.ctl.hold_ns(), hold_after_engage);
        assert_eq!(w.status(), SyncStatus::Acquiring);
        assert!(!w.has_event(|e| matches!(e, SyncEvent::SeedMissed { .. })));

        // After hold + margin the first correction lands in full and the
        // seed miss is reported once, negative (early).
        let settle_s = hold_after_engage as f64 / S as f64 + 0.8;
        w.run(settle_s);
        let missed: Vec<i64> = w
            .events
            .iter()
            .filter_map(|e| match e {
                SyncEvent::SeedMissed { error_ms } => Some(*error_ms),
                _ => None,
            })
            .collect();
        assert_eq!(missed.len(), 1, "{missed:?}");
        // hold + pipeline − needed: (needed − 120 − 250) + 120 − needed, give
        // or take the frame the timecode quantises to.
        assert!(
            (-280..=-220).contains(&missed[0]),
            "seed miss {}",
            missed[0]
        );
        assert!(
            w.ctl.hold_ns() > hold_after_engage,
            "an early seed means more hold"
        );

        // The simulated pipeline now presents on target (the world derives
        // presentation from the current hold), so the filtered error decays
        // to zero and the loop locks.
        w.run(2.0);
        assert_eq!(w.status(), SyncStatus::Locked);
        let snap = w.ctl.snapshot();
        assert!(snap.error_ms.abs() <= 40, "error {}", snap.error_ms);
        assert!(snap.added_ms > 0);
    }

    #[test]
    fn an_offset_change_steps_the_hold_and_unlocks() {
        let mut w = World::new();
        w.run(3.0);
        w.mapping_valid = true;
        w.ctl.note_released();
        w.run(6.0);
        assert_eq!(w.status(), SyncStatus::Locked);
        let before = w.ctl.hold_ns();

        w.offset_ms = 7000;
        w.generation += 1;
        w.packet();
        // Two seconds more hold, give or take the one unlocked correction
        // that may land in the same packet on a single fresh error sample.
        let stepped = w.ctl.hold_ns() - before;
        assert!(
            (stepped - 2 * S).abs() <= 2 * FRAME_30,
            "stepped {}ms",
            stepped / MS
        );
        assert_eq!(w.status(), SyncStatus::Acquiring);

        // Lowering it is rate limited: the whole line must not become due at
        // once, so the hold walks down at the release rate (4% of the time
        // since the last adjustment, at most a settle interval ago) rather
        // than stepping by two seconds.
        let high = w.ctl.hold_ns();
        w.offset_ms = 5000;
        w.generation += 1;
        w.packet();
        let dropped = high - w.ctl.hold_ns();
        assert!(
            dropped > 0 && dropped < 250 * MS,
            "dropped {}ms in one packet",
            dropped / MS
        );
    }

    #[test]
    fn switching_off_gives_the_hold_back_gradually() {
        let mut w = World::new();
        w.run(3.0);
        let hold = w.ctl.hold_ns();
        assert!(hold > S);
        w.wanted = false;
        w.run(1.0);
        assert_eq!(w.status(), SyncStatus::Off);
        let released = hold - w.ctl.hold_ns();
        // 4% of a second, give or take a frame's worth.
        assert!(
            (released - 40 * MS).abs() < 3 * MS,
            "released {}ms in 1s",
            released / MS
        );
        // Re-enabling re-seeds from scratch.
        w.wanted = true;
        w.packet();
        assert_eq!(
            w.events
                .iter()
                .filter(|e| matches!(e, SyncEvent::Engaged { .. }))
                .count(),
            2
        );
    }

    /// A sender whose clock is minutes off (or a corrupt stamp) must not
    /// seed a hold of minutes: the reading is ignored and said once.
    #[test]
    fn an_implausible_latency_is_ignored_not_held() {
        let mut w = World::new();
        w.latency_ns = 5 * 60 * S;
        w.run(6.0);
        assert_eq!(w.ctl.hold_ns(), 0);
        assert!(!w.has_event(|e| matches!(e, SyncEvent::Engaged { .. })));
        let said = w
            .events
            .iter()
            .filter(|e| matches!(e, SyncEvent::Implausible { .. }))
            .count();
        assert_eq!(said, 1);
        assert_eq!(w.status(), SyncStatus::NoTimecode);
    }

    /// Switching sync off forgets where the stream was placed, so a later
    /// restart of the audio clock does not place it again.
    #[test]
    fn switching_sync_off_forgets_the_placement() {
        let mut w = World::new();
        w.run(3.0);
        assert!(!w.last.forget_bias);
        w.wanted = false;
        w.packet();
        assert!(w.last.forget_bias);
    }

    #[test]
    fn timecodes_that_stop_make_the_source_unalignable() {
        let mut w = World::new();
        w.run(3.0);
        assert!(w.ctl.snapshot().timecode.is_some());
        w.stamp = false;
        w.run(2.9);
        assert_eq!(w.status(), SyncStatus::Acquiring);
        w.run(0.3);
        assert_eq!(w.status(), SyncStatus::NoTimecode);
        assert_eq!(w.ctl.snapshot().tc_reason, TcReason::Absent);
        assert_eq!(w.ctl.snapshot().timecode, None);
        // The hold stays put: the feed keeps its place, and when timecodes
        // come back the loop resumes instead of seeding a step.
        let hold = w.ctl.hold_ns();
        w.run(5.0);
        assert_eq!(w.ctl.hold_ns(), hold);
        w.stamp = true;
        w.run(3.0);
        let engaged = w
            .events
            .iter()
            .filter(|e| matches!(e, SyncEvent::Engaged { .. }))
            .count();
        assert_eq!(engaged, 1, "resumed, not engaged again");
        assert!((w.ctl.hold_ns() - hold).abs() < 50 * MS);
    }

    #[test]
    fn the_pre_roll_is_taken_over_by_the_delay_line() {
        let mut w = World::new();
        w.run(3.0);
        let hold = w.ctl.hold_ns();
        w.defer_ns = Some(400 * MS);
        let out = w.packet();
        assert_eq!(out.shift_delay_ns, 400 * MS);
        assert!(out.events.iter().any(|e| *e
            == SyncEvent::PreRollTaken {
                defer_ms: 400,
                hold_ms: (hold + 400 * MS) / MS
            }));
        assert_eq!(w.ctl.hold_ns(), hold + 400 * MS);
        // Nothing pending: nothing shifts.
        assert_eq!(w.packet().shift_delay_ns, 0);
    }

    #[test]
    fn the_bias_sample_is_the_playout_offset_the_target_implies() {
        let mut w = World::new();
        w.run(3.0);
        let out = w.packet();
        let sample = out.bias_sample_ns.expect("bias");
        // target − pts, where target = now + offset − latency and pts is the
        // frame count: within a frame of that.
        let pts = w.frame * FRAME_30;
        let expected = (w.now_ns as i64 - FRAME_30) + 5000 * MS - 1500 * MS - pts;
        assert!(
            (sample - expected).abs() <= FRAME_30,
            "{sample} vs {expected}"
        );
    }

    #[test]
    fn snapshots_publish_at_five_hertz() {
        let mut w = World::new();
        let mut published = 0;
        for _ in 0..300 {
            if w.packet().publish.is_some() {
                published += 1;
            }
        }
        // At 30fps, 200ms is 6 frames less 2ns, so the next publish lands on
        // the 7th frame: 300 / 7 rounded up.
        assert_eq!(published, 43);
    }
}
