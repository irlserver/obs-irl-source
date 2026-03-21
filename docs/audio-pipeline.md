# Audio pipeline

How the plugin keeps audio stable on unreliable mobile connections, and why an 80ms buffer works where Media Source needs seconds.

## The problem with big buffers

Media Source handles network jitter the simple way: buffer a lot of data, play it back with a delay. If the network hiccups, the buffer absorbs it. This works, but every millisecond of buffer is a millisecond of extra latency. For IRL streaming — where you're reading chat and reacting live — a 2-second buffer means a 2-second delay on top of everything else.

The plugin takes the opposite approach: keep the buffer as small as possible and actively compensate for the problems that a small buffer exposes.

## How it works

The audio pipeline has four layers that work together:

```
decode -> PTS repair -> jitter buffer -> adaptive speed -> OBS output
```

### 1. Jitter buffer (absorbs short-term network jitter)

A ring buffer sized in milliseconds, not bytes. Default settings:

| Parameter | Default | Purpose |
|---|---|---|
| Target | 80ms | Where the buffer tries to stay |
| Min | 40ms | Playback starts when buffer reaches this level |
| Max | 200ms | Upper bound before the buffer is considered overfull |

The buffer holds decoded audio (interleaved float PCM) regardless of the input codec. AAC, Opus, or anything else goes in; smooth PCM comes out.

Audio is pulled from the buffer by OBS's mixer thread via the `audio_render` callback, which reads 1024 samples (21.3ms at 48kHz) per tick. The adaptive speed controller varies how many source frames are read per tick to keep the buffer near its target level.

### 2. Adaptive playback speed (prevents buffer drift)

Even with the drain loop, the buffer level drifts over time due to clock differences between the sender and receiver, network throughput variation, and codec timing. The adaptive speed controller corrects this by micro-adjusting the playback rate.

When the buffer is above target, the plugin reports a slightly higher `samples_per_sec` to OBS (e.g., 50400 instead of 48000). OBS's audio subsystem resamples accordingly, effectively playing audio ~5% faster. When the buffer drops below target, it reports a lower rate to slow down.

A ±15ms dead zone around the target prevents oscillation. If the buffer is between 65ms and 95ms (with the default 80ms target), speed stays at 1.0 — no resampling, no artifacts. Speed correction only kicks in outside this range, scaling proportionally toward the configured min/max (0.95x/1.05x).

The adjustment range is 0.95x to 1.05x. Changes below 5% are inaudible — no pitch-shifting library is needed. An exponential moving average (500ms ramp) smooths the transitions so the rate doesn't jump between chunks.

The result: the buffer stays at 80ms indefinitely, even if the sender's clock drifts or the network throughput fluctuates. Media Source has no equivalent — its buffer either grows unbounded (increasing latency) or drains (causing stuttering).

### 3. PTS repair (handles timestamp discontinuities)

Mobile connections drop packets. Cell tower handoffs cause gaps. SRT retransmissions arrive late. All of these produce gaps or jumps in the audio PTS (presentation timestamp) that confuse the decoder and cause audible artifacts.

The PTS repair system classifies gaps into three tiers:

| Gap size | Action | What it sounds like without repair |
|---|---|---|
| < 70ms | **Interpolate** — replace the PTS with the expected value (last PTS + last duration). The gap was probably just jitter. | Brief audio stutter or pop |
| 70ms – 2s | **Silence insertion** — keep the PTS but insert the appropriate duration of silence before the frame. Something was actually lost. | Loud click followed by audio jump |
| > 2s | **Full reset** — flush the buffer, re-arm the keyframe gate, restart from scratch. The stream has fundamentally changed. | Extended silence, possibly wrong audio |

Thresholds are configurable (Small Gap and Large Gap settings in the UI).

### 4. Fade in/out (eliminates clicks on disconnect)

When the stream drops, the last audio chunk in the buffer gets a 50ms linear fade-out (gain ramp from 1.0 to 0.0). When the stream reconnects and audio resumes, the first chunk gets a matching fade-in. This prevents the sharp transient that sounds like a loud click/pop.

## Timestamp handling

OBS expects audio timestamps in its system clock domain (`os_gettime_ns()`). Live streams use MPEG-TS PTS values that can be hours or days into an arbitrary epoch — passing these raw causes OBS to report "audio is lagging by millions of ms" and restart the source repeatedly.

The plugin uses OBS's **pull-based `audio_render` callback** instead of the push-based `obs_source_output_audio()`. The mixer thread calls `irl_audio_render()` every ~21.33ms, asking for exactly 1024 samples of planar float32 at the mixer's sample rate. The plugin reads from its jitter buffer, resamples if needed (source rate may differ from mixer rate), and provides a simple monotonic timestamp.

**Why pull-based?** Push-based audio goes through OBS's `source_output_audio_data()` which applies timestamp smoothing (`TS_SMOOTHING_THRESHOLD = 70ms`), `timing_adjust`, and lag detection. The valid range for `audio_ts` is only ~21ms wide, but the smoothing window is 70ms — an architectural mismatch that causes persistent "audio is lagging" cascades on live streams with any timing jitter. The `audio_render` callback **bypasses all of this**: no smoothing, no `timing_adjust`, no `discard_audio`, no lag detection cascade. We set `audio_ts` directly.

**Adaptive speed** works by varying how many source frames are read per callback. At speed 1.05x, 1075 frames are read and resampled to 1024 output frames. This is cleaner than the push-based approach of lying about `samples_per_sec`.

Video uses a similar rebasing approach (anchoring stream PTS to the system clock via `video_sys_base` / `video_pts_base`).

## What this means in practice

| Scenario | Media Source | IRL Source |
|---|---|---|
| Stable connection | Works fine, but adds seconds of latency | Works fine, adds ~80ms of latency |
| Brief packet loss (< 70ms) | Audio pop, possible stutter | Interpolated silently, inaudible |
| Cell tower handoff (100-500ms gap) | Loud click, audio jumps ahead | Silence inserted, smooth transition |
| Sender clock drift | Buffer grows forever, latency increases | Speed adjusts, buffer stays at 80ms |
| Connection drops and reconnects | Loud click on disconnect, possibly corrupted frames on reconnect | Fade out, clean reconnect, keyframe gate, fade in |
| Long stream (hours) | Timestamp epoch causes OBS sync issues | Timestamps anchored to system clock |

The tradeoff: if the network drops for longer than the max buffer (200ms), there's no cushion left and you'll hear it. But for SRTLA with bonded connections, sustained 200ms+ gaps are rare — and when they happen, you'd rather know immediately than have the problem hidden behind seconds of buffer.
