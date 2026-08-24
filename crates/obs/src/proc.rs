//! Proc handlers and calldata.
//!
//! `calldata_init`, `calldata_free` and every typed `calldata_set_*` /
//! `calldata_get_*` are `static inline` in `callback/calldata.h`; they are
//! reimplemented here over the three exported functions (`calldata_set_data`,
//! `calldata_get_data`, `calldata_get_string`) with the C widths: `long long`
//! = 8, `double` = 8, `bool` = 1, pointer = `size_of::<*mut c_void>()`, string
//! = `strlen + 1`.

use core::ffi::{CStr, c_void};
use core::marker::PhantomData;
use core::ptr::NonNull;

/// A borrowed `proc_handler_t`.
#[derive(Debug, Clone, Copy)]
pub struct ProcHandler<'a>(NonNull<obs_sys::proc_handler_t>, PhantomData<&'a ()>);

impl ProcHandler<'_> {
    /// # Safety
    /// `ptr` must be a live proc handler for `'a`.
    pub unsafe fn from_raw<'a>(ptr: NonNull<obs_sys::proc_handler_t>) -> ProcHandler<'a> {
        ProcHandler(ptr, PhantomData)
    }

    /// `proc_handler_add`. `decl` is the OBS declaration string, e.g.
    /// `c"void get_stats(out int buffer_fill_ms, out float current_speed)"`.
    /// The returned [`ProcCallback`] owns the boxed closure; keep it alive as
    /// long as the handler can be called (for a source: until `Drop`).
    #[must_use]
    pub fn add(&self, decl: &CStr, callback: Box<dyn Fn(&mut CallData) + Send + Sync>) -> ProcCallback {
        let _ = (decl, callback);
        todo!("W1-A: box the closure, install a guarded extern \"C\" trampoline")
    }

    /// `proc_handler_call`.
    pub fn call(&self, name: &CStr, cd: &mut CallData) -> bool {
        let _ = (name, cd);
        todo!("W1-A")
    }
}

/// Owner of a closure installed by [`ProcHandler::add`]. Dropping it frees the
/// closure; safe for a source because `obs_source_destroy` runs `destroy`
/// (our `Drop`) before tearing down the proc handler, and nothing can call
/// the proc in between.
#[derive(Debug)]
pub struct ProcCallback(*mut c_void);

unsafe impl Send for ProcCallback {}
unsafe impl Sync for ProcCallback {}

impl Drop for ProcCallback {
    fn drop(&mut self) {
        todo!("W1-A: Box::from_raw the closure")
    }
}

/// An owned `calldata_t` (`calldata_init` on new, `calldata_free` on drop).
pub struct CallData(obs_sys::calldata_t);

impl CallData {
    pub fn new() -> Self {
        todo!("W1-A: zeroed calldata_t")
    }

    /// # Safety
    /// `ptr` must be a live `calldata_t` for the returned borrow.
    pub unsafe fn from_raw_mut<'a>(ptr: *mut obs_sys::calldata_t) -> &'a mut Self {
        // CallData is #[repr(transparent)]-equivalent over calldata_t.
        unsafe { &mut *(ptr as *mut Self) }
    }

    pub fn set_i64(&mut self, name: &CStr, value: i64) {
        let _ = (name, value);
        todo!("W1-A")
    }

    pub fn set_f64(&mut self, name: &CStr, value: f64) {
        let _ = (name, value);
        todo!("W1-A")
    }

    pub fn set_bool(&mut self, name: &CStr, value: bool) {
        let _ = (name, value);
        todo!("W1-A")
    }

    pub fn set_str(&mut self, name: &CStr, value: &CStr) {
        let _ = (name, value);
        todo!("W1-A")
    }

    pub fn set_ptr(&mut self, name: &CStr, value: *mut c_void) {
        let _ = (name, value);
        todo!("W1-A")
    }

    pub fn get_i64(&self, name: &CStr) -> Option<i64> {
        let _ = name;
        todo!("W1-A")
    }

    pub fn get_f64(&self, name: &CStr) -> Option<f64> {
        let _ = name;
        todo!("W1-A")
    }

    pub fn get_bool(&self, name: &CStr) -> Option<bool> {
        let _ = name;
        todo!("W1-A")
    }

    /// `calldata_get_string` (borrowed from the calldata's own stack).
    pub fn get_str(&self, name: &CStr) -> Option<&str> {
        let _ = name;
        todo!("W1-A")
    }

    pub fn get_ptr(&self, name: &CStr) -> Option<*mut c_void> {
        let _ = name;
        todo!("W1-A")
    }

    pub fn as_mut_ptr(&mut self) -> *mut obs_sys::calldata_t {
        &mut self.0
    }
}

impl Default for CallData {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CallData {
    fn drop(&mut self) {
        todo!("W1-A: bfree(stack) unless fixed")
    }
}
