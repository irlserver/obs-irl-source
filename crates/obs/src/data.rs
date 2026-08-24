//! `obs_data_t` wrappers.

use core::ffi::CStr;
use core::marker::PhantomData;
use core::ptr::NonNull;

/// A borrowed, non-owning `obs_data_t` (settings passed into callbacks).
#[derive(Debug, Clone, Copy)]
pub struct Data<'a>(NonNull<obs_sys::obs_data_t>, PhantomData<&'a ()>);

impl Data<'_> {
    /// # Safety
    /// `ptr` must be a live `obs_data_t` for `'a`.
    pub unsafe fn from_raw<'a>(ptr: NonNull<obs_sys::obs_data_t>) -> Data<'a> {
        Data(ptr, PhantomData)
    }

    pub fn as_ptr(&self) -> *mut obs_sys::obs_data_t {
        self.0.as_ptr()
    }

    /// `obs_data_get_string`; `None` when the value is empty, matching the C
    /// idiom `if (url && *url)`.
    pub fn get_str(&self, key: &CStr) -> Option<String> {
        // SAFETY: live handle; libobs returns a NUL-terminated string owned by
        // the obs_data_t, valid until the item is overwritten or released —
        // which cannot happen while `self` is borrowed. The copy ends the
        // borrow immediately.
        let raw = unsafe { obs_sys::obs_data_get_string(self.as_ptr(), key.as_ptr()) };
        if raw.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(raw) };
        if s.is_empty() {
            return None;
        }
        Some(s.to_string_lossy().into_owned())
    }

    pub fn get_i64(&self, key: &CStr) -> i64 {
        // SAFETY: live handle, NUL-terminated key.
        unsafe { obs_sys::obs_data_get_int(self.as_ptr(), key.as_ptr()) }
    }

    pub fn get_bool(&self, key: &CStr) -> bool {
        // SAFETY: live handle, NUL-terminated key.
        unsafe { obs_sys::obs_data_get_bool(self.as_ptr(), key.as_ptr()) }
    }

    pub fn get_f64(&self, key: &CStr) -> f64 {
        // SAFETY: live handle, NUL-terminated key.
        unsafe { obs_sys::obs_data_get_double(self.as_ptr(), key.as_ptr()) }
    }

    pub fn set_str(&self, key: &CStr, value: &CStr) {
        // SAFETY: live handle; libobs copies the string during the call.
        unsafe { obs_sys::obs_data_set_string(self.as_ptr(), key.as_ptr(), value.as_ptr()) };
    }

    pub fn set_i64(&self, key: &CStr, value: i64) {
        // SAFETY: live handle, NUL-terminated key.
        unsafe { obs_sys::obs_data_set_int(self.as_ptr(), key.as_ptr(), value) };
    }

    pub fn set_bool(&self, key: &CStr, value: bool) {
        // SAFETY: live handle, NUL-terminated key.
        unsafe { obs_sys::obs_data_set_bool(self.as_ptr(), key.as_ptr(), value) };
    }

    pub fn set_f64(&self, key: &CStr, value: f64) {
        // SAFETY: live handle, NUL-terminated key.
        unsafe { obs_sys::obs_data_set_double(self.as_ptr(), key.as_ptr(), value) };
    }

    pub fn set_array(&self, key: &CStr, array: &DataArray) {
        // SAFETY: both handles are live; `obs_data_set_array` takes its own
        // reference, so `array` keeps owning the one it holds.
        unsafe { obs_sys::obs_data_set_array(self.as_ptr(), key.as_ptr(), array.as_ptr()) };
    }

    pub fn set_default_str(&self, key: &CStr, value: &CStr) {
        // SAFETY: live handle; libobs copies the string during the call.
        unsafe {
            obs_sys::obs_data_set_default_string(self.as_ptr(), key.as_ptr(), value.as_ptr())
        };
    }

    pub fn set_default_i64(&self, key: &CStr, value: i64) {
        // SAFETY: live handle, NUL-terminated key.
        unsafe { obs_sys::obs_data_set_default_int(self.as_ptr(), key.as_ptr(), value) };
    }

    pub fn set_default_bool(&self, key: &CStr, value: bool) {
        // SAFETY: live handle, NUL-terminated key.
        unsafe { obs_sys::obs_data_set_default_bool(self.as_ptr(), key.as_ptr(), value) };
    }
}

/// An owned `obs_data_t` (`obs_data_create`), released in `Drop`.
#[derive(Debug)]
pub struct OwnedData(NonNull<obs_sys::obs_data_t>);

impl OwnedData {
    pub fn new() -> Self {
        // SAFETY: no arguments; libobs returns a fresh reference.
        let ptr = unsafe { obs_sys::obs_data_create() };
        Self(NonNull::new(ptr).expect("obs_data_create returned NULL"))
    }

    /// Borrow as [`Data`] for reading/writing.
    pub fn data(&self) -> Data<'_> {
        Data(self.0, PhantomData)
    }

    /// # Safety
    /// `ptr` must be an `obs_data_t` whose reference this value now owns.
    pub unsafe fn from_raw(ptr: NonNull<obs_sys::obs_data_t>) -> Self {
        Self(ptr)
    }
}

impl Default for OwnedData {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for OwnedData {
    fn drop(&mut self) {
        // SAFETY: this value owns exactly one reference, released once.
        unsafe { obs_sys::obs_data_release(self.0.as_ptr()) };
    }
}

/// An owned `obs_data_array_t`.
#[derive(Debug)]
pub struct DataArray(NonNull<obs_sys::obs_data_array_t>);

impl DataArray {
    pub fn new() -> Self {
        // SAFETY: no arguments; libobs returns a fresh reference.
        let ptr = unsafe { obs_sys::obs_data_array_create() };
        Self(NonNull::new(ptr).expect("obs_data_array_create returned NULL"))
    }

    pub fn push_back(&self, item: &OwnedData) {
        // SAFETY: both handles are live; push_back takes its own reference to
        // the item, so `item` keeps owning the one it holds.
        unsafe { obs_sys::obs_data_array_push_back(self.as_ptr(), item.0.as_ptr()) };
    }

    pub fn as_ptr(&self) -> *mut obs_sys::obs_data_array_t {
        self.0.as_ptr()
    }
}

impl Default for DataArray {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DataArray {
    fn drop(&mut self) {
        // SAFETY: this value owns exactly one reference, released once.
        unsafe { obs_sys::obs_data_array_release(self.0.as_ptr()) };
    }
}
