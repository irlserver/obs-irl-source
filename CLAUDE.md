# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

IRL Source is a third-party OBS Studio plugin (Rust 2024, AGPL-3.0) that receives live IRL streams over SRT, RTMP, RIST or any FFmpeg-supported protocol. It handles what IRL feeds need: audio jitter buffering, PTS repair, adaptive playback speed, keyframe gating, hardware decoding, mid-stream resolution changes and lip sync for senders whose video trails their audio.

Read `docs/architecture.md` before changing threads, timing or A/V sync. Read `docs/audio-timing-pitfalls.md` before touching `crates/irl-core/src/speed.rs`.

## Build

cargo drives everything. Two prerequisites:

1. **The bundled media stack.** The plugin statically links its own FFmpeg, libsrt, librist and mbedTLS (`deps/README.md`). Run `./deps/build-deps.sh` once per version bump. It writes `deps/.build/prefix/irl-deps.env`, which `crates/ffmpeg/build.rs` replays as link lines. On Windows it runs inside MSYS2 with the MSVC environment active; see the `windows-x64` job in `.github/workflows/build.yml`.
2. **libclang**, for bindgen in `ffmpeg-sys-next`. Set `LIBCLANG_PATH` on Windows and macOS.

```bash
# Linux
sudo apt install build-essential cmake pkg-config nasm meson ninja-build \
    clang libclang-dev libobs-dev libva-dev
./deps/build-deps.sh
cargo build --release
./scripts/verify-plugin.sh target/release/libobs_irl_source.so
```

libobs is never linked; `libobs-dev` is only needed to run tests. `.cargo/config.toml` sets `FFMPEG_DIR` to `deps/.build/prefix`; override it and `IRL_DEPS_PREFIX` to build against another prefix. `rust-toolchain.toml` pins stable; the plugin must never need nightly.

`make` runs every gate with an explicit config out of `.config/`, so a machine's global settings cannot change the result:

```bash
make check        # style-check + lint + test + spell-check + tls-provider, what CI runs
make style        # the only target that rewrites files; plain `cargo fmt` picks the wrong width
make test-shim    # libobs-dependent tests on macOS, against scripts/libobs-shim/
make sim          # speed controller closed-loop simulation; run it whenever you touch speed.rs
cargo test -p obs-sys --features layout-test       # struct layouts vs the real libobs headers
cargo build --release -p irl-source --features deadlocks   # use whenever you touch threading
scripts/package.sh linux target/release dist       # the release archive, locally
```

`scripts/verify-plugin.sh` asserts what a compile does not prove: no `libav*` dependency, nothing exported but `obs_module_*`, undefined symbols from libobs and libc only, and `#![forbid(unsafe_code)]` still on `irl-core` and `irl-source`. CI runs it on every build.

## Layout

- `crates/obs-sys`: hand-written libobs FFI, checked by `layout-test`.
- `crates/obs`: the safe libobs API this plugin uses.
- `crates/ffmpeg` (package `irl-ffmpeg`): RAII over `ffmpeg-sys-next`.
- `crates/irl-core`: everything pure. Jitter buffer, PTS repair, speed controller, output clock, pacing, video delay, audio hold, stats table, tuning constants.
- `crates/irl-provider`: the plugin side of `docs/provider-protocol.md`.
- `crates/irl-source`: the plugin. `source.rs` (lifecycle), `settings.rs` (dialog), `providers.rs`, `config.rs`, `shared.rs` (per-run shared state), `receiver/`, `video/`, `audio/`, `websocket.rs`.

## Rules

- **Unsafe** lives only in `obs-sys`, `obs` and `ffmpeg`, and every `unsafe` block carries a `// SAFETY:` comment. If plugin code needs a raw pointer, add a safe wrapper in one of those crates.
- **Lock order** is `audio_state` → `audio_buf` → `hot.watermarks`. `video.q` is never held together with any of them. The audio pump takes `audio_state` once per iteration and passes `&mut AudioState` down; parking_lot mutexes are not recursive, so a nested acquire deadlocks.
- **Clocks.** OBS timestamps come from `obs::time::gettime_ns`, never `std::time::Instant`. FFmpeg-side timers stay in the `av_gettime` microsecond domain. `irl-core` takes both as parameters so they cannot be mixed.
- **Audio timestamps** submitted to OBS stay contiguous and the submitted sample rate never changes. Speed is applied inside the plugin. See `docs/architecture.md`.
- **Panics** never cross an FFI boundary: `obs::panic::guard` wraps every `extern "C"` shim and `shared::spawn_worker` wraps every worker thread.
- **Logging** goes through `irl_info!` / `irl_warn!` / `irl_error!` / `irl_debug!`, never `blog` directly.
- **URLs** never reach the log whole. Plugin lines go through `log::redacted_input_url`, FFmpeg's through `log::redacted_log_line`, because FFmpeg prints the user's URL (with `passphrase=` and `streamid=`) in its own errors.
- **UI strings** never pass English to `module_text`. A new string goes in the call site and `data/locale/en-US.ini`; `tests/locale_keys.rs` checks both directions.
- **Stats** are one table: a new stat is one line in `irl_core::stats::FIELDS` plus its `StatsSnapshot` field and `values()`. The proc, the calldata writer and the websocket loop walk the table. Update the README table too. A stat stays only if it diagnoses real issues.
- **Tuning values** live in `irl_core::consts`, pinned by `consts_are_pinned`. Nothing else hardcodes a threshold.
- **Frozen interface:** settings keys and defaults, the names of the stats, and the media-control behavior that NOALBS's `!fix` relies on. Log text is free to change.
- **Source flags:** `OBS_SOURCE_AUDIO | OBS_SOURCE_ASYNC_VIDEO | OBS_SOURCE_DO_NOT_DUPLICATE | OBS_SOURCE_CONTROLLABLE_MEDIA`.

## Tests

- `irl-core` and `ffmpeg` have unit tests in each module. Everywhere else, tests live in `tests/`. The link arguments that resolve libobs only reach integration-test targets, so `crates/irl-source` sets `test = false` on its lib and `crates/obs` keeps its lib free of `#[cfg(test)]`. Shared fixtures for irl-source live in `crates/irl-source/tests/common/`.
- Tests that touch libobs run on Linux. CI skips them on Windows and macOS. `calldata_*` is safe anywhere. On a Mac, `make test-shim` runs the pacing, audio-core and network tests against a four-function stand-in; plain `cargo test -p irl-source` there dies with SIGSEGV before its first assertion.
- `tests/network_sim.rs` drives the real jitter buffer, PTS repair, speed controller, output clock and packet queue against a synthetic sender on a virtual clock (stall, burst, dropouts, a sender off wall clock, video behind audio). It asserts the design's promises: the OBS clock never jumps except across a declared restart, audio is never skipped once primed, latency does not ratchet, decoded memory does not grow with Target Buffer.
- Where you read the buffer fill decides the number. It oscillates by one chunk within every cycle: before the pump's read it averages the target, after it a chunk lower. The stats line's `buf=` is a random sample of that oscillation.
- `tests/video_pipeline.rs` reads the real clock and can flake under parallel load; rerun with `-- --test-threads=1` before suspecting the code.
- Real-stream validation is manual: run the same feed through this build and a known-good one and compare the 30-second stats line field by field.

## CI and releases

`.github/workflows/build.yml` builds Linux x64 (Ubuntu 22.04, glibc 2.35, so the artifact loads in the Flatpak sandbox), Windows x64 and macOS ARM64. Each job builds the media stack (cached on `deps/versions.env` plus `deps/build-deps.sh`), then build, clippy, tests and the isolation checks. The Windows job also compiles the Inno Setup installer on every push. The Linux job installs OBS from `ppa:obsproject/obs-studio` only so test binaries have a `libobs.so`.

`api_version` in `declare_module!` is the oldest supported OBS line. libobs gates plugins on major and minor only, so one binary loads there and on every newer release. Raise it only to drop old OBS releases.

Releases are tag driven (`RELEASING.md`). Pushing `vX.Y.Z` checks the tag against `[workspace.package] version`, builds, packages, and creates a draft release whose notes come from `scripts/changelog.sh`, grouped by conventional commit type. Commit subjects are the release notes, so write them as such.

## Shipped files

- `data/locale/en-US.ini` must ship: the lookup falls back to the key, so a package without it shows bare identifiers in the dialog.
- `data/providers.json` is the Provider dropdown's list; a deployment edits it to show only its own provider. `crates/irl-provider/tests/catalog.rs` parses the repo copy.
- `THIRD_PARTY_NOTICES.md` ships in every archive, because LGPLv3 FFmpeg wants its notices conveyed with the object code.
- `installer/obs-irl-source.iss` resolves the OBS folder from the registry and requires OBS 32.1 or newer (a minimum, not an exact match).
- `irl-stats.lua` is an example overlay script that reads the stats proc.

## Contributing

If you contribute PRs, understand what you are changing, and write review replies yourself rather than pasting them from an AI. This plugin was built with heavy LLM assistance. The author (datagutt) knows video and SRT(LA) well but is less familiar with the OBS codebase. Tagged releases are fully tested; individual commits may not be.
