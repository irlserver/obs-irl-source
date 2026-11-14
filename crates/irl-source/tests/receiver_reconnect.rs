//! The reconnect loop while nothing is connected (`src/receiver/`), run
//! against a loopback port nobody listens on, so every attempt is refused at
//! once.

mod common;

use std::ffi::CString;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use irl_core::consts;
use obs_irl_source::receiver::receiver_thread;
use obs_irl_source::shared::StreamConfig;

/// Once an attempt fails, the source stays reconnecting through every wait
/// and every attempt after it, until one opens (#42). Media state reads
/// OPENING for all of it. When the flag dropped as each wait ended, the
/// attempts after it read as connected, and a stale video frame then made
/// the source read PLAYING with nothing connected.
#[test]
fn the_source_stays_reconnecting_between_failed_attempts() {
    let mut hot = common::hot_values(consts::DEFAULT_BUFFER_TARGET_MS as i32);
    hot.reconnect_delay_s = consts::RECONNECT_DELAY_MIN_S;
    let stream = StreamConfig {
        url: CString::new("tcp://127.0.0.1:9").unwrap(),
        ..common::stream_config(false)
    };
    let shared = common::shared(stream, hot);
    shared.flags.thread_active.store(true, Relaxed);
    let worker = {
        let shared = shared.clone();
        std::thread::spawn(move || receiver_thread(shared))
    };

    let poll = Duration::from_millis(2);
    let first_failure = Instant::now() + Duration::from_secs(5);
    while !shared.flags.reconnecting.load(Relaxed) {
        assert!(
            Instant::now() < first_failure,
            "the first attempt never failed"
        );
        std::thread::sleep(poll);
    }
    // Two and a half waits, so the window spans the end of at least two of
    // them and the attempts that follow.
    let watch_until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < watch_until {
        assert!(
            shared.flags.reconnecting.load(Relaxed),
            "read as connected between attempts"
        );
        std::thread::sleep(poll);
    }

    shared.flags.thread_active.store(false, Relaxed);
    worker.join().unwrap();
}
