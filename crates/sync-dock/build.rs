//! Compiles the Qt dock (`cxx/sync-dock.cpp`) when Qt6 Widgets can be found,
//! and builds the crate as a stub otherwise — the same bargain the C plugin's
//! CMake made (`IRL_ENABLE_DOCK`, silently off without Qt): sync itself runs
//! headless and its status stays readable over the obs-websocket vendor.
//!
//! Where Qt comes from, in order:
//!
//! 1. `IRL_QT_PREFIX`: the root of a Qt install (obs-deps' `*-deps-qt6-*`
//!    archives on Windows and macOS, or any other layout with `include/` and
//!    `lib/`). On macOS the frameworks under `lib/` are used for headers only;
//!    the plugin already links with `-undefined dynamic_lookup`, so Qt's
//!    symbols resolve against the Qt the OBS process loaded, exactly as
//!    libobs's do. On Windows the import libraries under `lib/` are linked.
//! 2. `pkg-config Qt6Widgets` (Linux distros, Homebrew with PKG_CONFIG_PATH).
//!
//! `IRL_DOCK=0` skips the dock deliberately, whatever is installed.
//!
//! The dock adds `libstdc++` / the Qt6 libraries to the plugin's dependencies
//! on Linux. OBS's own frontend is C++ and Qt, so they are already loaded in
//! every process that can host a dock.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Qt {
    include_dirs: Vec<PathBuf>,
    /// Extra compiler flags (`-F<dir>` on macOS).
    cflags: Vec<String>,
    /// `cargo::rustc-link-*` lines to emit.
    link: Vec<String>,
}

fn env_flag_off(name: &str) -> bool {
    matches!(
        env::var(name).as_deref().map(str::trim),
        Ok("0") | Ok("off") | Ok("OFF") | Ok("no") | Ok("false")
    )
}

/// Qt from an explicit prefix.
fn qt_from_prefix(prefix: &Path, target_os: &str) -> Option<Qt> {
    let include = prefix.join("include");
    let lib = prefix.join("lib");
    let modules = ["QtCore", "QtGui", "QtWidgets"];

    if target_os == "macos" && lib.join("QtWidgets.framework").is_dir() {
        // Framework layout: headers live inside each framework, and
        // `#include <QtCore/qglobal.h>` inside Qt's own headers wants -F.
        let mut include_dirs: Vec<PathBuf> = modules
            .iter()
            .map(|m| lib.join(format!("{m}.framework")).join("Headers"))
            .collect();
        if include.is_dir() {
            include_dirs.push(include);
        }
        return Some(Qt {
            include_dirs,
            cflags: vec![format!("-F{}", lib.display())],
            link: Vec::new(),
        });
    }

    if !include.join("QtWidgets").is_dir() {
        return None;
    }
    let mut include_dirs = vec![include.clone()];
    include_dirs.extend(modules.iter().map(|m| include.join(m)));

    let link = if target_os == "windows" {
        let mut lines = vec![format!("cargo::rustc-link-search=native={}", lib.display())];
        for name in ["Qt6Widgets", "Qt6Gui", "Qt6Core"] {
            lines.push(format!("cargo::rustc-link-lib={name}"));
        }
        lines
    } else if target_os == "macos" {
        // Dynamic lookup against the OBS process; see the module comment.
        Vec::new()
    } else {
        let mut lines = vec![format!("cargo::rustc-link-search=native={}", lib.display())];
        for name in ["Qt6Widgets", "Qt6Gui", "Qt6Core"] {
            lines.push(format!("cargo::rustc-link-lib={name}"));
        }
        lines
    };

    Some(Qt {
        include_dirs,
        cflags: Vec::new(),
        link,
    })
}

/// Qt from pkg-config.
fn qt_from_pkg_config(target_os: &str) -> Option<Qt> {
    let pkg_config = env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".to_owned());
    let out = Command::new(&pkg_config)
        .args(["--cflags", "--libs", "Qt6Widgets"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);

    let mut qt = Qt {
        include_dirs: Vec::new(),
        cflags: Vec::new(),
        link: Vec::new(),
    };
    let mut words = text.split_whitespace().peekable();
    while let Some(word) = words.next() {
        if let Some(dir) = word.strip_prefix("-I") {
            qt.include_dirs.push(PathBuf::from(dir));
        } else if let Some(dir) = word.strip_prefix("-L") {
            qt.link
                .push(format!("cargo::rustc-link-search=native={dir}"));
        } else if let Some(lib) = word.strip_prefix("-l") {
            if target_os != "macos" {
                qt.link.push(format!("cargo::rustc-link-lib={lib}"));
            }
        } else if let Some(dir) = word.strip_prefix("-F") {
            qt.cflags.push(format!("-F{dir}"));
        } else if word == "-framework" {
            // Headers only on macOS (dynamic lookup); consume the name.
            words.next();
        } else if word.starts_with("-D") {
            qt.cflags.push(word.to_owned());
        }
    }
    if qt.include_dirs.is_empty() {
        return None;
    }
    Some(qt)
}

fn main() {
    println!("cargo::rustc-check-cfg=cfg(irl_dock)");
    println!("cargo::rerun-if-env-changed=IRL_DOCK");
    println!("cargo::rerun-if-env-changed=IRL_QT_PREFIX");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo::rerun-if-changed=cxx/sync-dock.cpp");
    println!("cargo::rerun-if-changed=cxx/irl-sync-dock.h");
    println!("cargo::rerun-if-changed=build.rs");

    if env_flag_off("IRL_DOCK") {
        println!("cargo::warning=IRL_DOCK is off; building without the IRL Sync dock");
        return;
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let qt = env::var_os("IRL_QT_PREFIX")
        .map(PathBuf::from)
        .and_then(|prefix| qt_from_prefix(&prefix, &target_os))
        .or_else(|| qt_from_pkg_config(&target_os));

    let Some(qt) = qt else {
        println!(
            "cargo::warning=Qt6 Widgets not found; building without the IRL Sync dock \
             (install qt6-base-dev, or point IRL_QT_PREFIX at a Qt install). \
             Timecode sync still works and is readable over obs-websocket."
        );
        return;
    };

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .file("cxx/sync-dock.cpp")
        .include("cxx")
        .warnings(false);
    for dir in &qt.include_dirs {
        build.include(dir);
    }
    for flag in &qt.cflags {
        build.flag(flag);
    }
    if build.get_compiler().is_like_msvc() {
        // Qt6 headers require the real __cplusplus value and standards
        // conformance; exceptions are Qt's default too.
        build
            .flag("/Zc:__cplusplus")
            .flag("/permissive-")
            .flag("/EHsc");
    } else {
        // Qt is built with -reduce-relocations and insists on PIC users;
        // hidden visibility keeps the dock's own symbols out of the plugin's
        // export table (rustc's version script hides them anyway).
        build
            .flag("-fPIC")
            .flag("-fvisibility=hidden")
            .flag("-fvisibility-inlines-hidden");
    }
    build.compile("irl_sync_dock");

    for line in &qt.link {
        println!("{line}");
    }
    if target_os == "windows" {
        // GetModuleHandleA / GetProcAddress.
        println!("cargo::rustc-link-lib=kernel32");
    }
    println!("cargo::rustc-cfg=irl_dock");
}
