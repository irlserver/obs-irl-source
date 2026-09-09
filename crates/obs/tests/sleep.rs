//! `sleep_ms` must block for the requested time even at the audio pump's
//! 1 ms minimum. This guards against routing it back through libobs's
//! `os_sleep_ms`, which on Windows 8+ turns a 1 ms wait into `Sleep(0)`.
//!
//! Windows only: that is the platform the regression exists on, and every
//! other platform's `os_sleep_ms(1)` is a real sleep, so the assertions could
//! never fail there. The binary references no libobs symbol, so unlike the
//! rest of this crate's tests it needs no `obs.dll` to load against.
#![cfg(windows)]

use std::time::{Duration, Instant};

#[test]
fn one_millisecond_waits_do_not_degenerate_into_yields() {
    let start = Instant::now();
    for _ in 0..20 {
        obs::time::sleep_ms(1);
    }
    assert!(
        start.elapsed() >= Duration::from_millis(20),
        "sleep_ms(1) returned early; an idle audio thread would busy-poll"
    );
}

#[test]
fn longer_waits_do_not_return_early() {
    let start = Instant::now();
    obs::time::sleep_ms(20);
    assert!(start.elapsed() >= Duration::from_millis(20));
}
