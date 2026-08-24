//! obs-websocket vendor API (API version 3), reimplementing the `static
//! inline` helpers of `obs-websocket-api.h` over libobs's global proc handler.
//!
//! Nothing links against obs-websocket: its private proc handler is fetched
//! lazily through `obs_get_proc_handler()` → `obs_websocket_api_get_ph`, and
//! every call degrades to `None`/`false` when it is absent. Register from
//! `obs_module_post_load`, because obs-websocket publishes its proc from its
//! own `obs_module_load` and inter-module load order is undefined.

use core::ffi::{CStr, c_void};

use crate::data::{Data, OwnedData};

/// obs-websocket's API version, or `None` when obs-websocket is not loaded.
pub fn api_version() -> Option<u32> {
    todo!("W1-A: get_api_version proc")
}

/// A registered vendor (`vendor_register`). There is no unregister call in
/// the API, so this is never freed.
#[derive(Debug, Clone, Copy)]
pub struct Vendor(*mut c_void);

unsafe impl Send for Vendor {}
unsafe impl Sync for Vendor {}

/// `vendor_register`. `None` when obs-websocket is absent.
pub fn register_vendor(name: &CStr) -> Option<Vendor> {
    let _ = name;
    todo!("W1-A")
}

impl Vendor {
    /// `vendor_request_register`. The callback receives the request data and
    /// the response object to fill; it runs on obs-websocket's thread and is
    /// guarded. The boxed closure is leaked deliberately (no unregister path).
    pub fn register_request(
        &self,
        request_type: &CStr,
        callback: Box<dyn Fn(&Data<'_>, &OwnedData) + Send + Sync>,
    ) -> bool {
        let _ = (request_type, callback);
        todo!("W1-A")
    }
}
