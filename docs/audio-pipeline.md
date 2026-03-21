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

Audio is output in 20ms chunks. The output loop drains multiple chunks per decoded frame if needed, keeping the buffer near its target. Without this, a codec producing frames larger than 20ms (AAC's 1024 samples at 48kHz = 21.3ms) would cause the buffer to grow by 1.3ms per frame — eventually overflowing and silently dropping audio.

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

The plugin uses **direct timestamps** — `os_gettime_ns() + 25ms` — with PLL correction that snaps for large errors. The PTS is advanced by the exact sample count of each output chunk.

**Why +25ms headroom?** OBS checks `audio_ts < ts.start` to detect lagging sources. With ~0ms of OBS audio buffering, `ts.start ≈ os_gettime_ns()`. Any push whose timestamp is even 1ms behind triggers the check. The 25ms headroom (one mixer tick) keeps `audio_ts` consistently ahead of `ts.start`. Direct timestamps (within 2 seconds of the system clock) are essential — OBS sets `timing_adjust = 0`, preserving the headroom. Non-direct timestamps don't work because OBS's `timing_adjust` absorbs any constant offset.

**PLL with snap:** For normal jitter (<50ms), 25% correction per chunk keeps PTS smooth. For network stalls >50ms (PTS freezes while the clock advances), PTS snaps directly to `os_gettime_ns() + 25ms`. This snap also exceeds OBS's 70ms `TS_SMOOTHING_THRESHOLD` (because PTS jumps forward past the stale expected position), forcing OBS to use our actual PTS instead of the drifted smoothed value — preventing restart cascades. The PLL also maintains audio/video sync since video timestamps track the system clock.

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
