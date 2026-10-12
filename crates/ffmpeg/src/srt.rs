//! A process-lifetime reference on libsrt's global state.
//!
//! FFmpeg's `libsrt` protocol calls `srt_startup()` on every open and
//! `srt_cleanup()` on every close. libsrt counts those calls, and when the
//! count returns to zero it stops its garbage-collector thread, which refuses
//! to exit until every closed socket is over a second old. With FFmpeg as the
//! only user, every SRT close therefore blocked for about a second, and a
//! close runs on the receiver thread that the source joins from OBS's graphics
//! thread (hide, deactivate, media controls): a second of stalled rendering on
//! every scene switch with Close When Inactive on.
//!
//! Holding one reference for as long as the plugin is loaded keeps the count
//! above zero, so FFmpeg's cleanup is a decrement and closed sockets are
//! reaped in the background by the garbage collector instead.

use core::ffi::c_int;
use std::sync::atomic::{AtomicBool, Ordering};

// The bundled libsrt is linked statically through `build.rs`.
unsafe extern "C" {
    fn srt_startup() -> c_int;
    fn srt_cleanup() -> c_int;
}

static HELD: AtomicBool = AtomicBool::new(false);

/// Take the plugin's reference. Idempotent. Returns false when libsrt could
/// not start its garbage collector; SRT URLs then fail to open, as they would
/// have without this call.
pub fn hold() -> bool {
    if HELD.swap(true, Ordering::SeqCst) {
        return true;
    }
    // SAFETY: srt_startup takes no arguments and is internally synchronised.
    // libsrt counts the call even when it fails, so the reference is released
    // in `release` either way.
    unsafe { srt_startup() >= 0 }
}

/// Drop the plugin's reference. Must run before the module is unloaded: the
/// garbage-collector thread executes code inside this binary. Blocks for up to
/// a second when a socket was closed just before.
pub fn release() {
    if !HELD.swap(false, Ordering::SeqCst) {
        return;
    }
    // SAFETY: pairs with the `srt_startup` in `hold`, which HELD proves ran.
    unsafe {
        srt_cleanup();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use crate::{Dictionary, FormatContext, InterruptWatch};

    /// An aborted listener open goes through the same `srt_close` +
    /// `srt_cleanup` as a stream closing. With the reference held it must not
    /// wait out libsrt's one-second socket reaping.
    #[test]
    fn closing_an_srt_socket_does_not_block() {
        assert!(super::hold());

        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .and_then(|s| s.local_addr())
            .map(|a| a.port())
            .unwrap();
        let url = std::ffi::CString::new(format!("srt://127.0.0.1:{port}?mode=listener")).unwrap();

        let active = Arc::new(AtomicBool::new(true));
        let watch = InterruptWatch::new(active.clone(), 10_000_000);
        watch.set_awaits_caller(true);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            active.store(false, Ordering::Relaxed);
        });

        let start = Instant::now();
        let result = FormatContext::open(&url, Dictionary::new(), watch);
        let elapsed = start.elapsed();
        stopper.join().unwrap();

        assert!(result.is_err(), "nobody called in, so the open must abort");
        assert!(
            elapsed < Duration::from_millis(600),
            "aborting the listener took {elapsed:?}"
        );
        super::release();
    }
}
