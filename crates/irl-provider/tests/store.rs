//! The state file: recovery from damage, the round trip, and the promise
//! that no pull URL is in it.

use std::fs;
use std::path::PathBuf;

use irl_provider::Ingest;
use irl_provider::discovery::{OidcEndpoints, ProviderDoc};
use irl_provider::store::{Stored, load_all, parse, remove, save};

fn stored(id: &str) -> Stored {
    Stored {
        base_url: "https://provider.example".to_owned(),
        doc: ProviderDoc {
            protocol_version: 1,
            id: id.to_owned(),
            name: "Example".to_owned(),
            issuer: "https://auth.provider.example".to_owned(),
            client_id: None,
            scope: "openid".to_owned(),
            ingests_endpoint: "https://api.provider.example/ingests".to_owned(),
            min_plugin_version: None,
        },
        oidc: OidcEndpoints {
            authorization_endpoint: "https://auth.provider.example/authorize".to_owned(),
            token_endpoint: "https://auth.provider.example/token".to_owned(),
            registration_endpoint: None,
            revocation_endpoint: None,
        },
        client_id: Some("client".to_owned()),
        refresh_token: Some("refresh-secret".to_owned()),
        ingests: vec![Ingest {
            id: "a1:eu".to_owned(),
            name: "Main phone".to_owned(),
            detail: Some("Europe".to_owned()),
            online: Some(true),
            bitrate_kbps: Some(4200),
        }],
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "irl-provider-store-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn damaged_files_read_as_signed_out() {
    assert!(parse("").is_none());
    assert!(parse("{").is_none());
    assert!(parse(r#"{"base_url":"https://x","refresh_token":"t"}"#).is_none());
    let bad_id = serde_json::to_string(&stored("Bad Id")).unwrap();
    assert!(parse(&bad_id).is_none());
}

#[test]
fn save_and_load_round_trip() {
    let dir = temp_dir("roundtrip");
    let a = stored("alpha");
    let b = stored("beta");
    save(&dir, &a).unwrap();
    save(&dir, &b).unwrap();

    // A file whose name disagrees with the id inside it was not written by
    // `save` and is ignored.
    fs::write(
        dir.join("gamma.json"),
        serde_json::to_string(&stored("delta")).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("notes.txt"), "not json").unwrap();

    let mut loaded = load_all(&dir);
    loaded.sort_by(|x, y| x.doc.id.cmp(&y.doc.id));
    assert_eq!(loaded, vec![a.clone(), b.clone()]);

    remove(&dir, "alpha");
    assert_eq!(load_all(&dir), vec![b]);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn the_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("mode");
    save(&dir, &stored("alpha")).unwrap();
    let mode = fs::metadata(dir.join("alpha.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn no_pull_url_is_ever_serialized() {
    // `Ingest` has no url field, so this is a type-level guarantee; the test
    // pins the wire shape so a field added later is a conscious change.
    let json = serde_json::to_string(&stored("alpha")).unwrap();
    assert!(!json.contains("srt://"));
    assert!(!json.contains("\"url\""));
    assert!(json.contains("refresh-secret"));
}
