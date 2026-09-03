//! Integration-test target (libobs is linked for test binaries only; see
//! build.rs). Wave-2 agents add their tests as further files in this dir.
#[test]
fn crate_links() {
    assert!(!obs_irl_source::PLUGIN_VERSION.is_empty());
}
