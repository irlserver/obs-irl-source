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
            println!("cargo::rustc-link-arg-tests=-lobs");
        }
        // Windows: raw-dylib needs no import library, and a cdylib exports
        // only its #[no_mangle] items.
        _ => {}
    }
    println!("cargo::rustc-env=IRL_PLUGIN_VERSION={}", std::env::var("CARGO_PKG_VERSION").unwrap());
}
