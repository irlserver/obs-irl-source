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
        let _ = key;
        todo!("W1-A")
    }

    pub fn get_i64(&self, key: &CStr) -> i64 {
        let _ = key;
        todo!("W1-A")
    }

    pub fn get_bool(&self, key: &CStr) -> bool {
        let _ = key;
        todo!("W1-A")
    }

    pub fn get_f64(&self, key: &CStr) -> f64 {
        let _ = key;
        todo!("W1-A")
    }

    pub fn set_str(&self, key: &CStr, value: &CStr) {
        let _ = (key, value);
        todo!("W1-A")
    }

    pub fn set_i64(&self, key: &CStr, value: i64) {
        let _ = (key, value);
        todo!("W1-A")
    }

    pub fn set_bool(&self, key: &CStr, value: bool) {
        let _ = (key, value);
        todo!("W1-A")
    }

    pub fn set_f64(&self, key: &CStr, value: f64) {
        let _ = (key, value);
        todo!("W1-A")
    }

    pub fn set_array(&self, key: &CStr, array: &DataArray) {
        let _ = (key, array);
        todo!("W1-A")
    }

    pub fn set_default_str(&self, key: &CStr, value: &CStr) {
        let _ = (key, value);
        todo!("W1-A")
    }

    pub fn set_default_i64(&self, key: &CStr, value: i64) {
        let _ = (key, value);
        todo!("W1-A")
    }

    pub fn set_default_bool(&self, key: &CStr, value: bool) {
        let _ = (key, value);
        todo!("W1-A")
    }
}

/// An owned `obs_data_t` (`obs_data_create`), released in `Drop`.
#[derive(Debug)]
pub struct OwnedData(NonNull<obs_sys::obs_data_t>);

impl OwnedData {
    pub fn new() -> Self {
        todo!("W1-A: obs_data_create")
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
        todo!("W1-A: obs_data_release")
    }
}

/// An owned `obs_data_array_t`.
#[derive(Debug)]
pub struct DataArray(NonNull<obs_sys::obs_data_array_t>);

impl DataArray {
    pub fn new() -> Self {
        todo!("W1-A: obs_data_array_create")
    }

    pub fn push_back(&self, item: &OwnedData) {
        let _ = item;
        todo!("W1-A")
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
        todo!("W1-A: obs_data_array_release")
    }
}
