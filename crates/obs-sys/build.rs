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
    pub fn generate() {
        todo!("W1-A: bindgen obs.h/obs-module.h/callback headers into OUT_DIR/obs_bindgen.rs and emit libobs_minor_ge_1 from obs-config.h");
    }
}
