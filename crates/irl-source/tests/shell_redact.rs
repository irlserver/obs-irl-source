//! Input-URL redaction (port of master 706372c): log lines keep the endpoint
//! identity, never credentials.

use obs_irl_source::log::redacted_input_url;

#[test]
fn redaction_keeps_protocol_host_and_port_only() {
    assert_eq!(
        redacted_input_url(c"srt://ingest.example:9000?passphrase=supersecret"),
        "srt://ingest.example:9000"
    );
    assert_eq!(
        redacted_input_url(c"rtmp://user:hunter2@live.example/app/streamkey-123"),
        "rtmp://live.example"
    );
    assert_eq!(
        redacted_input_url(c"srt://[2001:db8::1]:9000?streamid=key"),
        "srt://[2001:db8::1]:9000"
    );
    assert_eq!(redacted_input_url(c"not a url"), "<redacted>");
    assert_eq!(
        redacted_input_url(c"file:///home/user/secret.ts"),
        "file://<redacted>"
    );
}

/// The lines below are FFmpeg's own, verbatim from the bundled tree:
/// `libavformat/libsrt.c` prints `h->filename` (the whole URL, passphrase and
/// all) at both WARNING and ERROR, and `libavformat/avio.c` prints a bare
/// option string with no scheme in front of it.
#[test]
fn ffmpeg_lines_lose_the_url_and_keep_the_diagnosis() {
    use obs_irl_source::log::redacted_log_line;

    assert_eq!(
        redacted_log_line(
            "[srt @ 0x55f0] Connection to srt://ingest.example:9000?passphrase=hunter2&streamid=publish/live/abc failed: Connection timed out"
        ),
        "[srt @ 0x55f0] Connection to srt://ingest.example:9000 failed: Connection timed out"
    );
    assert_eq!(
        redacted_log_line(
            "[srt @ 0x55f0] Connection to srt://[2001:db8::1]:9000?passphrase=x failed (Operation timed out), trying next address"
        ),
        "[srt @ 0x55f0] Connection to srt://[2001:db8::1]:9000 failed (Operation timed out), trying next address"
    );
    assert_eq!(
        redacted_log_line(
            "[rtmp @ 0x1] Cannot open connection rtmp://user:pw@live.example/app/key-123."
        ),
        "[rtmp @ 0x1] Cannot open connection rtmp://live.example."
    );
    // Quoted, the way avio.c and concatdec.c write one.
    assert_eq!(
        redacted_log_line("Impossible to open 'srt://ingest.example:9000?passphrase=s'"),
        "Impossible to open 'srt://ingest.example:9000'"
    );
    // No scheme to find: the option string on its own.
    assert_eq!(
        redacted_log_line(
            "[srt @ 0x1] Error parsing options string passphrase=hunter2&latency=2000"
        ),
        "[srt @ 0x1] Error parsing options string passphrase=<redacted>&latency=2000"
    );
}

#[test]
fn redaction_leaves_ordinary_diagnostics_alone() {
    use obs_irl_source::log::redacted_log_line;

    for line in [
        "[mpegts @ 0x1] Could not detect TS packet size, defaulting to non-FEC/DVHS",
        "[h264 @ 0x1] Increasing reorder buffer to 1",
        "[srt @ 0x1] Protocol 'srt' not on whitelist 'file,crypto'!",
        "[srt @ 0x1] failed to set option passphrase on socket: Invalid value",
        // A word that merely ends in a sensitive name is not a parameter.
        "[x @ 0x1] monkey=42",
        // A bare "://" with no scheme in front of it is not a URL.
        "[x @ 0x1] wrote ://",
    ] {
        assert_eq!(redacted_log_line(line), line);
    }
}
