//! Links the transitive static libraries of the bundled media stack.
//!
//! ffmpeg-sys-next (a dependency, so its build script runs first) emits the
//! five libav*/libsw* archives from `$FFMPEG_DIR/lib`. Everything they depend
//! on — libsrt, librist, mbedTLS, zlib on Windows, libva on Linux, the C++
//! runtime, macOS frameworks — is listed in `irl-deps.env`, written by
//! `deps/build-deps.sh` in single-pass link order. This script replays it.
//!
//! Format of `irl-deps.env` (plain `KEY=value`, lists `;`-separated, absolute
//! native paths, no quoting):
//!   IRL_DEPS_HOST, IRL_DEPS_PREFIX, IRL_DEPS_INCLUDE_DIR, IRL_DEPS_LIBDIR,
//!   IRL_DEPS_FFMPEG_VERSION, IRL_DEPS_SRT_VERSION, IRL_DEPS_LIBRIST_VERSION,
//!   IRL_DEPS_MBEDTLS_VERSION,
//!   IRL_DEPS_TRANSITIVE_LIBS   (bare -l names, order matters, libav* omitted)
//!   IRL_DEPS_TRANSITIVE_PATHS  (absolute archive paths for the same list)
//!   IRL_DEPS_SYSTEM_LIBS       (bare names, dylib)
//!   IRL_DEPS_FRAMEWORKS        (macOS only)

use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-env-changed=IRL_DEPS_PREFIX");
    let prefix = std::env::var_os("IRL_DEPS_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("deps/.build/prefix"));
    let env_file = prefix.join("irl-deps.env");
    println!("cargo::rerun-if-changed={}", env_file.display());

    let deps = match std::fs::read_to_string(&env_file) {
        Ok(text) => parse(&text),
        Err(err) => panic!(
            "\n\nBundled media stack not found ({}: {err}).\n\
             Run ./deps/build-deps.sh first, or set IRL_DEPS_PREFIX to a prefix built elsewhere.\n\n",
            env_file.display()
        ),
    };

    let get = |k: &str| deps.get(k).cloned().unwrap_or_default();
    let list = |k: &str| -> Vec<String> {
        get(k)
            .split(';')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };

    println!("cargo::rustc-link-search=native={}", get("IRL_DEPS_LIBDIR"));
    for lib in list("IRL_DEPS_TRANSITIVE_LIBS") {
        println!("cargo::rustc-link-lib=static={lib}");
    }
    for lib in list("IRL_DEPS_SYSTEM_LIBS") {
        println!("cargo::rustc-link-lib=dylib={lib}");
    }
    for fw in list("IRL_DEPS_FRAMEWORKS") {
        println!("cargo::rustc-link-lib=framework={fw}");
    }
    for key in [
        "IRL_DEPS_FFMPEG_VERSION",
        "IRL_DEPS_SRT_VERSION",
        "IRL_DEPS_LIBRIST_VERSION",
        "IRL_DEPS_MBEDTLS_VERSION",
    ] {
        println!("cargo::rustc-env={key}={}", get(key));
    }
    // Downstream crates (the plugin) can read these through DEP_IRL_DEPS_*.
    println!("cargo::metadata=PREFIX={}", prefix.display());
}

fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

fn workspace_root() -> PathBuf {
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("crates/<name>/ layout")
}
