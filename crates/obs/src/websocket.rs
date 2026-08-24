//! obs-websocket vendor API (API version 3), reimplementing the `static
//! inline` helpers of `obs-websocket-api.h` over libobs's global proc handler.
//!
//! Nothing links against obs-websocket: its private proc handler is fetched
//! lazily through `obs_get_proc_handler()` → `obs_websocket_api_get_ph`, and
//! every call degrades to `None`/`false` when it is absent. Register from
//! `obs_module_post_load`, because obs-websocket publishes its proc from its
//! own `obs_module_load` and inter-module load order is undefined.

use core::ffi::{CStr, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::data::{Data, OwnedData};
use crate::panic::guard_unit;
use crate::proc::{CallData, ProcHandler};

/// The C header's file-static `_ph` cache. Null means "not looked up yet, or
/// obs-websocket is not loaded"; the lookup is cheap enough to repeat, and
/// repeating it is what lets a plugin ask again after a later load.
static PH: AtomicPtr<obs_sys::proc_handler_t> = AtomicPtr::new(core::ptr::null_mut());

/// `obs_websocket_ensure_ph`: the cached handler, or one fresh lookup.
fn vendor_ph() -> Option<ProcHandler<'static>> {
    let cached = PH.load(Ordering::Acquire);
    if let Some(ptr) = NonNull::new(cached) {
        // SAFETY: obs-websocket's proc handler lives as long as the module,
        // which is never unloaded before ours in practice — the C header makes
        // the same assumption with its file-static cache.
        return Some(unsafe { ProcHandler::from_raw(ptr) });
    }

    // SAFETY: no arguments; the global handler exists once obs_startup ran.
    let global = unsafe { obs_sys::obs_get_proc_handler() };
    let global = NonNull::new(global)?;
    // SAFETY: libobs owns the global handler for the life of the process.
    let global = unsafe { ProcHandler::from_raw(global) };

    let mut cd = CallData::new();
    // A false return is the normal case on an OBS without obs-websocket.
    if !global.call(c"obs_websocket_api_get_ph", &mut cd) {
        return None;
    }
    let ptr = cd.get_ptr(c"ph")?.cast::<obs_sys::proc_handler_t>();
    let ptr = NonNull::new(ptr)?;

    PH.store(ptr.as_ptr(), Ordering::Release);
    // SAFETY: as above.
    Some(unsafe { ProcHandler::from_raw(ptr) })
}

/// obs-websocket's API version, or `None` when obs-websocket is not loaded.
#[must_use]
pub fn api_version() -> Option<u32> {
    let ph = vendor_ph()?;
    let mut cd = CallData::new();
    if !ph.call(c"get_api_version", &mut cd) {
        // API v1 predates the `get_api_version` proc, so a missing proc on a
        // handler that exists means v1 — the C header says so explicitly.
        return Some(1);
    }
    Some(cd.get_i64(c"version").unwrap_or(0) as u32)
}

/// A registered vendor (`vendor_register`). There is no unregister call in
/// the API, so this is never freed.
#[derive(Debug, Clone, Copy)]
pub struct Vendor(*mut c_void);

unsafe impl Send for Vendor {}
unsafe impl Sync for Vendor {}

/// `vendor_register`. `None` when obs-websocket is absent.
#[must_use]
pub fn register_vendor(name: &CStr) -> Option<Vendor> {
    let ph = vendor_ph()?;
    let mut cd = CallData::new();
    cd.set_str(c"name", name);
    ph.call(c"vendor_register", &mut cd);
    let vendor = cd.get_ptr(c"vendor")?;
    if vendor.is_null() {
        return None;
    }
    Some(Vendor(vendor))
}

/// The boxed request closure, reached through the callback record's
/// `priv_data`.
type BoxedRequest = Box<dyn Fn(&Data<'_>, &OwnedData) + Send + Sync>;

impl Vendor {
    /// `vendor_request_register`. The callback receives the request data and
    /// the response object to fill; it runs on obs-websocket's thread and is
    /// guarded. The boxed closure is leaked deliberately (no unregister path).
    // The boxed-closure parameter is spelled out rather than hidden behind the
    // private `BoxedRequest` alias so the public signature reads on its own.
    #[allow(clippy::type_complexity)]
    pub fn register_request(
        &self,
        request_type: &CStr,
        callback: Box<dyn Fn(&Data<'_>, &OwnedData) + Send + Sync>,
    ) -> bool {
        let Some(ph) = vendor_ph() else {
            return false;
        };

        // Leaked on purpose: the API has no unregister that could tell us when
        // obs-websocket has stopped holding the record, and registrations last
        // for the life of the process. This mirrors the C plugin exactly.
        let boxed: Box<BoxedRequest> = Box::new(callback);
        let priv_data = Box::into_raw(boxed).cast::<c_void>();

        // obs-websocket copies this record out of the calldata, so a stack
        // local is enough — the C header does the same.
        let record = obs_sys::obs_websocket_request_callback {
            callback: Some(request_trampoline),
            priv_data,
        };

        let mut cd = CallData::new();
        cd.set_str(c"type", request_type);
        cd.set_ptr(c"callback", (&raw const record).cast_mut().cast::<c_void>());
        // `obs_websocket_vendor_run_simple_proc`: the vendor goes in last.
        cd.set_ptr(c"vendor", self.0);
        ph.call(c"vendor_request_register", &mut cd);
        cd.get_bool(c"success").unwrap_or(false)
    }
}

/// # Safety
/// obs-websocket calls this with the two `obs_data_t`s it owns and the
/// `priv_data` from the registration record.
unsafe extern "C" fn request_trampoline(
    request_data: *mut obs_sys::obs_data_t,
    response_data: *mut obs_sys::obs_data_t,
    priv_data: *mut c_void,
) {
    guard_unit("websocket vendor request", || {
        let (Some(request), Some(response), false) = (
            NonNull::new(request_data),
            NonNull::new(response_data),
            priv_data.is_null(),
        ) else {
            return;
        };

        // SAFETY: `priv_data` is the leaked `Box<BoxedRequest>`, alive for the
        // life of the process.
        let callback = unsafe { &*priv_data.cast::<BoxedRequest>() };

        // SAFETY: both objects are owned by obs-websocket and live for the
        // duration of the call.
        let request = unsafe { Data::from_raw(request) };
        // The response object belongs to obs-websocket too, so its reference
        // must not be released. `OwnedData`'s API is what the callback wants
        // (it writes through `data()`), so it is built from the raw pointer
        // and wrapped in ManuallyDrop, which is the only thing standing
        // between this borrow and a double release.
        // SAFETY: as above; the ManuallyDrop guarantees Drop never runs.
        let response = core::mem::ManuallyDrop::new(unsafe { OwnedData::from_raw(response) });

        callback(&request, &response);
    });
}
