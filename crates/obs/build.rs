//! Test binaries are executables, so unlike the plugin cdylib they cannot
//! carry undefined libobs symbols. Link libobs for `cargo test` only.
fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match os.as_str() {
        "linux" => println!("cargo::rustc-link-arg-tests=-lobs"),
        "macos" => println!("cargo::rustc-link-arg-tests=-Wl,-undefined,dynamic_lookup"),
        _ => {}
    }
}
