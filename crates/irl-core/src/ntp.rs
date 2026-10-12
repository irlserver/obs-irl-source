//! SNTP (RFC 4330) arithmetic for the timecode sync clock: the wire format,
//! the offset/delay pair, the server policy, the drift fit and the offset
//! filter. Port of the pure half of the C branch's `sync-ntp.c`; the socket,
//! the thread and the poll schedule live in the plugin crate.
//!
//! The result of it all is an offset from the OBS monotonic clock
//! (`os_gettime_ns`) to UTC, kept in the plugin and used only to place
//! received frames on a shared timeline. Nothing system-wide is stepped.
//!
//! ## Drift
//!
//! `os_gettime_ns` counts this machine's crystal, and crystals are wrong: tens
//! of ppm is normal, which is tens of milliseconds per minute of poll interval
//! — the same order as the alignment sync is trying to hold. A rolling window
//! of accepted samples is fitted (Theil–Sen, so one bad sample cannot tilt it)
//! to recover that error as a slope, and under the burst policy the slope is
//! used to extrapolate the offset between polls. Under the standard policy the
//! slope is measured and shown but not applied: one sample a minute is too
//! sparse to trust with the timeline.
//!
//! Note what the number is measured against. `os_gettime_ns` is
//! `QueryPerformanceCounter` on Windows, which nothing disciplines, so the fit
//! is the raw crystal error. On Linux it is `CLOCK_MONOTONIC`, which the
//! system NTP daemon does slew, so a machine already running chrony reports a
//! drift near zero. Both are correct: it is the error of the clock this plugin
//! actually schedules against.

use crate::consts;

// ── Wire format ──────────────────────────────────────────────

/// One measured sample: the offset from the local clock to UTC and the round
/// trip it was measured over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sample {
    /// `utc_ns − local_ns` at the time of the exchange.
    pub offset_ns: i64,
    /// Round-trip time, less the server's own processing time.
    pub rtt_ns: i64,
}

/// Why a query ended the way it did. The two kiss-o'-death codes are the
/// server telling us our polling is unwelcome, which is a different thing
/// from not reaching it and calls for a different answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryResult {
    /// A usable sample.
    Ok(Sample),
    /// No answer, or one that cannot be used.
    Fail,
    /// Kiss-o'-death `RATE`: slow down.
    Rate,
    /// Kiss-o'-death `DENY` / `RSTR`: go away.
    Deny,
}

fn read_be32(p: &[u8]) -> u32 {
    u32::from_be_bytes([p[0], p[1], p[2], p[3]])
}

/// An NTP 64-bit timestamp is seconds since 1900 plus a 32-bit binary
/// fraction. Converted to nanoseconds since the Unix epoch; 0 for a value
/// before 1970 (which includes the all-zero "unset" timestamp).
pub fn ntp_ts_to_unix_ns(p: &[u8]) -> i64 {
    let seconds = u64::from(read_be32(p));
    let fraction = u64::from(read_be32(&p[4..]));
    if seconds < consts::NTP_UNIX_EPOCH_DELTA {
        return 0;
    }
    let mut ns = (seconds - consts::NTP_UNIX_EPOCH_DELTA) as i64 * 1_000_000_000;
    // fraction / 2^32 × 1e9, without overflowing: 1e9 < 2^30, so the product
    // of a 32-bit fraction and 1e9 fits in 62 bits.
    ns += ((fraction * 1_000_000_000) >> 32) as i64;
    ns
}

/// The client request: LI = 0 (no warning), VN = 4, Mode = 3 (client), and
/// nothing else set. The local timestamps come from the caller's monotonic
/// clock rather than the packet, which is the whole point.
pub fn request_packet() -> [u8; consts::NTP_PACKET_BYTES] {
    let mut packet = [0u8; consts::NTP_PACKET_BYTES];
    packet[0] = 0x23;
    packet
}

/// Interpret one reply. `t1` and `t4` are the local monotonic clock at send
/// and receive, in nanoseconds; `t2` and `t3` are read from the packet.
pub fn parse_reply(reply: &[u8], t1_ns: u64, t4_ns: u64) -> QueryResult {
    if reply.len() != consts::NTP_PACKET_BYTES {
        return QueryResult::Fail;
    }

    // Mode 4 = server.
    let mode = reply[0] & 0x07;
    let stratum = reply[1];
    if mode != 4 {
        return QueryResult::Fail;
    }

    // Stratum 0 is never a time source: it is a kiss-o'-death packet, and the
    // reference identifier says which. RFC 4330 asks for the poll interval to
    // be increased on RATE and for the server to be dropped on DENY or RSTR,
    // which is the difference between a pool that is busy and one that has
    // blocked us.
    if stratum == 0 {
        return match &reply[12..16] {
            b"RATE" => QueryResult::Rate,
            b"DENY" | b"RSTR" => QueryResult::Deny,
            _ => QueryResult::Fail,
        };
    }
    if stratum > 15 {
        return QueryResult::Fail;
    }

    let t2 = ntp_ts_to_unix_ns(&reply[32..40]); // server receive
    let t3 = ntp_ts_to_unix_ns(&reply[40..48]); // server transmit
    if t2 <= 0 || t3 <= 0 {
        return QueryResult::Fail;
    }

    let t1 = t1_ns as i64;
    let t4 = t4_ns as i64;
    let rtt = ((t4 - t1) - (t3 - t2)).max(0);

    // Offset from our monotonic clock to UTC, averaging out the path so long
    // as it is roughly symmetric.
    QueryResult::Ok(Sample {
        offset_ns: ((t2 - t1) + (t3 - t4)) / 2,
        rtt_ns: rtt,
    })
}

// ── Server policy ────────────────────────────────────────────

/// Whether `host` is one of the servers run for this plugin, where the burst
/// policy is both wanted and welcome. Matched case-insensitively, either
/// exactly or as a parent domain, so an entry of `example.com` also covers
/// `ntp1.example.com` — but not `notexample.com`.
pub fn host_is_burst(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    consts::NTP_BURST_HOSTS.iter().any(|entry| {
        if host.eq_ignore_ascii_case(entry) {
            return true;
        }
        // Subdomain: the tail must match and be preceded by a dot.
        host.len() > entry.len() + 1
            && host.as_bytes()[host.len() - entry.len() - 1] == b'.'
            && host[host.len() - entry.len()..].eq_ignore_ascii_case(entry)
    })
}

// ── Randomised intervals ─────────────────────────────────────

/// xorshift32, for the randomised poll intervals: every client that starts
/// with OBS would otherwise poll in step and turn a steady trickle into a
/// spike.
#[derive(Debug, Clone)]
pub struct XorShift32(u32);

impl XorShift32 {
    /// Seed from anything non-zero; a zero seed is replaced with a constant so
    /// the generator cannot get stuck.
    pub fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9e37_79b9 } else { seed })
    }

    /// A value in `lo..=hi`.
    pub fn range(&mut self, lo: u32, hi: u32) -> u32 {
        let mut s = self.0;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        self.0 = s;
        lo + s % (hi - lo + 1)
    }
}

// ── Drift window ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct DriftSample {
    t_ns: u64,
    offset_ns: i64,
    rtt_ns: i64,
}

/// The rolling window of accepted samples the drift is fitted from.
///
/// Offsets are a property of the path as much as of the clock — two servers
/// disagree by their own asymmetry, which is the same order as the drift
/// signal itself — so the window cannot span a change of server; the plugin
/// resets it on every failover, recovery and reconfiguration.
#[derive(Debug, Clone, Default)]
pub struct DriftWindow {
    samples: Vec<DriftSample>,
}

/// A fitted drift: the slope of the offset against local time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftFit {
    /// Offset nanoseconds per local nanosecond.
    pub slope: f64,
    /// The fitted offset at `anchor_ns`.
    pub offset_ns: i64,
    /// Local time the intercept is taken at: the newest sample's.
    pub anchor_ns: u64,
    /// Samples that survived the filters.
    pub used: usize,
}

fn median_sorted(v: &[f64]) -> f64 {
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

impl DriftWindow {
    /// Empty the window (a change of reference).
    pub fn reset(&mut self) {
        self.samples.clear();
    }

    /// Samples held.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Nothing held.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Append one accepted sample taken at local time `now_ns`, dropping the
    /// oldest once the window is full.
    pub fn add(&mut self, sample: &Sample, now_ns: u64) {
        if self.samples.len() == consts::NTP_DRIFT_CAPACITY {
            self.samples.remove(0);
        }
        self.samples.push(DriftSample {
            t_ns: now_ns,
            offset_ns: sample.offset_ns,
            rtt_ns: sample.rtt_ns,
        });
    }

    /// Theil–Sen: the median of the slopes of every pair. A least-squares line
    /// through NTP samples follows whichever sample had the worst queueing;
    /// the median of pairs ignores it, which matters on the paths this plugin
    /// runs over.
    ///
    /// `None` until there are enough recent, low-round-trip samples over a
    /// long enough baseline, or when the fit is outside what any crystal
    /// does.
    pub fn fit(&self) -> Option<DriftFit> {
        if self.samples.len() < consts::NTP_DRIFT_MIN_SAMPLES {
            return None;
        }
        let newest = self.samples.last()?.t_ns;

        // Age first, so the round-trip floor below is the best of what is
        // still relevant rather than of a sample from an hour ago.
        let recent: Vec<&DriftSample> = self
            .samples
            .iter()
            .filter(|s| (newest.wrapping_sub(s.t_ns) as i64) <= consts::NTP_DRIFT_MAX_AGE_NS)
            .collect();
        if recent.len() < consts::NTP_DRIFT_MIN_SAMPLES {
            return None;
        }
        let min_rtt = recent.iter().map(|s| s.rtt_ns).min()?;

        // Samples whose round trip is much worse than the best in the window
        // carry a correspondingly worse offset, so they are dropped before
        // fitting.
        let rtt_limit = min_rtt + min_rtt / 2 + consts::NTP_DRIFT_RTT_SLACK_NS;
        let kept: Vec<&DriftSample> = recent
            .into_iter()
            .filter(|s| s.rtt_ns <= rtt_limit)
            .collect();
        if kept.len() < consts::NTP_DRIFT_MIN_SAMPLES {
            return None;
        }

        let first = kept[0].t_ns;
        let last = kept[kept.len() - 1].t_ns;
        if (last.wrapping_sub(first) as i64) < consts::NTP_DRIFT_MIN_SPAN_NS {
            return None;
        }

        let mut slopes = Vec::with_capacity(kept.len() * (kept.len() - 1) / 2);
        for (a, sa) in kept.iter().enumerate() {
            for sb in &kept[a + 1..] {
                let dt = sb.t_ns.wrapping_sub(sa.t_ns) as i64;
                // Pairs closer together than this are noise divided by a
                // small number, so they are left out of the median rather
                // than allowed to widen it.
                if dt < consts::NTP_DRIFT_MIN_PAIR_NS {
                    continue;
                }
                let doff = sb.offset_ns - sa.offset_ns;
                slopes.push(doff as f64 / dt as f64);
            }
        }
        if slopes.len() < consts::NTP_DRIFT_MIN_PAIRS {
            return None;
        }
        slopes.sort_by(f64::total_cmp);
        let slope = median_sorted(&slopes);
        if !slope.is_finite() || slope.abs() * 1e6 > consts::NTP_DRIFT_MAX_PPM {
            return None;
        }

        // The intercept gets the same treatment: the median residual against
        // the newest sample's time, so the published offset is a filtered
        // value rather than whatever the last packet happened to measure.
        let mut residuals: Vec<f64> = kept
            .iter()
            .map(|s| {
                let dt = s.t_ns.wrapping_sub(last) as i64;
                s.offset_ns as f64 - slope * dt as f64
            })
            .collect();
        residuals.sort_by(f64::total_cmp);

        Some(DriftFit {
            slope,
            offset_ns: median_sorted(&residuals) as i64,
            anchor_ns: last,
            used: kept.len(),
        })
    }
}

// ── The published offset ─────────────────────────────────────

/// The clock as published to the rest of the plugin: the current offset, the
/// drift being applied to it (burst policy only) and the sample it came from.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OffsetEstimate {
    /// Whether any sample has been accepted since the last reference change.
    pub synced: bool,
    /// `utc_ns = local_ns + offset_ns` at `anchor`, before extrapolation.
    pub offset_ns: i64,
    /// Round-trip time of the sample the offset came from.
    pub rtt_ns: i64,
    /// Local time of that sample.
    pub sampled_at_ns: u64,
    /// The fit is valid (measured under either policy).
    pub drift_valid: bool,
    /// This machine's crystal error, positive when the local clock runs
    /// fast, in parts per million.
    pub drift_ppm: f64,
    /// Slope being *applied*; zero under the standard policy.
    pub drift_slope: f64,
    /// Local time the applied slope is anchored at.
    pub drift_anchor_ns: u64,
}

impl OffsetEstimate {
    /// Fold one accepted sample in. `fit` is the drift window's current fit
    /// (already including this sample); it drives the offset only under the
    /// burst policy.
    ///
    /// Returns whether this was the first sync since the reference changed
    /// (the plugin logs that once).
    pub fn publish(
        &mut self,
        best: &Sample,
        burst_mode: bool,
        fit: Option<DriftFit>,
        now_ns: u64,
    ) -> bool {
        match (burst_mode, fit) {
            (true, Some(fit)) => {
                // The fit is already a filtered estimate over minutes of
                // samples, so it replaces the exponential average rather than
                // feeding it — smoothing a smoothed value only adds lag.
                self.offset_ns = fit.offset_ns;
                self.drift_slope = fit.slope;
                self.drift_anchor_ns = fit.anchor_ns;
            }
            _ => {
                if !self.synced
                    || (best.offset_ns - self.offset_ns).abs() > consts::NTP_RESEAT_THRESHOLD_NS
                {
                    self.offset_ns = best.offset_ns;
                } else {
                    let delta = (best.offset_ns - self.offset_ns) as f64;
                    self.offset_ns += (delta * consts::NTP_OFFSET_SMOOTHING) as i64;
                }
                // Measured but not applied: no extrapolation on this path.
                self.drift_slope = 0.0;
                self.drift_anchor_ns = 0;
            }
        }

        self.drift_valid = fit.is_some();
        // Sign flip: a local clock that runs slow makes the offset to UTC
        // grow, and reads to everyone else as a negative ppm error.
        self.drift_ppm = fit.map_or(0.0, |f| -f.slope * 1e6);

        let first = !self.synced;
        self.synced = true;
        self.rtt_ns = best.rtt_ns;
        self.sampled_at_ns = now_ns;
        first
    }

    /// The reference changed (new server, failover, recovery): the drift
    /// belongs to the reference it was measured against, so it goes with it.
    /// The offset itself is kept — it is a few milliseconds stale at worst,
    /// and dropping the sync would stall every source over an outage the
    /// fallback is about to paper over.
    pub fn forget_drift(&mut self) {
        self.drift_valid = false;
        self.drift_ppm = 0.0;
        self.drift_slope = 0.0;
        self.drift_anchor_ns = 0;
    }

    /// The offset as of `now_ns`, including the drift extrapolation when one
    /// is running, capped so a fit that survived every check and is still
    /// wrong cannot do more than a bounded amount of damage.
    pub fn offset_at(&self, now_ns: u64) -> i64 {
        let mut offset = self.offset_ns;
        if self.drift_slope != 0.0 && self.drift_anchor_ns != 0 && now_ns > self.drift_anchor_ns {
            let elapsed = now_ns.wrapping_sub(self.drift_anchor_ns) as i64;
            let correction = (self.drift_slope * elapsed as f64).clamp(
                -(consts::NTP_DRIFT_MAX_CORRECTION_NS as f64),
                consts::NTP_DRIFT_MAX_CORRECTION_NS as f64,
            );
            offset += correction as i64;
        }
        offset
    }

    /// UTC now, or `None` while unsynced.
    pub fn utc_now_ns(&self, now_ns: u64) -> Option<i64> {
        self.synced.then(|| now_ns as i64 + self.offset_at(now_ns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn write_ts(p: &mut [u8], unix_ns: i64) {
        let secs = (unix_ns / 1_000_000_000) as u64 + consts::NTP_UNIX_EPOCH_DELTA;
        let frac = ((unix_ns % 1_000_000_000) as u64) << 32;
        let frac = frac / 1_000_000_000;
        p[..4].copy_from_slice(&(secs as u32).to_be_bytes());
        p[4..8].copy_from_slice(&(frac as u32).to_be_bytes());
    }

    fn reply(stratum: u8, t2: i64, t3: i64) -> [u8; 48] {
        let mut r = [0u8; 48];
        r[0] = 0x24; // LI 0, VN 4, mode 4
        r[1] = stratum;
        write_ts(&mut r[32..40], t2);
        write_ts(&mut r[40..48], t3);
        r
    }

    #[test]
    fn timestamps_round_trip_to_the_nanosecond() {
        let mut p = [0u8; 8];
        let ns = 1_700_000_000_123_456_789i64;
        write_ts(&mut p, ns);
        let back = ntp_ts_to_unix_ns(&p);
        // The 32-bit fraction resolves to ~0.23ns, so the tail is exact to
        // within a nanosecond.
        assert!((back - ns).abs() <= 1, "{back} vs {ns}");
        // Before 1970 (or unset) reads as zero.
        assert_eq!(ntp_ts_to_unix_ns(&[0; 8]), 0);
        assert_eq!(request_packet()[0], 0x23);
        assert!(request_packet()[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_reply_yields_the_classic_offset_and_delay() {
        // Local clock at 100s; server is 5s ahead; 40ms each way, 2ms of
        // server processing.
        let t1 = 100 * S;
        let t2 = 105_040_000_000i64;
        let t3 = 105_042_000_000i64;
        let t4 = 100 * S + 82_000_000;
        match parse_reply(&reply(2, t2, t3), t1, t4) {
            QueryResult::Ok(s) => {
                assert!(
                    (s.offset_ns - 5 * S as i64).abs() <= 1,
                    "offset {}",
                    s.offset_ns
                );
                assert!((s.rtt_ns - 80_000_000).abs() <= 1, "rtt {}", s.rtt_ns);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn kiss_o_death_and_junk_are_classified() {
        let mut rate = reply(0, 0, 0);
        rate[12..16].copy_from_slice(b"RATE");
        assert_eq!(parse_reply(&rate, 0, 1), QueryResult::Rate);
        let mut deny = reply(0, 0, 0);
        deny[12..16].copy_from_slice(b"DENY");
        assert_eq!(parse_reply(&deny, 0, 1), QueryResult::Deny);
        let mut rstr = reply(0, 0, 0);
        rstr[12..16].copy_from_slice(b"RSTR");
        assert_eq!(parse_reply(&rstr, 0, 1), QueryResult::Deny);
        let mut other = reply(0, 0, 0);
        other[12..16].copy_from_slice(b"INIT");
        assert_eq!(parse_reply(&other, 0, 1), QueryResult::Fail);

        assert_eq!(parse_reply(&reply(16, 1, 1), 0, 1), QueryResult::Fail);
        let mut client = reply(2, S as i64, S as i64);
        client[0] = 0x23;
        assert_eq!(parse_reply(&client, 0, 1), QueryResult::Fail);
        assert_eq!(parse_reply(&reply(2, 0, S as i64), 0, 1), QueryResult::Fail);
        assert_eq!(parse_reply(&[0u8; 47], 0, 1), QueryResult::Fail);
    }

    #[test]
    fn burst_hosts_match_exactly_or_as_a_parent_domain() {
        assert!(host_is_burst("ntp.kringkast.com"));
        assert!(host_is_burst("NTP.Kringkast.COM"));
        assert!(host_is_burst("eu.ntp.kringkast.com"));
        assert!(!host_is_burst("notntp.kringkast.com"));
        assert!(!host_is_burst("pool.ntp.org"));
        assert!(!host_is_burst(""));
    }

    #[test]
    fn xorshift_stays_in_range_and_moves() {
        let mut rng = XorShift32::new(0);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let v = rng.range(1000, 30000);
            assert!((1000..=30000).contains(&v));
            seen.insert(v);
        }
        assert!(seen.len() > 100);
        assert_eq!(XorShift32::new(42).range(5, 5), 5);
    }

    /// A machine whose crystal runs `ppm` fast, observed every 15s with a
    /// little symmetric path noise.
    fn window_with_drift(ppm: f64, secs: u64, rtt_ns: i64) -> DriftWindow {
        let mut w = DriftWindow::default();
        let mut t = 0u64;
        let mut i = 0i64;
        while t <= secs * S {
            let true_offset = 5 * S as i64 - (t as f64 * ppm / 1e6) as i64;
            let noise = (i % 5 - 2) * 200_000; // ±0.4ms
            w.add(
                &Sample {
                    offset_ns: true_offset + noise,
                    rtt_ns: rtt_ns + (i % 3) * 100_000,
                },
                t,
            );
            t += 15 * S;
            i += 1;
        }
        w
    }

    #[test]
    fn the_drift_fit_recovers_the_crystal_error() {
        let w = window_with_drift(40.0, 10 * 60, 8_000_000);
        let fit = w.fit().expect("fit");
        assert!(
            (-fit.slope * 1e6 - 40.0).abs() < 2.0,
            "slope {}",
            fit.slope * 1e6
        );
        assert!(fit.used >= consts::NTP_DRIFT_MIN_SAMPLES);
        assert_eq!(fit.anchor_ns, 10 * 60 * S);
        // The intercept is the filtered offset at the newest sample.
        let expected = 5 * S as i64 - ((10 * 60 * S) as f64 * 40.0 / 1e6) as i64;
        assert!(
            (fit.offset_ns - expected).abs() < 500_000,
            "{} vs {expected}",
            fit.offset_ns
        );
    }

    #[test]
    fn the_drift_fit_waits_for_a_baseline_and_rejects_nonsense() {
        // Enough samples but only two minutes of them.
        assert!(window_with_drift(40.0, 120, 8_000_000).fit().is_none());
        // Seven samples over five minutes: too few.
        let mut sparse = DriftWindow::default();
        for i in 0..7u64 {
            sparse.add(&Sample::default(), i * 45 * S);
        }
        assert!(sparse.fit().is_none());
        // A slope no crystal has.
        assert!(window_with_drift(900.0, 10 * 60, 8_000_000).fit().is_none());
        // One queued sample does not tilt the median.
        let mut w = window_with_drift(20.0, 10 * 60, 8_000_000);
        w.add(
            &Sample {
                offset_ns: 5 * S as i64 + 400_000_000,
                rtt_ns: 8_000_000,
            },
            10 * 60 * S + S,
        );
        let fit = w.fit().expect("fit");
        assert!(
            (-fit.slope * 1e6 - 20.0).abs() < 3.0,
            "slope {}",
            fit.slope * 1e6
        );
        // A sample with a far worse round trip is left out entirely.
        let mut noisy = window_with_drift(20.0, 10 * 60, 8_000_000);
        let before = noisy.fit().unwrap().used;
        noisy.add(
            &Sample {
                offset_ns: 0,
                rtt_ns: 300_000_000,
            },
            10 * 60 * S + S,
        );
        assert_eq!(noisy.fit().unwrap().used, before);
        // Aged-out samples do not count.
        let mut old = DriftWindow::default();
        for i in 0..20u64 {
            old.add(&Sample::default(), i * 15 * S);
        }
        old.add(&Sample::default(), 60 * 60 * S);
        assert!(old.fit().is_none());
        assert_eq!(old.len(), 21);
        old.reset();
        assert!(old.is_empty());
    }

    #[test]
    fn the_window_is_bounded() {
        let mut w = DriftWindow::default();
        for i in 0..(consts::NTP_DRIFT_CAPACITY as u64 + 10) {
            w.add(&Sample::default(), i * S);
        }
        assert_eq!(w.len(), consts::NTP_DRIFT_CAPACITY);
    }

    #[test]
    fn the_offset_estimate_reseats_smooths_and_extrapolates() {
        let mut est = OffsetEstimate::default();
        assert_eq!(est.utc_now_ns(5), None);

        let first = est.publish(
            &Sample {
                offset_ns: 1000,
                rtt_ns: 10,
            },
            false,
            None,
            100,
        );
        assert!(first);
        assert_eq!(
            (est.offset_ns, est.rtt_ns, est.sampled_at_ns),
            (1000, 10, 100)
        );
        assert!(!est.drift_valid);
        assert_eq!(est.utc_now_ns(200), Some(1200));

        // A second sample eases in at a quarter …
        assert!(!est.publish(
            &Sample {
                offset_ns: 2000,
                rtt_ns: 10
            },
            false,
            None,
            200
        ));
        assert_eq!(est.offset_ns, 1250);
        // … unless it is far enough off to be a different problem.
        est.publish(
            &Sample {
                offset_ns: 1250 + 3 * S as i64,
                rtt_ns: 10,
            },
            false,
            None,
            300,
        );
        assert_eq!(est.offset_ns, 1250 + 3 * S as i64);

        // Standard policy: a fit is reported but not applied.
        let fit = DriftFit {
            slope: -40e-6,
            offset_ns: 7 * S as i64,
            anchor_ns: 400,
            used: 10,
        };
        est.publish(
            &Sample {
                offset_ns: 7 * S as i64,
                rtt_ns: 10,
            },
            false,
            Some(fit),
            400,
        );
        assert!(est.drift_valid);
        assert!((est.drift_ppm - 40.0).abs() < 1e-9);
        assert_eq!(est.drift_slope, 0.0);
        assert_eq!(est.offset_at(400 + 1000 * S), est.offset_ns);

        // Burst policy: the fit replaces the average and extrapolates, capped.
        est.publish(
            &Sample {
                offset_ns: 0,
                rtt_ns: 10,
            },
            true,
            Some(fit),
            400,
        );
        assert_eq!(est.offset_ns, 7 * S as i64);
        // 60s later at -40ppm: 2.4ms less.
        assert_eq!(est.offset_at(400 + 60 * S), 7 * S as i64 - 2_400_000);
        // Hours later the cap holds.
        assert_eq!(
            est.offset_at(400 + 10_000 * S),
            7 * S as i64 - consts::NTP_DRIFT_MAX_CORRECTION_NS
        );
        // Nothing before the anchor.
        assert_eq!(est.offset_at(100), 7 * S as i64);

        est.forget_drift();
        assert!(!est.drift_valid);
        assert_eq!(est.offset_at(400 + 60 * S), 7 * S as i64);
        assert!(est.synced);
    }
}
