//! Source registration and the non-owning source handle.

use core::ffi::{CStr, c_void};
use core::ptr::NonNull;

use crate::audio::AudioFrame;
use crate::data::Data;
use crate::panic::{guard, guard_unit};
use crate::proc::ProcHandler;
use crate::properties::Properties;
use crate::video::VideoFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    Input,
    Filter,
    Transition,
}

impl SourceType {
    fn to_sys(self) -> obs_sys::obs_source_type {
        use obs_sys::obs_source_type as T;
        match self {
            Self::Input => T::OBS_SOURCE_TYPE_INPUT,
            Self::Filter => T::OBS_SOURCE_TYPE_FILTER,
            Self::Transition => T::OBS_SOURCE_TYPE_TRANSITION,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconType {
    Unknown,
    Media,
    Camera,
    Custom,
}

impl IconType {
    fn to_sys(self) -> obs_sys::obs_icon_type {
        use obs_sys::obs_icon_type as I;
        match self {
            Self::Unknown => I::OBS_ICON_TYPE_UNKNOWN,
            Self::Media => I::OBS_ICON_TYPE_MEDIA,
            Self::Camera => I::OBS_ICON_TYPE_CAMERA,
            Self::Custom => I::OBS_ICON_TYPE_CUSTOM,
        }
    }
}

/// `enum obs_media_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaState {
    None,
    Playing,
    Opening,
    Buffering,
    Paused,
    Stopped,
    Ended,
    Error,
}

impl MediaState {
    fn to_sys(self) -> obs_sys::obs_media_state {
        use obs_sys::obs_media_state as M;
        match self {
            Self::None => M::OBS_MEDIA_STATE_NONE,
            Self::Playing => M::OBS_MEDIA_STATE_PLAYING,
            Self::Opening => M::OBS_MEDIA_STATE_OPENING,
            Self::Buffering => M::OBS_MEDIA_STATE_BUFFERING,
            Self::Paused => M::OBS_MEDIA_STATE_PAUSED,
            Self::Stopped => M::OBS_MEDIA_STATE_STOPPED,
            Self::Ended => M::OBS_MEDIA_STATE_ENDED,
            Self::Error => M::OBS_MEDIA_STATE_ERROR,
        }
    }
}

/// An OBS source type. One `impl` per registered source id.
///
/// Callbacks take `&self`: OBS-thread callbacks and proc-handler calls (which
/// arrive on other threads, e.g. obs-websocket's) may overlap, so interior
/// mutability is the only sound option. `create` returns the boxed instance;
/// `destroy` is `Drop`.
pub trait Source: Send + Sync + Sized + 'static {
    /// `obs_source_info::id`.
    const ID: &'static CStr;
    const TYPE: SourceType = SourceType::Input;
    /// `obs_source_info::output_flags` (`sys::OBS_SOURCE_*`).
    const OUTPUT_FLAGS: u32;
    const ICON_TYPE: IconType = IconType::Unknown;
    const VERSION: u32 = 0;

    /// Display name (`get_name`). The pointer is kept by OBS, hence `'static`.
    fn type_name() -> &'static CStr;

    /// `get_defaults`.
    fn defaults(settings: &Data<'_>);

    /// `get_properties`. `instance` is `None` when OBS builds the dialog for a
    /// source that does not exist yet.
    fn properties(instance: Option<&Self>) -> Properties;

    /// `create`. Returning `None` makes OBS treat creation as failed; `destroy`
    /// is then never called.
    fn create(settings: &Data<'_>, source: SourceHandle) -> Option<Box<Self>>;

    fn update(&self, _settings: &Data<'_>) {}
    fn video_tick(&self, _seconds: f32) {}
    fn activate(&self) {}
    fn deactivate(&self) {}
    fn show(&self) {}
    fn hide(&self) {}

    fn media_play_pause(&self, _pause: bool) {}
    fn media_restart(&self) {}
    fn media_stop(&self) {}
    fn media_get_state(&self) -> MediaState {
        MediaState::None
    }
}

// ── Callback shims ─────────────────────────────────────────────────────
//
// Every one of these is an `extern "C"` frame libobs calls, so every one
// starts with a panic guard. `data` is the `Box<T>` returned by `create`,
// alive until `destroy`; libobs never passes it to another source's shims.

/// # Safety
/// `data` must be the `Box<T>` pointer this source's `create` returned.
unsafe fn instance<'a, T: Source>(data: *mut c_void) -> Option<&'a T> {
    if data.is_null() {
        None
    } else {
        // SAFETY: `data` is a live `Box<T>` and the trait requires Sync, so
        // handing out a shared reference from any thread is sound.
        Some(unsafe { &*data.cast::<T>() })
    }
}

unsafe extern "C" fn shim_get_name<T: Source>(_type_data: *mut c_void) -> *const core::ffi::c_char {
    guard("get_name", core::ptr::null(), || T::type_name().as_ptr())
}

unsafe extern "C" fn shim_create<T: Source>(
    settings: *mut obs_sys::obs_data_t,
    source: *mut obs_sys::obs_source_t,
) -> *mut c_void {
    guard("create", core::ptr::null_mut(), || {
        let (Some(settings), Some(source)) = (NonNull::new(settings), NonNull::new(source)) else {
            return core::ptr::null_mut();
        };
        // SAFETY: libobs owns both for the duration of the call; the settings
        // borrow ends when `create` returns, and the source outlives the
        // plugin data by construction (it is destroyed after `destroy` runs).
        let settings = unsafe { Data::from_raw(settings) };
        let handle = unsafe { SourceHandle::from_raw(source) };
        match T::create(&settings, handle) {
            Some(instance) => Box::into_raw(instance).cast::<c_void>(),
            None => core::ptr::null_mut(),
        }
    })
}

unsafe extern "C" fn shim_destroy<T: Source>(data: *mut c_void) {
    guard_unit("destroy", || {
        if data.is_null() {
            return;
        }
        // SAFETY: reclaims the single Box `create` leaked. libobs calls
        // destroy exactly once and never uses `data` afterwards.
        drop(unsafe { Box::from_raw(data.cast::<T>()) });
    });
}

unsafe extern "C" fn shim_get_defaults<T: Source>(settings: *mut obs_sys::obs_data_t) {
    guard_unit("get_defaults", || {
        let Some(settings) = NonNull::new(settings) else {
            return;
        };
        // SAFETY: live for the call.
        let settings = unsafe { Data::from_raw(settings) };
        T::defaults(&settings);
    });
}

unsafe extern "C" fn shim_get_properties<T: Source>(
    data: *mut c_void,
) -> *mut obs_sys::obs_properties_t {
    guard("get_properties", core::ptr::null_mut(), || {
        // SAFETY: `data` is this source's instance, or NULL when OBS builds
        // the dialog before the source exists.
        let instance = unsafe { instance::<T>(data) };
        T::properties(instance).into_raw()
    })
}

unsafe extern "C" fn shim_update<T: Source>(data: *mut c_void, settings: *mut obs_sys::obs_data_t) {
    guard_unit("update", || {
        // SAFETY: as above.
        let (Some(this), Some(settings)) = (unsafe { instance::<T>(data) }, NonNull::new(settings))
        else {
            return;
        };
        // SAFETY: live for the call.
        let settings = unsafe { Data::from_raw(settings) };
        this.update(&settings);
    });
}

unsafe extern "C" fn shim_video_tick<T: Source>(data: *mut c_void, seconds: f32) {
    guard_unit("video_tick", || {
        // SAFETY: as above.
        if let Some(this) = unsafe { instance::<T>(data) } {
            this.video_tick(seconds);
        }
    });
}

macro_rules! simple_shim {
    ($($shim:ident => $method:ident / $what:literal),* $(,)?) => {$(
        unsafe extern "C" fn $shim<T: Source>(data: *mut c_void) {
            guard_unit($what, || {
                // SAFETY: `data` is this source's instance.
                if let Some(this) = unsafe { instance::<T>(data) } {
                    this.$method();
                }
            });
        }
    )*};
}

simple_shim! {
    shim_activate => activate / "activate",
    shim_deactivate => deactivate / "deactivate",
    shim_show => show / "show",
    shim_hide => hide / "hide",
    shim_media_restart => media_restart / "media_restart",
    shim_media_stop => media_stop / "media_stop",
}

unsafe extern "C" fn shim_media_play_pause<T: Source>(data: *mut c_void, pause: bool) {
    guard_unit("media_play_pause", || {
        // SAFETY: `data` is this source's instance.
        if let Some(this) = unsafe { instance::<T>(data) } {
            this.media_play_pause(pause);
        }
    });
}

unsafe extern "C" fn shim_media_get_state<T: Source>(
    data: *mut c_void,
) -> obs_sys::obs_media_state {
    guard(
        "media_get_state",
        obs_sys::obs_media_state::OBS_MEDIA_STATE_NONE,
        || {
            // SAFETY: `data` is this source's instance.
            match unsafe { instance::<T>(data) } {
                Some(this) => this.media_get_state().to_sys(),
                None => obs_sys::obs_media_state::OBS_MEDIA_STATE_NONE,
            }
        },
    )
}

/// Register `T` with libobs (`obs_register_source_s`). Call from
/// `obs_module_load`. The `obs_source_info` is built once into a static and
/// every callback slot is a guarded shim that forwards to the trait.
pub fn register_source<T: Source>() {
    // A `static` cannot be generic, so the info is heap-allocated and leaked
    // instead. libobs keeps the pointer for the life of the process and
    // registration happens once per module load, so this is a fixed cost of a
    // few hundred bytes, not a leak that grows.
    let info = Box::leak(Box::new(obs_sys::obs_source_info {
        id: T::ID.as_ptr(),
        type_: T::TYPE.to_sys(),
        output_flags: T::OUTPUT_FLAGS,
        get_name: Some(shim_get_name::<T>),
        create: Some(shim_create::<T>),
        destroy: Some(shim_destroy::<T>),
        get_width: None,
        get_height: None,
        get_defaults: Some(shim_get_defaults::<T>),
        get_properties: Some(shim_get_properties::<T>),
        update: Some(shim_update::<T>),
        activate: Some(shim_activate::<T>),
        deactivate: Some(shim_deactivate::<T>),
        show: Some(shim_show::<T>),
        hide: Some(shim_hide::<T>),
        video_tick: Some(shim_video_tick::<T>),
        video_render: None,
        filter_video: None,
        filter_audio: None,
        enum_active_sources: None,
        save: None,
        load: None,
        mouse_click: None,
        mouse_move: None,
        mouse_wheel: None,
        focus: None,
        key_click: None,
        filter_remove: None,
        type_data: core::ptr::null_mut(),
        free_type_data: None,
        audio_render: None,
        enum_all_sources: None,
        transition_start: None,
        transition_stop: None,
        get_defaults2: None,
        get_properties2: None,
        audio_mix: None,
        icon_type: T::ICON_TYPE.to_sys(),
        media_play_pause: Some(shim_media_play_pause::<T>),
        media_restart: Some(shim_media_restart::<T>),
        media_stop: Some(shim_media_stop::<T>),
        media_next: None,
        media_previous: None,
        media_get_duration: None,
        media_get_time: None,
        media_set_time: None,
        media_get_state: Some(shim_media_get_state::<T>),
        version: T::VERSION,
        // NULL means "the id is already unversioned", which is what libobs
        // assumes for a source that never renamed itself.
        unversioned_id: core::ptr::null(),
        missing_files: None,
        video_get_color_space: None,
        filter_add: None,
    }));

    // SAFETY: `info` lives for the rest of the process and every pointer in it
    // is either NULL or `'static`. Passing our own `size_of` is what lets an
    // older libobs accept the struct: it copies `min(size, its own sizeof)`.
    unsafe {
        obs_sys::obs_register_source_s(info, size_of::<obs_sys::obs_source_info>());
    }
}

/// A non-owning `obs_source_t*`. Valid for the lifetime of the source that
/// owns the plugin data, which by construction outlives every use.
#[derive(Debug, Clone, Copy)]
pub struct SourceHandle(NonNull<obs_sys::obs_source_t>);

// libobs source functions are thread-safe; the handle carries no state.
unsafe impl Send for SourceHandle {}
unsafe impl Sync for SourceHandle {}

impl SourceHandle {
    /// # Safety
    /// `ptr` must be a live `obs_source_t` that outlives the handle's use.
    pub unsafe fn from_raw(ptr: NonNull<obs_sys::obs_source_t>) -> Self {
        Self(ptr)
    }

    pub fn as_ptr(&self) -> *mut obs_sys::obs_source_t {
        self.0.as_ptr()
    }

    /// `obs_source_get_name` (owned copy; the C string belongs to libobs).
    pub fn name(&self) -> String {
        // SAFETY: live handle; libobs returns a NUL-terminated name that can
        // be renamed from the OBS thread, so it is copied out immediately.
        let raw = unsafe { obs_sys::obs_source_get_name(self.as_ptr()) };
        cstr_to_string(raw)
    }

    /// `obs_source_get_unversioned_id`.
    pub fn unversioned_id(&self) -> String {
        // SAFETY: live handle; the id is a static string in the source info.
        let raw = unsafe { obs_sys::obs_source_get_unversioned_id(self.as_ptr()) };
        cstr_to_string(raw)
    }

    pub fn showing(&self) -> bool {
        // SAFETY: live handle.
        unsafe { obs_sys::obs_source_showing(self.as_ptr()) }
    }

    pub fn active(&self) -> bool {
        // SAFETY: live handle.
        unsafe { obs_sys::obs_source_active(self.as_ptr()) }
    }

    pub fn width(&self) -> u32 {
        // SAFETY: live handle.
        unsafe { obs_sys::obs_source_get_width(self.as_ptr()) }
    }

    pub fn height(&self) -> u32 {
        // SAFETY: live handle.
        unsafe { obs_sys::obs_source_get_height(self.as_ptr()) }
    }

    /// `obs_source_set_async_unbuffered`.
    pub fn set_async_unbuffered(&self, unbuffered: bool) {
        // SAFETY: live handle.
        unsafe { obs_sys::obs_source_set_async_unbuffered(self.as_ptr(), unbuffered) };
    }

    /// `obs_source_set_async_decoupled`.
    pub fn set_async_decoupled(&self, decoupled: bool) {
        // SAFETY: live handle.
        unsafe { obs_sys::obs_source_set_async_decoupled(self.as_ptr(), decoupled) };
    }

    /// `obs_source_media_started`.
    pub fn media_started(&self) {
        // SAFETY: live handle.
        unsafe { obs_sys::obs_source_media_started(self.as_ptr()) };
    }

    /// `obs_source_get_proc_handler`.
    ///
    /// # Panics
    /// If libobs has no proc handler for the source, which cannot happen for
    /// a live source (it is created with one).
    pub fn proc_handler(&self) -> ProcHandler<'_> {
        // SAFETY: live handle; the handler belongs to the source and so lives
        // at least as long as the borrow the return type carries.
        let ptr = unsafe { obs_sys::obs_source_get_proc_handler(self.as_ptr()) };
        let ptr = NonNull::new(ptr).expect("obs_source_get_proc_handler returned NULL");
        unsafe { ProcHandler::from_raw(ptr) }
    }

    /// `obs_source_output_video`. libobs copies the planes during the call;
    /// the borrow in `frame` ends when this returns.
    pub fn output_video(&self, frame: &VideoFrame<'_>) {
        // SAFETY: live handle; libobs reads the frame (and the planes it
        // points at, kept alive by the frame's lifetime) only during the call.
        unsafe { obs_sys::obs_source_output_video(self.as_ptr(), frame.as_sys()) };
    }

    /// `obs_source_output_video(source, NULL)`: clear the async frame.
    pub fn output_video_none(&self) {
        // SAFETY: live handle; NULL is the documented "no frame" argument.
        unsafe { obs_sys::obs_source_output_video(self.as_ptr(), core::ptr::null()) };
    }

    /// `obs_source_output_audio`. libobs copies during the call.
    pub fn output_audio(&self, audio: &AudioFrame<'_>) {
        // SAFETY: live handle; as for output_video.
        unsafe { obs_sys::obs_source_output_audio(self.as_ptr(), audio.as_sys()) };
    }

    /// `obs_source_get_ref`: `None` if the source is being destroyed.
    pub fn get_ref(&self) -> Option<OwnedSource> {
        // SAFETY: live handle; libobs returns NULL for a source already on
        // its way out, and otherwise a reference this call now owns.
        let ptr = unsafe { obs_sys::obs_source_get_ref(self.as_ptr()) };
        NonNull::new(ptr).map(|p| OwnedSource(Self(p)))
    }
}

/// Copy a libobs-owned C string, treating NULL as empty (libobs returns NULL
/// for a source with no name rather than an empty string).
fn cstr_to_string(raw: *const core::ffi::c_char) -> String {
    if raw.is_null() {
        return String::new();
    }
    // SAFETY: non-NULL libobs strings are NUL-terminated and stable for the
    // duration of this copy.
    unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned()
}

/// An owned reference (`obs_source_get_ref` / `obs_get_source_by_name`),
/// released with `obs_source_release` in `Drop`.
#[derive(Debug)]
pub struct OwnedSource(SourceHandle);

impl OwnedSource {
    pub fn handle(&self) -> SourceHandle {
        self.0
    }

    /// # Safety
    /// `ptr` must be an `obs_source_t` whose reference this value now owns.
    pub unsafe fn from_raw(ptr: NonNull<obs_sys::obs_source_t>) -> Self {
        Self(SourceHandle(ptr))
    }
}

impl Drop for OwnedSource {
    fn drop(&mut self) {
        // SAFETY: this value owns exactly one reference, released once.
        unsafe { obs_sys::obs_source_release(self.0.as_ptr()) };
    }
}

/// `obs_get_source_by_name`.
pub fn get_source_by_name(name: &CStr) -> Option<OwnedSource> {
    // SAFETY: NUL-terminated name; libobs returns an owned reference or NULL.
    let ptr = unsafe { obs_sys::obs_get_source_by_name(name.as_ptr()) };
    NonNull::new(ptr).map(|p| OwnedSource(SourceHandle(p)))
}

/// The closure an enumeration trampoline reaches through the `void *param`.
type EnumFn<'a> = &'a mut dyn FnMut(SourceHandle) -> bool;

/// # Safety
/// `param` must be a `*mut EnumFn` valid for the enumeration.
unsafe extern "C" fn enum_sources_trampoline(
    param: *mut c_void,
    source: *mut obs_sys::obs_source_t,
) -> bool {
    // A panic stops nothing: returning `true` keeps libobs walking its list,
    // which is the only state it is prepared for mid-enumeration.
    guard("source enumeration", true, || {
        let (Some(source), false) = (NonNull::new(source), param.is_null()) else {
            return true;
        };
        // SAFETY: `param` is the &mut closure passed to obs_enum_* below, and
        // libobs calls this synchronously from that call, one at a time.
        let f = unsafe { &mut *param.cast::<EnumFn<'_>>() };
        // SAFETY: libobs holds the source for the duration of the callback.
        f(unsafe { SourceHandle::from_raw(source) })
    })
}

/// `obs_enum_sources`. Return `false` from `f` to stop.
pub fn enum_sources(f: &mut dyn FnMut(SourceHandle) -> bool) {
    let mut f: EnumFn<'_> = f;
    // SAFETY: the trampoline reads `&mut f` only during this call.
    unsafe {
        obs_sys::obs_enum_sources(Some(enum_sources_trampoline), (&raw mut f).cast::<c_void>());
    }
}

/// Shared with `scene::enum_scenes`, which enumerates the same shape.
pub(crate) fn enum_scenes_inner(f: &mut dyn FnMut(SourceHandle) -> bool) {
    let mut f: EnumFn<'_> = f;
    // SAFETY: as for `enum_sources`.
    unsafe {
        obs_sys::obs_enum_scenes(Some(enum_sources_trampoline), (&raw mut f).cast::<c_void>());
    }
}
