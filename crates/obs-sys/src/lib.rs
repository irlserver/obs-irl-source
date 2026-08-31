//! Raw libobs declarations, hand-written (no bindgen at build time).
//!
//! Only the surface obs-irl-source uses is declared here: 58 functions, the
//! structs libobs passes by value or fills in, and the enums/flags those take.
//! Layout is verified by `cargo test -p obs-sys --features layout-test`
//! against real libobs headers; the declarations follow OBS 32.1.2, and
//! everything except `obs_transform_info::crop_to_bounds` is identical back to
//! 30.0.
//!
//! Linking: on Windows every extern block is `raw-dylib` against `obs.dll`, so
//! no import library is needed; on Linux the symbols stay undefined and
//! resolve against the already-loaded libobs at dlopen time; on macOS the
//! plugin crate passes `-undefined dynamic_lookup`.
//!
//! Callers must never invoke `pthread_*` or link w32-pthreads: the reason the
//! C plugin had to (librist's pthread shim colliding with w32-pthreads on
//! MSVC) does not exist in Rust, and nothing here depends on it.

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    clippy::missing_safety_doc
)]

use core::ffi::{c_char, c_int, c_long, c_void};
use core::marker::{PhantomData, PhantomPinned};

/// Declares an opaque libobs handle type: zero-sized, `!Send`/`!Sync` by
/// construction, only ever used behind a raw pointer.
macro_rules! opaque {
    ($($name:ident => $alias:ident),* $(,)?) => {$(
        #[repr(C)]
        pub struct $name {
            _data: [u8; 0],
            _marker: PhantomData<(*mut u8, PhantomPinned)>,
        }
        pub type $alias = $name;
    )*};
}

opaque! {
    obs_source => obs_source_t,
    obs_data => obs_data_t,
    obs_data_array => obs_data_array_t,
    obs_properties => obs_properties_t,
    obs_property => obs_property_t,
    obs_scene => obs_scene_t,
    obs_scene_item => obs_sceneitem_t,
    proc_handler => proc_handler_t,
    obs_module => obs_module_t,
    text_lookup => lookup_t,
    gs_effect => gs_effect_t,
}

// ── Versioning ─────────────────────────────────────────────────────────

/// `MAKE_SEMANTIC_VERSION(major, minor, patch)` from obs-config.h.
pub const fn make_semantic_version(major: u32, minor: u32, patch: u32) -> u32 {
    (major << 24) | (minor << 16) | patch
}

// ── Log levels (util/base.h) ───────────────────────────────────────────

pub const LOG_ERROR: c_int = 100;
pub const LOG_WARNING: c_int = 200;
pub const LOG_INFO: c_int = 300;
pub const LOG_DEBUG: c_int = 400;

// ── Source flags (obs-source.h) ────────────────────────────────────────

pub const OBS_SOURCE_VIDEO: u32 = 1 << 0;
pub const OBS_SOURCE_AUDIO: u32 = 1 << 1;
pub const OBS_SOURCE_ASYNC: u32 = 1 << 2;
pub const OBS_SOURCE_ASYNC_VIDEO: u32 = OBS_SOURCE_ASYNC | OBS_SOURCE_VIDEO;
pub const OBS_SOURCE_DO_NOT_DUPLICATE: u32 = 1 << 7;
pub const OBS_SOURCE_CONTROLLABLE_MEDIA: u32 = 1 << 13;

pub const OBS_PROPERTIES_DEFER_UPDATE: u32 = 1 << 0;

// ── Alignment flags (obs.h) ────────────────────────────────────────────

pub const OBS_ALIGN_CENTER: u32 = 0;
pub const OBS_ALIGN_LEFT: u32 = 1 << 0;
pub const OBS_ALIGN_RIGHT: u32 = 1 << 1;
pub const OBS_ALIGN_TOP: u32 = 1 << 2;
pub const OBS_ALIGN_BOTTOM: u32 = 1 << 3;

// ── Enums ──────────────────────────────────────────────────────────────
// C enums with only non-negative values; every compiler we target lays them
// out as a 4-byte unsigned/int, which the layout test asserts.

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum obs_source_type {
    OBS_SOURCE_TYPE_INPUT = 0,
    OBS_SOURCE_TYPE_FILTER = 1,
    OBS_SOURCE_TYPE_TRANSITION = 2,
    OBS_SOURCE_TYPE_SCENE = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum obs_icon_type {
    OBS_ICON_TYPE_UNKNOWN = 0,
    OBS_ICON_TYPE_IMAGE = 1,
    OBS_ICON_TYPE_COLOR = 2,
    OBS_ICON_TYPE_SLIDESHOW = 3,
    OBS_ICON_TYPE_AUDIO_INPUT = 4,
    OBS_ICON_TYPE_AUDIO_OUTPUT = 5,
    OBS_ICON_TYPE_DESKTOP_CAPTURE = 6,
    OBS_ICON_TYPE_WINDOW_CAPTURE = 7,
    OBS_ICON_TYPE_GAME_CAPTURE = 8,
    OBS_ICON_TYPE_CAMERA = 9,
    OBS_ICON_TYPE_TEXT = 10,
    OBS_ICON_TYPE_MEDIA = 11,
    OBS_ICON_TYPE_BROWSER = 12,
    OBS_ICON_TYPE_CUSTOM = 13,
    OBS_ICON_TYPE_PROCESS_AUDIO_OUTPUT = 14,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum obs_media_state {
    OBS_MEDIA_STATE_NONE = 0,
    OBS_MEDIA_STATE_PLAYING = 1,
    OBS_MEDIA_STATE_OPENING = 2,
    OBS_MEDIA_STATE_BUFFERING = 3,
    OBS_MEDIA_STATE_PAUSED = 4,
    OBS_MEDIA_STATE_STOPPED = 5,
    OBS_MEDIA_STATE_ENDED = 6,
    OBS_MEDIA_STATE_ERROR = 7,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum obs_bounds_type {
    OBS_BOUNDS_NONE = 0,
    OBS_BOUNDS_STRETCH = 1,
    OBS_BOUNDS_SCALE_INNER = 2,
    OBS_BOUNDS_SCALE_OUTER = 3,
    OBS_BOUNDS_SCALE_TO_WIDTH = 4,
    OBS_BOUNDS_SCALE_TO_HEIGHT = 5,
    OBS_BOUNDS_MAX_ONLY = 6,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum obs_scale_type {
    OBS_SCALE_DISABLE = 0,
    OBS_SCALE_POINT = 1,
    OBS_SCALE_BICUBIC = 2,
    OBS_SCALE_BILINEAR = 3,
    OBS_SCALE_LANCZOS = 4,
    OBS_SCALE_AREA = 5,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum obs_text_type {
    OBS_TEXT_DEFAULT = 0,
    OBS_TEXT_PASSWORD = 1,
    OBS_TEXT_MULTILINE = 2,
    OBS_TEXT_INFO = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum obs_combo_type {
    OBS_COMBO_TYPE_INVALID = 0,
    OBS_COMBO_TYPE_EDITABLE = 1,
    OBS_COMBO_TYPE_LIST = 2,
    OBS_COMBO_TYPE_RADIO = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum obs_combo_format {
    OBS_COMBO_FORMAT_INVALID = 0,
    OBS_COMBO_FORMAT_INT = 1,
    OBS_COMBO_FORMAT_FLOAT = 2,
    OBS_COMBO_FORMAT_STRING = 3,
    OBS_COMBO_FORMAT_BOOL = 4,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum gs_color_space {
    GS_CS_SRGB = 0,
    GS_CS_SRGB_16F = 1,
    GS_CS_709_EXTENDED = 2,
    GS_CS_709_SCRGB = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum video_format {
    VIDEO_FORMAT_NONE = 0,
    VIDEO_FORMAT_I420 = 1,
    VIDEO_FORMAT_NV12 = 2,
    VIDEO_FORMAT_YVYU = 3,
    VIDEO_FORMAT_YUY2 = 4,
    VIDEO_FORMAT_UYVY = 5,
    VIDEO_FORMAT_RGBA = 6,
    VIDEO_FORMAT_BGRA = 7,
    VIDEO_FORMAT_BGRX = 8,
    VIDEO_FORMAT_Y800 = 9,
    VIDEO_FORMAT_I444 = 10,
    VIDEO_FORMAT_BGR3 = 11,
    VIDEO_FORMAT_I422 = 12,
    VIDEO_FORMAT_I40A = 13,
    VIDEO_FORMAT_I42A = 14,
    VIDEO_FORMAT_YUVA = 15,
    VIDEO_FORMAT_AYUV = 16,
    VIDEO_FORMAT_I010 = 17,
    VIDEO_FORMAT_P010 = 18,
    VIDEO_FORMAT_I210 = 19,
    VIDEO_FORMAT_I412 = 20,
    VIDEO_FORMAT_YA2L = 21,
    VIDEO_FORMAT_P216 = 22,
    VIDEO_FORMAT_P416 = 23,
    VIDEO_FORMAT_V210 = 24,
    VIDEO_FORMAT_R10L = 25,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum video_colorspace {
    VIDEO_CS_DEFAULT = 0,
    VIDEO_CS_601 = 1,
    VIDEO_CS_709 = 2,
    VIDEO_CS_SRGB = 3,
    VIDEO_CS_2100_PQ = 4,
    VIDEO_CS_2100_HLG = 5,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum video_range_type {
    VIDEO_RANGE_DEFAULT = 0,
    VIDEO_RANGE_PARTIAL = 1,
    VIDEO_RANGE_FULL = 2,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum video_trc {
    VIDEO_TRC_DEFAULT = 0,
    VIDEO_TRC_SRGB = 1,
    VIDEO_TRC_PQ = 2,
    VIDEO_TRC_HLG = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum speaker_layout {
    SPEAKERS_UNKNOWN = 0,
    SPEAKERS_MONO = 1,
    SPEAKERS_STEREO = 2,
    SPEAKERS_2POINT1 = 3,
    SPEAKERS_4POINT0 = 4,
    SPEAKERS_4POINT1 = 5,
    SPEAKERS_5POINT1 = 6,
    SPEAKERS_7POINT1 = 8,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum audio_format {
    AUDIO_FORMAT_UNKNOWN = 0,
    AUDIO_FORMAT_U8BIT = 1,
    AUDIO_FORMAT_16BIT = 2,
    AUDIO_FORMAT_32BIT = 3,
    AUDIO_FORMAT_FLOAT = 4,
    AUDIO_FORMAT_U8BIT_PLANAR = 5,
    AUDIO_FORMAT_16BIT_PLANAR = 6,
    AUDIO_FORMAT_32BIT_PLANAR = 7,
    AUDIO_FORMAT_FLOAT_PLANAR = 8,
}

// ── Structs ────────────────────────────────────────────────────────────

pub const MAX_AV_PLANES: usize = 8;

pub type obs_source_enum_proc_t = Option<
    unsafe extern "C" fn(parent: *mut obs_source_t, child: *mut obs_source_t, param: *mut c_void),
>;
pub type proc_handler_proc_t = Option<unsafe extern "C" fn(data: *mut c_void, cd: *mut calldata_t)>;

/// `struct obs_source_info`, OBS 32.1.2 field order (identical since 30.0).
///
/// Invariant: never append a field beyond what the oldest supported OBS
/// declares. `obs_register_source_s` rejects an info whose size exceeds the
/// host's own `sizeof(struct obs_source_info)`, so a newer field would make the
/// plugin fail to load on the OBS line it is built for.
#[repr(C)]
pub struct obs_source_info {
    pub id: *const c_char,
    pub type_: obs_source_type,
    pub output_flags: u32,
    pub get_name: Option<unsafe extern "C" fn(type_data: *mut c_void) -> *const c_char>,
    pub create: Option<
        unsafe extern "C" fn(settings: *mut obs_data_t, source: *mut obs_source_t) -> *mut c_void,
    >,
    pub destroy: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub get_width: Option<unsafe extern "C" fn(data: *mut c_void) -> u32>,
    pub get_height: Option<unsafe extern "C" fn(data: *mut c_void) -> u32>,
    pub get_defaults: Option<unsafe extern "C" fn(settings: *mut obs_data_t)>,
    pub get_properties: Option<unsafe extern "C" fn(data: *mut c_void) -> *mut obs_properties_t>,
    pub update: Option<unsafe extern "C" fn(data: *mut c_void, settings: *mut obs_data_t)>,
    pub activate: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub deactivate: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub show: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub hide: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub video_tick: Option<unsafe extern "C" fn(data: *mut c_void, seconds: f32)>,
    pub video_render: Option<unsafe extern "C" fn(data: *mut c_void, effect: *mut gs_effect_t)>,
    pub filter_video: Option<
        unsafe extern "C" fn(
            data: *mut c_void,
            frame: *mut obs_source_frame,
        ) -> *mut obs_source_frame,
    >,
    pub filter_audio:
        Option<unsafe extern "C" fn(data: *mut c_void, audio: *mut c_void) -> *mut c_void>,
    pub enum_active_sources: Option<
        unsafe extern "C" fn(
            data: *mut c_void,
            enum_callback: obs_source_enum_proc_t,
            param: *mut c_void,
        ),
    >,
    pub save: Option<unsafe extern "C" fn(data: *mut c_void, settings: *mut obs_data_t)>,
    pub load: Option<unsafe extern "C" fn(data: *mut c_void, settings: *mut obs_data_t)>,
    pub mouse_click: Option<
        unsafe extern "C" fn(
            data: *mut c_void,
            event: *const c_void,
            type_: i32,
            mouse_up: bool,
            click_count: u32,
        ),
    >,
    pub mouse_move:
        Option<unsafe extern "C" fn(data: *mut c_void, event: *const c_void, mouse_leave: bool)>,
    pub mouse_wheel: Option<
        unsafe extern "C" fn(
            data: *mut c_void,
            event: *const c_void,
            x_delta: c_int,
            y_delta: c_int,
        ),
    >,
    pub focus: Option<unsafe extern "C" fn(data: *mut c_void, focus: bool)>,
    pub key_click:
        Option<unsafe extern "C" fn(data: *mut c_void, event: *const c_void, key_up: bool)>,
    pub filter_remove: Option<unsafe extern "C" fn(data: *mut c_void, source: *mut obs_source_t)>,
    pub type_data: *mut c_void,
    pub free_type_data: Option<unsafe extern "C" fn(type_data: *mut c_void)>,
    pub audio_render: Option<
        unsafe extern "C" fn(
            data: *mut c_void,
            ts_out: *mut u64,
            audio_output: *mut c_void,
            mixers: u32,
            channels: usize,
            sample_rate: usize,
        ) -> bool,
    >,
    pub enum_all_sources: Option<
        unsafe extern "C" fn(
            data: *mut c_void,
            enum_callback: obs_source_enum_proc_t,
            param: *mut c_void,
        ),
    >,
    pub transition_start: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub transition_stop: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub get_defaults2:
        Option<unsafe extern "C" fn(type_data: *mut c_void, settings: *mut obs_data_t)>,
    pub get_properties2: Option<
        unsafe extern "C" fn(data: *mut c_void, type_data: *mut c_void) -> *mut obs_properties_t,
    >,
    pub audio_mix: Option<
        unsafe extern "C" fn(
            data: *mut c_void,
            ts_out: *mut u64,
            audio_output: *mut c_void,
            channels: usize,
            sample_rate: usize,
        ) -> bool,
    >,
    pub icon_type: obs_icon_type,
    pub media_play_pause: Option<unsafe extern "C" fn(data: *mut c_void, pause: bool)>,
    pub media_restart: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub media_stop: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub media_next: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub media_previous: Option<unsafe extern "C" fn(data: *mut c_void)>,
    pub media_get_duration: Option<unsafe extern "C" fn(data: *mut c_void) -> i64>,
    pub media_get_time: Option<unsafe extern "C" fn(data: *mut c_void) -> i64>,
    pub media_set_time: Option<unsafe extern "C" fn(data: *mut c_void, milliseconds: i64)>,
    pub media_get_state: Option<unsafe extern "C" fn(data: *mut c_void) -> obs_media_state>,
    pub version: u32,
    pub unversioned_id: *const c_char,
    pub missing_files: Option<unsafe extern "C" fn(data: *mut c_void) -> *mut c_void>,
    pub video_get_color_space: Option<
        unsafe extern "C" fn(
            data: *mut c_void,
            count: usize,
            preferred_spaces: *const gs_color_space,
        ) -> gs_color_space,
    >,
    pub filter_add: Option<unsafe extern "C" fn(data: *mut c_void, source: *mut obs_source_t)>,
}

/// `struct obs_source_frame` (obs.h). Passed by pointer to
/// `obs_source_output_video`, which copies it; the trailing `refs`/`prev_frame`
/// are libobs-internal but must exist for the size to match.
#[repr(C)]
pub struct obs_source_frame {
    pub data: [*mut u8; MAX_AV_PLANES],
    pub linesize: [u32; MAX_AV_PLANES],
    pub width: u32,
    pub height: u32,
    pub timestamp: u64,
    pub format: video_format,
    pub color_matrix: [f32; 16],
    pub full_range: bool,
    pub max_luminance: u16,
    pub color_range_min: [f32; 3],
    pub color_range_max: [f32; 3],
    pub flip: bool,
    pub flags: u8,
    pub trc: u8,
    pub refs: c_long,
    pub prev_frame: bool,
}

/// `struct obs_source_audio` (obs.h).
#[repr(C)]
pub struct obs_source_audio {
    pub data: [*const u8; MAX_AV_PLANES],
    pub frames: u32,
    pub speakers: speaker_layout,
    pub format: audio_format,
    pub samples_per_sec: u32,
    pub timestamp: u64,
}

/// `struct vec2` (graphics/vec2.h): a union of `{x, y}` and `float ptr[2]`,
/// which has the same layout as two floats.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct vec2 {
    pub x: f32,
    pub y: f32,
}

/// `struct obs_transform_info` (obs.h), including `crop_to_bounds` (added
/// after 30.0; present in the 32.1 floor the plugin targets).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obs_transform_info {
    pub pos: vec2,
    pub rot: f32,
    pub scale: vec2,
    pub alignment: u32,
    pub bounds_type: obs_bounds_type,
    pub bounds_alignment: u32,
    pub bounds: vec2,
    pub crop_to_bounds: bool,
}

/// `struct obs_video_info` (obs.h).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obs_video_info {
    pub graphics_module: *const c_char,
    pub fps_num: u32,
    pub fps_den: u32,
    pub base_width: u32,
    pub base_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub output_format: video_format,
    pub adapter: u32,
    pub gpu_conversion: bool,
    pub colorspace: video_colorspace,
    pub range: video_range_type,
    pub scale_type: obs_scale_type,
}

/// libobs fills `obs_video_info` and reads `obs_transform_info` by *its*
/// `sizeof`. These wrappers carry 64 bytes of slack so a future OBS that
/// appends a member cannot overrun our stack frame. Only the two call sites
/// that hand the struct to libobs use them.
#[repr(C)]
pub struct obs_video_info_slack {
    pub ovi: obs_video_info,
    pub _slack: [u8; 64],
}

#[repr(C)]
pub struct obs_transform_info_slack {
    pub info: obs_transform_info,
    pub _slack: [u8; 64],
}

/// `struct calldata` (callback/calldata.h). `calldata_init` is a memset to
/// zero and `calldata_free` is `bfree(stack)`; both are `static inline` in the
/// header and reimplemented in the `obs` crate, as are every typed
/// `calldata_set_*`/`calldata_get_*` helper (over `calldata_set_data` /
/// `calldata_get_data`).
#[repr(C)]
pub struct calldata_t {
    pub stack: *mut u8,
    pub size: usize,
    pub capacity: usize,
    pub fixed: bool,
}

/// `struct obs_websocket_request_response` is not needed: vendor requests are
/// registered through obs-websocket's proc handler with this callback record
/// (from `obs-websocket-api.h`, API version 3), passed by address and copied
/// by obs-websocket.
pub type obs_websocket_request_callback_function = Option<
    unsafe extern "C" fn(
        request_data: *mut obs_data_t,
        response_data: *mut obs_data_t,
        priv_data: *mut c_void,
    ),
>;

#[repr(C)]
pub struct obs_websocket_request_callback {
    pub callback: obs_websocket_request_callback_function,
    pub priv_data: *mut c_void,
}

// ── Functions ──────────────────────────────────────────────────────────
//
// Every declaration matches the prototype in the libobs header named above
// its group. `raw-dylib` on Windows means no import library is needed; on
// Linux/macOS the symbols stay undefined and resolve against the libobs the
// host process already loaded.
//
// `obs_get_video_info` and `obs_sceneitem_set_info2` are declared with the
// exact struct types, but their two call sites in the `obs` crate hand them a
// pointer into an [`obs_video_info_slack`] / [`obs_transform_info_slack`], so
// a newer libobs that appended a member cannot overrun the caller's frame.

#[cfg_attr(windows, link(name = "obs", kind = "raw-dylib"))]
unsafe extern "C" {
    // ── obs-source.h ───────────────────────────────────────────────────
    pub fn obs_register_source_s(info: *const obs_source_info, size: usize);

    // ── obs.h: core ────────────────────────────────────────────────────
    pub fn obs_get_video_info(ovi: *mut obs_video_info) -> bool;
    pub fn obs_get_proc_handler() -> *mut proc_handler_t;
    pub fn obs_enum_sources(
        enum_proc: Option<
            unsafe extern "C" fn(param: *mut c_void, source: *mut obs_source_t) -> bool,
        >,
        param: *mut c_void,
    );
    pub fn obs_enum_scenes(
        enum_proc: Option<
            unsafe extern "C" fn(param: *mut c_void, source: *mut obs_source_t) -> bool,
        >,
        param: *mut c_void,
    );
    pub fn obs_get_source_by_name(name: *const c_char) -> *mut obs_source_t;

    // ── obs.h: sources ─────────────────────────────────────────────────
    pub fn obs_source_get_ref(source: *mut obs_source_t) -> *mut obs_source_t;
    pub fn obs_source_release(source: *mut obs_source_t);
    pub fn obs_source_get_name(source: *const obs_source_t) -> *const c_char;
    pub fn obs_source_get_unversioned_id(source: *const obs_source_t) -> *const c_char;
    pub fn obs_source_get_width(source: *mut obs_source_t) -> u32;
    pub fn obs_source_get_height(source: *mut obs_source_t) -> u32;
    pub fn obs_source_showing(source: *const obs_source_t) -> bool;
    pub fn obs_source_active(source: *const obs_source_t) -> bool;
    pub fn obs_source_output_video(source: *mut obs_source_t, frame: *const obs_source_frame);
    pub fn obs_source_output_audio(source: *mut obs_source_t, audio: *const obs_source_audio);
    pub fn obs_source_get_proc_handler(source: *const obs_source_t) -> *mut proc_handler_t;
    pub fn obs_source_set_async_unbuffered(source: *mut obs_source_t, unbuffered: bool);
    pub fn obs_source_set_async_decoupled(source: *mut obs_source_t, decouple: bool);
    pub fn obs_source_media_started(source: *mut obs_source_t);

    // ── obs.h: scenes and scene items ──────────────────────────────────
    pub fn obs_scene_from_source(source: *const obs_source_t) -> *mut obs_scene_t;
    pub fn obs_scene_enum_items(
        scene: *mut obs_scene_t,
        callback: Option<
            unsafe extern "C" fn(
                scene: *mut obs_scene_t,
                item: *mut obs_sceneitem_t,
                param: *mut c_void,
            ) -> bool,
        >,
        param: *mut c_void,
    );
    pub fn obs_sceneitem_get_source(item: *const obs_sceneitem_t) -> *mut obs_source_t;
    pub fn obs_sceneitem_locked(item: *const obs_sceneitem_t) -> bool;
    /// Added after OBS 30.0; present in the 32.1 floor the plugin targets.
    pub fn obs_sceneitem_get_bounds_crop(item: *const obs_sceneitem_t) -> bool;
    /// Added after OBS 30.0 (the `crop_to_bounds`-aware `obs_sceneitem_set_info`).
    pub fn obs_sceneitem_set_info2(item: *mut obs_sceneitem_t, info: *const obs_transform_info);

    // ── obs-data.h ─────────────────────────────────────────────────────
    pub fn obs_data_create() -> *mut obs_data_t;
    pub fn obs_data_release(data: *mut obs_data_t);
    pub fn obs_data_get_string(data: *mut obs_data_t, name: *const c_char) -> *const c_char;
    pub fn obs_data_get_int(data: *mut obs_data_t, name: *const c_char) -> i64;
    pub fn obs_data_get_bool(data: *mut obs_data_t, name: *const c_char) -> bool;
    pub fn obs_data_get_double(data: *mut obs_data_t, name: *const c_char) -> f64;
    pub fn obs_data_set_string(data: *mut obs_data_t, name: *const c_char, val: *const c_char);
    pub fn obs_data_set_int(data: *mut obs_data_t, name: *const c_char, val: i64);
    pub fn obs_data_set_bool(data: *mut obs_data_t, name: *const c_char, val: bool);
    pub fn obs_data_set_double(data: *mut obs_data_t, name: *const c_char, val: f64);
    pub fn obs_data_set_default_string(
        data: *mut obs_data_t,
        name: *const c_char,
        val: *const c_char,
    );
    pub fn obs_data_set_default_int(data: *mut obs_data_t, name: *const c_char, val: i64);
    pub fn obs_data_set_default_bool(data: *mut obs_data_t, name: *const c_char, val: bool);
    pub fn obs_data_set_array(
        data: *mut obs_data_t,
        name: *const c_char,
        array: *mut obs_data_array_t,
    );
    pub fn obs_data_array_create() -> *mut obs_data_array_t;
    pub fn obs_data_array_push_back(array: *mut obs_data_array_t, obj: *mut obs_data_t) -> usize;
    pub fn obs_data_array_release(array: *mut obs_data_array_t);

    // ── obs-properties.h ───────────────────────────────────────────────
    pub fn obs_properties_create() -> *mut obs_properties_t;
    /// Only reached when a half-built dialog is abandoned (a caught panic);
    /// the normal path hands the object to libobs from `get_properties`.
    pub fn obs_properties_destroy(props: *mut obs_properties_t);
    pub fn obs_properties_set_flags(props: *mut obs_properties_t, flags: u32);
    pub fn obs_properties_add_text(
        props: *mut obs_properties_t,
        name: *const c_char,
        description: *const c_char,
        type_: obs_text_type,
    ) -> *mut obs_property_t;
    pub fn obs_properties_add_int(
        props: *mut obs_properties_t,
        name: *const c_char,
        description: *const c_char,
        min: c_int,
        max: c_int,
        step: c_int,
    ) -> *mut obs_property_t;
    /// The slider variant of `obs_properties_add_int`: same value, drawn as a
    /// slider plus a spin box instead of a bare spin box.
    pub fn obs_properties_add_int_slider(
        props: *mut obs_properties_t,
        name: *const c_char,
        description: *const c_char,
        min: c_int,
        max: c_int,
        step: c_int,
    ) -> *mut obs_property_t;
    /// Unit drawn after the value of an int property (`"%"`, `"ms"`, …).
    pub fn obs_property_int_set_suffix(p: *mut obs_property_t, suffix: *const c_char);
    pub fn obs_properties_add_bool(
        props: *mut obs_properties_t,
        name: *const c_char,
        description: *const c_char,
    ) -> *mut obs_property_t;
    pub fn obs_properties_add_list(
        props: *mut obs_properties_t,
        name: *const c_char,
        description: *const c_char,
        type_: obs_combo_type,
        format: obs_combo_format,
    ) -> *mut obs_property_t;
    pub fn obs_property_list_add_int(
        p: *mut obs_property_t,
        name: *const c_char,
        val: i64,
    ) -> usize;

    // ── callback/calldata.h ────────────────────────────────────────────
    // The typed helpers around these three are `static inline` in the header
    // and are reimplemented in the `obs` crate (`proc::CallData`).
    pub fn calldata_get_data(
        data: *const calldata_t,
        name: *const c_char,
        out: *mut c_void,
        size: usize,
    ) -> bool;
    pub fn calldata_set_data(
        data: *mut calldata_t,
        name: *const c_char,
        in_: *const c_void,
        new_size: usize,
    );
    pub fn calldata_get_string(
        data: *const calldata_t,
        name: *const c_char,
        str_: *mut *const c_char,
    ) -> bool;

    // ── callback/proc.h ────────────────────────────────────────────────
    pub fn proc_handler_add(
        handler: *mut proc_handler_t,
        decl_string: *const c_char,
        proc: proc_handler_proc_t,
        data: *mut c_void,
    );
    pub fn proc_handler_call(
        handler: *mut proc_handler_t,
        name: *const c_char,
        params: *mut calldata_t,
    ) -> bool;

    // ── obs.h / util/text-lookup.h: module locale ──────────────────────
    pub fn obs_module_load_locale(
        module: *mut obs_module_t,
        default_locale: *const c_char,
        locale: *const c_char,
    ) -> *mut lookup_t;
    pub fn text_lookup_destroy(lookup: *mut lookup_t);
    pub fn text_lookup_getstr(
        lookup: *mut lookup_t,
        lookup_val: *const c_char,
        out: *mut *const c_char,
    ) -> bool;

    // ── util/base.h ────────────────────────────────────────────────────
    /// Variadic. Always called as `blog(level, c"%s", msg)` so a `%` in the
    /// message can never be read as a format directive.
    pub fn blog(log_level: c_int, format: *const c_char, ...);

    // ── util/platform.h ────────────────────────────────────────────────
    pub fn os_gettime_ns() -> u64;
    pub fn os_sleep_ms(duration: u32);

    // ── util/bmem.h ────────────────────────────────────────────────────
    pub fn bfree(ptr: *mut c_void);

    // ── media-io/video-io.h ────────────────────────────────────────────
    pub fn video_format_get_parameters_for_format(
        color_space: video_colorspace,
        range: video_range_type,
        format: video_format,
        matrix: *mut f32,
        min_range: *mut f32,
        max_range: *mut f32,
    ) -> bool;
}

#[cfg(all(test, feature = "layout-test"))]
mod layout_test;
