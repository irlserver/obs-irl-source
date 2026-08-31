# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

IRL Source is a third-party OBS Studio plugin (Rust 2024, AGPL-3.0) for receiving live IRL streams over SRT, RTMP, RIST, or any FFmpeg-supported protocol. It solves IRL-specific problems: audio jitter buffering, PTS discontinuity repair, adaptive playback speed, keyframe gating, hardware-accelerated decoding, and mid-stream resolution changes.

Version 2.0.0 is a full port of the 1.x C plugin. The C tree is gone; its last commit (`c727912`) is the specification, and `git show c727912:src/<file>.c` is the way to check what the original did. Behaviour is identical apart from the deliberate deviations listed at the bottom of this file.

## Build commands

cargo drives everything; there is no CMake. Two prerequisites:

1. **The bundled media stack.** The plugin statically links its own FFmpeg, libsrt, librist and mbedTLS (see `deps/README.md`), so `./deps/build-deps.sh` runs first. It is incremental, so this is a one-time cost per version bump. It writes `deps/.build/prefix/irl-deps.env`, which `crates/ffmpeg/build.rs` replays as link lines.
2. **libclang.** `ffmpeg-sys-next` runs bindgen over the bundled headers at build time.

libobs is neither built nor linked. `crates/obs-sys` declares the ~58 functions the plugin uses and the symbols resolve against the host OBS process at load time (`raw-dylib` from `obs.dll` on Windows, undefined symbols elsewhere). `libobs-dev` is only needed to *test*.

### Linux

```bash
sudo apt install build-essential cmake pkg-config nasm meson ninja-build \
    clang libclang-dev libobs-dev libva-dev
./deps/build-deps.sh
cargo build --release
./scripts/verify-plugin.sh target/release/libobs_irl_source.so
```

### Windows (MSVC)

`deps/build-deps.sh` runs inside MSYS2 with the MSVC environment active (FFmpeg's configure needs a POSIX shell even when driving `cl.exe`); the cargo build runs from a normal MSVC prompt. See the `windows-x64` job in `.github/workflows/build.yml` for the exact setup.

```powershell
$env:LIBCLANG_PATH = "$env:ProgramFiles\LLVM\bin"
cargo build --release
```

### macOS (Apple Silicon)

```bash
brew install cmake pkg-config nasm meson ninja
export LIBCLANG_PATH=/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib
./deps/build-deps.sh
cargo build --release
./scripts/verify-plugin.sh target/release/libobs_irl_source.dylib
```

### Environment

`FFMPEG_DIR` is set to `deps/.build/prefix` by `.cargo/config.toml` (relative, `force = false`), which keeps ffmpeg-sys-next on its prebuilt-tree branch instead of probing pkg-config. Override it, and `IRL_DEPS_PREFIX`, to build against a prefix produced elsewhere. `rust-toolchain.toml` pins the stable toolchain; the plugin must never need nightly.

### Everything else

```bash
cargo xlint     # clippy --workspace --all-targets -- -D warnings
cargo xtest     # test --workspace
cargo test -p obs-sys --features layout-test   # struct layouts vs real libobs headers
cargo build --release -p irl-source --features deadlocks
scripts/package.sh linux target/release dist   # the release archive, locally
```

`make` wraps the same gates plus the ones cargo does not cover, each with an explicit config out of `.config/` so a machine's global settings cannot change the result. `make style` is the only target that rewrites files; `cargo fmt` on its own picks the wrong width, so always go through the Makefile or pass `--config-path .config/rustfmt.toml`.

```bash
make style        # cargo fmt
make style-check
make lint         # cargo xlint
make test         # cargo xtest
make spell-check  # codespell
make check        # style-check + lint + test + spell-check, what CI runs
make sim          # the speed-controller simulation; not a CI target
```

`--features deadlocks` spawns parking_lot's deadlock detector at module load and logs any cycle with backtraces. It replaces the C build's `IRL_CHECKED_LOCKS`; use it whenever you touch the threading model. Development only.

`cargo build` names the artifact `libobs_irl_source.so` / `obs_irl_source.dll` / `libobs_irl_source.dylib`. `scripts/package.sh` is what renames it to `obs-irl-source.*` and stages the platform's install layout.

`scripts/verify-plugin.sh` is not optional polish. It asserts what a successful compile does not prove: the binary carries no `libav*` dependency, it exports nothing but `obs_module_*`, its undefined symbols are libobs and libc only, and `#![forbid(unsafe_code)]` is still on `irl-core` and `irl-source`. CI runs it (and a `dumpbin` equivalent on Windows) on every build.

## Architecture

One cdylib, five workspace crates. The rule that shapes the split: **all unsafe code lives in `obs-sys`, `obs` and `ffmpeg`.** `irl-core` and `irl-source` carry `#![forbid(unsafe_code)]`, so no port of C pointer arithmetic can sneak in.

| crate | what it is |
| --- | --- |
| `crates/obs-sys` | Hand-written libobs FFI: `#[repr(C)]` structs, `extern` declarations, constants. No safety, no abstraction. A `layout-test` feature runs bindgen over the real headers and asserts every field offset. |
| `crates/obs` | Safe, plugin-agnostic libobs API: the `Source` trait and registration, `declare_module!`, `Data`/`Properties`/`CallData`/`ProcHandler`, `VideoFrame`/`AudioFrame` builders, scene transforms, the obs-websocket vendor helper, `panic::guard`. Knows nothing about IRL streaming. |
| `crates/ffmpeg` | RAII over `ffmpeg-sys-next` (package `irl-ffmpeg`, lib name `ffmpeg`): `FormatContext`, `CodecContext`, `Frame`, `Packet`, `HwDeviceContext`, `FramePool`, `Resampler`, `Scaler`, `InterruptWatch`, and `log::route_to`, which hands the bundled FFmpeg's `av_log` to a caller-supplied sink. `build.rs` replays `irl-deps.env`. |
| `crates/irl-core` | Everything that needs neither libobs nor FFmpeg: the jitter buffer, PTS repair, the speed controller, output-clock arithmetic, video pacing, demuxer options, config derivation, the stats table, every tuning constant. Plain data in, plain data out — and therefore the only crate with a real unit-test suite. |
| `crates/irl-source` | The plugin itself: module entry points, the source lifecycle and the three worker threads. |

### Data flow

```
[receiver thread]: FFmpeg URL, demux
  audio: decode, PTS repair, resample, write to jitter buffer
  video: push the *compressed packet* onto the video queue

[video thread]: decode packets as they come due, keyframe gate, HW frame
                transfer, hold until due, format conversion,
                OBS async video output

[audio thread]: drain jitter buffer, speed correction, concealment,
                OBS audio output
```

### Audio output contract (verified against libobs source)

The audio core is built around three facts about libobs:

1. OBS timestamps must be contiguous (`ts[n+1] = ts[n] + frames/rate`). Deviations under 70ms are smoothed, 70ms to 2s gaps are zero filled by OBS (audible), larger jumps flush all queued audio. The plugin therefore derives timestamps from a pure sample counter anchored once at prime time and never jumps the clock outside declared restarts.
2. Changing `samples_per_sec` between submissions makes OBS destroy and recreate its per source resampler with no crossfade (a click per change). Playback speed is instead applied inside the plugin with a persistent swresample compensation, and the rate submitted to OBS never changes.
3. The OBS mixer consumes 21.3ms ticks against wall clock. A source whose queued audio runs dry gets a tick of silence plus a time shifted splice (crackle), and a source that falls behind the mix window causes OBS to permanently add global audio buffering. After priming, the pump always emits (real audio or shaped concealment silence) and keeps a fixed lead ahead of wall clock.

Buffer regulation happens through playback speed only, asymmetric like IRLToolkit's player: builds at an inaudible -2%, drains post-stall backlog at up to the Catch-Up Speed setting (+5% by default, mild chipmunk). The loop is PI, not P: a slow integral trim under the proportional ramp removes the standing error a sender whose media clock is not wall clock would otherwise leave, and it converges on the sender's rate without measuring it. `docs/audio-timing-pitfalls.md` is why every part of it is shaped the way it is, and is required reading before touching `speed.rs`. Content is never skipped once playback has primed. Backlog beyond a fill ceiling is pushed back into the transport by pausing the read loop (TCP/RTMP backpressure; SRT bounds itself via its latency window), and startup backlog is trimmed only before priming.

### `crates/irl-core`

| module | contents |
| --- | --- |
| `consts.rs` | Every tuning constant, with a test pinning the values to the C plugin's. Nothing else may hardcode a threshold. |
| `audio_buffer.rs` | The jitter buffer: a ring sized in milliseconds with a parallel 256-entry PTS chunk queue. Carries no lock of its own; the caller's mutex is the lock. `resize` grows and never shrinks. |
| `pts_repair.rs` | Three-tier discontinuity repair plus the relock path: small gaps interpolated, medium gaps get silence, large gaps reset the timeline. |
| `speed.rs` | The PI playback-speed controller, regulating a ~2.5s EMA of the buffer level rather than the level itself (a batching upstream otherwise makes it modulate playback at the batch period): the proportional ramp (sloped deadband, EMA, asymmetric limits), `SpeedTrim` (the integral term, with its error window and anti-windup), `SpeedCarry` (the fractional output-sample debt that makes sub-0.1% speeds applicable at all) and `DrainWatch`, which notices a buffer that stopped draining. `examples/speed-controller-sim.rs` drives all of it closed-loop. |
| `pacing.rs` | The video-thread pacing queue: bounded by frames and bytes, `reschedule` re-derives due times, `due_now` returns Emit / EmitEarly / Wait. |
| `timing.rs` | Output-clock arithmetic: next timestamp, lead, expected samples, soft compensation, prime threshold. |
| `dsp.rs` | Fades, shaped concealment silence, last-sample memory. |
| `video_time.rs` | Mapping video PTS through the audio playout offset, the fallback anchor and its clamps, the frame-interval EMA. |
| `url_opts.rs` | The demuxer option table (probe sizes, SRT latency, RIST/UDP buffers, `tls_verify=0`) and parsing of the user's FFmpeg Options. |
| `stats.rs` | `FIELDS`, `StatsSnapshot`, `proc_declaration()`. |
| `config.rs` | `HwDecode`, `Watermarks::derive`. |

### `crates/irl-source`

| file | ports |
| --- | --- |
| `lib.rs` | `plugin.c`: `declare_module!`, load → the FFmpeg log route and `register_source::<IrlSource>()`, post_load → `websocket::register()`, the deadlock poller under the feature. |
| `log.rs` | `irl_info!` / `irl_warn!` / `irl_error!` / `irl_debug!`, which bind the `[irl-source]` prefix, plus the redaction (`redacted_input_url`, `redacted_log_line`) and the `[ffmpeg]` sink. |
| `source.rs` | `irl-source.c`: create/update/tick/activate/deactivate/show/hide/Drop, the media callbacks and the `media_stopped` latch, `start_receiver`/`stop_receiver`, fit-to-canvas, the `get_stats` proc. |
| `settings.rs` | `settings.c`: defaults and the properties dialog. |
| `config.rs` | `config_load` / `config_requires_restart` / `config_apply_hot`. |
| `shared.rs` | The decomposition of the C `struct irl_source` into owners (see below). |
| `receiver/{mod,stream,decode,audio_in}.rs` | `receiver.c`, `receiver-stream.c`, the audio half of `receiver-decode.c`, and the intake half of `receiver-audio.c`. |
| `audio/{mod,pump}.rs` | The output half of `receiver-audio.c`: the pump, concealment, speed application, re-anchoring. |
| `video/{mod,thread,decode,intake,output}.rs` | `receiver-video.c`, `video-handler.c` and the video half of `receiver-decode.c`. |
| `websocket.rs` | `websocket-vendor.c`. |

`update` diffs the new settings against the live config: URL, FFmpeg Options, Hardware Decode and Low Latency Audio are latched at stream open and force a reconnect; everything else is swapped in place through `Config::apply_hot`, so a settings tweak neither drops the connection nor clears the stats counters. Retuning Target Buffer live goes through `AudioBuffer::resize`, which grows the ring (never shrinks it) and only then publishes the new watermarks — if the resize fails the old target stays in force, including in the OBS-thread config that the next diff compares against.

`video_tick` runs the one-shot fit-to-canvas: a source created without a URL (so, freshly added rather than restored from a scene collection) applies the same `obs_transform_info` as the frontend's Fit to Screen action to every scene item referencing it, once, as soon as the source reports a non-zero size.

When "Close Stream When Inactive" is enabled, show/activate start the receiver and hide/deactivate stop it; otherwise those callbacks are no-ops and the stream runs from create to destroy. Every "the stream stopped" clear — hide/deactivate, a restart-forcing settings edit, and the disconnect in `receiver/stream.rs` — is gated on "Show Nothing When the Stream Ends" (`clear_on_disconnect`, on by default). Turning it off leaves the last decoded frame frozen on screen until the stream returns.

`OBS_SOURCE_CONTROLLABLE_MEDIA` and its four callbacks exist because that flag is what makes the source addressable through obs-websocket's `TriggerMediaInputAction` / `GetMediaInputStatus`, which is how NOALBS's `!fix` reconnects a stalled feed (it enumerates candidates by media state, so a source reporting `OBS_MEDIA_STATE_NONE` is invisible to it), and it is also what puts the source in the media controls dock. A live stream has nothing to seek or pause, so they reduce to "run the receiver" and "don't", with a `media_stopped` latch that survives show/activate and is cleared by Restart or a settings edit. Note that `!fix` for `ffmpeg_source` works by writing empty settings, relying on `ffmpeg_source_update` restarting unconditionally; that trick deliberately does not work here, because `update` diffs and hot-applies. Restart is the explicit request.

### Threading model and the lock contract

Four threads. The C plugin enforced its lock contract by convention and a debug-only checker; the Rust port enforces most of it by ownership, which is the point of `shared.rs`.

- **OBS thread** — `IrlSource`: create, destroy, update, tick, get_properties, activate/deactivate/show/hide (the last four only matter with "Close Stream When Inactive"). Everything it owns sits in one `Mutex<ObsState>` (config, `fit_pending`, `media_stopped`, the running threads). It is behind a mutex only because the stats proc can arrive on another thread.
- **`Shared`** — built fresh at every `start_receiver`, which is what replaces the C `reset_runtime_state()`: everything that function zeroed is a field of `Shared` and starts zeroed, and everything it deliberately kept lives in `LifetimeStats`, which is an `Arc` carried across runs.
- **Receiver thread** — owns demux and the *audio* decoder as a plain struct on its own stack. Writes to the jitter buffer, and pushes video packets onto the video channel without decoding them. Never touches the GPU.
- **Video thread** — owns the video decoder, handed over by the receiver at stream open (`VideoMsg::Decoder`, ordered ahead of the packets it belongs to). Decodes packets only as they approach their due time, does the HW transfer, paces each frame, converts and calls `obs_source_output_video`. Its pacing queue is a local, lock-free `PacingQueue`; only the counters are mirrored into `LifetimeStats`.

- **Audio thread** — drains the jitter buffer and submits audio, paced against the sample-counter output clock.

Video decode is on the video thread and not the receiver for two reasons, and the second is the load-bearing one. Decoding eagerly would mean holding the stream's whole latency as decoded frames — 8s of 4K60 is ~6GB — where the same 8s of packets is ~20MB. And the receiver spends a network stall blocked in `av_read_frame`, which is exactly when video must keep draining the buffer it already has, so the thread that decodes cannot be the thread that reads.

Lock order, and the whole of it: **`audio_state` → `audio_buf` → `hot.watermarks`.** `video.q` is never held together with any of them. The audio pump takes `audio_state` exactly once per iteration and passes `&mut AudioState` down, so nothing below it can take it again — parking_lot mutexes are not recursive, and a nested acquire would hang the audio thread and then the video thread behind it.

Hot config (`reconnect_delay_s`, `adaptive_speed`, `catchup_percent`, `wait_for_keyframe`, `clear_on_disconnect`) is atomics, read with `Relaxed` on the worker threads. `catchup_percent` is read once per controller cycle and passed down as a speed, because the ramp, the anti-windup, the actuator clamp and the stuck-drain watch all have to agree on the same ceiling within a cycle. The three watermarks publish together under a mutex because they must never be read torn mid-resize. Stat counters are relaxed atomics: unsynchronised in C, explicitly relaxed here, same values.

Panics never cross an FFI boundary. `obs::panic::guard` wraps every `extern "C"` shim (source callbacks, proc handlers, enumeration trampolines, vendor requests, module exports) and `shared::spawn_worker` wraps every worker thread: a panic is logged, `thread_active` is cleared (which also trips the FFmpeg interrupt watch, so a receiver blocked in `av_read_frame` unblocks), the video sleeper is woken, and the normal stop path takes over.

### Conventions

- **Unsafe.** Only in `obs-sys`, `obs` and `ffmpeg`, and every `unsafe` block there carries a `// SAFETY:` comment. If a port needs a raw pointer, the answer is a new safe wrapper in one of those crates, not an `unsafe` block in `irl-source`.
- **Logging.** `irl_info!("…")`, never `blog` directly; the macros bind the `[irl-source]` prefix. Log strings are part of the interface people grep for — keep them byte-identical to the C where the C had one.
- **Credentials in the log.** A URL never reaches the log whole. The plugin's own lines go through `log::redacted_input_url` (protocol, host and port; the C `irl_log_input_url`), and FFmpeg's go through `log::redacted_log_line`, because FFmpeg prints `h->filename` — the user's `srt://…?passphrase=…&streamid=…` — for its own connect failures. Anything new that logs a URL, or a string that might contain one, belongs behind one of the two.
- **Clocks.** OBS timestamps come from `obs::time::gettime_ns` (`os_gettime_ns`), never `std::time::Instant`. FFmpeg-side timers stay in the `av_gettime` microsecond domain. `irl-core` takes both as parameters so the two can never be mixed by accident.
- **UI strings.** Never pass English text to `module_text`. A new string belongs in two places: the call site and `data/locale/en-US.ini`, keyed by a short identifier. The version in the About block is substituted with `str::replace` on a `%1` token rather than a format string, so a bad translation renders oddly instead of failing.
- **Stats.** A new stat is *one line* in `irl_core::stats::FIELDS` plus its field in `StatsSnapshot` and `values()`. The proc declaration, the calldata writer (`source.rs`) and the websocket copy loop (`websocket.rs`) all walk that table, so they cannot drift. The README table is the only other place to update.
- **Tuning values.** Every threshold lives in `irl_core::consts`, pinned by a test.
- **Source flags.** `OBS_SOURCE_AUDIO | OBS_SOURCE_ASYNC_VIDEO | OBS_SOURCE_DO_NOT_DUPLICATE | OBS_SOURCE_CONTROLLABLE_MEDIA`.

### Tests

`irl-core` has unit tests (`#[cfg(test)]` in each module) derived from the C plugin's thresholds; they are the regression net for the port.

Everywhere else, tests live in `tests/`, never inside the lib. The link arguments that make a test binary resolve libobs (`cargo::rustc-link-arg-tests`) only reach integration-test targets, so `crates/irl-source` sets `test = false` on the lib (its `cdylib` half must not gain test harness code either) and `crates/obs` keeps its lib free of `#[cfg(test)]`. A test that touches libobs runs on Linux (with `libobs-dev`) and is skipped in CI on Windows and macOS, where there is no libobs for the binary to load against. `calldata_*` is the exception that is safe to call anywhere: it is pure bookkeeping over libobs's allocator and needs no `obs_startup`.

`crates/irl-source/tests/network_sim.rs` is the end-to-end harness for the conditions the plugin exists to survive: a stall, a burst, repeated dropouts, a sender whose media clock is not wall clock, one too fast to ever catch. It drives the real jitter buffer, PTS repair, speed controller, output clock and packet queue against a synthetic sender on a virtual clock, and asserts what the design promises — the OBS clock never jumps except across a *declared* restart or re-anchor, audio is never skipped once primed, latency does not ratchet across dropouts, and decoded memory does not grow with Target Buffer. It does not cover the demuxer or the video decoder: the bundled FFmpeg carries only the decoders the plugin needs (no rawvideo), so there is no packet a decoder here would accept, and video is driven at the two ends of the decoder instead.

Note the sampling point in it. The jitter buffer's level oscillates by one whole chunk within every cycle, so *where* you read the fill decides what number you get: before the pump's read (what the controller regulates) it averages the target, and after it, a chunk lower. The stats line's `buf=` is a random sample of that oscillation, which is why it reads low as often as not.

`crates/irl-source/tests/locale_keys.rs` is the mechanical half of the "a new UI string belongs in two places" rule: it scans `settings.rs` and `source.rs` for `module_text` keys and fails if one has no `data/locale/en-US.ini` entry, or if the ini carries a string nothing uses. `module_text` falls back to returning the key, so without it a missing string is only noticed by opening the properties dialog.

The speed controller has one more check that is not a test, because a controller that limit-cycles still passes every assertion you would think to write about one sample of it:

```bash
cargo run -p irl-core --example speed-controller-sim
```

It runs the real `irl_core::speed` closed-loop against a simulated sender and exits non-zero if the loop fails to settle at any buffer target or if a requested speed is not applied faithfully. Not a CI target; run it by hand whenever you touch `speed.rs`, and read `docs/audio-timing-pitfalls.md` first.

Real-stream validation is manual: run the same feed through this build and a known-good one and compare the 30-second stats line field by field.

## CI

GitHub Actions (`.github/workflows/build.yml`) builds on Linux x64 (Ubuntu 26.04), Windows x64 (VS 2026) and macOS ARM64 (macos-15). Every job builds the bundled media stack first (cached on the hash of `deps/versions.env` plus `deps/build-deps.sh`), then runs `cargo build --release --workspace`, clippy with `-D warnings`, the tests, and the isolation checks. `Swatinem/rust-cache@v2` caches the cargo build. No job builds libobs any more.

`OBS_VERSION` at the top of the workflow documents the oldest supported OBS line; the value that actually reaches libobs is `api_version` in the `declare_module!` call. `obs_init_module` gates a plugin on `(mod.ver() & 0xFFFF0000) <= LIBOBS_API_VER` — major and minor only — and looks up nothing but `obs_module_*` symbols, so declaring the oldest supported line yields one binary that loads there and on every newer release. Raise it only to drop support for older OBS releases, never to chase a newer one.

Releases are tag driven (`.github/workflows/release.yml`, see `RELEASING.md`). Pushing `vX.Y.Z` verifies the tag against `[workspace.package] version` in `Cargo.toml`, runs the build workflow, calls `scripts/package.sh` once per platform, generates `sha256sums.txt`, and creates a draft GitHub release whose body is `.github/release-notes-header.md` plus a changelog. Publishing the draft is manual, after testing the artifacts.

`scripts/changelog.sh` builds that changelog by grouping the commits since the previous `v*` tag on their conventional commit type. Commit subjects are the release notes, so write them as such. Run `scripts/changelog.sh HEAD` to preview.

## Deliberate deviations from the C plugin

The port is behaviour-identical except for these, which are intentional:

1. The dead `network_buffer_mb` setting is gone. Nothing read it; the transport buffer is `irl_core::consts::NETWORK_BUFFER_MB`.
2. The `video_decoder_flushes` stat is gone (it was always 0 after the video decoder stopped being flushed). 27 stat fields remain.
3. `irl-stats.lua` finds the source by its plugin id instead of by display name, and takes source names as script properties.
4. The vestigial `hw_map_ok` flag is not ported.
5. `w32-pthreads.dll` is no longer shipped on Windows: Rust never calls `pthread_*`, so the librist shim hazard that `include/irl-threading.h` existed for is gone. The installer deletes a stale copy.
6. `obs_get_video_info` and `obs_sceneitem_set_info2` go through slack wrappers with 64 trailing bytes, since libobs reads and writes those structs by *its* size.
7. Two latent races where the stats snapshot read unlocked video-thread writes are closed by mirroring the anchors into atomics. Same values.
8. The stats field list is one table (`irl_core::stats::FIELDS`) instead of three hand-synchronised copies.
9. Version 2.0.0. The artifact is built by cargo, and CI no longer builds libobs.
10. The speed-controller simulation **links** the controller instead of replicating it. `tools/speed-controller-sim.c` copy-pasted the constants and both update rules, because the real controller read `struct irl_source`; `crates/irl-core/examples/speed-controller-sim.rs` calls `irl_core::speed` directly, so the C file's standing caveat — "change them there and you must change them here too, or this quietly starts simulating a controller that no longer exists" — does not apply. `tools/` is gone with it.
11. The UI strings are checked against `data/locale/en-US.ini` by a test rather than by convention (`crates/irl-source/tests/locale_keys.rs`).
12. Video is decoded on the video thread, just before each frame is due, and the receiver → video queue carries compressed packets instead of decoded frames. The C decoded eagerly on the receiver thread, which made the Target Buffer cost decoded-frame memory: 1 GiB of pacing budget is 5.7s of 1080p60 but only 1.4s of 4K60 and 0.7s of 4K60 10-bit, and past that frames were emitted early and dropped. Decoded memory is now bounded by `VIDEO_DECODE_LEAD_MS` regardless of the target. `PacingQueue` gained the matching soft/hard bound split: holding the decode lead is normal and must not emit early, while the byte and frame ceilings are memory limits that still do. The stats line reports `pktq=` instead of `pinned_peak=`, since no decoded frame pins a decoder surface any more.

## Contributing

If you wish to contribute PRs to this project, please understand what you are changing. You should be able to write any replies to reviews/PRs yourself — don't copy and paste replies directly from AI.

This plugin was heavily built with LLM assistance, including the Rust port. The author (datagutt) has experience with video and SRT(LA) protocols but is less familiar with the OBS Studio codebase. Tagged releases are fully tested; individual commits may not be.

## Other files

- **`irl-stats.lua`** — Example OBS Lua script that reads plugin stats via `proc_handler` and updates a text source overlay.
- **`installer/obs-irl-source.iss`** — Inno Setup script for the Windows setup .exe. It resolves the OBS folder from the registry (`Uninstall\OBS Studio` in HKLM64 then HKCU, then `HKLM\SOFTWARE\OBS Studio`), validates it by finding `bin\64bit\obs64.exe`, and installs the same payload as the release zip. The OBS version check is a *minimum* (32.1), not an exact match, because nothing binds the plugin to one OBS line. The Windows job in `build.yml` compiles it on every push, not just at tag time, so a broken `.iss` fails a normal build instead of a release.
- **`data/locale/en-US.ini`** — UI strings, loaded by `declare_module!`'s locale exports. Shipping it is not optional: the lookup falls back to returning the key, so a package built without it renders the dialog as bare identifiers like `AudioBufferHelp`. All three release archives and the installer place it where `obs_module_file()` looks (`data/locale/` next to the binary on Linux, `data/obs-plugins/obs-irl-source/locale/` on Windows, `Contents/Resources/locale/` inside the macOS bundle).
- **`THIRD_PARTY_NOTICES.md`** — Licenses for the statically linked stack and the Rust crates, shipped inside every release archive rather than only living in the repo, because LGPLv3 FFmpeg wants its notices conveyed with the object code. `deps/README.md` has the reasoning behind the license choices; this file is the artifact-facing copy.
- **`docs/audio-pipeline.md`** — Deep dive on the buffered vs low-latency audio paths, jitter buffer, adaptive latency control, PTS repair tiers, and timestamp handling.
- **`docs/viewer-quality-plan.md`** — The viewer-quality policy and the recovery/diagnostics behavior that implements it (what stats to watch and what healthy looks like).
- **`docs/audio-timing-pitfalls.md`** — What was built wrong first in the audio timing path, and the media-clock estimator that was built, measured and deleted. Required reading before changing `crates/irl-core/src/speed.rs`; most of it is re-inventable.
- **`Makefile`**, **`.config/`** — The quality gates and their explicit configs (`rustfmt.toml`, `codespellrc`), so `make check` gives the same answer everywhere.
- **`AGENTS.md`**, **`GEMINI.md`** — Symlinks to this file (`CLAUDE.md`).
