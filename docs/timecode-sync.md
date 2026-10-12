# Timecode sync

Multiple feeds (a main camera and a chase camera, say) arrive over different
uplinks with different delays, so cutting between them jumps backwards and
forwards in time. Timecode sync fixes that: each feed is held until the moment
it was *captured*, plus a shared offset, so every synced feed shows the same
instant at the same instant.

The alignment is absolute, not relative. Feeds never negotiate with each other
and none of them needs to know the others exist: they simply all present the
frame captured at time T at time T + offset. Two OBS instances anywhere, on the
same NTP reference and the same offset, therefore show the same moment at the
same moment. That is what makes the offset one shared number rather than a
per-source setting, and it is why co-streamers only have to agree on two things:
the NTP pool and the offset.

Timecode sync was designed and written by [Spillmaker](https://github.com/Spillmaker)
for the C plugin (the `nal-timecodes` branch) and ported to the Rust plugin
from there. This document is his, adapted to where the code lives now.

## What you need

Sync reads SEI timecodes embedded by the sender, so the sender has to put them
there. Today that means **Moblin**:

- Settings → Streams → *(your stream)* → Video → **Timecodes**, with an **NTP
  pool** configured (Moblin will not stamp anything without one).
- **H.265/HEVC** only, over **SRT, SRTLA or RIST**. Moblin's H.264 path is
  disabled in its own source, and its RTMP path never carries the timecode.

Then, in OBS: tick **Sync** on each source and open **View → Docks → IRL
Sync**. Click the server name on the reference clock to set the same NTP pool,
click the offset badge to set the offset, and turn the switch on.

Without the dock (a build made without Qt, or a headless OBS) the same three
settings are set over obs-websocket with `SetSyncConfig`; see
[below](#reading-status-over-obs-websocket).

## Choosing an offset

The offset is set in **whole seconds**, because it is a number co-streamers
read to each other and "six" is something two people can agree on over a call.

It must be larger than the feed's real arrival latency, or its frames are
already past their slot when they get here. The dock lists every source with
Sync ticked, and shows:

- **Latency**: how stale the freshest data is. A property of the sender and
  the network; nothing you set changes it.
- **Added**: how much extra hold the plugin is applying to reach the target.
- **Drift**: how far the actual presentation lands from the target. Near zero
  once locked, and a residual rather than a fault: the loop is what corrects
  it. Averaged over about a second, because the per-packet reading
  carries roughly a frame of noise that the alignment underneath it does not.

Changing it is a several-second step for every synced feed at once, so it
happens on OK in the dialog rather than as you type.

A feed that cannot reach the offset turns red and says the value that would fix
it: its rolling 60 second peak latency, rounded up to a whole second. The peak
rather than the instantaneous value, because bonded cellular does not degrade
gently and an offset chosen against the current reading holds right up until
the first bitrate dip. Raise the offset to that, or pass the number to whoever
is holding the phone.

Raising the offset raises latency for everyone, so it is a real cost. Sync
means every feed waits for the slowest one.

## What the statuses mean

| Status | Meaning |
|---|---|
| **Locked** | Aligned, within a frame or two |
| **Acquiring** | Converging. Normal at startup and after an offset change |
| **Too slow** | The feed arrives later than the offset. Raise the offset, or improve that uplink |
| **No timecode** | Nothing to align against. The dock says which of three causes it is: *no NTP reference here*, *stream is not H.265*, or *sender is not stamping*. A sender whose timecodes read minutes off this machine's clock (its clock is not on NTP) also lands here, and the OBS log says so. Not a sync failure, and the offset will not help |
| **No data** | The feed stopped delivering |
| **Off** | Nothing to align: the master switch is off, this source is not ticked, Low Latency Audio is on for it, or no stream is arriving |

"No timecode" is by far the most common reason sync appears not to work, and it
is always a sender-side configuration problem.

## Accuracy, and what it is not

Expect **within one or two frames**. The error budget is NTP discipline at both
ends (a few ms each), the timecode's own one-frame resolution, and OBS's render
tick. That is well inside what anyone notices between camera angles.

Sync holds at the point of composition, not at the viewer. Two OBS instances
will composite the same captured moment at the same time, but their outbound
encode and CDN paths still differ, so a viewer watching two *streams* side by
side sees them offset by that difference. No plugin can control it.

## How it works

Each synced source holds encoded packets in front of its decoder until they are
due. Holding compressed data is what makes a multi-second offset practical:
five seconds of a 6 Mbit/s feed is under 4 MB, while the same five seconds of
decoded 1080p60 would be several gigabytes.

Nothing about the audio pipeline changes. Delaying the input shifts the whole
PTS-to-OBS mapping with it, so the jitter buffer, the adaptive speed controller
and the video pacing keep doing exactly what they did; they just see the stream
arrive later.

The alignment itself is made once, not converged on. When a source's audio
output starts, its clock is anchored where the timecodes say that audio belongs;
nothing is playing yet, so choosing that instant costs only pre-roll. After
that the only thing left is real drift (the sender's clock against ours, and
the network moving), which is slow enough for the existing speed correction to
absorb inaudibly.

The plugin runs its own SNTP client rather than trusting the system clock,
which on Windows defaults to roughly one second accuracy, two orders of
magnitude too coarse for this. Moblin does the same on its end, for the same
reason.

It polls from the moment the plugin loads, not from the moment sync is switched
on, so the dock's clock is running before you commit to anything. That is what
you read to decide whether sync is worth turning on at all.

### Which server you set changes how it polls

How hard a client may poll is a property of the server, so the server name
picks the policy.

**A server run for this plugin** (today `ntp.kringkast.com`, which is the
default) is there to be asked. The client sends four packets on arrival and
keeps the one with the lowest round trip, then one packet every 1 to 30
seconds, at random. Random rather than fixed so that every OBS on the network
does not poll in step.

That sample rate is what pays for the second number on the clock line: the
**ppm** figure is this machine's own crystal error, measured by fitting the
last quarter hour of samples. A PC that runs 40 ppm fast loses 2.4ms of
alignment every minute, which is a frame and a half at 60fps, and on this
policy the client extrapolates that away between polls instead of waiting for
the next packet to correct it.

**A public pool** (`pool.ntp.org`, or any other host you type in) is donated
capacity. There the client sends one packet per poll, every 64 seconds, doubles
that interval if the server answers with a rate-limit kiss-o'-death, and stops
polling altogether if it answers with a denial. The crystal error is still
measured and shown, but it is not applied: one sample a minute is too sparse a
fit to hand the timeline to.

If the configured server stops answering, three failed polls in a row hand over
to `pool.ntp.org` so the clock keeps running, and the clock line says
`fallback` while the server name above it changes to the pool. The original is
probed again every 30 to 60 seconds (randomised again, so that everyone
knocked off by one outage does not all come back at the same instant), and the
client returns to it on the first answer.

The master switch, the offset and the NTP server are stored per machine (in the
plugin's own config directory, as `sync.json`), not in the scene collection. A
scene collection copied to another machine brings its sources but not its
offset, which is intended: the offset describes an agreement between people,
not a layout.

## Building the dock

The IRL Sync dock is the only thing in the plugin that needs Qt. `cargo build`
builds it in when Qt6 Widgets is found, and builds without it (with a
`cargo::warning` saying so) when it is not. Sync itself runs headless either
way, and its status stays readable (and its settings writable) over the
obs-websocket vendor; you just get no dock.

- **Linux:** `sudo apt install qt6-base-dev`; `pkg-config Qt6Widgets` is what
  the build looks for.
- **Windows and macOS:** point `IRL_QT_PREFIX` at a Qt install. The
  `windows-deps-qt6-*` and `macos-deps-qt6-*` archives from the same
  [obs-deps](https://github.com/obsproject/obs-deps/releases) release OBS is
  built with have the right layout, and CI uses exactly those.
- `IRL_DOCK=0` skips the dock deliberately.

The dock links the Qt that the host OBS already loaded; nothing Qt is bundled.

## Reading status over obs-websocket

The plugin's vendor (`obs-irl-source`, see the root
[README](../README.md#reading-stats-over-obs-websocket)) adds two requests:

| Request | Params | Returns |
|---|---|---|
| `GetSyncStatus` | none | `sync_enabled`, `offset_ms`, `clock`, `sources`, `out_of_sync_count`, `recommended_offset_ms` |
| `SetSyncConfig` | any of `sync_enabled` (bool), `offset_ms` (int, clamped to 0 to 30000), `ntp_server` (string; empty stops polling) | `sync_enabled`, `offset_ms`, `ntp_server` as they stand afterwards |

`GetSyncStatus` is the alerting path for timecode sync. A dock only helps when
somebody is looking at it, and during a live show the person who can actually
fix an out-of-sync feed is the one out in the field, so a bot polling this can
put "chase cam out of sync, needs 7.4s" in chat, where it will be seen.
`out_of_sync_count` and `recommended_offset_ms` are pre-computed so a bot does
not have to reimplement the policy, and `SetSyncConfig` lets the same bot act
on the recommendation. `clock` reports the NTP reference as `{synced, server,
primary_server, offset_ms, rtt_ms, age_ms, utc_ms, burst_mode, fallback_active,
drift_valid, drift_ppm}`; a large `age_ms` means the reference has gone stale
and everything downstream of it is suspect, and `fallback_active` means
`server` is the pool the client failed over to rather than the
`primary_server` it was asked for. Each entry in `sources` carries
`source_name`, `sync_enabled`, `status`, `timecode`, `latency_ms`,
`latency_peak_ms`, `added_ms`, `error_ms`, `required_offset_ms` and
`timecode_reason` (`ok`, `no_clock`, `codec` or `absent`), which says which of
the three causes of a `no_timecode` status applies.

Sync status is not part of a source's `get_stats`: everything a source reports
about sync is in its `sources` entry here.

---

# Working on this code

This half is for someone changing the feature rather than using it.

## Files

The feature is split by what each piece needs, following the workspace's rule
that everything FFI-free lives in `irl-core` and is unit-tested there.

| file | what it owns |
|---|---|
| `crates/irl-core/src/sei.rs` | Annex B scan and `time_code` decode |
| `crates/irl-core/src/sync.rs` | the controller as pure state: SEI to latency, the hold loop, the error filter, the frame-rate learner, the latency peak, the playout anchor arithmetic, and the generic `DelayLine` |
| `crates/irl-core/src/ntp.rs` | the SNTP wire format, the server policy, the Theil-Sen drift fit and the offset filter |
| `crates/irl-core/src/consts.rs` | every tuning value, pinned by `consts_are_pinned` |
| `crates/irl-source/src/sync/mod.rs` | global settings and their store (`sync.json`), the source registry, the websocket handlers, module lifecycle |
| `crates/irl-source/src/sync/control.rs` | the controller on the receiver thread: the delay line of `ffmpeg::Packet`s, the pipeline inputs, the log lines, the audio-side anchor placement |
| `crates/irl-source/src/sync/ntp.rs` | the poll thread: sockets, schedule, failover |
| `crates/irl-source/src/sync/dock.rs` | the backend the dock reads and writes through |
| `crates/sync-dock/` | the Qt dock itself: `cxx/sync-dock.cpp` is the C plugin's widget, `cxx/irl-sync-dock.h` the C ABI between it and Rust, `src/lib.rs` the Rust side of that boundary, `build.rs` the Qt probe |

## How it attaches to the plugin

Outside `sync/` the feature is a handful of calls, each marked with a
"Timecode sync" comment so the whole set greps out at once:

```
grep -rn "Timecode sync\|timecode sync" crates/irl-source/src --include=*.rs
```

| file | calls |
|---|---|
| `lib.rs` | `sync::module_load` / `module_post_load` / `module_unload` |
| `receiver/mod.rs` | `sync_wait_for_room`, `sync.intercept`, `sync_drain`, the wait until `sync.next_release_ns`, `sync.reset` at the end of a run |
| `receiver/stream.rs` | `sync.set_video_stream` and `sync.set_audio_stream` at open, `sync.reset` on a new connection and on disconnect |
| `audio/pump.rs` | `prime_held`, `fresh_anchor_ns` at prime, at the stall restart and at the concealment re-anchor |
| `source.rs` | register / unregister, `set_source_sync_enabled` |
| `settings.rs` | the `Sync` checkbox and its help text |
| `websocket.rs` | `GetSyncStatus`, `SetSyncConfig` |
| `shared.rs` | `HotConfig::sync_enabled`, `AudioState::sync` (the present bias and the pre-roll handback, under `audio_state`), `Shared::sync_flags` (the priming gate and the handback flag, atomics) |

The receiver thread owns `SyncControl`. Nothing else looks inside it: the
audio thread reads the two atomics and the `AudioState::sync` link, and the
OBS thread reads the registry.

## Control model

Two mechanisms, and it matters which does what.

**The anchor places the stream. Once.** When the audio output primes, its clock
is anchored. That is the only moment the stream can be positioned exactly and
for free, because nothing is playing yet and starting later costs only pre-roll.
`irl_core::sync::playout_anchor` says where the audio in hand belongs, and the
output starts there. The anchor can only wait, never start early, so the hold
seeded at engage errs early by `SYNC_SEED_BIAS_NS` and the anchor absorbs the
difference. Every restart of the clock line (prime, the concealment
re-anchor, the stalled clock restart) goes through the same door, so an outage
costs the audio it costs but not the alignment.

**The loop only holds it there.** After placement the residual is real drift:
the sender's clock against ours, and the network moving. Tens of milliseconds
over minutes, which the existing adaptive speed controller absorbs inaudibly.
The loop measures where presentation will settle, not where it is: the jitter
buffer's distance from its target beyond the speed controller's deadband is
taken out of the error, because the speed controller removes it on its own. A
loop that also steered it out would fight the speed controller, and the two
would swing each other by a couple of hundred milliseconds.

The delay line is *storage*, not steering. Moving the hold does not move the
presentation time. It moves content between the delay line and the jitter
buffer, and the total delay is unchanged. Only the speed controller draining or
building that buffer actually presents anything earlier or later, at +5% and
-2%. Every rate in the controller is set under those two numbers for that
reason, and no amount of tuning here can beat them.

## Things that will bite

- **The frame is the resolution floor.** `n_frames` is a 9-bit index within its
  timecode second, so capture time is known to one frame: 33 ms at 30 fps.
  Frame-accurate alignment is reachable; phase-coherent audio mixing is not.
- **The frame rate is never assumed.** It is learned from the timecodes by
  agreement between recent seconds, not by taking the maximum: a corrupt SEI
  once made a 30 fps sender look like 41 fps and put a sawtooth into every
  latency reading.
- **Latency arithmetic is modulo a quarter hour.** A sender may write local
  calendar components, every time zone is a whole number of quarter hours from
  UTC, and the timecode carries no date, so zones and midnight all fold out. A
  reading outside -2 s to 60 s is a broken stamp or a sender clock off NTP,
  and is ignored rather than seeded into a hold of minutes.
- **Nothing is seeded from video that is catching up.** A relay replays from
  its last keyframe at connect, and the probe reads a backlog, both of which
  read seconds of latency no live packet has. The controller waits for video
  to arrive at its live pace (`irl_core::arrival`) before it seeds the hold or
  records the latency peak.
- **Without audio there is nothing to steer.** The video-only fallback anchors
  at the first frame and the hold cannot move it, so a stream without audio
  is placed open loop by the seed, with no bias, and never corrected.
- **Low Latency Audio is left alone.** It has no speed controller to absorb a
  hold change and no jitter buffer to anchor in, so sync does not act on it.
- **A mid-stream re-engage is a step.** If a source's timecodes go away for
  longer than `SYNC_TC_STALE_NS` and come back, the hold is seeded again on a
  running pipeline, which starves it for the size of the step. The priming
  gate only covers the first engage.
- **`now` cancels out of the target.** `target = now + offset - (now - capture)`
  is `capture + offset`, so arrival jitter is not in the error signal. Filtering
  the latency would put it back in.
- **Priming is gated.** Engage seeds a multi-second hold, and a hold applied to
  an already-playing pipeline starves it. The priming gate keeps the audio
  output from priming until the delay line is flowing.
- **A hold change takes a hold to arrive.** The delay line is a queue holding
  seconds of content, so the loop waits one settle interval between corrections
  or it winds itself up. The pre-roll takeover at prime is the exception: it
  shifts the queued packets too, so it lands immediately.
- **Growing the hold starves; shrinking it floods.** Both are rate-limited, and
  the shift helper only ever moves release times later.
- **Release follows the timeline, not arrival.** Each packet is released at
  its DTS plus its stream's floor (the lowest `arrival - DTS` seen) plus the
  hold (`irl_core::sync::ReleaseClock`). A packet that a gap on the link
  delayed still gets the slot it would have had, so a gap shorter than the
  hold never reaches the decoders. Released on arrival plus the hold, the gap
  would pass straight through, a hold later. Each stream has its own floor
  and its own queue: the skew between audio and video on the wire is kept,
  so the audio hold reads what it would without sync, while each stream's
  jitter is taken out. A stream whose DTS goes back by more than a second
  restarted its timestamps, and the floors start over. The read runs on a
  reader thread so that releases go on while `av_read_frame` is blocked.

## Testing

`irl-core`'s `sync` module drives the whole controller on a virtual clock
against a simulated sender (`World` in its tests): engage, the priming gate and
its deadline, the once-per-settle correction, lock and alarm hysteresis, offset
steps in both directions, the release rate, staleness, the pre-roll takeover
and the 5 Hz publish cadence. `sei` builds real Annex B access units with
emulation prevention and checks the parser against them; `ntp` checks the wire
format, the kiss-o'-death codes and the drift fit against a synthetic crystal.

`crates/irl-source/tests/av_sync_sim.rs` runs the real sync stage, audio pump
and video thread on one virtual clock against a sender that stamps timecodes,
and measures when each frame reaches the screen against its capture time plus
the offset: a clean link, two links a second apart showing the same instant,
video behind its audio with the audio hold in force, a relay catching up,
stalls shorter and longer than the hold, a stream without audio, and Low
Latency Audio. Run it whenever you
touch the controller, the anchor or the playout mapping.

What none of that covers is a real sender. As with the rest of the plugin,
that validation is manual: a Moblin feed with Timecodes on, the dock, and the
`Sync` lines in the OBS log.
