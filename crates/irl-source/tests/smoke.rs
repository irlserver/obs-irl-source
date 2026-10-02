//! Integration-test target (libobs is linked for test binaries only; see
//! build.rs).
#[test]
fn crate_links() {
    assert!(!obs_irl_source::PLUGIN_VERSION.is_empty());
}
