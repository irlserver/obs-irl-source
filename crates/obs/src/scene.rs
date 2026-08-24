//! Scene enumeration and scene-item transforms (for fit-to-canvas).

use core::ptr::NonNull;

use crate::source::SourceHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoundsType {
    #[default]
    None,
    Stretch,
    ScaleInner,
    ScaleOuter,
    ScaleToWidth,
    ScaleToHeight,
    MaxOnly,
}

/// `struct obs_transform_info` in Rust terms.
#[derive(Debug, Clone, Copy, Default)]
pub struct TransformInfo {
    pub pos: (f32, f32),
    pub rot: f32,
    pub scale: (f32, f32),
    /// `sys::OBS_ALIGN_*` bits.
    pub alignment: u32,
    pub bounds_type: BoundsType,
    pub bounds_alignment: u32,
    pub bounds: (f32, f32),
    pub crop_to_bounds: bool,
}

/// The parts of `struct obs_video_info` a source needs.
#[derive(Debug, Clone, Copy)]
pub struct VideoInfo {
    pub base_width: u32,
    pub base_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

/// `obs_get_video_info` (through the slack wrapper).
pub fn get_video_info() -> Option<VideoInfo> {
    todo!("W1-A")
}

/// `obs_enum_scenes`. Return `false` from `f` to stop.
pub fn enum_scenes(f: &mut dyn FnMut(SourceHandle) -> bool) {
    let _ = f;
    todo!("W1-A: guarded trampoline")
}

#[derive(Debug, Clone, Copy)]
pub struct Scene(NonNull<obs_sys::obs_scene_t>);

impl Scene {
    /// `obs_scene_from_source`.
    pub fn from_source(source: SourceHandle) -> Option<Scene> {
        let _ = source;
        todo!("W1-A")
    }

    /// `obs_scene_enum_items`. Return `false` from `f` to stop.
    pub fn enum_items(&self, f: &mut dyn FnMut(SceneItem) -> bool) {
        let _ = f;
        todo!("W1-A: guarded trampoline")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SceneItem(NonNull<obs_sys::obs_sceneitem_t>);

impl SceneItem {
    pub fn source(&self) -> SourceHandle {
        todo!("W1-A")
    }

    pub fn is_locked(&self) -> bool {
        todo!("W1-A")
    }

    pub fn bounds_crop(&self) -> bool {
        todo!("W1-A")
    }

    /// `obs_sceneitem_set_info2` (through the slack wrapper).
    pub fn set_info2(&self, info: &TransformInfo) {
        let _ = info;
        todo!("W1-A")
    }
}
