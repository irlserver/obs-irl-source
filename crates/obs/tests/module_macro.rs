//! Compile-and-link smoke test for `declare_module!` and `register_source`.
//!
//! Neither is called: `obs_module_ver` is the only export that can run without
//! a live OBS. What this target proves is that the macro expansion and the
//! generic source shims type-check and link — the two places a mistake would
//! otherwise only surface when OBS tried to load the plugin.

use core::ffi::CStr;

use obs::{Data, IconType, MediaState, Properties, Source, SourceHandle, SourceType};

fn load() -> bool {
    // Never called here; `register_source` is exercised for its types only.
    obs::register_source::<Dummy>();
    true
}

fn post_load() {}
fn unload() {}

obs::declare_module! {
    module_name: "obs-test-module",
    default_locale: "en-US",
    api_version: (32, 1, 2),
    name: "Test Module",
    description: "declare_module! expansion test",
    author: "obs crate tests",
    load: load,
    post_load: post_load,
    unload: unload,
}

struct Dummy;

impl Source for Dummy {
    const ID: &'static CStr = c"dummy_source";
    const TYPE: SourceType = SourceType::Input;
    const OUTPUT_FLAGS: u32 = obs::sys::OBS_SOURCE_AUDIO
        | obs::sys::OBS_SOURCE_ASYNC_VIDEO
        | obs::sys::OBS_SOURCE_DO_NOT_DUPLICATE;
    const ICON_TYPE: IconType = IconType::Media;

    fn type_name() -> &'static CStr {
        c"Dummy Source"
    }

    fn defaults(_settings: &Data<'_>) {}

    fn properties(_instance: Option<&Self>) -> Properties {
        Properties::new()
    }

    fn create(_settings: &Data<'_>, _source: SourceHandle) -> Option<Box<Self>> {
        Some(Box::new(Dummy))
    }

    fn media_get_state(&self) -> MediaState {
        MediaState::Playing
    }
}

#[test]
fn module_version_is_packed_the_way_obs_reads_it() {
    // obs_init_module gates on the top 16 bits (major.minor) only.
    assert_eq!(obs_module_ver(), obs::sys::make_semantic_version(32, 1, 2));
    assert_eq!(obs_module_ver() >> 16, (32 << 8) | 1);
}

#[test]
fn module_metadata_strings_are_nul_terminated() {
    // SAFETY: the macro builds these from string literals via `_cstr!`.
    let name = unsafe { CStr::from_ptr(obs_module_name()) };
    let description = unsafe { CStr::from_ptr(obs_module_description()) };
    let author = unsafe { CStr::from_ptr(obs_module_author()) };

    assert_eq!(name.to_str().unwrap(), "Test Module");
    assert_eq!(
        description.to_str().unwrap(),
        "declare_module! expansion test"
    );
    assert_eq!(author.to_str().unwrap(), "obs crate tests");
    assert_eq!(MODULE_NAME, "obs-test-module");
    assert_eq!(MODULE_DEFAULT_LOCALE.to_str().unwrap(), "en-US");
}

#[test]
fn module_text_falls_back_to_the_key_without_a_locale() {
    // No locale has been loaded, so the lookup is NULL and every key must come
    // back unchanged — the behavior that makes a package shipped without its
    // .ini render bare identifiers instead of crashing.
    assert_eq!(module_text(c"AudioBufferHelp"), c"AudioBufferHelp");
}
