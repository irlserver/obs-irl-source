//! Source lifecycle (port of `src/irl-source.c`). W2-D.

use std::ffi::CStr;

use obs::{Data, MediaState, Properties, Source, SourceHandle};

/// The IRL source instance.
pub struct IrlSource {
    _source: SourceHandle,
}

impl Source for IrlSource {
    const ID: &'static CStr = c"irl_source";
    const OUTPUT_FLAGS: u32 = obs::sys::OBS_SOURCE_ASYNC_VIDEO
        | obs::sys::OBS_SOURCE_AUDIO
        | obs::sys::OBS_SOURCE_DO_NOT_DUPLICATE
        | obs::sys::OBS_SOURCE_CONTROLLABLE_MEDIA;

    fn type_name() -> &'static CStr {
        crate::module_text(c"SourceName")
    }

    fn defaults(settings: &Data<'_>) {
        crate::settings::defaults(settings)
    }

    fn properties(instance: Option<&Self>) -> Properties {
        crate::settings::properties(instance)
    }

    fn create(settings: &Data<'_>, source: SourceHandle) -> Option<Box<Self>> {
        let _ = settings;
        todo!("W2-D")
    }

    fn update(&self, settings: &Data<'_>) {
        let _ = settings;
        todo!("W2-D")
    }

    fn video_tick(&self, _seconds: f32) {
        todo!("W2-D")
    }

    fn activate(&self) {
        todo!("W2-D")
    }

    fn deactivate(&self) {
        todo!("W2-D")
    }

    fn show(&self) {
        todo!("W2-D")
    }

    fn hide(&self) {
        todo!("W2-D")
    }

    fn media_play_pause(&self, pause: bool) {
        let _ = pause;
        todo!("W2-D")
    }

    fn media_restart(&self) {
        todo!("W2-D")
    }

    fn media_stop(&self) {
        todo!("W2-D")
    }

    fn media_get_state(&self) -> MediaState {
        todo!("W2-D")
    }
}
