//! Properties dialog builder.

use core::ffi::CStr;
use core::marker::PhantomData;
use core::ptr::NonNull;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextType {
    Default,
    Password,
    Multiline,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComboFormat {
    Int,
    String,
}

/// `obs_properties_t` being built. Ownership passes to libobs when the
/// `get_properties` shim returns [`Properties::into_raw`].
#[derive(Debug)]
pub struct Properties(NonNull<obs_sys::obs_properties_t>);

impl Properties {
    pub fn new() -> Self {
        todo!("W1-A: obs_properties_create")
    }

    /// `obs_properties_set_flags` (e.g. `sys::OBS_PROPERTIES_DEFER_UPDATE`).
    pub fn set_flags(&self, flags: u32) {
        let _ = flags;
        todo!("W1-A")
    }

    pub fn add_text(&self, id: &CStr, description: &CStr, kind: TextType) {
        let _ = (id, description, kind);
        todo!("W1-A")
    }

    pub fn add_int(&self, id: &CStr, description: &CStr, min: i32, max: i32, step: i32) {
        let _ = (id, description, min, max, step);
        todo!("W1-A")
    }

    pub fn add_bool(&self, id: &CStr, description: &CStr) {
        let _ = (id, description);
        todo!("W1-A")
    }

    /// `obs_properties_add_list(..., OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_INT)`.
    pub fn add_int_list(&self, id: &CStr, description: &CStr) -> IntList<'_> {
        let _ = (id, description);
        todo!("W1-A")
    }

    pub fn into_raw(self) -> *mut obs_sys::obs_properties_t {
        let ptr = self.0.as_ptr();
        core::mem::forget(self);
        ptr
    }
}

impl Default for Properties {
    fn default() -> Self {
        Self::new()
    }
}

/// An int-valued combo list property.
#[derive(Debug)]
pub struct IntList<'p>(NonNull<obs_sys::obs_property_t>, PhantomData<&'p Properties>);

impl IntList<'_> {
    /// `obs_property_list_add_int`.
    pub fn add(&self, name: &CStr, value: i64) {
        let _ = (name, value);
        todo!("W1-A")
    }
}
