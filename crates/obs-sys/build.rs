//! No-op unless the `layout-test` feature is on. Then it runs bindgen against
//! the installed libobs headers and writes `$OUT_DIR/obs_bindgen.rs`, which
//! `src/layout_test.rs` compares field by field against the hand-written
//! declarations. It also emits `cargo::rustc-cfg=libobs_minor_ge_1` so the one
//! struct that grew after 30.0 (`obs_transform_info`, `crop_to_bounds`) can
//! gate its exact-size assertion on the header actually present.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(libobs_minor_ge_1)");
    println!("cargo::rerun-if-env-changed=OBS_INCLUDE_DIR");
    #[cfg(feature = "layout-test")]
    layout_test::generate();
}

#[cfg(feature = "layout-test")]
mod layout_test {
    use std::path::{Path, PathBuf};

    /// The types the hand-written declarations mirror. bindgen is pointed at
    /// these and nothing else: an unrestricted run over `obs.h` drags in the
    /// whole util tree (threading, profiler, uthash) and produces thousands of
    /// items that add nothing to a layout comparison.
    const TYPES: &[&str] = &[
        "obs_source_info",
        "obs_source_frame",
        "obs_source_audio",
        "obs_video_info",
        "obs_transform_info",
        "vec2",
        "calldata",
        "obs_source_type",
        "obs_icon_type",
        "obs_media_state",
        "obs_bounds_type",
        "obs_scale_type",
        "obs_text_type",
        "obs_combo_type",
        "obs_combo_format",
        "gs_color_space",
        "video_format",
        "video_colorspace",
        "video_range_type",
        "video_trc",
        "speaker_layout",
        "audio_format",
    ];

    pub fn generate() {
        let include_dir = PathBuf::from(
            std::env::var("OBS_INCLUDE_DIR").unwrap_or_else(|_| "/usr/include/obs".to_string()),
        );
        let obs_h = include_dir.join("obs.h");
        assert!(
            obs_h.is_file(),
            "libobs headers not found at {}; install libobs-dev or set OBS_INCLUDE_DIR",
            include_dir.display()
        );

        println!("cargo::rerun-if-changed={}", obs_h.display());

        emit_version_cfg(&include_dir);

        let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

        // A minimal translation unit: obs.h already pulls in the media-io,
        // callback and graphics declarations every mirrored type needs, and
        // none of the util threading/profiler headers obs-module.h adds.
        let wrapper = out_dir.join("obs_layout_wrapper.h");
        std::fs::write(
            &wrapper,
            "#include <obs.h>\n\
             #include <callback/calldata.h>\n\
             #include <callback/proc.h>\n\
             #include <media-io/video-io.h>\n",
        )
        .expect("write bindgen wrapper header");

        let mut builder = bindgen::Builder::default()
            .header(wrapper.to_string_lossy())
            .clang_arg(format!("-I{}", include_dir.display()))
            .clang_arg("-DHAVE_OBSCONFIG_H")
            // The comparison is spelled out by hand in layout_test.rs;
            // bindgen's own generated tests would only duplicate it.
            .layout_tests(false)
            .derive_default(false)
            .generate_comments(false)
            .prepend_enum_name(false)
            .default_enum_style(bindgen::EnumVariation::Rust {
                non_exhaustive: false,
            });

        for t in TYPES {
            builder = builder.allowlist_type(format!("^{t}$"));
        }

        let bindings = builder
            .generate()
            .expect("bindgen failed on the libobs headers");
        bindings
            .write_to_file(out_dir.join("obs_bindgen.rs"))
            .expect("write obs_bindgen.rs");
    }

    /// `LIBOBS_API_MAJOR_VER` > 30, or == 30 with `LIBOBS_API_MINOR_VER` >= 1:
    /// the first libobs that declares `obs_transform_info::crop_to_bounds`.
    fn emit_version_cfg(include_dir: &Path) {
        let config = include_dir.join("obs-config.h");
        println!("cargo::rerun-if-changed={}", config.display());
        let Ok(text) = std::fs::read_to_string(&config) else {
            return;
        };

        let define = |name: &str| -> Option<u32> {
            text.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("#define ")?
                    .strip_prefix(name)?
                    .trim()
                    .parse()
                    .ok()
            })
        };

        let (Some(major), Some(minor)) = (
            define("LIBOBS_API_MAJOR_VER"),
            define("LIBOBS_API_MINOR_VER"),
        ) else {
            return;
        };
        if major > 30 || (major == 30 && minor >= 1) {
            println!("cargo::rustc-cfg=libobs_minor_ge_1");
        }
    }
}
