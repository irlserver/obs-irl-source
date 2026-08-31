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

PTS repair telemetry is split into separate counters (normalization,
interpolation, silence, reset, last gap, max gap) so each recovery mechanism
can be tuned independently. The aggregate `pts_repairs` counter is kept for
script compatibility. The periodic stats log line reports buffer fill,
underruns, trims, OBS lead, timing state, and the split PTS counters together,
so a single log line describes the health of the whole path.

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
- Timestamped damaged H.264 frames are passed through during decoder
  corruption so video cadence stays smooth: H.264 concealment patches a damaged
  frame from the previous one, which is a usable picture.
- HEVC frames predicted from a missing reference are held back. HEVC has no
  concealment; FFmpeg synthesizes the missing reference as flat gray, so the
  choice is between a gray GOP and a freeze on the last good frame, and the
  freeze wins. Resumes at the next keyframe; `video_corrupt_held` counts them.
- The video decoder is never flushed. A flush clears the reference buffer and
  the decoder's recovery state, which is a guaranteed gray GOP on both codecs;
  repeated errors are counted and logged instead.
- Smooth frame cadence is preferred over last-good-frame freezes; gray frame
  output is avoided.

## Reading bad-signal logs

When validating against live lossy SRT logs:

- `silence_insertions` should rise only on real medium audio gaps or underruns.
- `pts_normalizations` can be high without audible artifacts (frame-sized
  cadence smoothing).
- `pts_interpolations` should be read together with gap size and audio quality.
- `pts_resets` should stay rare.
- `speed` stays near 1.000 in buffered mode; any deviation should be smooth and
  correlated with high/low fill.
- `resync_skips` happens only during low-latency resync or hidden/recovery
  backlog cleanup.
- `Audio trim` logs are hidden/recovery cleanup only, before old chunks become
  audible.
- Video corruption logs do not imply audio corruption unless audio decoder or
  PTS diagnostics also show damage.
- `corrupt=` in the stats line (`video_corrupt_frames`) is frames the decoder
  flagged as damaged; `held=` (`video_corrupt_held`) is the HEVC subset that
  was kept off screen. A `held` count that keeps climbing on an HEVC stream
  means references are being lost faster than keyframes arrive: shorten the
  encoder's keyframe interval or give SRT more latency to retransmit.
