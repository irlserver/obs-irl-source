//! Timecode sync: the global settings, the source registry behind the dock
//! and the websocket vendor, their persistence, and the module lifecycle.
//!
//! Port of the C branch's `src/sync/` (Spillmaker/obs-irl-source
//! `nal-timecodes`): `sync-config.c` is this file, `sync-control.c` is
//! [`control`] over the arithmetic in `irl_core::sync`, `sync-ntp.c` is
//! [`ntp`] over `irl_core::ntp`, `sync-sei.c` is `irl_core::sei`, and
//! `sync-dock.cpp` is the `sync-dock` crate behind [`dock`].
//!
//! The model is absolute, not relative. Every synced source independently
//! presents the frame stamped T at NTP time T + offset; sources never
//! negotiate with each other, and none of them needs to know the others
//! exist. Two OBS instances anywhere, on the same NTP reference and the same
//! offset, therefore composite the same captured moment at the same instant —
//! which is the point, and is why the offset is one global number rather than
//! a per-source setting.
//!
//! That choice also keeps the hot path free of cross-source coordination: the
//! receiver thread reads two atomics and its own timecodes. What lives here is
//! only the settings those atomics come from, plus a registry that exists so
//! the dock and the websocket vendor have something to enumerate.

pub mod control;
mod dock;
pub mod ntp;

use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::Relaxed};

use irl_core::consts;
use irl_core::sync::{SyncSnapshot, SyncStatus, apply_staleness, clamp_offset_ms};
use obs::{Data, DataArray, OwnedData, SourceHandle};
use parking_lot::Mutex;

/// The module config file the three global settings persist in.
const CONFIG_FILE: &CStr = c"sync.json";

/// The global settings, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    /// The master switch.
    pub enabled: bool,
    /// The shared presentation offset.
    pub offset_ms: i32,
    /// The NTP server or pool every sender must also use.
    pub ntp_server: String,
}

/// One registered source, as the dock and the vendor see it.
#[derive(Debug, Clone)]
pub struct SyncEntry {
    pub source_name: String,
    pub sync_enabled: bool,
    pub snap: SyncSnapshot,
}

/// Identifies a source in the registry: the `obs_source_t` address, which is
/// stable from `create` to `destroy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceKey(usize);

impl SourceKey {
    fn of(source: SourceHandle) -> Self {
        Self(source.as_ptr() as usize)
    }
}

struct Slot {
    key: SourceKey,
    /// Kept for the name lookup in [`collect`]; the source outlives its slot
    /// because `Drop` unregisters before libobs tears anything down.
    source: SourceHandle,
    sync_enabled: bool,
    snap: SyncSnapshot,
    /// When the receiver thread last published. A source whose receiver has
    /// stopped delivering stops publishing, and its last snapshot would
    /// otherwise keep reading as healthy forever.
    updated_ns: u64,
}

struct Cold {
    ntp_server: String,
    slots: Vec<Slot>,
}

struct Global {
    /// Hot: read per packet by every receiver thread.
    enabled: AtomicBool,
    offset_ms: AtomicI32,
    /// Bumped whenever the offset changes (or sync is switched on), so a
    /// receiver notices and re-seats instead of slewing several seconds at
    /// the drift rate.
    offset_generation: AtomicU32,
    /// Cold: settings, dock and vendor only.
    cold: Mutex<Cold>,
}

static SYNC: Global = Global {
    enabled: AtomicBool::new(false),
    offset_ms: AtomicI32::new(consts::SYNC_DEFAULT_OFFSET_MS),
    offset_generation: AtomicU32::new(0),
    cold: Mutex::new(Cold {
        ntp_server: String::new(),
        slots: Vec::new(),
    }),
};

// ── Hot-path readers ──────────────────────────────────────────

/// The master switch. Safe from any thread.
pub fn is_enabled() -> bool {
    SYNC.enabled.load(Relaxed)
}

/// The shared offset. Safe from any thread.
pub fn offset_ms() -> i32 {
    SYNC.offset_ms.load(Relaxed)
}

/// Bumped whenever the offset changes.
pub fn offset_generation() -> u32 {
    SYNC.offset_generation.load(Relaxed)
}

// ── Settings ──────────────────────────────────────────────────

/// Read the current settings. They persist in the module config directory,
/// loaded at module load and written by each setter — global rather than
/// per-source, so they live outside the scene collection: the offset is a
/// number a group of streamers agrees on, not a property of one machine's
/// layout. A scene collection copied to another machine therefore arrives
/// without it, which is the intended behaviour.
pub fn config() -> SyncConfig {
    SyncConfig {
        enabled: is_enabled(),
        offset_ms: offset_ms(),
        ntp_server: SYNC.cold.lock().ntp_server.clone(),
    }
}

/// Flip the master switch.
pub fn set_enabled(enabled: bool) {
    if is_enabled() == enabled {
        return;
    }
    SYNC.enabled.store(enabled, Relaxed);
    // Turning sync on is a re-seat for every source, same as an offset
    // change: their delay lines start from nothing.
    SYNC.offset_generation.fetch_add(1, Relaxed);
    irl_info!(
        "Timecode sync {} (offset {}ms)",
        if enabled { "enabled" } else { "disabled" },
        offset_ms()
    );
    save();
}

/// Set the shared offset, clamped to the settable range.
pub fn set_offset_ms(offset_ms: i32) {
    let offset_ms = clamp_offset_ms(offset_ms);
    if self::offset_ms() == offset_ms {
        return;
    }
    SYNC.offset_ms.store(offset_ms, Relaxed);
    // A user turning this knob is asking every synced source to shift at
    // once and expects a hitch. Bumping the generation makes the receivers
    // step rather than slew, which converges in one frame instead of the
    // minute a drift-rate correction would take.
    SYNC.offset_generation.fetch_add(1, Relaxed);
    irl_info!("Timecode sync offset set to {offset_ms}ms");
    save();
}

/// Change the NTP server. Takes effect on the next poll, and forces one
/// immediately. An empty host stops polling.
pub fn set_ntp_server(host: &str) {
    let applied = {
        let mut cold = SYNC.cold.lock();
        cold.ntp_server = host.to_owned();
        cold.ntp_server.clone()
    };
    ntp::set_server(&applied);
    save();
}

// ── Persistence ───────────────────────────────────────────────

/// `<module config dir>/sync.json`, creating the directory on demand: OBS
/// only makes a module's config directory when a module asks for it.
fn config_path() -> Option<CString> {
    let path = obs::module::config_path(CONFIG_FILE)?;
    if let Some(dir) = path.parent()
        && let Err(err) = std::fs::create_dir_all(dir)
    {
        irl_warn!("Could not create {}: {err}", dir.display());
    }
    CString::new(path.to_str()?).ok()
}

fn load() {
    // Defaults first, so a missing or unreadable file is simply "not
    // configured yet" rather than a failure to start.
    SYNC.enabled.store(false, Relaxed);
    SYNC.offset_ms
        .store(consts::SYNC_DEFAULT_OFFSET_MS, Relaxed);
    SYNC.cold.lock().ntp_server = consts::SYNC_DEFAULT_NTP_SERVER.to_owned();

    let Some(path) = config_path() else {
        return;
    };
    let Some(data) = OwnedData::from_json_file(&path) else {
        return;
    };
    let data = data.data();
    data.set_default_bool(c"enabled", false);
    data.set_default_i64(c"offset_ms", i64::from(consts::SYNC_DEFAULT_OFFSET_MS));
    let Ok(default_server) = CString::new(consts::SYNC_DEFAULT_NTP_SERVER) else {
        return;
    };
    data.set_default_str(c"ntp_server", &default_server);

    let offset = clamp_offset_ms(
        data.get_i64(c"offset_ms")
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
    );
    SYNC.enabled.store(data.get_bool(c"enabled"), Relaxed);
    SYNC.offset_ms.store(offset, Relaxed);
    // `get_str` reads an empty string as None: an emptied server is a real
    // setting ("no server configured"), so it is kept rather than defaulted.
    SYNC.cold.lock().ntp_server = data.get_str(c"ntp_server").unwrap_or_default();
}

fn save() {
    let Some(path) = config_path() else {
        return;
    };
    let data = OwnedData::new();
    data.data().set_bool(c"enabled", is_enabled());
    data.data().set_i64(c"offset_ms", i64::from(offset_ms()));
    let server = SYNC.cold.lock().ntp_server.clone();
    if let Ok(server) = CString::new(server) {
        data.data().set_str(c"ntp_server", &server);
    }
    if !data.data().save_json_safe(&path, c"tmp", c"bak") {
        irl_warn!("Could not save sync settings to {}", path.to_string_lossy());
    }
}

// ── Registry ──────────────────────────────────────────────────

/// Add a source to the registry, with its current "Sync" setting.
pub fn register_source(source: SourceHandle, sync_enabled: bool) {
    let mut cold = SYNC.cold.lock();
    if cold.slots.len() >= consts::SYNC_MAX_SOURCES {
        irl_warn!(
            "More than {} IRL sources; the newest will not appear in the sync dock",
            consts::SYNC_MAX_SOURCES
        );
        return;
    }
    cold.slots.push(Slot {
        key: SourceKey::of(source),
        source,
        sync_enabled,
        snap: SyncSnapshot::default(),
        updated_ns: 0,
    });
}

/// The source's "Sync" checkbox changed.
pub fn set_source_sync_enabled(source: SourceHandle, sync_enabled: bool) {
    let key = SourceKey::of(source);
    let mut cold = SYNC.cold.lock();
    if let Some(slot) = cold.slots.iter_mut().find(|s| s.key == key) {
        slot.sync_enabled = sync_enabled;
    }
}

/// Remove a source. Must run before the source is torn down: [`collect`]
/// reads names through the handles it holds.
pub fn unregister_source(source: SourceHandle) {
    let key = SourceKey::of(source);
    let mut cold = SYNC.cold.lock();
    if let Some(index) = cold.slots.iter().position(|s| s.key == key) {
        cold.slots.swap_remove(index);
    }
}

/// Publish this source's latest status. Called from the receiver thread.
pub fn publish(source: SourceHandle, snap: &SyncSnapshot) {
    let key = SourceKey::of(source);
    let mut cold = SYNC.cold.lock();
    if let Some(slot) = cold.slots.iter_mut().find(|s| s.key == key) {
        slot.snap = *snap;
        slot.updated_ns = obs::time::gettime_ns();
    }
}

/// Snapshot every registered source.
pub fn collect() -> Vec<SyncEntry> {
    let now_ns = obs::time::gettime_ns();
    // The lock is held across `obs_source_get_name()` so a source cannot be
    // unregistered — which `Drop` does before tearing anything down — while
    // its name is being read.
    let cold = SYNC.cold.lock();
    cold.slots
        .iter()
        .map(|slot| {
            let mut snap = slot.snap;
            apply_staleness(&mut snap, slot.updated_ns, now_ns);
            SyncEntry {
                source_name: slot.source.name(),
                sync_enabled: slot.sync_enabled,
                snap,
            }
        })
        .collect()
}

// ── Module lifecycle ──────────────────────────────────────────

/// `irl_sync_module_load`: start the clock and load the settings. Ordering:
/// the NTP client has to exist before the settings are loaded, because
/// loading them applies the configured server to it.
///
/// Polling is not gated on the master switch. It was, to avoid sending
/// packets to a third-party pool for users who never turn sync on, and the
/// cost of that was a dock whose clock sat dead until sync had been enabled
/// once — which is backwards, because the clock is what tells you whether
/// sync would work before you commit to it. One request a minute is the
/// price.
pub fn module_load() {
    ntp::start();
    load();
    let cfg = config();
    ntp::set_server(&cfg.ntp_server);
    ntp::set_enabled(true);
}

/// `irl_sync_module_post_load`: the dock needs the frontend, which is only
/// guaranteed once module loading has finished.
pub fn module_post_load() {
    dock::register();
}

/// `irl_sync_module_unload`.
pub fn module_unload() {
    dock::unregister();
    ntp::stop();
}

// ── obs-websocket vendor ──────────────────────────────────────

fn set_string(data: &Data<'_>, key: &CStr, value: &str) {
    if let Ok(value) = CString::new(value) {
        data.set_str(key, &value);
    }
}

/// `GetSyncStatus`: the whole sync picture in one call — what the group is
/// configured to do, whether this machine has a clock worth trusting, and
/// every source's status.
///
/// This is the alerting path. A dock only helps when someone is looking at
/// it, and during a live IRL show the operator may be nowhere near the
/// machine — whereas the person who can actually fix an out-of-sync feed is
/// the one holding the phone, and chat is how you reach them. A bot polling
/// this can say "chase cam out of sync, needs 7.4s" where it will be seen.
pub fn vendor_status(_request: &Data<'_>, response: &OwnedData) {
    let cfg = config();
    let response = response.data();
    response.set_bool(c"sync_enabled", cfg.enabled);
    response.set_i64(c"offset_ms", i64::from(cfg.offset_ms));

    let status = ntp::status();
    let clock = OwnedData::new();
    {
        let clock = clock.data();
        clock.set_bool(c"synced", status.synced);
        set_string(&clock, c"server", &status.server);
        clock.set_i64(c"offset_ms", status.offset_ns / 1_000_000);
        clock.set_i64(c"rtt_ms", status.rtt_ns / 1_000_000);
        clock.set_i64(c"age_ms", (status.age_ns / 1_000_000) as i64);
        // Which server is answering, and whether it is the one that was asked
        // for: a bot watching this is the thing most likely to notice that
        // everyone quietly failed over to the public pool.
        set_string(&clock, c"primary_server", &status.primary);
        clock.set_bool(c"burst_mode", status.burst_mode);
        clock.set_bool(c"fallback_active", status.fallback_active);
        clock.set_bool(c"drift_valid", status.drift_valid);
        clock.set_f64(c"drift_ppm", status.drift_ppm);
        if let Some(utc_ns) = ntp::utc_now_ns() {
            clock.set_i64(c"utc_ms", utc_ns / 1_000_000);
        }
    }
    response.set_obj(c"clock", &clock);

    let array = DataArray::new();
    let mut worst_required_ms = 0;
    let mut out_of_sync = 0;
    for entry in collect() {
        let item = OwnedData::new();
        {
            let item = item.data();
            set_string(&item, c"source_name", &entry.source_name);
            item.set_bool(c"sync_enabled", entry.sync_enabled);
            set_string(&item, c"status", entry.snap.status.name());
            // Which of the three causes of "no timecode" this is, so a bot
            // can say something actionable instead of just "not synced".
            set_string(&item, c"timecode_reason", entry.snap.tc_reason.name());
            item.set_i64(c"latency_ms", entry.snap.latency_ms);
            item.set_i64(c"latency_peak_ms", entry.snap.latency_peak_ms);
            item.set_i64(c"added_ms", entry.snap.added_ms);
            item.set_i64(c"error_ms", entry.snap.error_ms);
            item.set_i64(c"required_offset_ms", entry.snap.required_offset_ms);
            set_string(
                &item,
                c"timecode",
                &entry
                    .snap
                    .timecode
                    .map(|tc| tc.to_string())
                    .unwrap_or_default(),
            );
        }
        if entry.sync_enabled && entry.snap.required_offset_ms > worst_required_ms {
            worst_required_ms = entry.snap.required_offset_ms;
        }
        if matches!(entry.snap.status, SyncStatus::TooSlow | SyncStatus::Stale) {
            out_of_sync += 1;
        }
        array.push_back(&item);
    }
    response.set_array(c"sources", &array);

    // Pre-computed so a bot does not have to reimplement the policy: the
    // count worth alerting on, and the offset that would clear it.
    response.set_i64(c"out_of_sync_count", out_of_sync);
    response.set_i64(c"recommended_offset_ms", worst_required_ms);
    response.set_bool(c"success", true);
}

/// `SetSyncConfig`: write any of the three global settings. Each of
/// `sync_enabled` (bool), `offset_ms` (int, clamped to the settable range)
/// and `ntp_server` (string; empty stops polling) is optional, and the
/// response carries the settings as they stand afterwards.
///
/// Not in the C branch, whose settings were only reachable through the Qt
/// dock. Here the dock is built only when Qt is found, so the vendor needs a
/// way to operate sync headless — and it gives a bot the other half of the
/// alerting path: raising the offset to what `GetSyncStatus` recommends.
pub fn vendor_set_config(request: &Data<'_>, response: &OwnedData) {
    if request.has(c"sync_enabled") {
        set_enabled(request.get_bool(c"sync_enabled"));
    }
    if request.has(c"offset_ms") {
        let offset = request
            .get_i64(c"offset_ms")
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        set_offset_ms(offset);
    }
    if request.has(c"ntp_server") {
        set_ntp_server(request.get_str(c"ntp_server").unwrap_or_default().trim());
    }

    let cfg = config();
    let response = response.data();
    response.set_bool(c"sync_enabled", cfg.enabled);
    response.set_i64(c"offset_ms", i64::from(cfg.offset_ms));
    set_string(&response, c"ntp_server", &cfg.ntp_server);
    response.set_bool(c"success", true);
}
