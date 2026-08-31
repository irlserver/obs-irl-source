# Audio pipeline

How the plugin keeps audio stable on unreliable mobile connections, and how the buffered and low-latency modes differ.

## Quality policy

The output policy is viewer-first:

- Prefer short silence over jittery, glitchy, metallic, or artifacty audio.
- Prefer smooth video cadence; timestamped damaged frames are better than freezes when they are still a picture (H.264 concealment), while gray frames (HEVC missing references, which are held back instead), blank frames and decoder reset storms should be avoided.
- Prefer bounded latency movement over aggressive time-stretching.
- Expose every recovery mechanism in stats, so tuning runs off counters instead of guesswork.

## The problem with big buffers

Media Source handles network jitter the simple way: buffer a lot of data, play it back with a delay. If the network hiccups, the buffer absorbs it. This works, but every millisecond of buffer is a millisecond of extra latency. For IRL streaming, where you're reading chat and reacting live, a 2-3 second buffer means a 2-3 second delay on top of everything else.

The plugin takes the opposite approach: keep the buffer as small as possible and compensate for the problems that a small buffer exposes.

## How it works

The buffered audio pipeline has four stages:

```
decode -> PTS repair -> jitter buffer -> adaptive latency control -> OBS output
```

Low-latency mode uses a shorter path:

```
decode -> PTS repair -> minimal buffer -> OBS output
```

In that mode the plugin still repairs discontinuities and keeps monotonic OBS-facing timestamps, but it starts playback as soon as audio exists and it does not use buffered latency correction.

### 1. Jitter buffer (absorbs short-term network jitter)

A ring buffer sized in milliseconds, not bytes. Only the target is a user setting; the watermarks derive from it:

| Parameter | Value | Purpose |
|---|---|---|
| Target | 120ms (setting) | Where the speed controller tries to keep the buffer |
| Min | target / 2 | Low watermark. The speed controller slows playback as fill approaches this level |
| Max | target + 200ms | High watermark. The speed controller reaches maximum drain speed at this level |

The buffer holds decoded audio (interleaved float PCM) regardless of the input codec. AAC, Opus, or anything else goes in, PCM comes out.

The target can be retuned while the stream is live. Ring storage grows if the new maximum needs more room (it is never shrunk, since that would mean discarding queued audio), the watermarks move, and the fill is left alone for the speed controller to walk to the new target. Nothing is dropped and the connection is not restarted.

Playback primes once, when the buffer first reaches the target plus the fixed output lead. After priming, the output pump always emits: real audio when the buffer has data, shaped concealment silence when it does not. There is no fill level below which the pump stops feeding OBS: starving the OBS mixer produces a tick of silence plus a splice discontinuity (crackle) and can cause OBS to permanently add global audio buffering.

### 2. Adaptive latency control (prevents latency creep)

The buffer level drifts over time due to network throughput variation, clock mismatch, and decoder recovery events. Bounded playback speed correction is the only steady-state latency control.

Speed is applied inside the plugin with a persistent swresample compensation (the same mechanism ffplay uses for audio clock sync). The sample rate submitted to OBS never changes, because libobs destroys and rebuilds its per-source resampler with no crossfade whenever `samples_per_sec` changes, which produces a click per change.

The controller is PI — a fast proportional ramp with a deadband and asymmetric authority, plus a slow integral trim underneath it.

The **ramp** owns transients: below the target it slows toward 0.98x (building buffer, inaudible), above the target it speeds toward the Catch-Up Speed, 1.05x by default (draining backlog, a mild chipmunk effect). Draining 1s of backlog takes about 20s at full authority. Within ±20ms of target it is nearly flat, sloping to only ±0.2% at the edges of that band.

The **trim** owns the constant underneath, and exists because a proportional loop mathematically cannot hold one. Two independent clocks never agree exactly, and a sender configured against the wrong frame rate is off by ~0.1% by construction. A sender at 1.003x delivers 3ms of extra audio every second, forever; the only ramp position that consumes that is one with a permanent level error, so the buffer parks off-target and the latency parks with it. Simulated at the default target, a proportional-only loop parks 24ms high on a 0.3% fast sender and 22ms low on a 0.3% slow one, against ~1ms for the PI loop. Since every stream has some drift, that is latency every stream was carrying for nothing.

Those figures come from a simulation with a continuous buffer level, and overstate the achievable precision. On real audio the level moves in whole decoded chunks — 21.3ms for 1024-sample AAC frames — because writes and reads are both whole chunks, so a 120ms target is not a reachable state and the loop sits at 106ms or 128ms. What the trim actually buys is the removal of the systematic offset, which is worth tens of milliseconds on a drifting sender; sub-chunk accuracy is not a meaningful claim, since the controller cannot see below one chunk.

The trim is clamped to ±1% — far more than any real crystal (<0.01%) or frame-rate mismatch (~0.1%) needs, far below audibility (±1% is 17 cents), and small enough that the ramp keeps essentially all of its authority for real transients. It converges over a minute or two, survives a decoder flush, and resets on a reconnect or PTS reset where the next stream may not be the same encoder.

Three details make it safe rather than a new source of oscillation, and all three were defects before they were features:

- It integrates only within ±60ms of target, and never while the issued command is pinned at 0.98x or the catch-up ceiling. Outside those the level is reporting a backlog draining or a buffer starving — a transient — not the sender's rate. Without this gate the loop learns "the sender is fast" from a network stall. The pin test is on ramp + trim, not the ramp alone, because the actuator clamps their sum.
- The deadband slopes gently instead of being flat. A region with zero proportional feedback leaves an integrator undamped, and the pair limit-cycles through it indefinitely (simulated: ±20ms of buffer on a ~2 minute period, never settling). The 0.2% slope restores damping and is roughly 3.5 cents at its steepest.
- The level it reads is a ~2.5s EMA, not the instantaneous fill. Plenty of upstreams do not hand over a smooth stream — a remux hop delivers in batches — and the level then sawtooths across the whole ramp at the batch period. Chasing that modulates playback speed by 1-3%, which is audible as pitch wobble and, since video rides the audio playout offset, visible as judder at the same time. The smoothing is far longer than any batch period and far shorter than a post-stall drain.
- The speed request is applied with a fractional sample carry. The resampler is driven in whole samples per chunk, which quantises the applied speed to ~0.1% steps at 1024 frames — so before the carry existed, a requested +0.02% was discarded and a requested +0.05% came out at +0.098%. That is the whole range the slope and most of the trim operate in.

Notably, the trim converges to the sender's clock rate **without measuring it**. An earlier revision did measure it directly, and that estimator was deleted: see [audio-timing-pitfalls.md](audio-timing-pitfalls.md) for what it cost and why a measurement that cannot reach playback is worth more than one that converges faster.

The whole loop is in `crates/irl-core/src/speed.rs`, which is plain data in and plain data out, so it is unit-tested directly. `cargo run -p irl-core --example speed-controller-sim` drives that same code closed-loop against a simulated sender.

Backlog is never skipped once playback has primed. When a stall ends and delayed data floods back in, everything gets played, sped up, until latency returns to the target. Above a fill ceiling (about 1s) the receiver stops reading from the transport, so the excess buffers at the sender or in the TCP path instead of overflowing the local ring buffer. With an RTMP encoder that buffers during congestion, the stream pauses and resumes exactly where it stopped, then bleeds the extra delay off over the following minutes. SRT bounds its own backlog through the latency window, so this ceiling rarely engages there.

Recovery rules:

- Startup backlog is trimmed before playback primes (nothing was audible yet, so the trim is free). This is the only trim path.
- Once audio is audible, the plugin never trims old chunks. Extra delay is preferable to an audible skip, pop, or cadence discontinuity.
- If the buffer underruns, the pump emits concealment silence that decays from the last played sample, and the first real chunk after the dropout gets a short fade-in.
- If timestamps or decoder state go bad, the plugin flushes damaged state and re-enters playback cleanly instead of trying to stretch through corruption.

On stable links the correction should be inaudible; on unstable links it should not add artifacts of its own. If damaged audio cannot be made to sound natural, silence is preferred.

### 3. PTS repair (handles timestamp discontinuities)

Mobile connections drop packets, cell tower handoffs cause gaps, and SRT retransmissions arrive late. Each produces a gap or jump in the audio PTS (presentation timestamp) that confuses the decoder and causes audible artifacts.

The PTS repair system classifies gaps into three tiers:

| Gap size | Action | What it sounds like without repair |
|---|---|---|
| < 70ms | **Interpolate**: replace the PTS with the expected value (last PTS + last duration). The gap was probably just jitter. | Brief audio stutter or pop |
| 70ms to 2s | **Silence insertion**: keep the PTS but insert the appropriate duration of silence before the frame. Something was actually lost. | Loud click followed by audio jump |
| > 2s | **Full reset**: flush the buffer, reset timing state, and re-enter playback from scratch. The stream has fundamentally changed. | Extended silence, possibly wrong audio |

The thresholds are fixed constants (`IRL_SMALL_GAP_MS` 70ms, `IRL_LARGE_GAP_MS` 2000ms). They were once exposed as Small Gap and Large Gap settings, but nobody could reason about them without reading the source. Each threshold marks a real boundary: below 70ms is decoder timestamp wobble, above 2s the stream has changed underneath the plugin.

`pts_repairs` tracks non-normal PTS discontinuities. For tuning, use the split diagnostics: `pts_normalizations`, `pts_interpolations`, `silence_insertions`, `pts_resets`, `pts_last_gap_ms`, and `pts_max_gap_ms`. A high normalization count with low silence usually means frame-sized timestamp cadence smoothing, not packet-loss concealment.

### 4. Fade in/out (eliminates clicks on disconnect)

When the stream drops, the last audio chunk in the buffer gets a 50ms linear fade-out (gain ramp from 1.0 to 0.0). When the stream reconnects and audio resumes, the first chunk gets a matching fade-in. This prevents the sharp transient that sounds like a loud click/pop.

## Timestamp handling

OBS expects audio timestamps in its system clock domain (`os_gettime_ns()`). Live streams use PTS values in a stream-local epoch, and decoded frames may occasionally arrive with missing or damaged timestamps. Passing these raw causes OBS to report "audio is lagging" and restart the source repeatedly.

### Audio timestamps

Timestamps are a pure sample counter anchored once at prime time: `ts = anchor + samples_emitted / rate`. Every submitted timestamp is exactly contiguous with the previous one, whether the chunk was real audio or concealment silence.

This shape is dictated by how libobs treats submitted audio timestamps. Deviations under 70ms from the expected next timestamp are smoothed away (the seamless append path). Gaps between 70ms and 2s are placed by timestamp with zero-filled silence, which is audible. Jumps beyond 2s flush all queued audio for the source. A sample counter keeps the plugin on the seamless path permanently.

The wall clock is consulted for exactly two things:

1. **Pacing.** The pump emits whenever the end of submitted audio is less than a fixed lead (about 80ms) ahead of `os_gettime_ns()`. The lead absorbs thread scheduling jitter plus one OBS mixer tick (21.3ms), so the mixer never runs dry between submissions.
2. **Stall detection.** If the output clock falls far behind wall clock (audio thread starved, machine suspended), the clock line is restarted once, explicitly counted in `audio_output_restarts`, instead of letting OBS add permanent buffering for a late source.

The plugin separately tracks the source-side end PTS for the audio it has submitted. Video sync uses that mapping instead of assuming a fixed buffer delay.

When decoded audio frames arrive without a usable PTS, the plugin falls back to `best_effort_timestamp` and only synthesizes continuity from the previous repaired PTS when necessary. Frames with no safe starting point are dropped instead of pushing broken timing into OBS.

### Video timestamps

Video uses a rebasing approach: the first frame's stream PTS is anchored to `os_gettime_ns()` via `video_sys_base` / `video_pts_base`. Subsequent frames compute their timestamp as `video_sys_base + (frame_pts_ns - video_pts_base)`, preserving the inter-frame timing from the stream.

Instead of a fixed audio offset, video is delayed by the current buffered-audio age when audio exists. That tracks the real state of the audio path better than always adding the configured target buffer.

When there is no audio playout mapping yet (audio-less start), video falls back to the rebased timestamp, and if that drifts too far from wall clock (over 500ms) it is clamped rather than fully re-anchored. That avoids visible jumps while still preventing long freezes if the stream sends a bad future timestamp.

## What this means in practice

| Scenario | Media Source | IRL Source |
|---|---|---|
| Stable connection | Works fine, but adds seconds of latency | Buffered mode adds the target buffer plus a fixed output lead (about 200ms total at defaults), low-latency mode keeps the source much closer to real time |
| Brief packet loss (< 70ms) | Audio pop, possible stutter | Interpolated silently, inaudible |
| Cell tower handoff (100-500ms gap) | Loud click, audio jumps ahead | Silence inserted, smooth transition |
| Sender clock drift / slow latency creep | Buffer grows forever, latency increases | Bounded speed correction drains the creep gradually while video stays synced to audio |
| RTMP congestion with a buffering encoder | Stream skips ahead or dies | Stream pauses, resumes exactly where it stopped, and bleeds the extra delay off at up to the Catch-Up Speed (+5% by default) |
| Connection drops and reconnects | Loud click on disconnect, possibly corrupted frames on reconnect | Fade out, clean reconnect, keyframe gate, fade in |
| Decoder corruption | Gray/corrupt flicker until manual restart | H.264: timestamped concealed frames are passed through to preserve cadence. HEVC: frames predicted from a missing reference (rendered gray by FFmpeg) are held back until the next keyframe. The video decoder is never flushed; only the audio decoder is, on repeated hard errors |
| Long stream (hours) | Timestamp epoch causes OBS sync issues | Timestamps are repaired and anchored to system clock |

The tradeoff: buffered mode is more resilient to short stalls, but adds intentional latency. Low-latency mode reacts faster and works better with OBS async unbuffered audio, but it gives up most of that jitter cushion. For rough SRTLA field conditions, buffered mode should still be the default. Low-latency mode is there when absolute latency matters more than smoothing over short network wobble.
