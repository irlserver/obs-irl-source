//! Link arguments that apply to the plugin cdylib only.
fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match os.as_str() {
        // libobs symbols resolve against the host process at load time.
        "macos" => {
            println!("cargo::rustc-cdylib-link-arg=-Wl,-undefined,dynamic_lookup");
            println!("cargo::rustc-link-arg-tests=-Wl,-undefined,dynamic_lookup");
        }
        // rustc already restricts a cdylib's exports to #[no_mangle] items via
        // its own version script; --exclude-libs is belt and braces so no
        // static libav*/libsrt symbol can ever shadow the host OBS's copy.
        "linux" => {
            println!("cargo::rustc-cdylib-link-arg=-Wl,--exclude-libs,ALL");
            // Test binaries link the installed libobs. A libobs older than the
            // 32.1 floor lacks a few symbols the plugin declares (e.g.
            // obs_sceneitem_set_info2 in 30.0); tests never call them, so
            // unresolved symbols are tolerated for test targets only.
            println!("cargo::rustc-link-arg-tests=-lobs");
            println!("cargo::rustc-link-arg-tests=-Wl,--unresolved-symbols=ignore-all");
        }
        // Windows: raw-dylib needs no import library, and a cdylib exports
        // only its #[no_mangle] items.
        _ => {}
    }
    println!("cargo::rustc-env=IRL_PLUGIN_VERSION={}", std::env::var("CARGO_PKG_VERSION").unwrap());
}
