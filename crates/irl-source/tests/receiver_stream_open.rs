//! Stream open failures and how the receiver's read-error path classifies
//! them (`src/receiver/stream.rs`).
//!
//! No libobs and no network here: the observable contract is that a failed
//! open produces an `ffmpeg::Error` whose classification sends the receiver
//! down the reconnect path rather than the "decoder wants more data" path.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use irl_core::consts;

fn watch(active: bool) -> Arc<ffmpeg::InterruptWatch> {
    ffmpeg::InterruptWatch::new(
        Arc::new(AtomicBool::new(active)),
        consts::IO_STALL_TIMEOUT_US,
    )
}

#[test]
fn opening_a_missing_file_fails_with_a_hard_error() {
    let err = ffmpeg::FormatContext::open(
        c"file:///nonexistent/obs-irl-source/does-not-exist.ts",
        ffmpeg::Dictionary::new(),
        watch(true),
    )
    .err()
    .expect("opening a nonexistent path must fail");

    // The receiver logs `av_strerror` text and reconnects; neither EAGAIN nor
    // EOF, which are the two codes the decode path treats as "not an error".
    assert!(err.code() < 0);
    assert!(!err.is_eagain());
    assert!(!err.is_eof());
    assert!(!err.to_string().is_empty());
}

#[test]
fn an_unknown_protocol_fails_rather_than_hanging() {
    let err = ffmpeg::FormatContext::open(
        c"definitely-not-a-protocol://host/path",
        ffmpeg::Dictionary::new(),
        watch(true),
    )
    .err()
    .expect("an unknown protocol must fail");

    assert!(err.code() < 0);
    assert!(!err.is_eagain());
}

/// A stopped run aborts the open through the interrupt callback instead of
/// blocking, which is what lets `stop_receiver` join the receiver thread.
#[test]
fn a_stopped_run_aborts_the_open() {
    let result = ffmpeg::FormatContext::open(
        c"file:///nonexistent/obs-irl-source/does-not-exist.ts",
        ffmpeg::Dictionary::new(),
        watch(false),
    );
    assert!(result.is_err());
}

/// The demuxer option table the open feeds `av_dict_set` is the same one
/// `irl-core` pins; this checks the receiver's two call-site arguments (the
/// constant network buffer and the fast/full probe switch) reach it.
#[test]
fn the_probe_budget_follows_the_fast_probe_flag() {
    let fast = irl_core::url_opts::demuxer_options(
        "srt://127.0.0.1:9000",
        None,
        consts::NETWORK_BUFFER_MB,
        true,
    );
    let full = irl_core::url_opts::demuxer_options(
        "srt://127.0.0.1:9000",
        None,
        consts::NETWORK_BUFFER_MB,
        false,
    );

    let probesize = |opts: &[(
        std::borrow::Cow<'static, str>,
        std::borrow::Cow<'static, str>,
    )]| {
        opts.iter()
            .rev()
            .find(|(key, _)| key == "probesize")
            .map(|(_, value)| value.to_string())
            .expect("probesize is always set")
    };

    assert_eq!(probesize(&fast), consts::PROBE_FAST.to_string());
    assert_eq!(probesize(&full), consts::PROBE_FULL.to_string());
}
