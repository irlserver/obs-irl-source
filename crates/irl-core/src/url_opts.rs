//! Demuxer option table (port of `apply_demuxer_options`, `receiver-stream.c:20-187`).
//!
//! The C function writes straight into an `AVDictionary`; here it is an
//! ordered list the caller feeds to `av_dict_set` in order, which gives the
//! same result — later entries override earlier ones, and the user's own
//! options come last so they win.

use std::borrow::Cow;

use crate::consts;

/// Whether `url` starts with `scheme://` (prefix match, not substring).
///
/// The protocol test is on the scheme itself: a query parameter or a path
/// segment must not decide which options apply.
pub fn has_scheme(url: &str, scheme: &str) -> bool {
    url.strip_prefix(scheme)
        .is_some_and(|rest| rest.starts_with("://"))
}

fn owned(key: &'static str, value: String) -> (Cow<'static, str>, Cow<'static, str>) {
    (Cow::Borrowed(key), Cow::Owned(value))
}

fn borrowed(key: &'static str, value: &'static str) -> (Cow<'static, str>, Cow<'static, str>) {
    (Cow::Borrowed(key), Cow::Borrowed(value))
}

/// The ordered `(key, value)` list to feed `av_dict_set`; later entries
/// override earlier ones, and the user's `extra` options come last.
pub fn demuxer_options(
    url: &str,
    extra: Option<&str>,
    network_buffer_mb: i64,
    fast_probe: bool,
) -> Vec<(Cow<'static, str>, Cow<'static, str>)> {
    let mut opts: Vec<(Cow<'static, str>, Cow<'static, str>)> = Vec::with_capacity(20);

    // A reconnect probes fast: the previous session already showed what the
    // stream carries, and every probe byte is time the feed stays dark after
    // a `!fix`. The caller retries with the full probe if the short one comes
    // up missing a stream the last session had.
    let probe = if fast_probe {
        consts::PROBE_FAST
    } else {
        consts::PROBE_FULL
    };
    opts.push(owned("probesize", probe.to_string()));
    opts.push(owned("analyzeduration", probe.to_string()));

    // No +discardcorrupt: mpegts marks a PES corrupt on any continuity
    // counter discontinuity, which on a lossy uplink silently deletes video.
    opts.push(borrowed("fflags", "+genpts"));
    // mpegts only (the SRT/UDP/RIST carriers), ignored elsewhere: lets a new
    // PMT layout map onto the existing streams when a relay keeps the ingest
    // socket alive across an encoder swap.
    opts.push(borrowed("merge_pmt_versions", "1"));
    // udp:// only. A burst the receive ring cannot absorb is a fatal read
    // error by default, turning one hiccup into a full reconnect cycle.
    opts.push(borrowed("overrun_nonfatal", "1"));
    // rtmp(s):// only. Declare live intent, and shrink the client buffer hint
    // from FFmpeg's 3000 ms default, which nginx-rtmp paces delivery by.
    opts.push(borrowed("rtmp_live", "live"));
    opts.push(owned("rtmp_buffer", consts::RTMP_BUFFER_MS.to_string()));
    // Every TCP-based transport (rtmp, http, the tcp under tls).
    opts.push(borrowed("tcp_nodelay", "1"));
    // http(s) inputs only; harmless no-ops elsewhere. An FFmpeg-internal
    // reconnect keeps the decoders and the keyframe gate warm.
    opts.push(borrowed("reconnect", "1"));
    opts.push(borrowed("reconnect_streamed", "1"));
    opts.push(borrowed("reconnect_on_network_error", "1"));
    // FFmpeg 9.0 flipped tls_verify to default on, and the bundled mbedTLS
    // backend has no system trust store to fall back on, so every https:// and
    // rtmps:// ingest would fail its handshake. Restoring the pre-9.0 default
    // keeps working setups working; `extra` can turn it back on.
    opts.push(borrowed("tls_verify", "0"));

    if network_buffer_mb > 0 {
        let bytes = (network_buffer_mb * 1024 * 1024).to_string();
        // "buffer_size" is bytes for udp:// (and rtp/rtsp, which forward it),
        // but librist reuses the name for its recovery window in
        // milliseconds, declared with a max of 30000 — a byte count there
        // fails avformat_open_input outright with ERANGE.
        if !has_scheme(url, "rist") {
            opts.push(owned("buffer_size", bytes.clone()));
        }
        // "recv_buffer_size" is bytes for tcp:// and libsrt, and is what
        // gives the setting any effect on rtmp(s):// and http(s)://.
        opts.push(owned("recv_buffer_size", bytes));

        // udp:// also has a userspace ring between its receive thread and the
        // demuxer, sized in 188-byte TS packets (default 7*4096 ≈ 5.3 MB).
        // Grow it with the setting, never shrink it.
        let fifo_pkts = network_buffer_mb * 1024 * 1024 / 188;
        if fifo_pkts > consts::UDP_FIFO_DEFAULT_PACKETS {
            opts.push(owned("fifo_size", fifo_pkts.to_string()));
        }
    }

    if has_scheme(url, "srt") {
        opts.push(owned("latency", consts::SRT_LATENCY_US.to_string()));
    }

    if let Some(extra) = extra {
        for (key, value) in parse_extra(extra) {
            opts.push((Cow::Owned(key), Cow::Owned(value)));
        }
    }

    opts
}

/// Parse space-separated `key=value` pairs; entries without `=` are ignored.
///
/// Ports the `strtok_r(dup, " ")` loop: runs of spaces collapse, and a token
/// splits at its first `=`.
pub fn parse_extra(extra: &str) -> Vec<(String, String)> {
    extra
        .split(' ')
        .filter(|token| !token.is_empty())
        .filter_map(|token| {
            token
                .split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Does this URL wait to be called, rather than dialing out?
///
/// It decides whether the I/O stall deadline applies before a connection
/// exists. `srt://` and `rist://` both spell it in the query string, and
/// rendezvous waits the same way from the caller's point of view.
///
/// The test is on the query only: a path or a passphrase that happens to
/// contain the word must not decide this.
pub fn url_awaits_caller(url: &str) -> bool {
    let Some((_, query)) = url.split_once('?') else {
        return false;
    };
    query.contains("mode=listener")
        || query.contains("mode=rendezvous")
        || query.contains("listen=1")
}

#[cfg(test)]
mod awaits_caller_tests {
    use super::url_awaits_caller;

    #[test]
    fn listener_and_rendezvous_urls_wait_to_be_called() {
        assert!(url_awaits_caller("srt://0.0.0.0:7000?mode=listener"));
        assert!(url_awaits_caller("srt://0.0.0.0:7000?mode=rendezvous"));
        assert!(url_awaits_caller("rist://0.0.0.0:7000?listen=1"));
        assert!(url_awaits_caller(
            "srt://0.0.0.0:7000?latency=200000&mode=listener"
        ));
    }

    #[test]
    fn caller_urls_dial_out() {
        assert!(!url_awaits_caller("srt://host.example:7000"));
        assert!(!url_awaits_caller("srt://host.example:7000?mode=caller"));
        assert!(!url_awaits_caller("rtmp://host.example/app/key"));
        assert!(!url_awaits_caller(""));
    }

    #[test]
    fn only_the_query_decides() {
        // A path or a passphrase containing the word must not flip it: a
        // caller URL that never times out would hang on a dead host forever.
        assert!(!url_awaits_caller("file:///media/mode=listener/clip.ts"));
        assert!(!url_awaits_caller("srt://host.example:7000#mode=listener"));
        assert!(url_awaits_caller(
            "srt://host:7000?passphrase=mode%3Dlistener&mode=listener"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(opts: &'a [(Cow<'static, str>, Cow<'static, str>)], key: &str) -> Option<&'a str> {
        // Later entries override earlier ones, so the effective value is the
        // last one set — which is what av_dict_set leaves in the dictionary.
        opts.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_ref())
    }

    fn has(opts: &[(Cow<'static, str>, Cow<'static, str>)], key: &str) -> bool {
        opts.iter().any(|(k, _)| k == key)
    }

    #[test]
    fn scheme_is_a_prefix_not_a_substring() {
        assert!(has_scheme("srt://host:1234", "srt"));
        assert!(has_scheme("rist://host:1234", "rist"));
        // A path segment or query parameter must not decide the options.
        assert!(!has_scheme("http://host/srt://x", "srt"));
        assert!(!has_scheme("http://host/?p=rist://x", "rist"));
        assert!(!has_scheme("srt:/host", "srt"));
        assert!(!has_scheme("srtla://host", "srt"));
        assert!(!has_scheme("", "srt"));
        // A bare scheme still matches, as the C strncmp pair does.
        assert!(has_scheme("srt://", "srt"));
    }

    #[test]
    fn fast_probe_selects_the_one_megabyte_probe() {
        let opts = demuxer_options("srt://h:1", None, 2, true);
        assert_eq!(find(&opts, "probesize"), Some("1000000"));
        assert_eq!(find(&opts, "analyzeduration"), Some("1000000"));

        let opts = demuxer_options("srt://h:1", None, 2, false);
        assert_eq!(find(&opts, "probesize"), Some("5000000"));
        assert_eq!(find(&opts, "analyzeduration"), Some("5000000"));
    }

    #[test]
    fn the_unconditional_options_are_always_present() {
        let opts = demuxer_options("rtmp://h/live/key", None, 2, false);
        assert_eq!(find(&opts, "fflags"), Some("+genpts"));
        assert_eq!(find(&opts, "merge_pmt_versions"), Some("1"));
        assert_eq!(find(&opts, "overrun_nonfatal"), Some("1"));
        assert_eq!(find(&opts, "rtmp_live"), Some("live"));
        assert_eq!(find(&opts, "rtmp_buffer"), Some("1000"));
        assert_eq!(find(&opts, "tcp_nodelay"), Some("1"));
        assert_eq!(find(&opts, "reconnect"), Some("1"));
        assert_eq!(find(&opts, "reconnect_streamed"), Some("1"));
        assert_eq!(find(&opts, "reconnect_on_network_error"), Some("1"));
        // No +discardcorrupt anywhere: damaged packets must reach the decoder.
        assert!(!opts.iter().any(|(_, v)| v.contains("discardcorrupt")));
    }

    #[test]
    fn tls_verify_is_off_by_default() {
        let opts = demuxer_options("rtmps://h/live/key", None, 2, false);
        assert_eq!(find(&opts, "tls_verify"), Some("0"));
    }

    #[test]
    fn srt_gets_the_latency_window() {
        let opts = demuxer_options("srt://h:1234?streamid=x", None, 2, false);
        assert_eq!(find(&opts, "latency"), Some("200000"));

        let opts = demuxer_options("rtmp://h/live/key", None, 2, false);
        assert!(!has(&opts, "latency"));
    }

    #[test]
    fn rist_omits_buffer_size_but_keeps_recv_buffer_size() {
        let opts = demuxer_options("rist://h:1234", None, 2, false);
        assert!(
            !has(&opts, "buffer_size"),
            "librist reads buffer_size as milliseconds and fails on a byte count"
        );
        assert_eq!(find(&opts, "recv_buffer_size"), Some("2097152"));

        let opts = demuxer_options("udp://h:1234", None, 2, false);
        assert_eq!(find(&opts, "buffer_size"), Some("2097152"));
        assert_eq!(find(&opts, "recv_buffer_size"), Some("2097152"));
    }

    #[test]
    fn no_buffer_options_without_a_buffer_size() {
        let opts = demuxer_options("udp://h:1234", None, 0, false);
        assert!(!has(&opts, "buffer_size"));
        assert!(!has(&opts, "recv_buffer_size"));
        assert!(!has(&opts, "fifo_size"));
    }

    #[test]
    fn fifo_size_is_only_set_above_ffmpegs_own_default() {
        // The 2 MB default is ~11 154 packets, below FFmpeg's 7*4096.
        let opts = demuxer_options("udp://h:1234", None, consts::NETWORK_BUFFER_MB, false);
        assert!(!has(&opts, "fifo_size"));

        // 8 MB is ~44 620 packets, above it.
        let opts = demuxer_options("udp://h:1234", None, 8, false);
        assert_eq!(find(&opts, "fifo_size"), Some("44620"));
    }

    #[test]
    fn extras_are_applied_last_and_override() {
        let opts = demuxer_options(
            "srt://h:1234",
            Some("probesize=32 latency=500000 tls_verify=1 ca_file=/tmp/ca.pem"),
            2,
            true,
        );
        assert_eq!(find(&opts, "probesize"), Some("32"));
        assert_eq!(find(&opts, "latency"), Some("500000"));
        assert_eq!(find(&opts, "tls_verify"), Some("1"));
        assert_eq!(find(&opts, "ca_file"), Some("/tmp/ca.pem"));

        // The overridden entries are still in the list, just earlier.
        let probes: Vec<&str> = opts
            .iter()
            .filter(|(k, _)| k == "probesize")
            .map(|(_, v)| v.as_ref())
            .collect();
        assert_eq!(probes, vec!["1000000", "32"]);
    }

    #[test]
    fn malformed_extras_are_ignored() {
        assert_eq!(parse_extra("nokey"), Vec::new());
        assert_eq!(
            parse_extra("  a=1   b=2  "),
            vec![("a".into(), "1".into()), ("b".into(), "2".into())]
        );
        assert_eq!(parse_extra(""), Vec::new());
        // Only the first '=' splits; the rest belongs to the value.
        assert_eq!(parse_extra("k=a=b"), vec![("k".into(), "a=b".into())]);
        // A trailing '=' is an empty value, as av_dict_set would store it.
        assert_eq!(parse_extra("k="), vec![("k".into(), "".into())]);

        let opts = demuxer_options("srt://h:1", Some("garbage more_garbage"), 2, false);
        assert_eq!(find(&opts, "probesize"), Some("5000000"));
    }

    #[test]
    fn option_order_matches_the_c_dictionary_writes() {
        let opts = demuxer_options("srt://h:1", Some("x=1"), 2, true);
        let keys: Vec<&str> = opts.iter().map(|(k, _)| k.as_ref()).collect();
        assert_eq!(
            keys,
            vec![
                "probesize",
                "analyzeduration",
                "fflags",
                "merge_pmt_versions",
                "overrun_nonfatal",
                "rtmp_live",
                "rtmp_buffer",
                "tcp_nodelay",
                "reconnect",
                "reconnect_streamed",
                "reconnect_on_network_error",
                "tls_verify",
                "buffer_size",
                "recv_buffer_size",
                "latency",
                "x",
            ]
        );
    }
}
