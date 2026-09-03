# Audio timing pitfalls

Things that were built wrong first in the plugin's audio timing path, and one
thing that was built, measured, and deliberately thrown away. Each item here
compiled clean, looked correct while being written, and was caught only by
simulating it (`cargo run -p irl-core --example speed-controller-sim`) or by
reading a real stream's log.

They are recorded because most of them are re-inventable — the next person to
touch the speed controller is likely to reach for the same wrong shape.

Read this before changing `crates/irl-core/src/speed.rs`.

## A proportional loop cannot hold a constant

Buffer regulation was proportional-only for a long time: speed away from 1.0
only while the level was away from target. That is the right shape for a
transient — a stall's backlog drains and the ramp relaxes — but a sender whose
media clock runs at 1.003x delivers 3ms of extra audio every second *forever*,
and the only ramp position that consumes it is one with a permanent level
error. The buffer parks off-target and latency parks with it.

Since no two clocks agree exactly, every stream carries some of this.
Simulated at the default 120ms target, a proportional-only loop parks 24ms high
on a 0.3% sender and 22ms low on a 0.3% slow one, against ~1ms for the PI loop
— but see the next section before quoting that last number.

The fix is an integral term, `SpeedTrim`. If you find yourself adding a
"correction factor" or a "bias" to a proportional controller, you are adding an
integrator — do it deliberately and read the next two sections.

## The buffer level is quantised to one chunk

Reads and writes are both whole decoded chunks, so the jitter buffer's residual
is always a multiple of one — 21.3ms for 1024-sample AAC frames. The level the
controller sees therefore steps: 85, 106, 128, 149ms and so on, and a 120ms
target is not a reachable state at all. On a real feed the loop sits at 106 or
128 and the configured target is a value it straddles rather than one it holds.

Two consequences worth remembering:

- **No controller here resolves below 21ms.** The simulation models a
  continuous level and will happily report a 1ms standing error; that number is
  a property of the model, not of the plugin. What the trim genuinely removes
  is the *systematic* offset — tens of milliseconds on a drifting sender — and
  that is the claim worth making.
- **The user gets less cushion than they configured**, by up to one chunk. A
  120ms target commonly runs at 106ms, and that missing 22ms is real margin
  against an underrun. This is not new — a flat-deadband proportional loop
  parks in the same place for the same reason — but it means the target reads
  more like a set point the level orbits than a floor it respects.

Anything that tries to tighten steady-state accuracy needs to fix the
resolution first, and that means decoupling the read size from the decoded
frame size. Nothing in the controller can help.

## A deadband and an integrator do not mix

The speed ramp had a flat deadband: exactly 1.0x anywhere within 20ms of
target. Harmless on its own. Fatal with an integrator underneath it, because a
region with **zero proportional feedback leaves the integrator undamped** — the
pair is a marginally stable second-order system and it limit-cycles through the
deadband indefinitely. Simulated, that was ±20ms of buffer on a ~2 minute
period, never settling, with a steady-state error barely better than having no
integrator at all.

`AUDIO_SPEED_DEADBAND_SLOPE` is the fix: the deadband slopes gently (0.2% at
its edges, ~3.5 cents) instead of being flat. Any future "let's not bother
correcting when we're close" optimisation reintroduces the bug.
`the_loop_settles_rather_than_limit_cycling` in `speed.rs` is the regression
test; the simulator's target sweep is the wider check.

## An integrator will happily learn from a network stall

The level only reports the sender's rate when the loop is *in control*. While a
backlog is draining or a buffer is starving it reports the transient, and an
integrator fed that learns "the sender is fast" from a three-second outage and
keeps the lesson.

Two gates prevent it, and both are load-bearing:

- `AUDIO_SPEED_TRIM_ERR_WINDOW_MS` — only integrate within ±60ms of target.
- Anti-windup — never integrate in the direction that made the command
  saturate. This must test the **command actually issued** (`ramp + trim`), not
  the ramp alone: the actuator clamps their sum, so with the trim near its own
  limit the sum saturates while the ramp is still short of the limit. That was
  a real bug.

A third gate has no C ancestor and covers the machine rather than the stream:
`AUDIO_SPEED_TRIM_MAX_DT_US` throws away any cycle whose dt exceeds a second,
because the audio thread not running (a debugger, a laptop sleep, starvation)
would otherwise credit the whole gap to the sender's clock.

Simulated, a 3s stall plus its backlog now moves the trim by +0.0002%.

## Not every upstream hands over a smooth stream

The controller regulated the level as it read it, once per emitted chunk. That
is correct when media arrives at roughly the rate it is consumed, and it is
wrong the moment something upstream batches.

A remux hop — MediaMTX, an RTMP relay, anything that reads from one socket and
writes to another on its own schedule — delivers in clumps. The jitter buffer
then sawtooths at the batch period, sweeping the whole speed ramp, and a
controller reading the instantaneous level chases it. Measured in the network
simulation against the pre-fix controller:

| batch period | target | playback speed swing |
| --- | --- | --- |
| 200ms | 120ms | 0.5% |
| 500ms | 120ms | 1.5% |
| 1s | 120ms | 2.2% |
| 1s | 500ms | 3.3% |

1% is 17 cents, so 2-3% is plainly audible pitch wobble. Worse, video due times
are the frame PTS plus the audio playout offset, so the same modulation lands on
video at the same time: the picture repeatedly speeds up, catches up and slows
down. It reads as a rendering or frame-rate problem and is neither.

The ramp's own EMA does not help. Its time constant is ~0.4s, which is shorter
than the batch periods that cause this, so the sawtooth passes straight through.

`AUDIO_SPEED_LEVEL_SMOOTHING` is the fix: regulate a ~2.5s EMA of the level
rather than the level. That is an order of magnitude longer than any batch
period worth smoothing and an order of magnitude shorter than a post-stall
drain, which lasts tens of seconds and must survive. It measured slightly
*faster* recovery from a stall, not slower, because the smoothed level holds the
ramp at full drain authority instead of relaxing on each dip.

What it cannot fix, and nothing can: a batch period longer than the whole
cushion. The buffer genuinely runs dry between batches, and the answer is a
Target Buffer above the batch period, not a cleverer loop.

## The resampler cannot apply arbitrarily small speeds

`apply_output_speed` asks swresample for a whole number of output samples per
chunk. Round each chunk independently and the applied speed is quantised to
multiples of `1/in_frames` — about **0.1% at 1024 frames**:

| requested | applied (naive rounding) |
| --- | --- |
| +0.02% | +0.000% — discarded |
| +0.05% | +0.098% — doubled |
| +0.50% | +0.491% |

Sub-0.1% is exactly where the deadband slope and most of the trim's range live,
so neither was being applied at the size it asked for. This predates the PI
work — the proportional ramp requested that range too, just outside its
deadband, and had the same corrections mangled.

Worse, the boundary between "discarded" and "doubled" is a threshold the buffer
level crosses continuously, so the compensation switched on and off from chunk
to chunk once the deadband stopped being flat. Audio that had passed through
untouched near target became continuously resampled, and it was audible —
reported as occasional sparkle on a mono AAC feed before the cause was known.

`SpeedCarry` carries the fractional remainder between chunks so the long-run
rate is exact at any requested speed. **Any new control authority below ~0.1%
depends on that carry existing.** `naive_rounding_would_fail_that_test` in
`speed.rs` pins the defect so a "simplification" that drops the carry cannot
pass silently.

## Do not rebuild the media-clock estimator

This one was built in full, measured on real streams, and deleted. It is
documented here so it does not come back.

The idea is natural and it is what obs-smooth-media does: measure the sender's
clock rate directly as `d(stream PTS) / d(wall clock)`, and feed it into the
speed controller. The implementation was a least-squares fit over a 20s sliding
window with bucket-coalesced observations, a standard error, and a one-shot
seed of the trim behind four gates (minimum span, minimum sample count, an
absolute standard-error cap, and a 2-sigma significance test). About 350 lines
plus a test harness.

**It works and it is still not worth having.** The reasons, in order:

1. **The trim already converges to the same number without it**, by watching
   the buffer level — a signal this code already trusts and already acts on.
   The estimator's only contribution to playback was arriving at the answer ~70
   seconds sooner, which is worth about 30ms of latency, temporarily, once per
   stream.

2. **Its error bars lie.** The standard error of a least-squares slope assumes
   independent residuals. The dominant residual here is the sender's PTS
   wobble, which is a slow sawtooth — heavily autocorrelated — so the reported
   uncertainty is optimistic. Measured on a real feed, two consecutive
   independent 19-second windows reported `+0.038% ±0.018%` and
   `−0.043% ±0.016%`: a 0.081% disagreement against a combined sigma of 0.024%,
   **3.4 sigma apart, for a quantity that physically cannot change**. The
   2-sigma gate passed both times. Every seed it ever produced on real hardware
   was noise.

3. **Everything it measures is contaminated by us.** Three separate versions of
   the same trap were hit while building it:
   - Feeding it `PtsRepair`'s output measures our own repair. On a frame-sized
     cadence offset that code discards the incoming PTS and substitutes a
     nominal advance — 65% of frames on a measured real feed.
   - Measuring arrival times while transport backpressure is engaged fits a
     line through our own pacing, because the read loop is what stopped. And
     that case is unfixable in the direction you want: the scenario the rate
     matters most for — a sender too fast for playback to ever catch — is
     precisely the one that keeps backpressure engaged indefinitely, so the
     window can never re-accumulate.
   - The observation point has to be intake, not playout, because everything
     downstream of intake is our buffering.

4. **It could not tell jitter from a discontinuity without being told twice.**
   The first version reset its window on any backward step in arrival time, and
   so reported "measuring" forever on exactly the lossy streams worth
   measuring. Switching it to raw sender PTS reproduced the identical bug on
   the other axis, because raw PTS wobbles by a frame or more on plenty of
   muxers.

The principle worth keeping: **a measurement that cannot reach playback cannot
destabilise it.** The trim needs no measurement, so it has no measurement to be
wrong about. If someone proposes reintroducing the estimator, the bar is not
"it would converge faster" — it is "here is why the 3.4-sigma disagreement
above will not happen to me", and the honest answer to that is a stability
requirement across independent windows, which costs most of the latency
advantage that motivated it in the first place.

## The buffer level has no single value

The level moves in whole chunks and both reads and writes are whole chunks, so
within every cycle it steps down by a chunk at the read and up by a chunk at the
write. *Where* you sample decides the number you get: before the pump's read —
the level the controller regulates — a 120ms target holds 117ms with the
reachable pair at 110/130; sampled after the pump has taken what it wants, the
same run reads 97ms.

Neither is wrong, but a test that samples one and quotes it as "the cushion" is.
This cost two false alarms while building the network simulation, both looking
like a controller that had drifted a chunk low. The stats line's `buf=` is a
sample of the same oscillation at an arbitrary phase, which is why it reads
below target as often as not.

## Verify by simulation, not by reading

Every defect above was invisible on inspection and obvious in a 200-line
harness. `crates/irl-core/examples/speed-controller-sim.rs` is that harness for
the controller in isolation, and `crates/irl-source/tests/network_sim.rs` is the
end-to-end one: a synthetic sender with programmable stalls, batching and clock
skew driving the real buffer, controller and queues on a virtual clock. The
batching defect above was found by writing that scenario, not by reading the
code. It
is not run by CI, so run it by hand when you touch this code:

```shell
cargo run -p irl-core --example speed-controller-sim
```

It exits non-zero if the loop fails to settle at any buffer target, or if a
requested speed is not applied faithfully. Unlike the C harness it replaces
(`tools/speed-controller-sim.c`, which copy-pasted the controller because the
real one read `struct irl_source`), it **links** `irl_core::speed`: its
constants cannot drift out of step with the plugin's, because they are the same
constants. The shape of the response is what it is for — settling versus
limit-cycling, and whether a transient leaks into the trim — not the
sub-millisecond errors it prints, for the reason in "The buffer level is
quantised to one chunk" above.
