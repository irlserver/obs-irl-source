//! Offline closed-loop simulation of the audio speed controller: buffer level
//! in, playback speed out, sender rate as the disturbance.
//!
//! ```shell
//! cargo run -p irl-core --example speed-controller-sim
//! ```
//!
//! Not a CI target and not part of `cargo xtest`. Run it by hand after
//! touching `speed.rs`; it exits non-zero if the loop fails to settle at any
//! buffer target, or if a requested speed is not applied faithfully.
//!
//! It exists because the PI speed controller is the one piece of real
//! mathematics in the audio path and is otherwise unfalsifiable: you cannot
//! tell a converging controller from a limit-cycling one by reading it. Every
//! defect the controller had during development was found here — the limit
//! cycle the trim produced against the original flat deadband, the windup a
//! stall leaked into the trim before the error window existed, and the sample
//! quantisation that was silently discarding or doubling every correction
//! under 0.1 %. `docs/audio-timing-pitfalls.md` is the write-up.
//!
//! Unlike the C harness this replaces (`tools/speed-controller-sim.c`, which
//! copy-pasted the controller because the real one read `struct irl_source`),
//! this **links** [`irl_core::speed`]. Its constants cannot drift out of step
//! with the plugin's, because they are the same constants.
//!
//! **Caveat:** the buffer level here is CONTINUOUS. On real audio it moves in
//! whole decoded chunks (21.3 ms for 1024-sample AAC), so the sub-millisecond
//! steady-state errors printed below are a property of the model and not of
//! the plugin. What the numbers are good for is the *shape* of the response:
//! settling versus limit-cycling, and whether a transient leaks into the trim.

use irl_core::{
    SpeedCarry, SpeedController, SpeedInputs, SpeedTrim, Watermarks, catchup_speed_max, consts,
};

/// One pump cycle is one decoded audio chunk: 1024 frames at 48 kHz.
const DT_US: u64 = 21_333;
const DT: f64 = DT_US as f64 / 1_000_000.0;

/// `av_gettime()` is microseconds since the epoch and never 0, which the trim
/// uses as its "not armed" sentinel.
const T0: u64 = 1_700_000 * 1_000_000;

struct Loop {
    speed: SpeedController,
    trim: SpeedTrim,
    fill_ms: f64,
    trim_enabled: bool,
    wm: Watermarks,
    max_speed: f32,
    step: u64,
}

impl Loop {
    fn new(target_ms: i32, trim_enabled: bool) -> Self {
        Self {
            speed: SpeedController::new(),
            trim: SpeedTrim::new(),
            fill_ms: target_ms as f64,
            trim_enabled,
            wm: Watermarks::derive(target_ms),
            max_speed: catchup_speed_max(consts::DEFAULT_CATCHUP_PERCENT as i32),
            step: 0,
        }
    }

    /// One cycle: run the real controller, then move the buffer by the
    /// difference between what the sender delivers and what playback consumes.
    /// Both rates are in seconds of media per second of wall clock.
    fn tick(&mut self, sender_rate: f64, recovery: bool) {
        let speed = self.speed.update(
            self.fill_ms as i32,
            &mut self.trim,
            SpeedInputs {
                wm: self.wm,
                adaptive: true,
                low_latency: false,
                max_speed: self.max_speed,
                now_us: T0 + self.step * DT_US,
                recovery_active: recovery,
            },
        );
        if !self.trim_enabled {
            self.trim.reset();
        }
        self.fill_ms = (self.fill_ms + (sender_rate - speed as f64) * DT * 1000.0).max(0.0);
        self.step += 1;
    }

    fn run(&mut self, sender_rate: f64, secs: f64, recovery: bool) {
        for _ in 0..(secs / DT) as u64 {
            self.tick(sender_rate, recovery);
        }
    }

    fn err_ms(&self) -> f64 {
        self.fill_ms - self.wm.target_ms as f64
    }
}

fn steady(what: &str, rate: f64, trim_on: bool) {
    let mut l = Loop::new(consts::DEFAULT_BUFFER_TARGET_MS as i32, trim_on);
    l.run(rate, 300.0, false);
    println!(
        "  {:<24} {:<8} fill {:7.1}ms (err {:+7.1})  speed {:.4}  trim {:+.3}%",
        what,
        if trim_on { "trim on" } else { "trim off" },
        l.fill_ms,
        l.err_ms(),
        l.speed.current(),
        l.trim.value() * 100.0,
    );
}

/// A 3 s delivery outage followed by the whole backlog landing at once. The
/// trim must learn nothing from this: it is a network event, not a clock. This
/// is the failure mode that makes naive integrators unusable.
fn stall(trim_on: bool) -> u32 {
    let mut l = Loop::new(consts::DEFAULT_BUFFER_TARGET_MS as i32, trim_on);
    l.run(1.0, 120.0, false);
    let before = l.trim.value();

    l.run(0.0, 3.0, true); // nothing arriving; the pump is concealing
    l.fill_ms += 3000.0; // the backlog lands
    l.run(1.0, 300.0, false);

    let leaked = l.trim.value().abs() >= 0.001;
    println!(
        "  {:<24} {:<8} trim {:+.4}% -> {:+.4}%  fill {:.1}ms  {}",
        "3s stall + 3s backlog",
        if trim_on { "trim on" } else { "trim off" },
        before * 100.0,
        l.trim.value() * 100.0,
        l.fill_ms,
        if leaked {
            "LEAKED"
        } else {
            "ok (learned nothing)"
        },
    );
    u32::from(leaked)
}

/// The speed actually realised over `chunks` chunks of `n` frames, through the
/// real [`SpeedCarry`].
fn applied_speed(requested: f32, n: i32, chunks: i32) -> f64 {
    let mut carry = SpeedCarry::new();
    let mut out: i64 = 0;
    for _ in 0..chunks {
        out += carry.output_frames(n, requested) as i64;
    }
    (n as i64 * chunks as i64) as f64 / out as f64
}

fn main() -> std::process::ExitCode {
    let mut fails = 0u32;

    let cases = [
        ("exact realtime", 1.0),
        ("crystal drift +0.01%", 1.0001),
        ("sender fast +0.3%", 1.003),
        ("sender slow -0.3%", 0.997),
        ("unwinnable sender +6%", 1.06),
    ];

    println!(
        "steady state after 300s (target {}ms)",
        consts::DEFAULT_BUFFER_TARGET_MS
    );
    for (name, rate) in cases {
        steady(name, rate, false);
        steady(name, rate, true);
    }

    println!("convergence, sender +0.3%, trim on");
    let mut l = Loop::new(consts::DEFAULT_BUFFER_TARGET_MS as i32, true);
    for t in (30..=300).step_by(30) {
        l.run(1.003, 30.0, false);
        println!(
            "    t={t:3}s  fill {:6.1}ms  trim {:+.4}%  speed {:.4}",
            l.fill_ms,
            l.trim.value() * 100.0,
            l.speed.current()
        );
    }

    println!("anti-windup");
    fails += stall(false);
    fails += stall(true);

    // The trim must not oscillate at any supported buffer target: the ramp's
    // slopes change with min_ms/max_ms, and with them the damping the trim
    // relies on. Up to the Target Buffer ceiling — a large target widens the
    // ramp span, which lowers the proportional gain, and the trim has to hold
    // the level anyway.
    println!("target sweep, sender +0.3%, trim on, 400s");
    for target in [40, 80, 120, 300, 500, 2000, consts::BUFFER_TARGET_MAX_MS] {
        let mut s = Loop::new(target, true);
        s.run(1.003, 300.0, false);

        let (mut peak, mut trough) = (s.fill_ms, s.fill_ms);
        for _ in 0..(100.0 / DT) as u64 {
            s.tick(1.003, false);
            peak = peak.max(s.fill_ms);
            trough = trough.min(s.fill_ms);
        }
        let settled = peak - trough < 5.0;
        fails += u32::from(!settled);
        println!(
            "    target {target:4}ms  fill {:6.1}ms (err {:+5.1})  trim {:+.4}%  swing {:.1}ms  {}",
            s.fill_ms,
            s.err_ms(),
            s.trim.value() * 100.0,
            peak - trough,
            if settled { "settled" } else { "OSCILLATING" },
        );
    }

    // How faithfully the requested speed is actually applied. The resampler is
    // driven in whole samples per chunk, so rounding each chunk independently
    // quantises the applied speed to multiples of 1/in_frames (~0.1 % at
    // 1024). That is the region the deadband slope and the trim both live in,
    // so the request was either discarded or doubled; `SpeedCarry` carries the
    // fractional remainder to fix it.
    println!("applied vs requested speed (1024-frame chunks)");
    for req in [
        1.0f32, 1.0002, 1.0005, 1.001, 1.002, 1.005, 1.01, 0.9995, 0.998, 0.99,
    ] {
        let got = applied_speed(req, 1024, 4000);
        let err = (got - req as f64) * 100.0;
        let bad = err.abs() > 0.005;
        fails += u32::from(bad);
        println!(
            "    req {:+7.4}%  applied {:+7.4}%  err {err:+7.4}%  {}",
            (req as f64 - 1.0) * 100.0,
            (got - 1.0) * 100.0,
            if bad { "FAIL" } else { "ok" },
        );
    }

    if fails > 0 {
        println!("FAILURES");
        std::process::ExitCode::FAILURE
    } else {
        println!("all settled");
        std::process::ExitCode::SUCCESS
    }
}
