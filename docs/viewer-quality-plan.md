# Viewer-quality policy

The plugin optimizes for what viewers hear and see during bad IRL signal.
Latency matters, but it is secondary to avoiding audio artifacts, gray frames,
and video cadence freezes.

## Policy

- Prefer silence over jittery, glitchy, metallic, or artifacty audio.
- Prefer timestamped damaged frames over cadence freezes when the damaged
  frame is still a picture (H.264 concealment); prefer a freeze on the last
  good frame over gray (HEVC missing references). Avoid gray/blank frames and
  decoder reset storms.
- Prefer bounded latency movement over continuous audio stretching, while
  preserving the plugin's latency advantage over multi-second Media Source
  buffering.
- Keep recovery behavior visible in logs and stats.

## Recovery is observable

The periodic stats log line reports buffer fill, speed, underruns, output
restarts, the largest PTS gap, the A/V skew and the delay and hold that cover
it, and the video queues together, so a single log line describes the health
of the whole path. Underruns, startup trims, output clock restarts and
re-anchors, and decoder corruption bursts also log a line each when they
happen.

## Audio behavior

- Small timestamp jitter is treated as timestamp repair (interpolation), not
  inserted silence.
- Real medium gaps get silence insertion, not time compression.
- Buffered audio stays near native rate by default. Steady-state latency
  recovery is done with bounded speed correction (build at -2%, drain at up to
  the Catch-Up Speed, +5% by default), which is smoothed and less audible
  than skips or pops.
- Audible buffered audio is never trimmed just to reduce delay. Hidden-backlog
  trimming runs only before playback primes (nothing was audible yet).
- Underruns emit shaped concealment silence so OBS timestamps remain monotonic.

## Video behavior

- First-keyframe gating is on by default.
- Video that reaches the plugin too late to be paced (the encoder sends video
  later than audio by more than Target Buffer covers) is delayed by a
  standing, measured amount rather than dropped or shown unpaced.
  `video_delay_ms` reports it. It is the lip-sync error, and the log line that
  sets it says how much more Target Buffer would remove it.
- Timestamped damaged H.264 frames are passed through during decoder
  corruption so video cadence stays smooth: H.264 concealment patches a damaged
  frame from the previous one, which is a usable picture.
- HEVC frames predicted from a missing reference are held back. HEVC has no
  concealment; FFmpeg synthesizes the missing reference as flat gray, so the
  choice is between a gray GOP and a freeze on the last good frame, and the
  freeze wins. Resumes at the next keyframe, and the line that says so
  reports how many frames were held.
- The video decoder is never flushed. A flush clears the reference buffer and
  the decoder's recovery state, which is a guaranteed gray GOP on both codecs;
  repeated errors are counted and logged instead.
- Smooth frame cadence is preferred over last-good-frame freezes; gray frame
  output is avoided.

## Reading bad-signal logs

When validating against live lossy SRT logs:

- `max_gap` is the largest audio PTS gap repaired on this connection. Below
  70 ms it was interpolated, up to 2 s it was filled with silence, and beyond
  that the timeline was reset.
- `speed` stays near 1.000 in buffered mode; any deviation should be smooth and
  correlated with high/low fill.
- `Audio trim` logs are hidden/recovery cleanup only, before old chunks become
  audible.
- Video corruption logs do not imply audio corruption unless audio decoder or
  PTS diagnostics also show damage.
- `HEVC video resumed (N frames held this connection)` reports the HEVC frames
  kept off screen. A count that keeps climbing means references are being
  lost faster than keyframes arrive: shorten the encoder's keyframe interval or
  give SRT more latency to retransmit.
