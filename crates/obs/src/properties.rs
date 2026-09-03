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

impl TextType {
    fn to_sys(self) -> obs_sys::obs_text_type {
        use obs_sys::obs_text_type as T;
        match self {
            Self::Default => T::OBS_TEXT_DEFAULT,
            Self::Password => T::OBS_TEXT_PASSWORD,
            Self::Multiline => T::OBS_TEXT_MULTILINE,
            Self::Info => T::OBS_TEXT_INFO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComboFormat {
    Int,
    String,
}

impl ComboFormat {
    fn to_sys(self) -> obs_sys::obs_combo_format {
        use obs_sys::obs_combo_format as F;
        match self {
            Self::Int => F::OBS_COMBO_FORMAT_INT,
            Self::String => F::OBS_COMBO_FORMAT_STRING,
        }
    }
}

/// `obs_properties_t` being built. Ownership passes to libobs when the
/// `get_properties` shim returns [`Properties::into_raw`].
#[derive(Debug)]
pub struct Properties(NonNull<obs_sys::obs_properties_t>);

impl Properties {
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: no arguments; libobs returns a fresh owned object.
        let ptr = unsafe { obs_sys::obs_properties_create() };
        Self(NonNull::new(ptr).expect("obs_properties_create returned NULL"))
    }

    /// `obs_properties_set_flags` (e.g. `sys::OBS_PROPERTIES_DEFER_UPDATE`).
    pub fn set_flags(&self, flags: u32) {
        // SAFETY: live handle owned by `self`.
        unsafe { obs_sys::obs_properties_set_flags(self.0.as_ptr(), flags) };
    }

    pub fn add_text(&self, id: &CStr, description: &CStr, kind: TextType) {
        // SAFETY: live handle; libobs copies both strings and owns the
        // returned obs_property_t, which stays inside the properties object.
        unsafe {
            obs_sys::obs_properties_add_text(
                self.0.as_ptr(),
                id.as_ptr(),
                description.as_ptr(),
                kind.to_sys(),
            )
        };
    }

    pub fn add_int(&self, id: &CStr, description: &CStr, min: i32, max: i32, step: i32) {
        // SAFETY: as above.
        unsafe {
            obs_sys::obs_properties_add_int(
                self.0.as_ptr(),
                id.as_ptr(),
                description.as_ptr(),
                min,
                max,
                step,
            )
        };
    }

    /// `obs_properties_add_int_slider`: the same value as [`Self::add_int`],
    /// drawn as a slider. The returned handle exists so a unit suffix can be
    /// attached.
    pub fn add_int_slider(
        &self,
        id: &CStr,
        description: &CStr,
        min: i32,
        max: i32,
        step: i32,
    ) -> IntProperty<'_> {
        // SAFETY: as above; the property belongs to this properties object,
        // which the returned handle borrows.
        let ptr = unsafe {
            obs_sys::obs_properties_add_int_slider(
                self.0.as_ptr(),
                id.as_ptr(),
                description.as_ptr(),
                min,
                max,
                step,
            )
        };
        IntProperty(
            NonNull::new(ptr).expect("obs_properties_add_int_slider returned NULL"),
            PhantomData,
        )
    }

    pub fn add_bool(&self, id: &CStr, description: &CStr) {
        // SAFETY: as above.
        unsafe {
            obs_sys::obs_properties_add_bool(self.0.as_ptr(), id.as_ptr(), description.as_ptr())
        };
    }

    /// `obs_properties_add_list(..., OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_INT)`.
    pub fn add_int_list(&self, id: &CStr, description: &CStr) -> IntList<'_> {
        // SAFETY: as above; the property belongs to this properties object,
        // which the returned IntList borrows.
        let ptr = unsafe {
            obs_sys::obs_properties_add_list(
                self.0.as_ptr(),
                id.as_ptr(),
                description.as_ptr(),
                obs_sys::obs_combo_type::OBS_COMBO_TYPE_LIST,
                ComboFormat::Int.to_sys(),
            )
        };
        IntList(
            NonNull::new(ptr).expect("obs_properties_add_list returned NULL"),
            PhantomData,
        )
    }

    /// Hand ownership to libobs. Every `get_properties` shim ends here.
    pub fn into_raw(self) -> *mut obs_sys::obs_properties_t {
        let this = core::mem::ManuallyDrop::new(self);
        this.0.as_ptr()
    }
}

impl Default for Properties {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Properties {
    fn drop(&mut self) {
        // Only reached when a half-built dialog is abandoned — a panic caught
        // by the `get_properties` guard, or an early return. libobs owns the
        // object from `into_raw` onwards, and that path never drops.
        // SAFETY: this value owns the object; destroyed exactly once.
        unsafe { obs_sys::obs_properties_destroy(self.0.as_ptr()) };
    }
}

/// An int-valued combo list property.
#[derive(Debug)]
pub struct IntList<'p>(
    NonNull<obs_sys::obs_property_t>,
    PhantomData<&'p Properties>,
);

/// One int property inside a [`Properties`], borrowed for as long as the
/// properties object it belongs to. libobs owns the property itself.
#[derive(Debug)]
pub struct IntProperty<'a>(
    NonNull<obs_sys::obs_property_t>,
    PhantomData<&'a Properties>,
);

impl IntProperty<'_> {
    /// `obs_property_int_set_suffix`: the unit drawn after the value.
    pub fn set_suffix(&self, suffix: &CStr) {
        // SAFETY: live property owned by the borrowed properties object;
        // libobs copies the string.
        unsafe { obs_sys::obs_property_int_set_suffix(self.0.as_ptr(), suffix.as_ptr()) };
    }
}

impl IntList<'_> {
    /// `obs_property_list_add_int`.
    pub fn add(&self, name: &CStr, value: i64) {
        // SAFETY: the property is alive for `'p` (owned by the Properties this
        // borrows); libobs copies the name.
        unsafe { obs_sys::obs_property_list_add_int(self.0.as_ptr(), name.as_ptr(), value) };
    }
}
