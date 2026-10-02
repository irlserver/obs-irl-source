//! UDP MPEG-TS relay that delays the video of a stream and passes everything
//! else through, to reproduce a sender whose video reaches the plugin later
//! than its audio.
//!
//! ```shell
//! cargo run -p irl-core --example skew-relay -- \
//!     --listen 127.0.0.1:9000 --to 127.0.0.1:9001 --delay-ms 500
//! ```
//!
//! Shifting timestamps (`ffmpeg -itsoffset`) does not reproduce that: the
//! plugin plays what the sender stamped. What a phone with stabilisation on,
//! or pocketSRT queueing its audio early, does is keep the stamps aligned and
//! deliver the video late, so this holds the TS packets of the video PID and
//! leaves the stamps alone. `docs/skew-testing.md` is the recipe around it.
//!
//! The clock of `--ramp` and `--stall-every` starts at the first datagram.
//! Run the tests with `cargo test -p irl-core --example skew-relay`.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: skew-relay --listen ADDR --to ADDR [--video-pid PID] \
                     [--delay-ms N | --ramp FROM:TO:SECONDS] [--stall-every SECONDS --stall-ms N]";

const TS_LEN: usize = 188;
const TS_SYNC: u8 = 0x47;
/// 7 x 188 = 1316 bytes, the usual MPEG-TS over UDP payload, under a 1500 MTU.
const PACKETS_PER_DATAGRAM: usize = 7;
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_HEVC: u8 = 0x24;
const STATUS_EVERY: Duration = Duration::from_secs(5);
const IDLE_WAIT: Duration = Duration::from_millis(20);
const MIN_WAIT: Duration = Duration::from_millis(1);

type Packet = [u8; TS_LEN];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Base {
    Constant { ms: f64 },
    Ramp { from_ms: f64, to_ms: f64, secs: f64 },
}

/// Video is held for `hold_ms` up to every multiple of `every_s`, then the
/// backlog goes out as one burst.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stall {
    every_s: f64,
    hold_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Schedule {
    base: Base,
    stall: Option<Stall>,
}

impl Schedule {
    /// The delay of a video packet that arrives `t` seconds after the first
    /// datagram.
    fn delay_ms(&self, t: f64) -> f64 {
        let base = match self.base {
            Base::Constant { ms } => ms,
            Base::Ramp {
                from_ms,
                to_ms,
                secs,
            } => from_ms + (to_ms - from_ms) * (t / secs).clamp(0.0, 1.0),
        };
        let stall = self.stall.map_or(0.0, |s| {
            let to_release_ms = (s.every_s - t.rem_euclid(s.every_s)) * 1000.0;
            if to_release_ms <= s.hold_ms {
                to_release_ms
            } else {
                0.0
            }
        });
        base + stall
    }
}

#[derive(Debug, PartialEq)]
struct Config {
    listen: SocketAddr,
    to: SocketAddr,
    video_pid: Option<u16>,
    schedule: Schedule,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Config, String> {
    let (mut listen, mut to, mut video_pid) = (None, None, None);
    let (mut delay, mut ramp, mut stall_every, mut stall_ms) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let bad = |what: &str| format!("{flag}: {what}, got {value:?}");
        match flag.as_str() {
            "--listen" => listen = Some(value.parse().map_err(|_| bad("not an address"))?),
            "--to" => to = Some(value.parse().map_err(|_| bad("not an address"))?),
            "--video-pid" => video_pid = Some(parse_pid(&value).ok_or_else(|| bad("not a PID"))?),
            "--delay-ms" => delay = Some(parse_non_negative(&value).ok_or_else(|| bad("not ms"))?),
            "--stall-ms" => {
                stall_ms = Some(parse_non_negative(&value).ok_or_else(|| bad("not ms"))?)
            }
            "--stall-every" => {
                let secs = parse_non_negative(&value).filter(|s| *s > 0.0);
                stall_every = Some(secs.ok_or_else(|| bad("not seconds"))?);
            }
            "--ramp" => {
                let parts: Option<Vec<f64>> = value.split(':').map(parse_non_negative).collect();
                match parts.as_deref() {
                    Some(&[from_ms, to_ms, secs]) if secs > 0.0 => {
                        ramp = Some(Base::Ramp {
                            from_ms,
                            to_ms,
                            secs,
                        });
                    }
                    _ => return Err(bad("expected FROM:TO:SECONDS")),
                }
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    let base = match (delay, ramp) {
        (Some(_), Some(_)) => return Err("--delay-ms and --ramp exclude each other".into()),
        (_, Some(ramp)) => ramp,
        (ms, None) => Base::Constant {
            ms: ms.unwrap_or(0.0),
        },
    };
    let stall = match (stall_every, stall_ms) {
        (None, None) => None,
        (Some(every_s), Some(hold_ms)) if hold_ms < every_s * 1000.0 => {
            Some(Stall { every_s, hold_ms })
        }
        (Some(_), Some(_)) => return Err("--stall-ms must be shorter than --stall-every".into()),
        _ => return Err("--stall-every and --stall-ms go together".into()),
    };
    Ok(Config {
        listen: listen.ok_or("--listen is required")?,
        to: to.ok_or("--to is required")?,
        video_pid,
        schedule: Schedule { base, stall },
    })
}

fn parse_non_negative(s: &str) -> Option<f64> {
    s.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)
}

fn parse_pid(s: &str) -> Option<u16> {
    let pid = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16).ok()?,
        None => s.parse().ok()?,
    };
    (pid < 0x1FFF).then_some(pid)
}

fn pid_of(pkt: &[u8]) -> u16 {
    (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2])
}

/// The PSI section that starts in `pkt`, from its table id up to its CRC.
/// Sections that continue into the next packet are ignored: PAT and PMT fit
/// in one for every stream this is meant for.
fn section(pkt: &[u8]) -> Option<&[u8]> {
    let starts_unit = pkt[1] & 0x40 != 0;
    let control = (pkt[3] >> 4) & 0x3;
    if !starts_unit || control & 0x1 == 0 {
        return None;
    }
    let mut at = 4;
    if control & 0x2 != 0 {
        at += 1 + usize::from(pkt[4]);
    }
    let s = pkt.get(at + 1 + usize::from(*pkt.get(at)?)..)?;
    let len = (usize::from(*s.get(1)? & 0x0F) << 8) | usize::from(*s.get(2)?);
    s.get(..3 + len)?.get(..(3 + len).checked_sub(4)?)
}

fn pmt_pids(pat: &[u8]) -> Vec<u16> {
    if pat.first() != Some(&0x00) {
        return Vec::new();
    }
    let programs = pat.get(8..).unwrap_or_default().chunks_exact(4);
    // Program 0 points at the network PID, not a PMT.
    programs
        .filter(|p| p[0] != 0 || p[1] != 0)
        .map(|p| pid_of(&p[1..]))
        .collect()
}

fn pmt_video_pid(pmt: &[u8]) -> Option<u16> {
    if pmt.first() != Some(&0x02) {
        return None;
    }
    let mut at = 12 + ((usize::from(*pmt.get(10)? & 0x0F) << 8) | usize::from(*pmt.get(11)?));
    while let Some(es) = pmt.get(at..at + 5) {
        if matches!(es[0], STREAM_TYPE_H264 | STREAM_TYPE_HEVC) {
            return Some(pid_of(es));
        }
        at += 5 + ((usize::from(es[3] & 0x0F) << 8) | usize::from(es[4]));
    }
    None
}

struct Relay {
    out: UdpSocket,
    to: SocketAddr,
    schedule: Schedule,
    video_pid: Option<u16>,
    pmt_pids: Vec<u16>,
    origin: Option<Instant>,
    /// Video in arrival order, each with its release time. Released strictly
    /// from the front, so a delay that falls faster than real time bunches
    /// packets up instead of reordering them.
    held: VecDeque<(Instant, Packet)>,
    batch: Vec<u8>,
    /// Packets in and out per PID.
    counts: BTreeMap<u16, (u64, u64)>,
    raw_datagrams: u64,
    send_errors: u64,
}

impl Relay {
    fn elapsed_s(&self, now: Instant) -> f64 {
        self.origin
            .map_or(0.0, |o| now.duration_since(o).as_secs_f64())
    }

    fn ingest(&mut self, datagram: &[u8], now: Instant) {
        let origin = *self.origin.get_or_insert(now);
        let aligned = !datagram.is_empty() && datagram.len().is_multiple_of(TS_LEN);
        if !aligned || datagram.chunks(TS_LEN).any(|p| p[0] != TS_SYNC) {
            // Not plain TS (RTP, or a stray sender): forward it untouched.
            self.raw_datagrams += 1;
            self.flush();
            self.send_errors += u64::from(self.out.send_to(datagram, self.to).is_err());
            return;
        }
        let delay_ms = self
            .schedule
            .delay_ms(now.duration_since(origin).as_secs_f64());
        let release = now + Duration::from_secs_f64(delay_ms / 1000.0);
        for chunk in datagram.chunks_exact(TS_LEN) {
            let pkt: Packet = chunk.try_into().expect("chunks_exact yields TS_LEN bytes");
            let pid = pid_of(&pkt);
            self.counts.entry(pid).or_default().0 += 1;
            self.learn(pid, &pkt);
            if Some(pid) != self.video_pid || (release <= now && self.held.is_empty()) {
                self.emit(&pkt);
            } else {
                self.held.push_back((release, pkt));
            }
        }
    }

    /// Follows PAT and PMT until the video PID is known, unless it was given.
    fn learn(&mut self, pid: u16, pkt: &[u8]) {
        if self.video_pid.is_some() {
            return;
        }
        let Some(sec) = section(pkt) else { return };
        if pid == 0 {
            self.pmt_pids = pmt_pids(sec);
        } else if self.pmt_pids.contains(&pid) {
            self.video_pid = pmt_video_pid(sec);
        }
    }

    fn next_release(&self) -> Option<Instant> {
        self.held.front().map(|(at, _)| *at)
    }

    fn release_due(&mut self, now: Instant) {
        while let Some((at, _)) = self.held.front()
            && *at <= now
        {
            let (_, pkt) = self.held.pop_front().expect("front was Some");
            self.emit(&pkt);
        }
    }

    fn emit(&mut self, pkt: &Packet) {
        self.counts.entry(pid_of(pkt)).or_default().1 += 1;
        self.batch.extend_from_slice(pkt);
        if self.batch.len() == PACKETS_PER_DATAGRAM * TS_LEN {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if !self.batch.is_empty() {
            self.send_errors += u64::from(self.out.send_to(&self.batch, self.to).is_err());
            self.batch.clear();
        }
    }

    fn status(&self, now: Instant) -> String {
        let t = self.elapsed_s(now);
        let mut line = format!(
            "[{t:7.1}s] delay={:.0}ms held={} raw={} send_err={}",
            self.schedule.delay_ms(t),
            self.held.len(),
            self.raw_datagrams,
            self.send_errors
        );
        for (pid, (inn, out)) in &self.counts {
            let tag = if Some(*pid) == self.video_pid {
                " video"
            } else {
                ""
            };
            line.push_str(&format!(" | {pid:#06x}{tag} {inn}/{out}"));
        }
        line
    }
}

fn run(cfg: Config) -> std::io::Result<()> {
    let listen = UdpSocket::bind(cfg.listen)?;
    let any: SocketAddr = if cfg.to.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()
    .expect("literal address");
    let mut relay = Relay {
        out: UdpSocket::bind(any)?,
        to: cfg.to,
        schedule: cfg.schedule,
        video_pid: cfg.video_pid,
        pmt_pids: Vec::new(),
        origin: None,
        held: VecDeque::new(),
        batch: Vec::with_capacity(PACKETS_PER_DATAGRAM * TS_LEN),
        counts: BTreeMap::new(),
        raw_datagrams: 0,
        send_errors: 0,
    };
    eprintln!("relaying {} -> {}, {:?}", cfg.listen, cfg.to, cfg.schedule);
    let mut buf = vec![0u8; 65536];
    let mut next_status = Instant::now() + STATUS_EVERY;
    loop {
        let now = Instant::now();
        let wait = relay
            .next_release()
            .map_or(IDLE_WAIT, |at| at.saturating_duration_since(now))
            .clamp(MIN_WAIT, IDLE_WAIT);
        listen.set_read_timeout(Some(wait))?;
        match listen.recv(&mut buf) {
            Ok(n) => relay.ingest(&buf[..n], Instant::now()),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }
        let now = Instant::now();
        relay.release_due(now);
        relay.flush();
        if now >= next_status {
            eprintln!("{}", relay.status(now));
            next_status += STATUS_EVERY;
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("{USAGE}");
        return if args.is_empty() {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }
    let cfg = match parse_args(args.into_iter()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("skew-relay: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A packet on `pid` carrying one PSI section: `table_id`, then `body`
    /// after the 5 bytes of the extended section header, then a dummy CRC.
    fn psi_packet(pid: u16, table_id: u8, body: &[u8]) -> Packet {
        let len = 5 + body.len() + 4;
        let mut pkt = [0xFF; TS_LEN];
        pkt[..5].copy_from_slice(&[TS_SYNC, 0x40 | (pid >> 8) as u8, pid as u8, 0x10, 0x00]);
        let mut sec = vec![
            table_id,
            0xB0 | (len >> 8) as u8,
            len as u8,
            0,
            1,
            0xC1,
            0,
            0,
        ];
        sec.extend_from_slice(body);
        sec.extend_from_slice(&[0; 4]);
        pkt[5..5 + sec.len()].copy_from_slice(&sec);
        pkt
    }

    #[test]
    fn pat_lists_pmt_pids_and_skips_the_network_pid() {
        let pkt = psi_packet(0, 0x00, &[0, 0, 0xE0, 0x10, 0, 1, 0xF0, 0x00]);
        assert_eq!(pmt_pids(section(&pkt).unwrap()), vec![0x1000]);
    }

    #[test]
    fn pmt_finds_the_video_pid_past_audio_and_descriptors() {
        let body = [
            0xE1, 0x00, 0xF0, 0x00, // PCR PID, no program info
            0x0F, 0xE1, 0x01, 0xF0, 0x02, 0x0A, 0x00, // AAC on 0x101, 2 bytes of descriptor
            0x24, 0xE1, 0x00, 0xF0, 0x00, // HEVC on 0x100
        ];
        let pkt = psi_packet(0x1000, 0x02, &body);
        assert_eq!(pmt_video_pid(section(&pkt).unwrap()), Some(0x100));
    }

    #[test]
    fn pmt_without_video_has_no_video_pid() {
        let pkt = psi_packet(
            0x1000,
            0x02,
            &[0xE1, 0x01, 0xF0, 0x00, 0x0F, 0xE1, 0x01, 0xF0, 0],
        );
        assert_eq!(pmt_video_pid(section(&pkt).unwrap()), None);
    }

    #[test]
    fn ramp_is_linear_then_holds() {
        let s = Schedule {
            base: Base::Ramp {
                from_ms: 0.0,
                to_ms: 1000.0,
                secs: 10.0,
            },
            stall: None,
        };
        assert_eq!(s.delay_ms(5.0), 500.0);
        assert_eq!(s.delay_ms(20.0), 1000.0);
    }

    #[test]
    fn stall_holds_up_to_each_period_boundary() {
        let stall = Some(Stall {
            every_s: 4.0,
            hold_ms: 200.0,
        });
        let s = Schedule {
            base: Base::Constant { ms: 500.0 },
            stall,
        };
        assert_eq!(s.delay_ms(1.0), 500.0);
        assert!((s.delay_ms(3.85) - 650.0).abs() < 1e-6);
        assert_eq!(s.delay_ms(4.0), 500.0);
    }

    #[test]
    fn args_parse_and_reject_conflicts() {
        let args = |s: &str| {
            s.split(' ')
                .map(String::from)
                .collect::<Vec<_>>()
                .into_iter()
        };
        let cfg = parse_args(args(
            "--listen 127.0.0.1:9000 --to 127.0.0.1:9001 --video-pid 0x100 \
                                   --ramp 0:1000:30 --stall-every 4 --stall-ms 200",
        ))
        .unwrap();
        assert_eq!(cfg.video_pid, Some(0x100));
        assert_eq!(
            cfg.schedule.stall,
            Some(Stall {
                every_s: 4.0,
                hold_ms: 200.0
            })
        );
        assert!(
            parse_args(args(
                "--listen 127.0.0.1:1 --to 127.0.0.1:2 --delay-ms 5 --ramp 0:1:1"
            ))
            .is_err()
        );
        assert!(parse_args(args("--listen 127.0.0.1:1 --to 127.0.0.1:2 --stall-ms 5")).is_err());
    }
}
