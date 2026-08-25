# tools/

Offline design aids for the audio timing work. **Not built, not linked, not
CI targets** — nothing in `CMakeLists.txt` references them, and the statement
that the plugin has no test suite still stands. They exist because the two
pieces of real mathematics in the audio path (the media-clock regression and
the PI speed controller) are otherwise unfalsifiable: you cannot tell a
converging controller from a limit-cycling one by reading it, and both bugs
found while writing this code were found here rather than by inspection.

Run them by hand after touching `src/media-clock.c` or the speed controller
in `src/receiver-audio.c`.

## `speed-controller-sim.c`

Closed-loop simulation of the ramp and trim: buffer level in, playback speed
out, sender rate as the disturbance.

```bash
cc -O1 -o /tmp/sim tools/speed-controller-sim.c -lm && /tmp/sim
```

Exits non-zero if the loop fails to settle at any buffer target. What it
covers: steady-state error with and without the trim, convergence over 300s,
the target sweep from 40ms to 500ms, a 3s stall followed by its backlog —
which the trim must learn *nothing* from, since it is a network event and not
a clock — and how faithfully the requested speed is actually applied. It caught the limit cycle the trim produced against the original
flat deadband, and the windup a stall leaked into the trim before the error
window was added.

**Caveat:** unlike the media-clock check, this one **replicates** the
controller rather than linking it, because the real one reads
`struct irl_source`. The constants and both update rules are copied verbatim
and the file says so at the top. Change them in `receiver-audio.c` and you
must change them here too, or this quietly starts simulating a controller
that no longer exists.
