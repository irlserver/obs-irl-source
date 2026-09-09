//! Properties dialog builder.
//!
//! The callbacks ([`ClickAction`], [`ModifiedAction`]) are monomorphised
//! trampolines over a marker type, never boxed closures. libobs offers no
//! destructor for a property callback, and the frontend builds a fresh
//! `obs_properties_t` on every dialog open and on every `update_properties`
//! signal, so one `Box::into_raw` per build would leak without bound. A generic
//! `fn` item allocates nothing and has nothing to free.

use core::ffi::{CStr, c_void};
use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::data::Data;
use crate::panic::guard;

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

/// Which combo widget a string list is drawn as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComboType {
    /// A plain dropdown. Stores the selected item's *value* while displaying
    /// its name, which is what lets an entry read "Relay 3" and resolve to a
    /// URL.
    ///
    /// When the saved value matches no item and the list is non-empty, the
    /// frontend writes item 0 back into settings on dialog open. A list bound
    /// to a key that can legitimately hold an off-list value therefore needs an
    /// item whose value is the empty string.
    List,
    /// A dropdown the user can also type into. Stores the *displayed text*, so
    /// its item names must equal their values.
    Editable,
}

impl ComboType {
    fn to_sys(self) -> obs_sys::obs_combo_type {
        use obs_sys::obs_combo_type as T;
        match self {
            Self::List => T::OBS_COMBO_TYPE_LIST,
            Self::Editable => T::OBS_COMBO_TYPE_EDITABLE,
        }
    }
}

/// What a button added with [`Properties::add_button`] does. Implement on a
/// marker type.
///
/// Takes nothing on purpose. libobs hands a button callback the source's
/// *private data*, a plugin-defined type this crate cannot name; a caller that
/// needs the source finds it with [`crate::source::enum_sources`].
pub trait ClickAction {
    /// Return `true` to re-create the dialog's widgets.
    ///
    /// Only the *widgets*, and only from the `obs_properties_t` that already
    /// exists; the `get_properties` builder is not re-run. So `true` is right
    /// when the click changed `settings`, and useless when it changed something
    /// only the builder reads. For the latter, return `false` and raise
    /// [`crate::source::SourceHandle::update_properties`] from another thread.
    fn clicked() -> bool;
}

/// What a property's value change does. Implement on a marker type.
///
/// Fires on every dialog open too, not only on a user change:
/// `obs_source_properties` calls `obs_properties_apply_settings`. A callback
/// that writes into another property must therefore be idempotent on the
/// values it leaves behind.
pub trait ModifiedAction {
    /// `settings` is live and writable: a modified callback is the one place a
    /// property may set another property's value. Return `true` to make the
    /// frontend rebuild the widgets from the (possibly mutated) settings.
    fn modified(settings: &Data<'_>) -> bool;
}

unsafe extern "C" fn click_trampoline<A: ClickAction>(
    _props: *mut obs_sys::obs_properties_t,
    _property: *mut obs_sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("property button", false, A::clicked)
}

unsafe extern "C" fn modified_trampoline<M: ModifiedAction>(
    _props: *mut obs_sys::obs_properties_t,
    _property: *mut obs_sys::obs_property_t,
    settings: *mut obs_sys::obs_data_t,
) -> bool {
    guard("property modified", false, || {
        let Some(settings) = NonNull::new(settings) else {
            return false;
        };
        // SAFETY: non-null, and libobs keeps it alive for the duration of the
        // callback.
        let settings = unsafe { Data::from_raw(settings) };
        M::modified(&settings)
    })
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

    /// `obs_properties_add_list(..., OBS_COMBO_FORMAT_STRING)`.
    pub fn add_string_list(
        &self,
        id: &CStr,
        description: &CStr,
        kind: ComboType,
    ) -> StringList<'_> {
        // SAFETY: as above; the property belongs to this properties object,
        // which the returned StringList borrows.
        let ptr = unsafe {
            obs_sys::obs_properties_add_list(
                self.0.as_ptr(),
                id.as_ptr(),
                description.as_ptr(),
                kind.to_sys(),
                ComboFormat::String.to_sys(),
            )
        };
        StringList(
            NonNull::new(ptr).expect("obs_properties_add_list returned NULL"),
            PhantomData,
        )
    }

    /// `obs_properties_add_button`, calling `A::clicked` on press.
    pub fn add_button<A: ClickAction>(&self, id: &CStr, text: &CStr) {
        // SAFETY: as above; the trampoline is a `'static` fn item with no
        // captured state, so there is nothing for libobs to free.
        unsafe {
            obs_sys::obs_properties_add_button(
                self.0.as_ptr(),
                id.as_ptr(),
                text.as_ptr(),
                Some(click_trampoline::<A>),
            )
        };
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

/// A string-valued combo list property.
#[derive(Debug)]
pub struct StringList<'p>(
    NonNull<obs_sys::obs_property_t>,
    PhantomData<&'p Properties>,
);

impl StringList<'_> {
    /// `obs_property_list_add_string`. See [`ComboType`] for which of `name`
    /// and `value` the frontend stores.
    pub fn add(&self, name: &CStr, value: &CStr) {
        // SAFETY: the property is alive for `'p` (owned by the Properties this
        // borrows); libobs copies both strings.
        unsafe {
            obs_sys::obs_property_list_add_string(self.0.as_ptr(), name.as_ptr(), value.as_ptr())
        };
    }

    /// `obs_property_set_modified_callback`, calling `M::modified` whenever
    /// the value changes.
    pub fn on_modified<M: ModifiedAction>(&self) {
        // SAFETY: live property; the trampoline is a `'static` fn item.
        unsafe {
            obs_sys::obs_property_set_modified_callback(
                self.0.as_ptr(),
                Some(modified_trampoline::<M>),
            );
        }
    }
}

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
