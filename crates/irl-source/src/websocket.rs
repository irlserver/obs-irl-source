//! obs-websocket vendor extension (port of `src/websocket-vendor.c`).
//!
//! Exposes the per-source stats over obs-websocket so an overlay, a bot or an
//! IRL dashboard can read them from another machine, without the Lua or
//! Python script the proc_handler path requires. Clients reach these through
//! obs-websocket's own CallVendorRequest:
//!
//! ```json
//! {"vendorName": "obs-irl-source", "requestType": "GetStats",
//!  "requestData": {"source_name": "IRL Source"}}
//! ```
//!
//! The stats are not read out of the source struct here: this module calls
//! the source's own `get_stats` proc, the same entry point scripts use, so
//! both transports are guaranteed to report the same numbers and the locked
//! snapshot stays in one place (`source.rs`).

use std::ffi::{CStr, CString};

use irl_core::consts;
use irl_core::stats::{FIELDS, StatKind};
use obs::{CallData, Data, DataArray, OwnedData, OwnedSource, SourceHandle};

/// One vendor request handler.
type RequestFn = fn(&Data<'_>, &OwnedData);

/// Register the vendor and its requests.
///
/// Must run from `obs_module_post_load`: obs-websocket publishes the global
/// proc this goes through from its own `obs_module_load`, and module load
/// order between plugins is not defined. Every module's load has finished by
/// the time any post_load runs.
///
/// There is no matching teardown, by design. The API has no vendor
/// unregister call — a registration is meant to last for the life of the
/// process — and the request-unregister proc would have to run during module
/// unload at shutdown, where obs-websocket may already have destroyed the
/// proc handler and the vendor object this holds.
pub fn register() {
    let Ok(vendor_name) = CString::new(consts::VENDOR_NAME) else {
        return;
    };
    let Some(vendor) = obs::websocket::register_vendor(&vendor_name) else {
        // The normal case on an OBS without obs-websocket enabled. Nothing
        // else in the plugin depends on it.
        irl_info!("obs-websocket not available; vendor requests disabled");
        return;
    };

    let requests: [(&'static CStr, RequestFn); 3] = [
        (c"GetStats", get_stats),
        (c"GetSourceList", get_source_list),
        (c"GetVersion", get_version),
    ];
    for (request_type, callback) in requests {
        if !vendor.register_request(request_type, Box::new(callback)) {
            irl_warn!(
                "Failed to register obs-websocket vendor request '{}'",
                request_type.to_string_lossy()
            );
        }
    }

    irl_info!(
        "Registered obs-websocket vendor '{}' (obs-websocket API v{})",
        consts::VENDOR_NAME,
        obs::websocket::api_version().unwrap_or(0)
    );
}

// ── Response helpers ──────────────────────────────────────────

/// Vendor requests have no status code of their own: obs-websocket reports
/// RequestStatus::Success as long as the callback ran, and hands the client
/// whatever the callback put in `response_data`. So the outcome is carried in
/// the payload. Every response has "success"; failures add "error".
fn respond_error(response: &OwnedData, message: &CStr) {
    let response = response.data();
    response.set_bool(c"success", false);
    response.set_str(c"error", message);
}

// ── Source lookup ─────────────────────────────────────────────

fn source_is_irl(source: SourceHandle) -> bool {
    source.unversioned_id() == consts::SOURCE_ID
}

/// Resolves which source the request is about. "source_name" names one
/// explicitly; without it, a scene collection holding exactly one IRL source
/// resolves to that source, because that is the common setup and it saves
/// every client a GetSourceList round trip first.
fn resolve_source(request: &Data<'_>, response: &OwnedData) -> Option<OwnedSource> {
    // Accept the obs-websocket house style as an alias: core requests all
    // take "sourceName", so that is what a client reaches for first.
    let name = request
        .get_str(c"source_name")
        .or_else(|| request.get_str(c"sourceName"));

    if let Some(name) = name {
        let Ok(name) = CString::new(name) else {
            respond_error(response, c"No source by that name");
            return None;
        };
        let Some(source) = obs::source::get_source_by_name(&name) else {
            respond_error(response, c"No source by that name");
            return None;
        };
        if !source_is_irl(source.handle()) {
            respond_error(response, c"That source is not an IRL Source");
            return None;
        }
        return Some(source);
    }

    let mut count = 0usize;
    let mut first: Option<OwnedSource> = None;
    obs::source::enum_sources(&mut |source| {
        if source_is_irl(source) {
            count += 1;
            if first.is_none() {
                // Not `get_ref`'s only job here: it returns None for a source
                // already on its way out, which is exactly the one we must
                // not hand back.
                first = source.get_ref();
            }
        }
        true
    });

    if count == 0 {
        respond_error(response, c"No IRL Source exists");
        return None;
    }
    if count > 1 {
        respond_error(
            response,
            c"More than one IRL Source exists; pass source_name (see GetSourceList)",
        );
        return None;
    }
    if first.is_none() {
        respond_error(response, c"IRL Source is being destroyed");
    }
    first
}

// ── Requests ──────────────────────────────────────────────────

fn get_stats(request: &Data<'_>, response: &OwnedData) {
    let Some(source) = resolve_source(request, response) else {
        return;
    };
    let source = source.handle();

    let mut cd = CallData::new();
    if !source.proc_handler().call(c"get_stats", &mut cd) {
        respond_error(response, c"Source did not answer get_stats");
        return;
    }

    let response = response.data();
    if let Ok(name) = CString::new(source.name()) {
        response.set_str(c"source_name", &name);
    }
    // calldata is a typed blob that cannot be enumerated, so the fields to
    // copy out have to be named: `FIELDS` is that list, shared with the proc
    // declaration and the snapshot writer. The names are the JSON keys
    // clients see. An absent field copies as zero, as in the C.
    for (name, kind) in FIELDS {
        let Ok(key) = CString::new(*name) else {
            continue;
        };
        match kind {
            StatKind::Int => response.set_i64(&key, cd.get_i64(&key).unwrap_or(0)),
            StatKind::Float => response.set_f64(&key, cd.get_f64(&key).unwrap_or(0.0)),
            StatKind::Bool => response.set_bool(&key, cd.get_bool(&key).unwrap_or(false)),
        }
    }
    response.set_bool(c"success", true);
}

fn get_source_list(_request: &Data<'_>, response: &OwnedData) {
    let array = DataArray::new();
    obs::source::enum_sources(&mut |source| {
        if source_is_irl(source) {
            let entry = OwnedData::new();
            if let Ok(name) = CString::new(source.name()) {
                entry.data().set_str(c"source_name", &name);
            }
            // Deliberately no URL: it can carry an SRT passphrase or a stream
            // key, and this list is readable by every connected websocket
            // client.
            entry.data().set_bool(c"active", source.active());
            entry.data().set_bool(c"showing", source.showing());
            array.push_back(&entry);
        }
        true
    });

    let response = response.data();
    response.set_array(c"sources", &array);
    response.set_bool(c"success", true);
}

fn get_version(_request: &Data<'_>, response: &OwnedData) {
    let response = response.data();
    if let Ok(version) = CString::new(crate::PLUGIN_VERSION) {
        response.set_str(c"plugin_version", &version);
    }
    response.set_i64(c"vendor_api_version", consts::VENDOR_API_VERSION);
    response.set_i64(
        c"obs_websocket_api_version",
        obs::websocket::api_version().unwrap_or(0) as i64,
    );
    response.set_bool(c"success", true);
}
