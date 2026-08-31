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
