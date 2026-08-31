# IRL Source

An OBS source built for IRL streams. Point it at your SRT or RTMP pull URL and it handles the things that go wrong on a phone in a moving vehicle: bitrate drops, cell tower handoffs, brief disconnects, resolution changes mid-stream.

> **Note:** This is an independent project by [irlserver.com](https://irlserver.com). It is not developed by, affiliated with, or endorsed by the OBS Project.

## What it does for your stream

**Audio survives a bad connection.** The plugin holds a small cushion of audio (120ms by default) so a hiccup on the way to your PC does not turn into a stutter on stream. When your connection craters and then comes back, it catches up by playing slightly fast (up to 5% by default, and you can set how fast) instead of cutting audio out. Nothing gets skipped.

**No pops, clicks or metallic garbage.** Timestamp jumps from cell tower handoffs and packet loss get repaired instead of passed through. If a piece of audio truly cannot be recovered, you get a short silence rather than something that sounds broken. Disconnects fade out instead of clicking.

**No blocky mess when the stream starts.** Video is held back until a clean frame arrives, so joining or reconnecting does not paint a few seconds of smeared blocks on your stream.

**It recovers on its own.** When bitrate starvation jams the decoder, the built-in Media Source can lose audio permanently until you restart the source. This one flushes and self-heals. Phone rotations and adaptive bitrate resolution changes keep playing without a restart.

**Lower delay.** An IRL SRT stream through a Media Source usually sits 2 to 3 seconds behind real life. This plugin stays well under that, and it does not creep: audio goes to OBS on a steady clock, so OBS never starts adding buffering of its own.

**Less clicking around.** A source you just added sizes itself to your canvas on the first frame, so there is no manual Fit to screen step.

Works with SRT, RTMP, RIST, UDP, TCP, HTTP, or anything else FFmpeg can open, and with H.264, HEVC (including 10-bit), AV1, VP9, AAC and Opus.

## Why not the built-in Media Source?

OBS ships with a Media Source that can play SRT, RTMP and RIST. It works, but it was written for playing files and general media, not for a live feed coming off a phone on mobile data.

| | Media Source | IRL Source |
| --- | --- | --- |
| Unstable connection | Relies on whatever cushion the protocol itself provides, and holds nothing after it | Adds its own cushion (120ms default) behind the protocol latency |
| After a stall | Latency climbs and stays there, with no way to catch up | Catches up at up to the Catch-Up Speed until latency is back on target, without skipping audio |
| Audio timing | Falls behind the mixer, so OBS adds up to a second of buffering that never goes away | Steady clock with a fixed lead, so OBS never adds hidden delay |
| Timestamp jumps | Passed straight through, so you get pops and freezes | Repaired: small gaps smoothed, medium gaps filled with silence, large gaps get a clean reset |
| Disconnect | Abrupt cutoff with a loud click | 50ms fade out, fade in on reconnect |
| Joining a stream | Waits for a keyframe on video, but lets pre-keyframe audio through | Waits on both, and gates ahead of the decoder rather than after it |
| Bitrate starvation | Decoder can get stuck and audio breaks until you restart the source | Flushes the decoder, resets timing, keeps video moving |
| Reconnect | 10 second default delay | 2 second default, tuned for how often IRL streams drop |
| Resolution changes | Usually survives them, but breaks on streams that need pixel format conversion | Handled on both paths |
| Stats | Duration, position and playback state | Buffer level, delay, frame counts, repairs and more, from a Lua or Python script or over obs-websocket |

[Compared with the Media Source](#compared-with-the-media-source) in the technical section has the code behind each row.

## Installation

Download from [Releases](../../releases). There is one archive per platform and it works on OBS 32.1 and newer. Older releases of this plugin shipped a separate build per OBS version; that is no longer necessary.

### Windows

Run `obs-irl-source-<version>-windows-x64-setup.exe`. It finds your OBS Studio folder from the registry, closes OBS if it is running, and can be removed again from Add or Remove Programs.

Or, from the zip:

1. Close OBS
2. Extract the zip into your OBS Studio install folder (usually `C:\Program Files\obs-studio`), so the DLLs land in `obs-plugins\64bit`
3. Start OBS

### macOS (Apple Silicon)

1. Close OBS
2. Extract the zip into `~/Library/Application Support/obs-studio/plugins/` (it contains `obs-irl-source.plugin`; if an older `obs-irl-source` folder is still there, delete it)
3. The binary is unsigned, so clear the quarantine flag once:
   `xattr -dr com.apple.quarantine "$HOME/Library/Application Support/obs-studio/plugins/obs-irl-source.plugin"`
4. Start OBS

### Linux

The release binary bundles its own media stack and resolves libobs symbols from the OBS process at load time rather than linking a libobs, so the OBS version it was built against does not matter. It is still compiled against Ubuntu's glibc; on an older distribution, build from source instead (see [Building from source](#building-from-source)).

1. Close OBS
2. Extract the tarball into `~/.config/obs-studio/plugins/`
3. Start OBS

## Usage

1. Add a new source: **IRL Source (irlserver.com)**
2. Enter your stream URL (for example `srt://your-server:4000?streamid=play/stream/key`)
3. Leave the rest alone unless you have a reason. The defaults are the tested path.

A source you just added sizes itself to the canvas when its first frame arrives, same result as Edit > Transform > Fit to screen (aspect preserved, nothing cropped). This happens once. A source loaded from a saved scene collection is never touched, and once you move or resize it the plugin leaves it alone.

### Settings

| Setting | Default | What it does |
| --- | --- | --- |
| URL | | Your pull URL. SRT, RTMP, or anything else FFmpeg can open |
| Reconnect Delay | 2s | How long to wait between reconnect attempts |
| Target Buffer | 120ms | How much audio cushion to hold, 20ms to 8s. This is your main latency knob: higher rides out a worse connection, lower is snappier and less forgiving. If the stats show `underruns` climbing, this is the setting to raise — an underrun means the cushion ran dry, and the concealment that covers it delays video by the same amount to keep lip sync. The whole target is paid as delay before the source starts, so raise it to what your connection actually needs rather than to the maximum. Memory cost is small and does not depend on resolution much: video is held compressed and only decoded just before it is shown |
| Adaptive Latency Control | On | Holds latency near your target by nudging playback speed (up to 2% slow, and up to Catch-Up Speed fast) instead of dropping audio |
| Catch-Up Speed | 5% | How fast playback may run while clearing a backlog, 2% to 15%. 5% recovers a second of backlog in about 20 seconds and is audible on music (roughly a semitone) but not on speech. Lower it if you play music and would rather the recovery take longer than be heard; raise it to get latency back sooner. Only applies with Adaptive Latency Control on |
| FFmpeg Options | | Extra options for the stream reader, `key1=val1 key2=val2` style. Use this to set the SRT `latency`, for example |
| Hardware Decode | Auto | Let the GPU decode video. Auto picks whatever your machine supports, Off forces the CPU, and NVDEC (Windows/Linux) explicitly requires NVIDIA CUDA/NVDEC |
| Wait for Keyframe | On | Hold video back until a clean frame arrives, so you never see blocky garbage on join |
| Low Latency Audio | Off | Play audio the moment it arrives, with no cushion. Lowest delay, least tolerant of a wobbly connection |
| Show Nothing When the Stream Ends | On | Blank the source as soon as the stream drops, instead of leaving the last frame frozen on screen until it reconnects. Same idea as the media source's "Show nothing when playback ends" |
| Close Stream When Inactive | Off | Stop pulling the stream when the source is neither showing nor active (the last frame goes black if Show Nothing When the Stream Ends is on), and reconnect when it becomes visible again |

Target Buffer, Reconnect Delay, Adaptive Latency Control, Catch-Up Speed, Wait for Keyframe, Show Nothing When the Stream Ends and Close Stream When Inactive can be changed while the stream is running. The connection stays up and the stats counters keep counting. The one exception is turning Close Stream When Inactive on while the source is already hidden, which is a request to stop receiving: that drops the connection and resets the stats counters, as it would on any later hide. Changing Target Buffer mid-stream keeps every buffered sample and walks the latency to the new value at up to the Catch-Up Speed or -2%, so you should not hear a seam. Changing URL, FFmpeg Options, Hardware Decode or Low Latency Audio reconnects, because those are set when the stream is opened.

Earlier versions exposed Min/Max Buffer, PTS gap thresholds, Network Buffer and Decoupled Audio. Those are now fixed or derived internally, so old scene collections keep working and ignore the stored values.

### Buffered or low latency?

`Low Latency Audio` does more than flip an OBS flag. It changes how the plugin buffers.

- Buffered mode is the default and the one to use for IRL. It keeps the cushion you asked for, plays at normal speed almost all the time, and covers dropouts with shaped silence instead of noise. Backlog from a stall gets played back sped up, never thrown away.
- Low latency mode plays audio as soon as it shows up and turns off the plugin's own correction. Use it when absolute delay matters more than surviving a rough connection.

## Stats overlay

The plugin exposes live stats that a Lua or Python script can read, so you can put a status overlay on your own stream or on a monitor.

Create a Text (GDI+) source called `IRL Stats`, then add [`irl-stats.lua`](irl-stats.lua) as a Lua script in OBS (Tools → Scripts). It polls the source's `get_stats` proc handler once a second and writes the formatted stats into the text source. It finds the IRL source by its plugin id, so renaming the source does not break it; the script properties let you name a specific one (for a scene with more than one) and change the text source.

The full list of readable fields is in [Stats reference](#stats-reference).

## Reading stats over obs-websocket

The same stats are exposed as an [obs-websocket](https://github.com/obsproject/obs-websocket) vendor extension, so an overlay, a bot or a dashboard on another machine can read them without running a script inside OBS. obs-websocket ships with OBS; turn it on under Tools → WebSocket Server Settings. If it is disabled or missing, the plugin registers nothing and behaves exactly as before.

Vendor name: `obs-irl-source`.

| Request | Request data | Response |
| --- | --- | --- |
| `GetStats` | `source_name`, optional when the scene collection has exactly one IRL source | `source_name` plus every field in [Stats reference](#stats-reference) |
| `GetSourceList` | none | `sources`: array of `{source_name, active, showing}` |
| `GetVersion` | none | `plugin_version`, `vendor_api_version`, `obs_websocket_api_version` |

Every response carries `success`. When it is `false`, `error` says why: no source by that name, that source is not an IRL Source, no IRL Source at all, or more than one with no `source_name` given. Stream URLs are deliberately not exposed, because they can carry an SRT passphrase or a stream key and every connected client would see them.

With [obs-websocket-js](https://github.com/obs-websocket-community-projects/obs-websocket-js):

```js
const { responseData } = await obs.call("CallVendorRequest", {
    vendorName: "obs-irl-source",
    requestType: "GetStats",
    requestData: { source_name: "IRL Source" },
});

console.log(responseData.stream_delay_ms, responseData.buffer_fill_ms);
```

There is no event stream, so poll `GetStats` at whatever rate your overlay refreshes; once a second is plenty. The request reads the same snapshot the Lua path does, so both transports always report identical numbers.

## Restarting the stream remotely

The source registers as a controllable media input, so it appears in OBS's media controls dock and answers the standard obs-websocket media requests — no vendor extension needed:

- `TriggerMediaInputAction` with `OBS_WEBSOCKET_MEDIA_INPUT_ACTION_RESTART` drops the connection and reconnects. This is the one to use for a chat bot's "fix my stream" command.
- `..._STOP` (or `..._PAUSE`) stops the receiver and blanks the source until a restart or a settings edit; `..._PLAY` starts it again.
- `GetMediaInputStatus` reports `OBS_MEDIA_STATE_PLAYING` once frames are flowing, `OBS_MEDIA_STATE_BUFFERING` while connected but still filling, `OBS_MEDIA_STATE_OPENING` while reconnecting, and `OBS_MEDIA_STATE_STOPPED` when the receiver is not running.

Unlike OBS's own media source, writing settings does **not** implicitly restart the stream: changing anything other than URL, FFmpeg Options, Hardware Decode or Low Latency Audio is applied to the running stream in place, so retuning the buffer never costs a reconnect. Ask for a restart explicitly.

## Why open source?

We run a commercial streaming service (relay infrastructure and more), so yes, we have plenty of closed-source code. But OBS itself is GPL-2.0. It is free software built by its community. Paywalling the thing that keeps your stream from dropping frames, just to upsell a subscription, feels like the wrong move.

The IRL streaming scene was mostly built in the open. Projects like [Moblin](https://github.com/eerimoq/moblin), [NOALBS](https://github.com/NOALBS/nginx-obs-automatic-low-bitrate-switching), and [BELABOX](https://github.com/BELABOX) all started open and pushed things forward because anyone could use them, learn from them, and build on top. Some competitors went the other way and shipped closed-source OBS plugins as a product feature, locking basic stream reliability behind a monthly fee. That is a bad trade for streamers and for the ecosystem. The plugin layer should be something everyone can use, inspect, and improve. That is why this is AGPL. If someone builds on it, those improvements come back too.

If you find this useful, great. If you want managed infrastructure on top of it, that's what [irlserver.com](https://irlserver.com) is for.

## AI usage

I (datagutt) don't really know too much C, and I am a bit unfamiliar with the OBS Studio code base.
What i do have is quite a bit of experience working with video and SRT(LA) protocols from other projects.

The initial version of this plugin was heavily built with LLM assistance. That includes most of this README (except this "AI Usage" section).

Rest assured I will go through both the README and codebase and clean this up, once I have the initial builds working well.

Any tagged release should at least be fully tested, single commits might not (though i will try to use branches).

## Contributing

If you wish to contribute PRs to this project, please understand what you are changing. Also, you should be able to write any replies to reviews/PRs yourself.
Please don't just copy and paste replies directly from the AI.

## License

AGPL-3.0-or-later. Copyright (C) 2026 Thomas Lekanger.

The released binaries statically link FFmpeg (LGPL-3.0), libsrt (MPL-2.0), librist (BSD-2-Clause) and Mbed TLS (Apache-2.0); see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md), which also ships inside every release archive.

See [irlserver.com](https://irlserver.com) for more information.

---

# Technical details

Everything below is for people who want to know how it works, or who want to build and hack on it. You do not need any of it to use the plugin.

## What it optimizes for

The plugin optimizes for what viewers hear and see, not for preserving every damaged packet.

- Audio must not sound jittery, glitchy, metallic, or artifacty. If audio cannot be reconstructed cleanly, short silence is preferred over audible corruption.
- Audio content is never skipped once playback has started. Backlog from a stall is played back slightly sped up (up to the Catch-Up Speed, +5% by default) until latency returns to target.
- Video cadence should stay smooth. During decoder damage, timestamped damaged frames are preferable to freezes as long as they are a picture: H.264 concealment produces one, so those pass through; HEVC has no concealment and renders a missing reference as flat gray, so those frames are held and the last good frame stays up until the next keyframe. Avoid gray/blank frames and decoder reset storms.
- Latency may drift if that protects viewer quality, but it should stay far below the 2 to 3 second live delay typical of an IRL SRT stream through the OBS Media Source. The source must never fall behind the OBS mix window, because that is what makes OBS add global audio buffering it never gives back.
- Diagnostics should make the recovery path visible: interpolation, silence insertion, resets, trims, underruns, and playback mode are tracked separately.

## Audio pipeline

The audio core is built around three properties of libobs:

1. Source timestamps must be contiguous. Deviations under 70ms get smoothed, gaps between 70ms and 2s are zero filled by OBS (audible), and larger jumps flush all queued audio. The plugin therefore derives timestamps from a sample counter anchored once at prime time, and never jumps the clock outside a declared restart.
2. Changing `samples_per_sec` between submissions makes OBS destroy and recreate its per-source resampler with no crossfade, which is a click every time. Playback speed is instead applied inside the plugin with a persistent swresample compensation, and the rate submitted to OBS never changes.
3. The OBS mixer consumes 21.3ms ticks against the wall clock. A source that runs dry gets a tick of silence plus a time shifted splice (crackle), and a source that falls behind the mix window makes OBS permanently add global audio buffering. After priming, the pump always emits (real audio or shaped concealment silence) and keeps a fixed lead ahead of the wall clock.

Buffer regulation happens through playback speed alone, asymmetric like IRLToolkit's player: it builds at an inaudible -2% and drains post-stall backlog at up to the Catch-Up Speed (+5% by default, mild chipmunk). A slow integral trim underneath the proportional ramp removes the standing error a sender whose media clock is not wall clock would otherwise leave (see `docs/audio-timing-pitfalls.md`). Content is never skipped once playback has primed. Backlog beyond a fill ceiling is pushed back into the transport by pausing the read loop (TCP and RTMP apply backpressure, SRT bounds itself through its latency window), and startup backlog is trimmed only before priming.

The jitter buffer is a ring sized in milliseconds with a parallel PTS chunk queue, so it adapts to any sample rate or channel count. The speed controller's watermarks derive from `Target Buffer`: low at half the target, full drain speed at target plus 200ms. Retuning the target live grows the ring if needed (it never shrinks) and moves the watermarks while keeping every queued sample.

`Low Latency Audio` switches the source to OBS async unbuffered semantics and drains chunks as soon as they are available, instead of building the startup cushion and running plugin-side correction.

The jitter buffer, adaptive latency control, PTS repair tiers and timestamp handling are covered in detail in [Audio pipeline](docs/audio-pipeline.md).

## PTS repair

Presentation timestamps arriving from a mobile encoder over a lossy link do not always advance cleanly. Repair runs in three tiers, plus a normalization path that does not count as damage:

- Frame-sized cadence offsets are normalized and not counted as a repair.
- Small non-frame-sized gaps are smoothed by interpolating the timestamp.
- Medium gaps (up to 2000ms) get silence inserted so the timeline stays continuous.
- Gaps beyond 2000ms trigger a full timing reset.

The 70ms and 2000ms thresholds are fixed internally and match the libobs behavior described above.

## Video path

- Keyframe gating happens at the packet level, so the decoder never sees pre-keyframe data on join or reconnect. Pre-keyframe audio is discarded rather than staged, so the audio decoder does not warm up on garbage.
- Low-delay decode: no B-frame reorder buffering, capped decode threading.
- Zero-copy for supported pixel formats, planes go straight to OBS. Native 10-bit passthrough for YUV420P10LE (I010) and P010. Unsupported formats fall back to swscale.
- Mid-stream resolution changes are detected and handled without recreating the source.
- Damaged H.264 frames are passed through with their timestamps rather than dropped, which preserves cadence instead of freezing on every corrupt frame. HEVC frames predicted from a reference that never arrived are held back instead: HEVC has no error concealment, so FFmpeg synthesizes the missing reference as flat gray and everything predicted from it is gray until the next keyframe. The last good frame stays on screen for that stretch (`video_corrupt_held` counts them).
- Video PTS is mapped through the audio playout offset for lip sync.

## Decoder recovery

Repeated audio decode errors trigger a throttled audio decoder flush and a reset of bad timing state. This is what stops SRT bitrate starvation from permanently breaking audio, which is the failure mode the built-in Media Source hits.

The video decoder is deliberately never flushed. Flushing empties the reference picture buffer and clears the decoder's recovery state, and neither the H.264 nor the HEVC decoder produces a real picture again until the next keyframe: H.264 paints frames gray until a recovery point, HEVC synthesizes each missing reference as flat gray. On a lossy stream that turned a few damaged frames into a whole GOP of gray. A decode error on a live stream is a property of the packet, not of the decoder, so the next intact packet decodes fine without a reset. Bursts are still counted and logged (`video_corrupt_frames`, and the `corrupt=`/`held=` fields of the stats line).

## Compared with the Media Source

How the built-in Media Source actually behaves on a network stream, which is where the comparison table near the top comes from. Read against OBS Studio 32.2 and confirmed against 32.1, the oldest line this plugin supports. The code is in `plugins/obs-ffmpeg/obs-ffmpeg-source.c`, `shared/media-playback/`, and `libobs/obs-audio.c`.

- No cushion of its own. SRT's TSBPD window absorbs jitter and retransmits before FFmpeg sees anything, which at a long IRL latency setting covers a lot. Nothing sits behind it: the "Network Buffering" slider (2MB default) sets the socket receive buffer, not a playout buffer, and a single thread demuxes, decodes and outputs, so a hiccup that outlasts the SRT window lands straight on the source.
- No catch-up. Playback speed is pinned to 100% for network inputs and the speed slider is hidden for them, so lateness accumulated during a stall stays.
- Audio timestamps are the stream PTS plus a fixed offset taken at open. Once a source's audio falls behind the mixer window, OBS adds global buffering, capped at 45 ticks (about 960ms at 48kHz), and that total only ever grows for the rest of the session. Past the cap OBS starts dropping the late source's audio instead.
- No PTS repair. The 2s and 3s guards in the media thread only clamp its own sleep pacing; the timestamps OBS receives are unmodified.
- Keyframe gating exists, but it runs on the decoded frame rather than the packet, and only on video. Pre-keyframe audio still reaches OBS while the audio decoder warms up on garbage.
- Decode errors are ignored. The only flush sits on the seek path, which does nothing for network input, so a drained decoder stays drained. The readiness check then treats a drained decoder as satisfied, which is how video keeps playing over dead audio.
- Resolution changes usually survive, because a plain H.264 or HEVC stream needs no pixel format conversion and the new size passes through. Streams that do need conversion break: the scaler and its output buffer are sized from the first frame and never rebuilt.
- Hardware decoding is the same. It tries CUDA, D3D11VA, DXVA2, VAAPI, VDPAU, QSV and VideoToolbox in that order and falls back to software.
- Stats are duration, frame count and playback state, through `proc_handler` and the media controls. Enough for a progress bar, nothing about connection or buffer health.

## Hardware decoding

Auto-detection tries, in order:

| Platform | APIs tried |
| --- | --- |
| Windows | D3D11VA (Intel/AMD/NVIDIA), CUDA (NVIDIA NVDEC) |
| macOS | VideoToolbox (Apple Silicon & Intel) |
| Linux | VAAPI (Intel/AMD), CUDA (NVIDIA) |

Auto falls back to software decoding if no hardware decoder is available. **Hardware Decode: Off** forces software. **Hardware Decode: NVDEC** attempts only NVIDIA CUDA/NVDEC and never falls back to software; if the NVIDIA driver, GPU, or stream codec cannot provide NVDEC, the video decoder cannot produce frames. NVDEC uses the default CUDA device and is available in the settings on Windows and Linux; a scene collection that selects it and is opened on macOS falls back to Auto.

The OBS log shows which decoder was requested at stream open:

```
[irl-source] Video stream 0: hevc 1920x1080 (d3d11va)
```

The first-keyframe line reports the ground truth from the actual decoded frame, which is the one to trust:

```
[irl-source] First keyframe received (1920x1080 fmt=171 hardware decode)
```

## Transport

- Any FFmpeg-supported protocol: SRT, RTMP, RIST, UDP, TCP, HTTP.
- 2MB default transport buffer, tuned for SRT live streaming, overridable through FFmpeg Options.
- Any demuxer option can be overridden from the UI (latency, probesize, and so on).

## Stats reference

Stats are exposed through OBS's `proc_handler` API under the `get_stats` call, and under the same names through the obs-websocket vendor `GetStats` request (see [Reading stats over obs-websocket](#reading-stats-over-obs-websocket)).

| Field | Type | Description |
| --- | --- | --- |
| `buffer_fill_ms` | int | Current audio jitter buffer fill level (ms) |
| `current_speed` | float | Current audio correction factor. Buffered mode keeps this near 1.0. |
| `adaptive_latency_control` | bool | Whether buffered steady-state latency correction is enabled |
| `reconnecting` | bool | Whether the source is currently reconnecting |
| `total_audio_frames` | int | Total audio frames decoded since connection |
| `total_video_frames` | int | Total video frames decoded since connection |
| `pts_repairs` | int | Number of non-normal PTS discontinuities repaired |
| `pts_normalizations` | int | Number of frame-sized PTS cadence offsets normalized without treating them as damage |
| `pts_interpolations` | int | Number of non-frame-sized small PTS gaps smoothed by timestamp interpolation |
| `pts_resets` | int | Number of large PTS gaps that triggered a timing reset |
| `pts_last_gap_ms` | int | Most recent repaired PTS gap size |
| `pts_max_gap_ms` | int | Largest repaired PTS gap size since the current connection/reset |
| `silence_insertions` | int | Number of silence insertions for gap filling |
| `audio_underruns` | int | Number of plugin-side underruns that emitted silence to keep OBS audio timestamps monotonic |
| `audio_resync_skipped_chunks` | int | Number of buffered audio chunks skipped by low-latency backlog capping or startup trims |
| `audio_hidden_trimmed_chunks` | int | Number of buffered chunks trimmed before playback primed (never audible) |
| `audio_quality_events` | int | Aggregate audible-risk counter for underruns, inserted silence, resyncs, PTS resets, and audio decoder flushes |
| `audio_output_restarts` | int | Output clock restarts after the audio thread stalled (should stay 0) |
| `obs_lead_ms` | int | How far ahead of real time audio is queued inside OBS (healthy is roughly 60 to 100ms) |
| `audio_decoder_flushes` | int | Number of audio decoder flushes after repeated decode errors |
| `video_corrupt_frames` | int | Decoded frames the decoder flagged as damaged (concealed slice errors on H.264, missing-reference prediction on HEVC) |
| `video_corrupt_held` | int | HEVC frames held back instead of shown because they were predicted from a missing reference and would have rendered gray; the last good frame stays on screen until the next keyframe |
| `video_lead_ms` | int | How far ahead of real time the last video frame was timestamped. Tracks the audio buffer; a value climbing well past Target Buffer and staying there means concealment has inflated the A/V mapping |
| `video_lead_excess` | int | Frames whose lead exceeded what OBS's async queue can absorb. Harmless while the lead is steady; sustained growth is what makes OBS drop queued video |
| `stream_delay_ms` | int | End-to-end stream delay (SRT latency + decode + buffering) |
| `low_latency_audio` | bool | Whether OBS async unbuffered low-latency mode is enabled |
| `reconnect_count` | int | Number of reconnect attempts since the source was created |

### OBS log stats

The plugin also logs stats to the OBS log every 30 seconds:

```
[irl-source] Stats: video=1801 audio=2997 buf=100ms target=120ms speed=1.000 ctrl=on pts_repairs=0 norm=0 interp=0 silence=0 resets=0 last_gap=0ms max_gap=0ms underruns=0 resync_skips=0 hidden_trims=0 quality_events=0 audio_flushes=0 corrupt=0 held=0 obs_lead=99ms chunk=960@48000 stream_chunk=20ms obs_chunk=20ms restarts=0 res=1920x1080
```

A healthy stream shows `speed=1.000`, `underruns=0`, `restarts=0`, and a constant `chunk` size. `buf` plus `obs_lead` is your plugin-side latency (fill wanders inside a deadband around the target by design).

## Building from source

The plugin is a Rust workspace under `crates/`, built with cargo. It statically links its own FFmpeg, libsrt, librist and mbedTLS rather than using OBS's, so building that stack is the first step; it only has to happen again when a version in `deps/versions.env` changes. See `deps/README.md` for the details.

libobs is neither built nor linked: the plugin binds to it through hand-written FFI (`crates/obs-sys`) and resolves the symbols from the OBS process at load time. libclang is a build dependency, because bindgen generates the FFmpeg bindings during the build.

### Linux

```bash
sudo apt install build-essential cmake pkg-config nasm meson ninja-build \
    clang libclang-dev libobs-dev libva-dev
./deps/build-deps.sh
cargo build --release
./scripts/verify-plugin.sh target/release/libobs_irl_source.so
```

`libobs-dev` is not needed for the plugin itself. It is what lets `cargo test` link the test binaries that call libobs, and what `cargo test -p obs-sys --features layout-test` checks the hand-written structs against.

### Windows (MSVC)

Requires Visual Studio 2026 and LLVM (for libclang). `deps/build-deps.sh` runs under MSYS2 with the MSVC environment active, because FFmpeg's configure needs a POSIX shell even when it is driving `cl.exe`; the cargo build itself runs from a normal MSVC prompt. See the `windows-x64` job in `.github/workflows/build.yml` for the exact setup.

```powershell
$env:LIBCLANG_PATH = "$env:ProgramFiles\LLVM\bin"
cargo build --release
```

### macOS (Apple Silicon)

```bash
brew install cmake pkg-config nasm meson ninja
./deps/build-deps.sh
cargo build --release
./scripts/verify-plugin.sh target/release/libobs_irl_source.dylib
```

### Packaging

cargo names the artifact `libobs_irl_source.so`, `obs_irl_source.dll` or `libobs_irl_source.dylib`. `scripts/package.sh` renames it and stages the platform's install layout, which is what the release workflow runs:

```bash
scripts/package.sh linux target/release dist
```

Earlier versions dynamically linked the FFmpeg that OBS bundles, and OBS lines differ in FFmpeg major version, which is why release archives used to be built per OBS line. Bundling removed that constraint; version 2.0.0 removed the remaining CMake and libobs build steps with the move to Rust and cargo.
