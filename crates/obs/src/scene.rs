//! Scene enumeration and scene-item transforms (for fit-to-canvas).

use core::ffi::c_void;
use core::ptr::NonNull;

use crate::panic::guard;
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

impl BoundsType {
    fn to_sys(self) -> obs_sys::obs_bounds_type {
        use obs_sys::obs_bounds_type as B;
        match self {
            Self::None => B::OBS_BOUNDS_NONE,
            Self::Stretch => B::OBS_BOUNDS_STRETCH,
            Self::ScaleInner => B::OBS_BOUNDS_SCALE_INNER,
            Self::ScaleOuter => B::OBS_BOUNDS_SCALE_OUTER,
            Self::ScaleToWidth => B::OBS_BOUNDS_SCALE_TO_WIDTH,
            Self::ScaleToHeight => B::OBS_BOUNDS_SCALE_TO_HEIGHT,
            Self::MaxOnly => B::OBS_BOUNDS_MAX_ONLY,
        }
    }
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

impl TransformInfo {
    fn to_sys(self) -> obs_sys::obs_transform_info {
        obs_sys::obs_transform_info {
            pos: obs_sys::vec2 {
                x: self.pos.0,
                y: self.pos.1,
            },
            rot: self.rot,
            scale: obs_sys::vec2 {
                x: self.scale.0,
                y: self.scale.1,
            },
            alignment: self.alignment,
            bounds_type: self.bounds_type.to_sys(),
            bounds_alignment: self.bounds_alignment,
            bounds: obs_sys::vec2 {
                x: self.bounds.0,
                y: self.bounds.1,
            },
            crop_to_bounds: self.crop_to_bounds,
        }
    }
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
#[must_use]
pub fn get_video_info() -> Option<VideoInfo> {
    // libobs fills the struct by *its* sizeof, so the call gets a wrapper with
    // 64 trailing bytes: a newer OBS that appended a member writes into the
    // padding instead of past our stack frame.
    // SAFETY: every member is a plain integer, pointer or C enum, so all-zero
    // is a valid value to start from; libobs overwrites what it knows.
    let mut slack: obs_sys::obs_video_info_slack = unsafe { core::mem::zeroed() };
    // SAFETY: `&mut slack.ovi` is a writable obs_video_info with room after it.
    let ok = unsafe { obs_sys::obs_get_video_info(&raw mut slack.ovi) };
    if !ok {
        // False means OBS video is not running (no canvas), which the
        // fit-to-canvas path treats as "try again next tick".
        return None;
    }
    let ovi = slack.ovi;
    Some(VideoInfo {
        base_width: ovi.base_width,
        base_height: ovi.base_height,
        output_width: ovi.output_width,
        output_height: ovi.output_height,
        fps_num: ovi.fps_num,
        fps_den: ovi.fps_den,
    })
}

/// `obs_enum_scenes`. Return `false` from `f` to stop.
pub fn enum_scenes(f: &mut dyn FnMut(SourceHandle) -> bool) {
    crate::source::enum_scenes_inner(f);
}

#[derive(Debug, Clone, Copy)]
pub struct Scene(NonNull<obs_sys::obs_scene_t>);

impl Scene {
    /// `obs_scene_from_source`.
    #[must_use]
    pub fn from_source(source: SourceHandle) -> Option<Scene> {
        // SAFETY: live handle; libobs returns NULL for a non-scene source and
        // otherwise a borrowed pointer owned by that source.
        let ptr = unsafe { obs_sys::obs_scene_from_source(source.as_ptr()) };
        NonNull::new(ptr).map(Scene)
    }

    /// `obs_scene_enum_items`. Return `false` from `f` to stop.
    pub fn enum_items(&self, f: &mut dyn FnMut(SceneItem) -> bool) {
        let mut f: ItemEnumFn<'_> = f;
        // SAFETY: the trampoline reads `&mut f` only during this call, and
        // libobs holds the scene's item list locked for its duration.
        unsafe {
            obs_sys::obs_scene_enum_items(
                self.0.as_ptr(),
                Some(enum_items_trampoline),
                (&raw mut f).cast::<c_void>(),
            );
        }
    }
}

type ItemEnumFn<'a> = &'a mut dyn FnMut(SceneItem) -> bool;

/// # Safety
/// `param` must be a `*mut ItemEnumFn` valid for the enumeration.
unsafe extern "C" fn enum_items_trampoline(
    _scene: *mut obs_sys::obs_scene_t,
    item: *mut obs_sys::obs_sceneitem_t,
    param: *mut c_void,
) -> bool {
    // As for source enumeration: a panic must not stop libobs mid-walk.
    guard("scene item enumeration", true, || {
        let (Some(item), false) = (NonNull::new(item), param.is_null()) else {
            return true;
        };
        // SAFETY: `param` is the &mut closure from `enum_items`, called
        // synchronously and one at a time.
        let f = unsafe { &mut *param.cast::<ItemEnumFn<'_>>() };
        f(SceneItem(item))
    })
}

#[derive(Debug, Clone, Copy)]
pub struct SceneItem(NonNull<obs_sys::obs_sceneitem_t>);

impl SceneItem {
    #[must_use]
    pub fn source(&self) -> SourceHandle {
        // SAFETY: live item; the source it references outlives the item.
        let ptr = unsafe { obs_sys::obs_sceneitem_get_source(self.0.as_ptr()) };
        let ptr = NonNull::new(ptr).expect("obs_sceneitem_get_source returned NULL");
        // SAFETY: borrowed, non-owning; valid while the item is.
        unsafe { SourceHandle::from_raw(ptr) }
    }

    #[must_use]
    pub fn is_locked(&self) -> bool {
        // SAFETY: live item.
        unsafe { obs_sys::obs_sceneitem_locked(self.0.as_ptr()) }
    }

    #[must_use]
    pub fn bounds_crop(&self) -> bool {
        // SAFETY: live item.
        unsafe { obs_sys::obs_sceneitem_get_bounds_crop(self.0.as_ptr()) }
    }

    /// `obs_sceneitem_set_info2` (through the slack wrapper).
    pub fn set_info2(&self, info: &TransformInfo) {
        // As with obs_get_video_info: libobs reads the struct by *its* sizeof,
        // so give it 64 bytes of readable padding behind ours.
        let slack = obs_sys::obs_transform_info_slack {
            info: info.to_sys(),
            _slack: [0u8; 64],
        };
        // SAFETY: live item; libobs reads the transform during the call.
        unsafe { obs_sys::obs_sceneitem_set_info2(self.0.as_ptr(), &raw const slack.info) };
    }
}
