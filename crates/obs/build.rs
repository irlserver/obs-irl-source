//! Test binaries are executables, so unlike the plugin cdylib they cannot
//! carry undefined libobs symbols. Link libobs for `cargo test` only.
//!
//! Two consequences worth knowing before adding a test here:
//!
//! - `rustc-link-arg-tests` reaches *test targets* (`crates/obs/tests/*.rs`),
//!   not the `#[cfg(test)]` modules compiled into the lib's own unittest
//!   binary. Any test that calls a libobs symbol therefore belongs in
//!   `tests/`, which is where the calldata and frame-builder tests live.
//! - The linked libobs is whatever `libobs-dev` provides. On a host older than
//!   the 32.1 floor the plugin targets (Ubuntu 24.04 ships 30.0.2), symbols
//!   added after that host's version — `obs_sceneitem_set_info2` and
//!   `obs_sceneitem_get_bounds_crop` — are simply absent, and a test that
//!   pulls in the codegen unit referencing them will fail to link. That is a
//!   test-only limit: the plugin cdylib links no libobs at all and resolves
//!   everything against the OBS that loads it.
fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match os.as_str() {
        "linux" => println!("cargo::rustc-link-arg-tests=-lobs"),
        "macos" => println!("cargo::rustc-link-arg-tests=-Wl,-undefined,dynamic_lookup"),
        _ => {}
    }
}
