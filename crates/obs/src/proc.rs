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

use crate::panic::guard_unit;

/// The boxed closure a [`ProcCallback`] owns, as stored in the `void *data`
/// libobs passes back to the trampoline.
type BoxedProc = Box<dyn Fn(&mut CallData) + Send + Sync>;

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
    pub fn add(
        &self,
        decl: &CStr,
        callback: Box<dyn Fn(&mut CallData) + Send + Sync>,
    ) -> ProcCallback {
        // Two boxes: the outer one gives the trait object a thin pointer that
        // can round-trip through `void *`.
        let boxed: Box<BoxedProc> = Box::new(callback);
        let data = Box::into_raw(boxed).cast::<c_void>();

        // SAFETY: `data` is a live `Box<BoxedProc>` for as long as the
        // returned ProcCallback lives, which the caller ties to the object
        // owning the proc handler. libobs copies `decl` during the call.
        unsafe {
            obs_sys::proc_handler_add(self.0.as_ptr(), decl.as_ptr(), Some(proc_trampoline), data);
        }

        ProcCallback(data)
    }

    /// `proc_handler_call`.
    pub fn call(&self, name: &CStr, cd: &mut CallData) -> bool {
        // SAFETY: live handle, NUL-terminated name, and a calldata this call
        // exclusively borrows.
        unsafe { obs_sys::proc_handler_call(self.0.as_ptr(), name.as_ptr(), cd.as_mut_ptr()) }
    }
}

/// The one `extern "C"` entry point every registered closure goes through.
///
/// # Safety
/// libobs calls this with the `data` pointer handed to `proc_handler_add` and
/// a live `calldata_t`.
unsafe extern "C" fn proc_trampoline(data: *mut c_void, cd: *mut obs_sys::calldata_t) {
    guard_unit("proc handler", || {
        if data.is_null() || cd.is_null() {
            return;
        }
        // SAFETY: `data` is the `Box<BoxedProc>` raw pointer from `add`, still
        // alive because its ProcCallback has not dropped; `cd` is libobs's.
        let callback = unsafe { &*data.cast::<BoxedProc>() };
        let call_data = unsafe { CallData::from_raw_mut(cd) };
        callback(call_data);
    });
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
        if self.0.is_null() {
            return;
        }
        // SAFETY: reclaims the single `Box<BoxedProc>` leaked in `add`.
        drop(unsafe { Box::from_raw(self.0.cast::<BoxedProc>()) });
        self.0 = core::ptr::null_mut();
    }
}

/// An owned `calldata_t` (`calldata_init` on new, `calldata_free` on drop).
#[repr(transparent)]
pub struct CallData(obs_sys::calldata_t);

impl CallData {
    #[must_use]
    pub fn new() -> Self {
        // `calldata_init` is a memset to zero; the C callers that build one on
        // the stack write `calldata_t cd = {0, 0, 0, 0}` instead, same thing.
        Self(obs_sys::calldata_t {
            stack: core::ptr::null_mut(),
            size: 0,
            capacity: 0,
            fixed: false,
        })
    }

    /// # Safety
    /// `ptr` must be a live `calldata_t` for the returned borrow.
    pub unsafe fn from_raw_mut<'a>(ptr: *mut obs_sys::calldata_t) -> &'a mut Self {
        // CallData is #[repr(transparent)]-equivalent over calldata_t.
        unsafe { &mut *(ptr as *mut Self) }
    }

    /// The shared body of every typed setter: `calldata_set_data(name, &v, n)`.
    fn set_data(&mut self, name: &CStr, value: *const c_void, size: usize) {
        // SAFETY: `self.0` is a valid calldata (this type owns or borrows one),
        // `name` is NUL-terminated, and libobs copies `size` bytes out of
        // `value` during the call.
        unsafe { obs_sys::calldata_set_data(&raw mut self.0, name.as_ptr(), value, size) };
    }

    /// The shared body of every typed getter. `false` means "absent, or a
    /// different type", exactly as in C.
    fn get_data(&self, name: &CStr, out: *mut c_void, size: usize) -> bool {
        // SAFETY: as above; libobs writes at most `size` bytes into `out`.
        unsafe { obs_sys::calldata_get_data(&raw const self.0, name.as_ptr(), out, size) }
    }

    pub fn set_i64(&mut self, name: &CStr, value: i64) {
        // C stores a `long long`, which is 8 bytes on every target OBS builds
        // for; a getter asking for a different width reads nothing.
        let v: i64 = value;
        self.set_data(name, (&raw const v).cast(), size_of::<i64>());
    }

    pub fn set_f64(&mut self, name: &CStr, value: f64) {
        let v: f64 = value;
        self.set_data(name, (&raw const v).cast(), size_of::<f64>());
    }

    pub fn set_bool(&mut self, name: &CStr, value: bool) {
        let v: bool = value;
        self.set_data(name, (&raw const v).cast(), size_of::<bool>());
    }

    /// `calldata_set_string`: the bytes *plus* the NUL, or a zero-size entry
    /// for a null string.
    pub fn set_str(&mut self, name: &CStr, value: &CStr) {
        let bytes = value.to_bytes_with_nul();
        self.set_data(name, bytes.as_ptr().cast(), bytes.len());
    }

    pub fn set_ptr(&mut self, name: &CStr, value: *mut c_void) {
        let v: *mut c_void = value;
        self.set_data(name, (&raw const v).cast(), size_of::<*mut c_void>());
    }

    pub fn get_i64(&self, name: &CStr) -> Option<i64> {
        let mut v: i64 = 0;
        self.get_data(name, (&raw mut v).cast(), size_of::<i64>())
            .then_some(v)
    }

    pub fn get_f64(&self, name: &CStr) -> Option<f64> {
        let mut v: f64 = 0.0;
        self.get_data(name, (&raw mut v).cast(), size_of::<f64>())
            .then_some(v)
    }

    pub fn get_bool(&self, name: &CStr) -> Option<bool> {
        let mut v: bool = false;
        self.get_data(name, (&raw mut v).cast(), size_of::<bool>())
            .then_some(v)
    }

    /// `calldata_get_string` (borrowed from the calldata's own stack).
    pub fn get_str(&self, name: &CStr) -> Option<&str> {
        let mut raw: *const core::ffi::c_char = core::ptr::null();
        // SAFETY: valid calldata and name; libobs writes one pointer into
        // `raw`, aimed at bytes inside the calldata's own stack.
        let found =
            unsafe { obs_sys::calldata_get_string(&raw const self.0, name.as_ptr(), &raw mut raw) };
        if !found || raw.is_null() {
            return None;
        }
        // SAFETY: the string lives in the calldata's stack allocation, which
        // outlives the returned borrow of `self`.
        unsafe { CStr::from_ptr(raw) }.to_str().ok()
    }

    pub fn get_ptr(&self, name: &CStr) -> Option<*mut c_void> {
        let mut v: *mut c_void = core::ptr::null_mut();
        self.get_data(name, (&raw mut v).cast(), size_of::<*mut c_void>())
            .then_some(v)
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
        // `calldata_free`: bfree(stack) unless the stack is caller-provided.
        // A borrowed CallData (from_raw_mut) is a `&mut Self`, never dropped.
        if !self.0.fixed && !self.0.stack.is_null() {
            // SAFETY: `stack` was allocated by libobs's bmem inside
            // calldata_set_data and is freed exactly once.
            unsafe { obs_sys::bfree(self.0.stack.cast()) };
            self.0.stack = core::ptr::null_mut();
            self.0.size = 0;
            self.0.capacity = 0;
        }
    }
}
