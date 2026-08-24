//! Source registration and the non-owning source handle.

use core::ffi::CStr;
use core::ptr::NonNull;

use crate::audio::AudioFrame;
use crate::data::Data;
use crate::proc::ProcHandler;
use crate::properties::Properties;
use crate::video::VideoFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    Input,
    Filter,
    Transition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconType {
    Unknown,
    Media,
    Camera,
    Custom,
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

/// Register `T` with libobs (`obs_register_source_s`). Call from
/// `obs_module_load`. The `obs_source_info` is built once into a static and
/// every callback slot is a guarded shim that forwards to the trait.
pub fn register_source<T: Source>() {
    todo!("W1-A: build obs_source_info in a static, install shims, obs_register_source_s(info, size_of)")
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
        todo!("W1-A")
    }

    /// `obs_source_get_unversioned_id`.
    pub fn unversioned_id(&self) -> String {
        todo!("W1-A")
    }

    pub fn showing(&self) -> bool {
        todo!("W1-A")
    }

    pub fn active(&self) -> bool {
        todo!("W1-A")
    }

    pub fn width(&self) -> u32 {
        todo!("W1-A")
    }

    pub fn height(&self) -> u32 {
        todo!("W1-A")
    }

    /// `obs_source_set_async_unbuffered`.
    pub fn set_async_unbuffered(&self, unbuffered: bool) {
        let _ = unbuffered;
        todo!("W1-A")
    }

    /// `obs_source_set_async_decoupled`.
    pub fn set_async_decoupled(&self, decoupled: bool) {
        let _ = decoupled;
        todo!("W1-A")
    }

    /// `obs_source_media_started`.
    pub fn media_started(&self) {
        todo!("W1-A")
    }

    /// `obs_source_get_proc_handler`.
    pub fn proc_handler(&self) -> ProcHandler<'_> {
        todo!("W1-A")
    }

    /// `obs_source_output_video`. libobs copies the planes during the call;
    /// the borrow in `frame` ends when this returns.
    pub fn output_video(&self, frame: &VideoFrame<'_>) {
        let _ = frame;
        todo!("W1-A")
    }

    /// `obs_source_output_video(source, NULL)`: clear the async frame.
    pub fn output_video_none(&self) {
        todo!("W1-A")
    }

    /// `obs_source_output_audio`. libobs copies during the call.
    pub fn output_audio(&self, audio: &AudioFrame<'_>) {
        let _ = audio;
        todo!("W1-A")
    }

    /// `obs_source_get_ref`: `None` if the source is being destroyed.
    pub fn get_ref(&self) -> Option<OwnedSource> {
        todo!("W1-A")
    }
}

/// An owned reference (`obs_source_get_ref` / `obs_get_source_by_name`),
/// released with `obs_source_release` in `Drop`.
#[derive(Debug)]
pub struct OwnedSource(SourceHandle);

impl OwnedSource {
    pub fn handle(&self) -> SourceHandle {
        self.0
    }
}

impl Drop for OwnedSource {
    fn drop(&mut self) {
        todo!("W1-A: obs_source_release")
    }
}

/// `obs_get_source_by_name`.
pub fn get_source_by_name(name: &CStr) -> Option<OwnedSource> {
    let _ = name;
    todo!("W1-A")
}

/// `obs_enum_sources`. Return `false` from `f` to stop.
pub fn enum_sources(f: &mut dyn FnMut(SourceHandle) -> bool) {
    let _ = f;
    todo!("W1-A: guarded trampoline; a panic logs and returns true")
}
