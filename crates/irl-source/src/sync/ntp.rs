//! The module-wide SNTP client (port of the C branch's `sync-ntp.c`,
//! Spillmaker/obs-irl-source `nal-timecodes`).
//!
//! Timecode sync compares a sender's wall clock against this machine's, so
//! both ends have to be disciplined against the same reference. The host
//! system clock is not good enough for that: Windows' w32time defaults to a
//! nine-hour poll and roughly one-second accuracy, two orders of magnitude
//! past the frame-level alignment this feature promises. Moblin does not
//! trust iOS's clock either — it runs its own NTP client and stamps frames
//! from that — so the receiving end matches it.
//!
//! One client per module, shared by every source. Thread-safe. Polling runs
//! from module load, not from the master sync switch: the clock is what an
//! operator reads to decide whether sync is worth turning on.
//!
//! Two polling policies, chosen from the server name (see
//! `irl_core::ntp::host_is_burst`):
//!
//! - **Burst** — servers this project runs, for users of this plugin. Four
//!   packets on acquisition, lowest round trip wins, then one packet every 1
//!   to 30 seconds at random. Roughly four packets a minute per client is
//!   nothing to a server that exists for this, and it buys the sample density
//!   the drift estimate needs.
//! - **Standard** — public pools, which are donated capacity. One packet per
//!   poll at the conventional 64 seconds, doubling that on a kiss-o'-death
//!   rate code and stopping altogether on a denial.
//!
//! A burst server that stops answering is a broken clock for everyone using
//! it, so three failed polls hand over to a public pool at the standard
//! cadence, while the configured server is probed again every 30 to 60
//! seconds (randomised, so that everyone knocked off by the same outage does
//! not come back in step).

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::thread::JoinHandle;
use std::time::Duration;

use irl_core::consts;
use irl_core::ntp::{DriftWindow, OffsetEstimate, QueryResult, Sample, XorShift32, host_is_burst};
use parking_lot::{Condvar, Mutex};

/// The client as the rest of the plugin sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NtpStatus {
    pub synced: bool,
    /// `utc_ns = os_gettime_ns() + offset_ns`, drift correction included.
    /// Only meaningful when `synced`.
    pub offset_ns: i64,
    /// Round-trip time of the sample the current offset came from, and how
    /// long ago it was taken: an offset from a 400ms round trip is worth less
    /// than one from 8ms, and a sync that stopped refreshing an hour ago is a
    /// broken reference that would otherwise keep ticking plausibly.
    pub rtt_ns: i64,
    pub age_ns: u64,
    /// The server actually being polled: the fallback pool rather than
    /// `primary` while `fallback_active`.
    pub server: String,
    pub primary: String,
    pub burst_mode: bool,
    pub fallback_active: bool,
    /// This machine's measured crystal error, positive when the local clock
    /// runs fast. Measured under both policies once enough samples exist,
    /// applied only under the burst policy.
    pub drift_valid: bool,
    pub drift_ppm: f64,
}

struct State {
    running: bool,
    /// Configured server.
    server: String,
    /// Being polled; differs from `server` while failed over.
    active: String,
    /// Set by any setter to mean "re-read the settings, do not sleep".
    config_dirty: bool,
    enabled: bool,
    burst_mode: bool,
    fallback_active: bool,
    estimate: OffsetEstimate,
}

struct Client {
    state: Mutex<State>,
    wake: Condvar,
    thread: Mutex<Option<JoinHandle<()>>>,
}

static NTP: Client = Client {
    state: Mutex::new(State {
        running: false,
        server: String::new(),
        active: String::new(),
        config_dirty: false,
        enabled: false,
        burst_mode: false,
        fallback_active: false,
        estimate: OffsetEstimate {
            synced: false,
            offset_ns: 0,
            rtt_ns: 0,
            sampled_at_ns: 0,
            drift_valid: false,
            drift_ppm: 0.0,
            drift_slope: 0.0,
            drift_anchor_ns: 0,
        },
    }),
    wake: Condvar::new(),
    thread: Mutex::new(None),
};

// ── Public API ────────────────────────────────────────────────

/// Start the polling thread. Idempotent.
pub fn start() {
    {
        let mut state = NTP.state.lock();
        if state.running {
            return;
        }
        state.running = true;
    }
    let spawned = std::thread::Builder::new()
        .name("irl-ntp".to_owned())
        .spawn(|| {
            let result = std::panic::catch_unwind(poll_thread);
            if let Err(payload) = result {
                let msg = obs::panic::payload_message(payload.as_ref());
                irl_error!("NTP thread panicked: {msg}; timecode sync stays unsynced");
                NTP.state.lock().running = false;
            }
        });
    match spawned {
        Ok(handle) => *NTP.thread.lock() = Some(handle),
        Err(_) => {
            NTP.state.lock().running = false;
            irl_error!("Failed to start NTP thread; timecode sync unavailable");
        }
    }
}

/// Stop the polling thread and wait for it. Idempotent.
pub fn stop() {
    {
        let mut state = NTP.state.lock();
        if !state.running {
            return;
        }
        state.running = false;
        NTP.wake.notify_all();
    }
    if let Some(handle) = NTP.thread.lock().take() {
        let _ = handle.join();
    }
}

/// Change the server. Takes effect on the next poll, and forces one
/// immediately. An empty host stops polling and drops the sync.
pub fn set_server(host: &str) {
    let mut state = NTP.state.lock();
    if !host.is_empty() {
        if state.server != host {
            state.server = host.to_owned();
            // The old offset belongs to the old reference.
            state.estimate.synced = false;
            state.config_dirty = true;
            NTP.wake.notify_all();
        }
    } else if !state.server.is_empty() {
        state.server.clear();
        state.estimate.synced = false;
        state.config_dirty = true;
        NTP.wake.notify_all();
    }
}

/// Start or stop polling. Disabling keeps the last offset rather than
/// dropping it — it simply ages, which [`status`] reports — so re-enabling
/// has something usable before the first fresh sample arrives.
pub fn set_enabled(enabled: bool) {
    let changed = {
        let mut state = NTP.state.lock();
        let changed = state.enabled != enabled;
        if changed {
            state.enabled = enabled;
            state.config_dirty = true;
            NTP.wake.notify_all();
        }
        changed
    };
    if changed {
        irl_info!(
            "NTP polling {}",
            if enabled { "started" } else { "stopped" }
        );
    }
}

/// The current state of the reference.
pub fn status() -> NtpStatus {
    let now = obs::time::gettime_ns();
    let state = NTP.state.lock();
    NtpStatus {
        synced: state.estimate.synced,
        offset_ns: state.estimate.offset_at(now),
        rtt_ns: state.estimate.rtt_ns,
        age_ns: if state.estimate.sampled_at_ns != 0 {
            now.wrapping_sub(state.estimate.sampled_at_ns)
        } else {
            0
        },
        server: if state.active.is_empty() {
            state.server.clone()
        } else {
            state.active.clone()
        },
        primary: state.server.clone(),
        burst_mode: state.burst_mode,
        fallback_active: state.fallback_active,
        drift_valid: state.estimate.drift_valid,
        drift_ppm: state.estimate.drift_ppm,
    }
}

/// Current UTC in nanoseconds since the Unix epoch, or `None` when no sync
/// has been established.
pub fn utc_now_ns() -> Option<i64> {
    let now = obs::time::gettime_ns();
    NTP.state.lock().estimate.utc_now_ns(now)
}

// ── One sample ────────────────────────────────────────────────

fn should_stop() -> bool {
    !NTP.state.lock().running
}

fn is_synced() -> bool {
    NTP.state.lock().estimate.synced
}

/// Exchange one packet with `addr`. The two local timestamps come from
/// `os_gettime_ns` rather than the system clock, which is the whole point:
/// the result is an offset onto the same monotonic timebase every other part
/// of the plugin schedules against.
fn query_once(addr: SocketAddr) -> QueryResult {
    let bind: SocketAddr = if addr.is_ipv4() {
        ([0, 0, 0, 0], 0).into()
    } else {
        ([0u16; 8], 0).into()
    };
    let Ok(sock) = UdpSocket::bind(bind) else {
        return QueryResult::Fail;
    };
    if sock.connect(addr).is_err()
        || sock
            .set_read_timeout(Some(Duration::from_millis(consts::NTP_RECV_TIMEOUT_MS)))
            .is_err()
    {
        return QueryResult::Fail;
    }

    let packet = irl_core::ntp::request_packet();
    let t1 = obs::time::gettime_ns();
    if sock.send(&packet).ok() != Some(packet.len()) {
        return QueryResult::Fail;
    }

    let mut reply = [0u8; consts::NTP_PACKET_BYTES];
    let Ok(got) = sock.recv(&mut reply) else {
        return QueryResult::Fail;
    };
    let t4 = obs::time::gettime_ns();
    irl_core::ntp::parse_reply(&reply[..got], t1, t4)
}

/// `samples` packets to `host`, keeping the one with the lowest round trip.
/// Cellular and Wi-Fi round trips vary by hundreds of milliseconds; the
/// minimum of a burst is far closer to the truth than any single sample.
fn poll(host: &str, samples: usize) -> QueryResult {
    let Some(addr) = (host, consts::NTP_PORT)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
    else {
        return QueryResult::Fail;
    };

    let mut best: Option<Sample> = None;
    let mut worst = QueryResult::Fail;

    for _ in 0..samples {
        // Checked between samples so shutdown waits out at most one receive
        // timeout rather than the whole burst. Without it, quitting OBS
        // mid-poll would block the join for seconds.
        if should_stop() {
            break;
        }
        match query_once(addr) {
            QueryResult::Ok(sample) => {
                if sample.rtt_ns > consts::NTP_MAX_USABLE_RTT_NS {
                    continue;
                }
                if best.is_none_or(|b| sample.rtt_ns < b.rtt_ns) {
                    best = Some(sample);
                }
            }
            // A kiss-o'-death answer is information; keep it to report if no
            // sample survives the burst.
            QueryResult::Fail => {}
            other => worst = other,
        }
    }

    match best {
        Some(sample) => QueryResult::Ok(sample),
        None => worst,
    }
}

// ── Publishing ────────────────────────────────────────────────

fn publish_sample(best: &Sample, burst_mode: bool, drift: &mut DriftWindow) {
    let now = obs::time::gettime_ns();
    drift.add(best, now);
    let fit = drift.fit();

    let (first, published) = {
        let mut state = NTP.state.lock();
        let first = state.estimate.publish(best, burst_mode, fit, now);
        (first, state.estimate.offset_ns)
    };

    if first {
        irl_info!(
            "NTP synced: offset {:+}ms, round trip {}ms",
            published / 1_000_000,
            best.rtt_ns / 1_000_000
        );
    }
}

/// Announce which server is being polled and under which policy. The drift
/// estimate belongs to the reference it was measured against, so it goes
/// with it.
fn set_active(host: &str, burst_mode: bool, fallback: bool) {
    let mut state = NTP.state.lock();
    state.active = host.to_owned();
    state.burst_mode = burst_mode;
    state.fallback_active = fallback;
    state.estimate.forget_drift();
}

// ── Poll loop ─────────────────────────────────────────────────

/// Thread-private scheduling state.
struct PollState {
    primary: String,
    burst: bool,
    fallback: bool,
    /// Next poll is the acquisition burst.
    acquire: bool,
    /// Server told us to stop, so we did.
    denied: bool,
    /// Failed over, and the primary is worth retrying.
    probe_primary: bool,
    consec_fail: u32,
    pool_interval_ms: u32,
    pool_due_ns: u64,
    probe_due_ns: u64,
}

impl PollState {
    fn new() -> Self {
        Self {
            primary: String::new(),
            burst: false,
            fallback: false,
            acquire: false,
            denied: false,
            probe_primary: false,
            consec_fail: 0,
            pool_interval_ms: consts::NTP_POLL_INTERVAL_MS,
            pool_due_ns: 0,
            probe_due_ns: 0,
        }
    }

    /// The configured server changed: everything starts over.
    fn reconfigure(&mut self, primary: &str) {
        self.primary = primary.to_owned();
        self.burst = host_is_burst(primary);
        self.fallback = false;
        self.probe_primary = false;
        self.acquire = true;
        self.denied = false;
        self.consec_fail = 0;
        self.pool_interval_ms = consts::NTP_POLL_INTERVAL_MS;
        self.pool_due_ns = 0;
        self.probe_due_ns = 0;
    }
}

fn ms_to_ns(ms: u32) -> u64 {
    u64::from(ms) * 1_000_000
}

fn burst_interval_ms(rng: &mut XorShift32) -> u32 {
    rng.range(
        consts::NTP_BURST_MIN_INTERVAL_MS,
        consts::NTP_BURST_MAX_INTERVAL_MS,
    )
}

fn probe_delay_ns(rng: &mut XorShift32) -> u64 {
    ms_to_ns(rng.range(
        consts::NTP_FALLBACK_PROBE_MIN_MS,
        consts::NTP_FALLBACK_PROBE_MAX_MS,
    ))
}

/// `probe` is false when the primary refused service rather than went
/// missing: the clock still moves to the pool, but a server that said go away
/// is not poked every half minute to see whether it meant it.
fn begin_fallback(st: &mut PollState, probe: bool, drift: &mut DriftWindow, rng: &mut XorShift32) {
    st.fallback = true;
    st.probe_primary = probe;
    st.pool_interval_ms = consts::NTP_POLL_INTERVAL_MS;
    // The pool is polled at once, to get the clock back on something live;
    // the server that just failed is left alone until the first probe is due.
    st.pool_due_ns = 0;
    st.probe_due_ns = if probe {
        obs::time::gettime_ns() + probe_delay_ns(rng)
    } else {
        u64::MAX
    };
    drift.reset();
    // The offset itself is kept: it is a few milliseconds stale at worst, and
    // dropping the sync would stall every source over an outage the pool is
    // about to paper over. The pool's own samples ease it across.
    set_active(consts::NTP_FALLBACK_SERVER, false, true);
}

fn end_fallback(st: &mut PollState, drift: &mut DriftWindow) {
    irl_info!("NTP server '{}' is back; resuming", st.primary);
    st.fallback = false;
    st.consec_fail = 0;
    st.acquire = true;
    drift.reset();
    set_active(&st.primary, st.burst, false);
}

/// Returns how long to wait before the next iteration, in milliseconds.
fn poll_primary(st: &mut PollState, drift: &mut DriftWindow, rng: &mut XorShift32) -> u32 {
    let samples = if st.acquire && st.burst {
        consts::NTP_BURST
    } else {
        1
    };

    match poll(&st.primary, samples) {
        QueryResult::Ok(best) => {
            publish_sample(&best, st.burst, drift);
            st.acquire = false;
            st.consec_fail = 0;
            if st.burst {
                burst_interval_ms(rng)
            } else {
                st.pool_interval_ms
            }
        }
        QueryResult::Rate => {
            // Being asked to slow down is not a failure, so it does not count
            // toward the failover — the server is answering.
            if st.pool_interval_ms < consts::NTP_POLL_MAX_INTERVAL_MS {
                st.pool_interval_ms *= 2;
            }
            irl_warn!(
                "NTP server '{}' asked for a slower poll; backing off to {}s",
                st.primary,
                st.pool_interval_ms / 1000
            );
            st.pool_interval_ms
        }
        QueryResult::Deny => {
            // Ignoring this is how a client gets an address blocked, so this
            // server is not asked again. If there is somewhere else to go, go
            // there; otherwise the last offset stays and ages, which the dock
            // reports.
            if !st.primary.eq_ignore_ascii_case(consts::NTP_FALLBACK_SERVER) {
                irl_warn!(
                    "NTP server '{}' refused service; using '{}' instead",
                    st.primary,
                    consts::NTP_FALLBACK_SERVER
                );
                begin_fallback(st, false, drift, rng);
                return 0;
            }
            st.denied = true;
            irl_warn!(
                "NTP server '{}' refused service; polling stopped. Set a different server in the IRL Sync dock",
                st.primary
            );
            consts::NTP_IDLE_INTERVAL_MS
        }
        QueryResult::Fail => {
            st.consec_fail += 1;

            if st.burst
                && st.consec_fail >= consts::NTP_FALLBACK_AFTER_FAILURES
                && !st.primary.eq_ignore_ascii_case(consts::NTP_FALLBACK_SERVER)
            {
                irl_warn!(
                    "NTP server '{}' unreachable; falling back to '{}'",
                    st.primary,
                    consts::NTP_FALLBACK_SERVER
                );
                begin_fallback(st, true, drift, rng);
                return 0; // poll the pool immediately
            }

            // Once per outage rather than once per attempt: at the burst
            // policy's retry gap this line would otherwise fill the log.
            if st.consec_fail == 1 {
                irl_warn!(
                    "NTP poll of '{}' failed; timecode sync stays unsynced until it succeeds",
                    st.primary
                );
            }

            if st.burst {
                consts::NTP_BURST_RETRY_MS
            } else if is_synced() {
                st.pool_interval_ms
            } else {
                consts::NTP_RETRY_INTERVAL_MS
            }
        }
    }
}

/// Failed over: probe the configured server on its own schedule, and keep the
/// clock alive off the pool on its.
fn poll_fallback(st: &mut PollState, drift: &mut DriftWindow, rng: &mut XorShift32) -> u32 {
    let mut now = obs::time::gettime_ns();

    if st.probe_primary && now >= st.probe_due_ns {
        if let QueryResult::Ok(best) = poll(&st.primary, 1) {
            end_fallback(st, drift);
            // Seeds the window the reset above emptied; the acquisition burst
            // follows a second later.
            publish_sample(&best, st.burst, drift);
            return consts::NTP_BURST_MIN_INTERVAL_MS;
        }
        st.probe_due_ns = obs::time::gettime_ns() + probe_delay_ns(rng);
    }

    now = obs::time::gettime_ns();
    if now >= st.pool_due_ns {
        let next = match poll(consts::NTP_FALLBACK_SERVER, 1) {
            QueryResult::Ok(best) => {
                publish_sample(&best, false, drift);
                st.pool_interval_ms
            }
            QueryResult::Rate => {
                if st.pool_interval_ms < consts::NTP_POLL_MAX_INTERVAL_MS {
                    st.pool_interval_ms *= 2;
                }
                st.pool_interval_ms
            }
            // Both servers are refusing. Nothing left to poll, so stop asking;
            // the probe still runs the primary.
            QueryResult::Deny => consts::NTP_POLL_MAX_INTERVAL_MS,
            QueryResult::Fail => {
                if is_synced() {
                    st.pool_interval_ms
                } else {
                    consts::NTP_RETRY_INTERVAL_MS
                }
            }
        };
        st.pool_due_ns = obs::time::gettime_ns() + ms_to_ns(next);
    }

    // Wake for whichever of the two is due first.
    now = obs::time::gettime_ns();
    let next_ns = st.probe_due_ns.min(st.pool_due_ns);
    if next_ns <= now {
        return 1;
    }
    let wait_ms = (next_ns - now) / 1_000_000;
    if wait_ms < 1 {
        return 1;
    }
    wait_ms.min(u64::from(consts::NTP_IDLE_INTERVAL_MS)) as u32
}

fn poll_thread() {
    let mut st = PollState::new();
    let mut drift = DriftWindow::default();
    // Seeded off the clock and this thread's own stack, so two OBS instances
    // started by the same script do not draw the same intervals.
    let seed_anchor = 0u8;
    let seed = (obs::time::gettime_ns() >> 8) as u32 ^ (&raw const seed_anchor as usize as u32);
    let mut rng = XorShift32::new(seed);

    loop {
        let (enabled, configured) = {
            let mut state = NTP.state.lock();
            if !state.running {
                break;
            }
            state.config_dirty = false;
            (state.enabled, state.server.clone())
        };

        if configured != st.primary {
            st.reconfigure(&configured);
            drift.reset();
            set_active(&st.primary, st.burst, false);
            if !st.primary.is_empty() {
                irl_info!(
                    "NTP server '{}': {} polling",
                    st.primary,
                    if st.burst { "burst" } else { "standard" }
                );
            }
        }

        let wait_ms = if !enabled || st.primary.is_empty() || st.denied {
            // Nothing to do: polling is off, no server is set, or the server
            // has refused us. Sit on the condvar until a setter says
            // otherwise. No packets leave the machine in this state.
            consts::NTP_IDLE_INTERVAL_MS
        } else if st.fallback {
            poll_fallback(&mut st, &mut drift, &mut rng)
        } else {
            poll_primary(&mut st, &mut drift, &mut rng)
        };

        // Woken early by a settings change or by shutdown. The predicate
        // re-check handles both, and a spurious wakeup just re-reads the
        // settings.
        let mut state = NTP.state.lock();
        if state.running && !state.config_dirty && wait_ms > 0 {
            NTP.wake
                .wait_for(&mut state, Duration::from_millis(u64::from(wait_ms)));
        }
    }
}
