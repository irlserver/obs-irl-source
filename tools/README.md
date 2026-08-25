# tools/

Offline design aids. **Not built, not linked, not CI targets** — nothing in
`CMakeLists.txt` references them, and the statement that the plugin has no
test suite still stands.

They exist because the PI speed controller is the one piece of real
mathematics in the audio path, and it is otherwise unfalsifiable: you cannot
tell a converging controller from a limit-cycling one by reading it. Every
defect the controller had during development was found here rather than by
inspection.

Run by hand after touching the speed controller in `src/receiver-audio.c`.

## `speed-controller-sim.c`

Closed-loop simulation of the ramp and trim: buffer level in, playback speed
out, sender rate as the disturbance.

```bash
cc -O1 -o /tmp/sim tools/speed-controller-sim.c -lm && /tmp/sim
```

Exits non-zero if the loop fails to settle at any buffer target. What it
covers: steady-state error with and without the trim, convergence over 300s,
the target sweep from 40ms to 500ms, a 3s stall followed by its backlog —
which the trim must learn *nothing* from, since it is a network event and
not a clock — and how faithfully the requested speed is actually applied.

It caught all three of the controller's real defects: the limit cycle the
trim produced against the original flat deadband, the windup a stall leaked
into the trim before the error window existed, and the sample quantisation
that was silently discarding or doubling every correction under 0.1%.

**Caveat:** this file **replicates** the controller rather than linking it,
because the real one reads `struct irl_source`. The constants and all three
update rules are copied verbatim and the file says so at the top. Change
them in `receiver-audio.c` and you must change them here too, or this
quietly starts simulating a controller that no longer exists.
