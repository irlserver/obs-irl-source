//! Module entry points.
//!
//! [`declare_module!`] expands to the `obs_module_*` exports libobs looks up
//! (`obs_module_load`, `obs_module_set_pointer` and `obs_module_ver` are
//! required; the rest optional) and the locale helpers that
//! `OBS_MODULE_USE_DEFAULT_LOCALE` provides in C. The macro bodies only call
//! the functions below, so a plugin gets the full C behavior without writing
//! any `extern "C"` itself.

use core::ffi::{CStr, c_char};

/// Store the `obs_module_t*` libobs hands to `obs_module_set_pointer`.
pub fn set_pointer(module: *mut obs_sys::obs_module_t) {
    let _ = module;
    todo!("W1-A: AtomicPtr store")
}

/// The pointer stored by [`set_pointer`] (the C `obs_current_module()`).
pub fn current_module() -> *mut obs_sys::obs_module_t {
    todo!("W1-A")
}

/// `obs_module_set_locale`: destroy the previous lookup and load
/// `default_locale`/`locale` for the current module.
pub fn set_locale(default_locale: &'static CStr, locale: *const c_char) {
    let _ = (default_locale, locale);
    todo!("W1-A: text_lookup_destroy(old); obs_module_load_locale(current_module(), default, locale)")
}

/// `obs_module_free_locale`.
pub fn free_locale() {
    todo!("W1-A")
}

/// `obs_module_get_string`: returns the translated string pointer if found.
pub fn get_string(key: *const c_char, out: *mut *const c_char) -> bool {
    let _ = (key, out);
    todo!("W1-A: text_lookup_getstr")
}

/// `obs_module_text`: the translation, or `key` itself when missing (so a
/// package built without its locale file shows bare identifiers rather than
/// crashing). The returned pointer is owned by the lookup and lives until the
/// locale is replaced or freed, which only happens on the OBS thread.
pub fn text(key: &'static CStr) -> &'static CStr {
    let _ = key;
    todo!("W1-A")
}

/// Declare the module exports.
///
/// ```ignore
/// obs::declare_module! {
///     module_name: "obs-irl-source",
///     default_locale: "en-US",
///     api_version: (32, 1, 2),
///     name: "IRL Source",
///     description: "...",
///     author: "...",
///     load: module_load,          // fn() -> bool
///     post_load: module_post_load,// fn()
///     unload: module_unload,      // fn()
/// }
/// ```
#[macro_export]
macro_rules! declare_module {
    (
        module_name: $module_name:literal,
        default_locale: $default_locale:literal,
        api_version: ($major:literal, $minor:literal, $patch:literal),
        name: $name:literal,
        description: $description:literal,
        author: $author:literal,
        load: $load:path,
        post_load: $post_load:path,
        unload: $unload:path $(,)?
    ) => {
        pub const MODULE_NAME: &str = $module_name;
        pub const MODULE_DEFAULT_LOCALE: &::core::ffi::CStr = $crate::_cstr!($default_locale);

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_set_pointer(module: *mut $crate::sys::obs_module_t) {
            $crate::panic::guard_unit("obs_module_set_pointer", || $crate::module::set_pointer(module));
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_ver() -> u32 {
            $crate::sys::make_semantic_version($major, $minor, $patch)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_load() -> bool {
            $crate::panic::guard("obs_module_load", false, || $load())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_post_load() {
            $crate::panic::guard_unit("obs_module_post_load", || $post_load())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_unload() {
            $crate::panic::guard_unit("obs_module_unload", || $unload())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_set_locale(locale: *const ::core::ffi::c_char) {
            $crate::panic::guard_unit("obs_module_set_locale", || {
                $crate::module::set_locale(MODULE_DEFAULT_LOCALE, locale)
            });
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_free_locale() {
            $crate::panic::guard_unit("obs_module_free_locale", || $crate::module::free_locale());
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_get_string(
            key: *const ::core::ffi::c_char,
            out: *mut *const ::core::ffi::c_char,
        ) -> bool {
            $crate::panic::guard("obs_module_get_string", false, || $crate::module::get_string(key, out))
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_name() -> *const ::core::ffi::c_char {
            $crate::_cstr!($name).as_ptr()
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_description() -> *const ::core::ffi::c_char {
            $crate::_cstr!($description).as_ptr()
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn obs_module_author() -> *const ::core::ffi::c_char {
            $crate::_cstr!($author).as_ptr()
        }

        /// `obs_module_text`: look a key up in the module locale.
        pub fn module_text(key: &'static ::core::ffi::CStr) -> &'static ::core::ffi::CStr {
            $crate::module::text(key)
        }
    };
}

/// Turn a string literal into a `&'static CStr` at compile time.
#[doc(hidden)]
#[macro_export]
macro_rules! _cstr {
    ($s:literal) => {
        match ::core::ffi::CStr::from_bytes_with_nul(concat!($s, "\0").as_bytes()) {
            Ok(c) => c,
            Err(_) => panic!("string literal contains an interior NUL"),
        }
    };
}
