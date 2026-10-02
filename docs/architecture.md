# Architecture

This page explains why the plugin is shaped the way it is: the threads, what each one owns, and how audio and video are kept in step. `CLAUDE.md` lists the rules that follow from it. `docs/audio-pipeline.md` covers the audio path in depth, and `docs/audio-timing-pitfalls.md` covers the speed controller.

## Crates

All unsafe code lives in `obs-sys`, `obs` and `ffmpeg`. `irl-core`, `irl-provider` and `irl-source` carry `#![forbid(unsafe_code)]`, so a raw pointer in plugin logic needs a new safe wrapper in one of the first three.

- `obs-sys`: hand-written libobs FFI. `#[repr(C)]` structs, `extern` declarations and constants. The `layout-test` feature runs bindgen over the real headers and asserts every field offset. libobs is never linked: the symbols resolve against the host OBS process at load time (`raw-dylib` from `obs.dll` on Windows, undefined symbols elsewhere).
- `obs`: the safe libobs API this plugin uses. The `Source` trait and registration, `declare_module!`, data, properties, calldata, frames, scene transforms, the obs-websocket vendor helper and `panic::guard`.
- `ffmpeg` (package `irl-ffmpeg`): RAII over `ffmpeg-sys-next`. `build.rs` replays the link lines that `deps/build-deps.sh` wrote to `irl-deps.env`.
- `irl-core`: everything that needs neither libobs nor FFmpeg. The jitter buffer, PTS repair, the speed controller, output clock arithmetic, video pacing and delay, the audio hold, demuxer options, config derivation, the stats table and every tuning constant. It is the crate with the real unit test suite.
- `irl-provider`: the plugin side of `docs/provider-protocol.md` (discovery, OAuth with PKCE over a loopback redirect, the ingest list, the resolve call). It has no libobs dependency; the plugin hands it a state directory, a logger and a callback through `init`.
- `irl-source`: the plugin. Module entry points, the source lifecycle and the three worker threads.

## Data flow

```
[receiver thread]: FFmpeg URL, demux
  audio: decode, PTS repair, resample, write to jitter buffer
  video: push the compressed packet onto the video queue

[video thread]: decode packets as they come due, keyframe gate, HW frame
                transfer, hold until due, format conversion,
                OBS async video output

[audio thread]: drain jitter buffer, speed correction, concealment,
                OBS audio output
```

Video decodes on the video thread, not the receiver, for two reasons. Decoding eagerly would hold the stream's whole latency as decoded frames (8 s of 4K60 is about 6 GB, against about 20 MB of packets). And the receiver spends a network stall blocked in `av_read_frame`, which is exactly when video must keep draining the buffer it already has. The thread that decodes cannot be the thread that reads.

## Threads and ownership

- **OBS thread** (`IrlSource`): create, destroy, update, tick, properties, and the show/hide/activate/deactivate callbacks. Its state sits in one `Mutex<ObsState>` only because the stats proc can arrive on another thread.
- **`Shared`** is built fresh at every `start_receiver`, so per-run state starts zeroed. What survives a restart lives in `LifetimeStats`, an `Arc` carried across runs.
- **Receiver thread** owns demux and the audio decoder as a plain struct on its own stack. It writes the jitter buffer and pushes video packets onto the video channel. It never touches the GPU.
- **Video thread** owns the video decoder, handed over by the receiver at stream open (`VideoMsg::Decoder`, queued ahead of its packets). Its pacing queue is a local `PacingQueue` with no lock.
- **Audio thread** drains the jitter buffer and submits audio, paced against the output clock.

The lock order is `audio_state` → `audio_buf` → `hot.watermarks`, and `video.q` is never held together with any of them. The audio pump takes `audio_state` once per iteration and passes `&mut AudioState` down. parking_lot mutexes are not recursive, so a nested acquire would hang the audio thread and then the video thread behind it.

Hot config (`reconnect_delay_s`, `adaptive_speed`, `catchup_percent`, `wait_for_keyframe`, `clear_on_disconnect`) is atomics, read with `Relaxed`. `catchup_percent` is read once per controller cycle and passed down, because the ramp, the anti-windup, the actuator clamp and the stuck-drain watch must agree on one ceiling within a cycle. The three watermarks publish together under a mutex so no reader sees them torn mid-resize.

A panic never crosses an FFI boundary. `obs::panic::guard` wraps every `extern "C"` shim and `shared::spawn_worker` wraps every worker thread. A panic is logged, `thread_active` is cleared (which also trips the FFmpeg interrupt watch, so a receiver blocked in `av_read_frame` returns), the video sleeper is woken, and the normal stop path takes over.

## Source lifecycle

`update` diffs the new settings against the live config. URL, FFmpeg Options, Hardware Decode and Low Latency Audio are latched at stream open and force a reconnect. Everything else is swapped in place through `Config::apply_hot`, so a settings tweak neither drops the connection nor clears the stats. Retuning Target Buffer live goes through `AudioBuffer::resize`, which grows the ring (never shrinks it) and only then publishes the new watermarks. If the resize fails, the old target stays in force, including in the OBS-thread config that the next diff compares against.

`video_tick` runs a one-shot fit to canvas. A source created without a URL (freshly added, not restored from a scene collection) applies the frontend's Fit to Screen transform to every scene item that references it, once, as soon as the source reports a non-zero size.

With "Close Stream When Inactive" on, show/activate start the receiver and hide/deactivate stop it. Otherwise those callbacks do nothing and the stream runs from create to destroy. Every "the stream stopped" clear (hide/deactivate, a restart-forcing settings edit, a disconnect) is gated on "Show Nothing When the Stream Ends" (`clear_on_disconnect`, on by default). With it off, the last frame stays frozen on screen until the stream returns.

`OBS_SOURCE_CONTROLLABLE_MEDIA` exists because that flag makes the source addressable through obs-websocket's `TriggerMediaInputAction` and `GetMediaInputStatus`. NOALBS's `!fix` reconnects a stalled feed that way, and it enumerates candidates by media state, so a source that reports `OBS_MEDIA_STATE_NONE` is invisible to it. A live stream has nothing to seek or pause, so the media callbacks reduce to "run the receiver" and "don't", with a `media_stopped` latch that survives show/activate and is cleared by Restart or a settings edit. `!fix` for `ffmpeg_source` writes empty settings and relies on `ffmpeg_source_update` restarting unconditionally. That trick does not work here, because `update` diffs. Restart is the explicit request.

## Audio output

Three facts about libobs shape the audio core:

1. OBS timestamps must be contiguous (`ts[n+1] = ts[n] + frames/rate`). Deviations under 70 ms are smoothed, gaps of 70 ms to 2 s are zero filled (audible), and larger jumps flush all queued audio. The plugin derives timestamps from a sample counter anchored once at prime time and never jumps the clock outside declared restarts.
2. Changing `samples_per_sec` between submissions makes OBS rebuild its per-source resampler with no crossfade, which clicks. Playback speed is applied inside the plugin with a persistent swresample compensation, and the rate submitted to OBS never changes.
3. The OBS mixer consumes 21.3 ms ticks against wall clock. A source whose queued audio runs dry gets a tick of silence and a time-shifted splice (crackle), and a source that falls behind the mix window makes OBS add global audio buffering for good. After priming, the pump always emits (real audio or shaped concealment silence) and keeps a fixed lead ahead of wall clock.

The buffer is regulated through playback speed only, asymmetric: it builds at an inaudible -2 % and drains post-stall backlog at up to the Catch-Up Speed setting (+5 % by default). The loop is PI. A slow integral trim under the proportional ramp removes the standing error that a sender whose media clock is not wall clock would otherwise leave. Content is never skipped once playback has primed. Backlog beyond a fill ceiling is pushed back into the transport by pausing the read loop (TCP and RTMP backpressure; SRT bounds itself through its latency window), and startup backlog is trimmed only before priming.

## Video pacing and the play head

Frames go to libobs a couple of canvas ticks before their due time. libobs is a scheduler too: `ready_async_frame` advances its play head by wall-clock deltas and takes the frame it has just passed, so a frame handed over exactly at its due time is not queued when its render tick runs and slips to the next one. At 30 fps on a 60 fps canvas that is visible judder. The frame keeps its due time as its timestamp, so the lead changes when libobs receives it, not when it is shown.

The exception is the frame that anchors libobs's play head after a start or a clear. `get_closest_frame` shows that frame on arrival whatever its timestamp and anchors from it, so a lead there would run the whole connection early. That frame goes at its due time, and the anchor only clears once a frame libobs received went out at its real due time.

While an audio stream is present, the anchor frame's due time must come from the audio mapping, not the video-only fallback. The two disagree by about 100 ms (the fallback schedules the first frame one Target Buffer out; the mapping puts it at the first audio chunk), and libobs freezes whichever error the anchor frame carried into the connection's lip sync. So video holds until audio primes (the pump wakes the video thread when it publishes the mapping), drops frames the mapping lands more than a canvas tick in the past, and anchors on the first frame that is on time. A stream whose audio never primes goes ahead on the fallback after `VIDEO_ANCHOR_WAIT_MARGIN_MS` past the expected prime.

## Video delay

A frame can only get the lead if it is in hand a lead before it is due. Its arrival margin (due time minus the OBS time its packet reached the video thread) is set by the sender. Video that leaves the encoder later than its audio has less margin, and once the skew exceeds what Target Buffer and the audio output lead cover, every frame is late. A late frame goes out on arrival, unpaced, and libobs drops one whenever two arrive inside a canvas tick: low fps.

`irl_core::video_delay` closes that gap with a floor under the video schedule. A frame maps through the audio playout offset `O` (its PTS plus `O` is when the audio of the same instant plays), and the floor `F` is the lowest such offset at which frames are still in hand when due: the worst `arrival - PTS` seen, plus a tick of allowance for the decode. Every frame is due at `pts + max(O, min(F, O + VIDEO_DELAY_MAX_MS))`. The video delay, `F - O` clamped to between zero and `VIDEO_DELAY_MAX_MS`, is never stored. It follows from the floor and the offset in force, and `video_delay_ms` reports it. It is the lip-sync error, traded for a smooth picture.

Because the delay is derived, nothing has to keep it in step with the audio. When the playout grows (an audio hold building, concealment during an outage) the delay shrinks by the same amount and due times stay where they were. When the playout shrinks (a re-anchor, a hold released, a smaller Target Buffer) video follows it down only as far as the floor. The bar is "not late", not "a full lead early": the lead covers the pacing timer oversleeping on a queued frame, and a frame handed over on arrival never sleeps.

- **Before the anchor**, a shortfall raises the floor at once, measured at packet arrival with one canvas tick of allowance for the decode still to come (`VideoThread::settle_anchor_candidate`), and rounded so that the delay it sets is a whole number of ticks. It measures only once video is live (`irl_core::arrival`): a relay hands a new subscriber video from its last keyframe next to live audio, so a connection can open with video a second behind and catching up faster than real time. Both streams share a mux, so the network cannot cause that reading; the test is whether the floor of arrival minus PTS has stopped falling across `VIDEO_LIVE_LOOKBACK_MS`, bounded by `VIDEO_LIVE_MAX_WAIT_MS`. It also measures only the newest frame in hand, because the probe backlog arrives as one burst with one arrival time and its older frames look late when they were only buffered.
- **After the anchor**, the measurement is the hand-over itself, and a raise moves the picture. It takes `VIDEO_DELAY_MIN_FRAMES` frames past due, spread across `VIDEO_DELAY_WINDOW_MS`. The delay is capped at `VIDEO_DELAY_MAX_MS`; a sender later than that anchors on its newest frame and plays unpaced.
- **Relax.** Every frame handed over also reports the floor it needed at arrival. When every frame across the last `VIDEO_DELAY_RELAX_WINDOW_MS` (a sliding window, with no gap in the video longer than a raise window) needed less than the floor by more than `VIDEO_DELAY_RELAX_MIN_MS`, the floor comes down to what they needed plus a tick. Only the part of the floor above the offset is on screen. That part slews at the Catch-Up Speed, so video plays a few percent fast and nothing jumps, and the rest goes at once. A raise cancels the slew. This matters because the pre-anchor measurement is one frame at connection start, and on a poor link it can read a second late on a stream with no skew. The window slides so that a sender that recovers is noticed when the audio hold notices it: a hold released over a stale floor would put the picture behind the sound for nothing.

The floor carries the stream's PTS epoch, so a clear, a decoder handover and a timeline reset (PTS repair starting a new timeline) forget it, together with the liveness history. Without a mapping, each frame keeps the fallback due time it was given at intake and the floor applies to that in the same way. The video-only fallback schedules its first frame at arrival, so a stream without audio always carries a delay of exactly one tick.

The log warns when the floor rises, says when it starts to come down, and says once when the playout moving takes the delay to or from zero.

## Audio hold

The video delay is a bridge, not the answer to a sender whose video trails its audio: a frame cannot be shown before it arrives, so the only way back to sync is for audio to wait. That is the audio hold (`irl_core::audio_hold`, `irl-source/src/audio/hold.rs`).

The receiver reads the skew as it pushes each video packet: its decode timestamp against the newest audio PTS decoded before it. Mux order makes the reading trustworthy, since loss and stalls delay both streams together. A relay's catch-up replay is kept out by an `ArrivalFloor` of its own. The part of the skew that Target Buffer and the output lead do not cover, plus `AUDIO_HOLD_MARGIN_MS`, is folded into the published watermarks, so their target is Target Buffer plus the hold and every reader of the target follows it. `Config::apply_hot` composes a Target Buffer edit with it.

- Before priming, the pump waits up to `AUDIO_HOLD_PRIME_WAIT_MS` for the first reading, so a sender that is always late starts in sync.
- After priming, the hold moves only while the speed controller regulates the buffer (not in low-latency mode, not with Adaptive Latency Control off). A skew sustained across `AUDIO_HOLD_RAISE_WINDOW_MS` raises it, and the controller builds it at -2 %. The playout offset grows as it builds, and the video delay is the video floor less that offset, so the delay drains with it: due times stay put while the lip-sync error goes. The offset re-anchor nets the hold out of its drift, since that growth is latency asked for, not concealment.
- Low-latency mode has no target to fold the hold into: the pump primes on the hold's worth of audio and keeps that much queued.

Shorter bursts stay the video delay's job, and the delay's warnings stop suggesting a Sync Offset where the hold can move, since both would correct the same skew. `audio_hold_ms` reports the hold.
