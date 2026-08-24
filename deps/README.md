# Bundled media stack

The plugin statically links its own FFmpeg, libsrt, librist and mbedTLS instead
of using whatever the host OBS ships. `build-deps.sh` builds that stack;
`versions.env` pins the versions.

## Why

Two problems, one fix.

**Stale transport libraries.** obs-deps has pinned libsrt 1.5.2 (September 2022)
and librist 0.2.7 for years. Every SRT and RIST fix since then is missing from
OBS's FFmpeg.

**FFmpeg majors split the releases.** OBS 32.1 bundles FFmpeg 7 (`avcodec-61`),
OBS 32.2 bundles FFmpeg 8.1 (`avcodec-62`). A plugin linked against one will not
load where the other is present, so the project had to publish a separate
binary per OBS line. Linking statically removes the dependency entirely, and one
artifact per platform now covers every OBS line.

## Usage

```bash
./deps/build-deps.sh            # builds into deps/.build/prefix
./deps/build-deps.sh --jobs 8
./deps/build-deps.sh --clean    # discard and rebuild everything
```

The build is incremental. Marker files inside the prefix record what finished,
so a second run is a no-op and CI only has to cache `deps/.build/prefix` (not
the much larger source and object trees).

The script writes `deps/.build/prefix/irl-deps.env` describing the exact link
line. `crates/ffmpeg/build.rs` reads it and replays it; nothing rediscovers the
stack on its own. (An `irl-deps.cmake` with the same content is still written
for anything outside cargo that wants it.)

### The `irl-deps.env` contract

Plain `KEY=value` lines, no quoting, lists separated by `;`, paths absolute and
native:

| Key | Meaning |
| --- | --- |
| `IRL_DEPS_HOST` | `linux`, `macos` or `windows` |
| `IRL_DEPS_PREFIX` | The prefix itself |
| `IRL_DEPS_INCLUDE_DIR`, `IRL_DEPS_LIBDIR` | Where the headers and archives are |
| `IRL_DEPS_FFMPEG_VERSION`, `IRL_DEPS_SRT_VERSION`, `IRL_DEPS_LIBRIST_VERSION`, `IRL_DEPS_MBEDTLS_VERSION` | Pinned upstream versions, logged by the plugin at load |
| `IRL_DEPS_TRANSITIVE_LIBS` | Bare `-l` names in single-pass link order (`srt;rist;mbedtls;mbedx509;mbedcrypto`, plus `z` on Windows). The five libav*/libsw* archives are omitted: ffmpeg-sys-next emits those itself, and its build script runs first, which is what keeps the order right |
| `IRL_DEPS_TRANSITIVE_PATHS` | The same list as absolute archive paths, for diagnostics |
| `IRL_DEPS_SYSTEM_LIBS` | Shared system libraries (`m`, `va`, `va-drm`, the C++ runtime) |
| `IRL_DEPS_FRAMEWORKS` | macOS frameworks |

`crates/ffmpeg/build.rs` looks for the file under `$IRL_DEPS_PREFIX`, falling
back to `deps/.build/prefix` next to the workspace, and fails with a message
pointing at this script when it is missing. `FFMPEG_DIR` (set in
`.cargo/config.toml`) points ffmpeg-sys-next at the same prefix, which is what
keeps it off its pkg-config branch.

### Platforms

Needs a C/C++ compiler, CMake, nasm, and meson/ninja (librist builds with
meson, everything else with CMake or autotools).

Linux and macOS run it directly. On Windows it runs inside MSYS2 with the MSVC
environment active, because FFmpeg's configure needs a POSIX shell even when it
is driving `cl.exe`. meson there must be the Windows-native one (installed with
pip) so it detects `cl.exe` rather than looking for a POSIX toolchain. See the
`windows-x64` job in `.github/workflows/build.yml`.

Linux additionally needs `libva-dev` for VAAPI. The script refuses to build
without it rather than silently producing a software-only binary. For a local
compile check on a machine without it:

```bash
IRL_DEPS_DISABLE_VAAPI=1 ./deps/build-deps.sh
```

Never ship that build.

## What is in the FFmpeg build

Decode only, `--disable-everything` plus an explicit component list: H.264,
HEVC, AV1, VP9, AAC, Opus, MP3, AC3/E-AC3 and PCM decoders; MPEG-TS, FLV, MP4,
Matroska, HLS and RTSP demuxers; SRT, RIST, RTMP(S), HTTP(S), TCP, UDP and RTP
protocols. Hardware decode is VAAPI and NVDEC on Linux, D3D11VA/DXVA2 and NVDEC
on Windows, VideoToolbox on macOS.

The component list is what the README advertises, no more and no less. Trimming
it further is a user-visible feature removal, not a size optimisation.

`configure` silently ignores unknown names in an `--enable-*` list, which makes
a typo invisible: `--enable-protocol=srt` (the component is `libsrt`) produced a
build with `CONFIG_LIBSRT=yes` and no SRT protocol at all. `build-deps.sh`
therefore asserts against the generated `ffbuild/config.mak` that every
component the plugin depends on actually landed, and fails the build otherwise.

FFmpeg 9.0 made `tls_verify` default to on. `tls_openssl.c` copes by falling
back to `SSL_CTX_set_default_verify_paths()`, but this stack has no OpenSSL and
`tls_mbedtls.c` loads a CA chain only from an explicit `ca_file`, so the default
build would reject every `https://` and `rtmps://` peer.
`irl_core::url_opts::demuxer_options` therefore sets `tls_verify=0`, which the
user's FFmpeg Options can override per source. Do not "fix" that by dropping
the option; without a bundled CA file it only turns the feature off entirely.

`fix_mbedtls_pc` exists for a related trap. libsrt and librist both record their
mbedTLS dependency as an absolute library path, which puts it in the wrong place
on a single-pass static link and, in librist's case, pinned the *host's* shared
`libmbedcrypto.so` instead of the copy in this prefix. The helper rewrites both
`.pc` files to plain `-l` flags resolved out of the prefix.

## Symbol isolation

The host OBS has already loaded its own FFmpeg, exporting the same symbol names
as the archives linked here. On ELF an exported `avcodec_open2` in the plugin
would resolve through the global scope to OBS's copy, so the plugin's calls
would run against a different library's structs.

rustc already emits its own export list for a cdylib — only
`#[unsafe(no_mangle)]` items, which here are exactly the `obs_module_*` entry
points libobs looks up — so every static libav* symbol stays hidden.
`crates/irl-source/build.rs` adds `--exclude-libs,ALL` on Linux as belt and
braces. `scripts/verify-plugin.sh` asserts the result after every build, and CI
fails if the plugin gains a `libav*` dependency or leaks a bundled symbol.

## Licensing

FFmpeg is configured LGPLv3 (`--enable-version3`, no `--enable-gpl`), which is
compatible with the plugin's AGPL-3.0. Nothing here needs GPL components since
the plugin decodes and never encodes. libsrt is MPL-2.0, librist is BSD-2-Clause
and mbedTLS is Apache-2.0, all compatible with (A)GPLv3.

Distributing a statically linked binary means distributing the corresponding
source. `versions.env` pins the exact upstream releases and this script is the
complete build recipe.

## Bumping a version

Edit `versions.env`, update the matching SHA256, and run the script. CI keys its
cache on the contents of `versions.env` and `build-deps.sh`, so a bump
invalidates it automatically.
