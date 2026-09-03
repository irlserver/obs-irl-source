//! Cross-check of the hand-written declarations against real libobs headers.
//!
//! Enabled by `cargo test -p obs-sys --features layout-test`, which makes
//! `build.rs` run bindgen over `$OBS_INCLUDE_DIR` (default `/usr/include/obs`)
//! and drop the result in `$OUT_DIR/obs_bindgen.rs`. Nothing here ships: the
//! feature exists so a hand-written `#[repr(C)]` struct cannot silently drift
//! from the C declaration it mirrors.
//!
//! What is asserted, per struct: `align_of` equal, `offset_of!` equal for
//! every field, and `size_of` equal — with two documented exceptions.
//!
//! - `obs_source_info` is compared with `<=`. The plugin must never declare
//!   more members than the *oldest* supported OBS has, because
//!   `obs_register_source_s` rejects an info bigger than its own; building
//!   against a newer header than the floor legitimately leaves trailing
//!   members off.
//! - `obs_transform_info` gained `crop_to_bounds` after 30.0, so its size is
//!   only compared when `build.rs` saw a header at 30.1 or newer
//!   (`cfg(libobs_minor_ge_1)`). The offsets of the fields that exist in 30.0
//!   are compared unconditionally, and `crop_to_bounds` is compared under the
//!   cfg.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

mod c {
    include!(concat!(env!("OUT_DIR"), "/obs_bindgen.rs"));
}

use core::mem::{align_of, offset_of, size_of};

/// Assert that `$ours` and `c::$theirs` agree on alignment, on the offset of
/// every listed field, and on size (`==` unless `size: le` is given).
macro_rules! assert_layout {
    ($ours:ident, $theirs:ident, $size_cmp:tt, [$($field:ident),* $(,)?]) => {{
        assert_eq!(
            align_of::<crate::$ours>(),
            align_of::<c::$theirs>(),
            concat!("align_of ", stringify!($ours))
        );
        $(
            assert_eq!(
                offset_of!(crate::$ours, $field),
                offset_of!(c::$theirs, $field),
                concat!("offset_of ", stringify!($ours), ".", stringify!($field))
            );
        )*
        assert_layout!(@size $size_cmp, $ours, $theirs);
    }};
    (@size eq, $ours:ident, $theirs:ident) => {
        assert_eq!(
            size_of::<crate::$ours>(),
            size_of::<c::$theirs>(),
            concat!("size_of ", stringify!($ours))
        );
    };
    (@size le, $ours:ident, $theirs:ident) => {
        assert!(
            size_of::<crate::$ours>() <= size_of::<c::$theirs>(),
            concat!(
                "size_of ",
                stringify!($ours),
                " must not exceed the host's; obs_register_source_s rejects a larger info"
            )
        );
    };
    (@size skip, $ours:ident, $theirs:ident) => {};
}

/// Assert the enum is 4 bytes wide and every variant carries the C value.
macro_rules! assert_enum {
    ($ours:ident, $theirs:ident, [$($variant:ident),* $(,)?]) => {{
        assert_eq!(size_of::<crate::$ours>(), 4, concat!("size_of ", stringify!($ours)));
        assert_eq!(size_of::<c::$theirs>(), 4, concat!("size_of c::", stringify!($theirs)));
        assert_eq!(
            align_of::<crate::$ours>(),
            align_of::<c::$theirs>(),
            concat!("align_of ", stringify!($ours))
        );
        $(
            assert_eq!(
                crate::$ours::$variant as u32,
                c::$theirs::$variant as u32,
                concat!(stringify!($ours), "::", stringify!($variant))
            );
        )*
    }};
}

#[test]
fn obs_source_info_layout() {
    assert_layout!(
        obs_source_info,
        obs_source_info,
        le,
        [
            id,
            type_,
            output_flags,
            get_name,
            create,
            destroy,
            get_width,
            get_height,
            get_defaults,
            get_properties,
            update,
            activate,
            deactivate,
            show,
            hide,
            video_tick,
            video_render,
            filter_video,
            filter_audio,
            enum_active_sources,
            save,
            load,
            mouse_click,
            mouse_move,
            mouse_wheel,
            focus,
            key_click,
            filter_remove,
            type_data,
            free_type_data,
            audio_render,
            enum_all_sources,
            transition_start,
            transition_stop,
            get_defaults2,
            get_properties2,
            audio_mix,
            icon_type,
            media_play_pause,
            media_restart,
            media_stop,
            media_next,
            media_previous,
            media_get_duration,
            media_get_time,
            media_set_time,
            media_get_state,
            version,
            unversioned_id,
            missing_files,
            video_get_color_space,
            filter_add,
        ]
    );
}

#[test]
fn obs_source_frame_layout() {
    assert_layout!(
        obs_source_frame,
        obs_source_frame,
        eq,
        [
            data,
            linesize,
            width,
            height,
            timestamp,
            format,
            color_matrix,
            full_range,
            max_luminance,
            color_range_min,
            color_range_max,
            flip,
            flags,
            trc,
            refs,
            prev_frame,
        ]
    );
}

#[test]
fn obs_source_audio_layout() {
    assert_layout!(
        obs_source_audio,
        obs_source_audio,
        eq,
        [data, frames, speakers, format, samples_per_sec, timestamp]
    );
}

#[test]
fn obs_video_info_layout() {
    assert_layout!(
        obs_video_info,
        obs_video_info,
        eq,
        [
            graphics_module,
            fps_num,
            fps_den,
            base_width,
            base_height,
            output_width,
            output_height,
            output_format,
            adapter,
            gpu_conversion,
            colorspace,
            range,
            scale_type,
        ]
    );

    // The slack wrapper must place the struct first and add room after it, so
    // a libobs that appended a member writes into the padding, not the stack.
    assert_eq!(offset_of!(crate::obs_video_info_slack, ovi), 0);
    assert!(size_of::<crate::obs_video_info_slack>() >= size_of::<c::obs_video_info>() + 64);
}

#[test]
fn obs_transform_info_layout() {
    // Fields present since 30.0: offsets and alignment always compared.
    #[cfg(libobs_minor_ge_1)]
    assert_layout!(
        obs_transform_info,
        obs_transform_info,
        eq,
        [
            pos,
            rot,
            scale,
            alignment,
            bounds_type,
            bounds_alignment,
            bounds,
            crop_to_bounds,
        ]
    );
    // Header predates `crop_to_bounds`: everything before it must still line
    // up, but our struct is legitimately larger.
    #[cfg(not(libobs_minor_ge_1))]
    assert_layout!(
        obs_transform_info,
        obs_transform_info,
        skip,
        [
            pos,
            rot,
            scale,
            alignment,
            bounds_type,
            bounds_alignment,
            bounds,
        ]
    );
    #[cfg(not(libobs_minor_ge_1))]
    assert!(
        size_of::<crate::obs_transform_info>() >= size_of::<c::obs_transform_info>(),
        "crop_to_bounds is appended, so our struct can only grow"
    );

    assert_eq!(offset_of!(crate::obs_transform_info_slack, info), 0);
    assert!(
        size_of::<crate::obs_transform_info_slack>() >= size_of::<crate::obs_transform_info>() + 64
    );
}

#[test]
fn vec2_layout() {
    // `struct vec2` is an anonymous union of `{float x, y}` and `float[2]`,
    // which bindgen renders as nested anonymous types; there are no named
    // fields to compare offsets against, and size + align pin it completely.
    assert_eq!(size_of::<crate::vec2>(), size_of::<c::vec2>());
    assert_eq!(align_of::<crate::vec2>(), align_of::<c::vec2>());
    assert_eq!(size_of::<crate::vec2>(), 2 * size_of::<f32>());
}

#[test]
fn calldata_layout() {
    assert_layout!(calldata_t, calldata, eq, [stack, size, capacity, fixed]);
}

#[test]
fn enums_match() {
    assert_enum!(
        obs_source_type,
        obs_source_type,
        [
            OBS_SOURCE_TYPE_INPUT,
            OBS_SOURCE_TYPE_FILTER,
            OBS_SOURCE_TYPE_TRANSITION,
            OBS_SOURCE_TYPE_SCENE,
        ]
    );
    assert_enum!(
        obs_icon_type,
        obs_icon_type,
        [
            OBS_ICON_TYPE_UNKNOWN,
            OBS_ICON_TYPE_IMAGE,
            OBS_ICON_TYPE_COLOR,
            OBS_ICON_TYPE_SLIDESHOW,
            OBS_ICON_TYPE_AUDIO_INPUT,
            OBS_ICON_TYPE_AUDIO_OUTPUT,
            OBS_ICON_TYPE_DESKTOP_CAPTURE,
            OBS_ICON_TYPE_WINDOW_CAPTURE,
            OBS_ICON_TYPE_GAME_CAPTURE,
            OBS_ICON_TYPE_CAMERA,
            OBS_ICON_TYPE_TEXT,
            OBS_ICON_TYPE_MEDIA,
            OBS_ICON_TYPE_BROWSER,
            OBS_ICON_TYPE_CUSTOM,
            OBS_ICON_TYPE_PROCESS_AUDIO_OUTPUT,
        ]
    );
    assert_enum!(
        obs_media_state,
        obs_media_state,
        [
            OBS_MEDIA_STATE_NONE,
            OBS_MEDIA_STATE_PLAYING,
            OBS_MEDIA_STATE_OPENING,
            OBS_MEDIA_STATE_BUFFERING,
            OBS_MEDIA_STATE_PAUSED,
            OBS_MEDIA_STATE_STOPPED,
            OBS_MEDIA_STATE_ENDED,
            OBS_MEDIA_STATE_ERROR,
        ]
    );
    assert_enum!(
        obs_bounds_type,
        obs_bounds_type,
        [
            OBS_BOUNDS_NONE,
            OBS_BOUNDS_STRETCH,
            OBS_BOUNDS_SCALE_INNER,
            OBS_BOUNDS_SCALE_OUTER,
            OBS_BOUNDS_SCALE_TO_WIDTH,
            OBS_BOUNDS_SCALE_TO_HEIGHT,
            OBS_BOUNDS_MAX_ONLY,
        ]
    );
    assert_enum!(
        obs_scale_type,
        obs_scale_type,
        [
            OBS_SCALE_DISABLE,
            OBS_SCALE_POINT,
            OBS_SCALE_BICUBIC,
            OBS_SCALE_BILINEAR,
            OBS_SCALE_LANCZOS,
            OBS_SCALE_AREA,
        ]
    );
    assert_enum!(
        obs_text_type,
        obs_text_type,
        [
            OBS_TEXT_DEFAULT,
            OBS_TEXT_PASSWORD,
            OBS_TEXT_MULTILINE,
            OBS_TEXT_INFO,
        ]
    );
    assert_enum!(
        obs_combo_type,
        obs_combo_type,
        [
            OBS_COMBO_TYPE_INVALID,
            OBS_COMBO_TYPE_EDITABLE,
            OBS_COMBO_TYPE_LIST,
            OBS_COMBO_TYPE_RADIO,
        ]
    );
    assert_enum!(
        obs_combo_format,
        obs_combo_format,
        [
            OBS_COMBO_FORMAT_INVALID,
            OBS_COMBO_FORMAT_INT,
            OBS_COMBO_FORMAT_FLOAT,
            OBS_COMBO_FORMAT_STRING,
            OBS_COMBO_FORMAT_BOOL,
        ]
    );
    assert_enum!(
        gs_color_space,
        gs_color_space,
        [
            GS_CS_SRGB,
            GS_CS_SRGB_16F,
            GS_CS_709_EXTENDED,
            GS_CS_709_SCRGB,
        ]
    );
    assert_enum!(
        video_format,
        video_format,
        [
            VIDEO_FORMAT_NONE,
            VIDEO_FORMAT_I420,
            VIDEO_FORMAT_NV12,
            VIDEO_FORMAT_YVYU,
            VIDEO_FORMAT_YUY2,
            VIDEO_FORMAT_UYVY,
            VIDEO_FORMAT_RGBA,
            VIDEO_FORMAT_BGRA,
            VIDEO_FORMAT_BGRX,
            VIDEO_FORMAT_Y800,
            VIDEO_FORMAT_I444,
            VIDEO_FORMAT_BGR3,
            VIDEO_FORMAT_I422,
            VIDEO_FORMAT_I40A,
            VIDEO_FORMAT_I42A,
            VIDEO_FORMAT_YUVA,
            VIDEO_FORMAT_AYUV,
            VIDEO_FORMAT_I010,
            VIDEO_FORMAT_P010,
            VIDEO_FORMAT_I210,
            VIDEO_FORMAT_I412,
            VIDEO_FORMAT_YA2L,
            VIDEO_FORMAT_P216,
            VIDEO_FORMAT_P416,
            VIDEO_FORMAT_V210,
            VIDEO_FORMAT_R10L,
        ]
    );
    assert_enum!(
        video_colorspace,
        video_colorspace,
        [
            VIDEO_CS_DEFAULT,
            VIDEO_CS_601,
            VIDEO_CS_709,
            VIDEO_CS_SRGB,
            VIDEO_CS_2100_PQ,
            VIDEO_CS_2100_HLG,
        ]
    );
    assert_enum!(
        video_range_type,
        video_range_type,
        [VIDEO_RANGE_DEFAULT, VIDEO_RANGE_PARTIAL, VIDEO_RANGE_FULL]
    );
    assert_enum!(
        video_trc,
        video_trc,
        [
            VIDEO_TRC_DEFAULT,
            VIDEO_TRC_SRGB,
            VIDEO_TRC_PQ,
            VIDEO_TRC_HLG,
        ]
    );
    assert_enum!(
        speaker_layout,
        speaker_layout,
        [
            SPEAKERS_UNKNOWN,
            SPEAKERS_MONO,
            SPEAKERS_STEREO,
            SPEAKERS_2POINT1,
            SPEAKERS_4POINT0,
            SPEAKERS_4POINT1,
            SPEAKERS_5POINT1,
            SPEAKERS_7POINT1,
        ]
    );
    assert_enum!(
        audio_format,
        audio_format,
        [
            AUDIO_FORMAT_UNKNOWN,
            AUDIO_FORMAT_U8BIT,
            AUDIO_FORMAT_16BIT,
            AUDIO_FORMAT_32BIT,
            AUDIO_FORMAT_FLOAT,
            AUDIO_FORMAT_U8BIT_PLANAR,
            AUDIO_FORMAT_16BIT_PLANAR,
            AUDIO_FORMAT_32BIT_PLANAR,
            AUDIO_FORMAT_FLOAT_PLANAR,
        ]
    );
}

/// `MAX_AV_PLANES` and the semantic-version packing, which no struct pins.
#[test]
fn constants_match() {
    assert_eq!(crate::MAX_AV_PLANES, 8);
    // MAKE_SEMANTIC_VERSION(30, 0, 2) == 0x1E000002.
    assert_eq!(crate::make_semantic_version(30, 0, 2), 0x1E00_0002);
}
