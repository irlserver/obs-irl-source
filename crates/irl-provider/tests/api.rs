//! The list response as the plugin reads it.

use irl_provider::api::{parse_ingest_list, resolve_endpoint, valid_ingest_id};

#[test]
fn ingest_ids_are_url_and_settings_safe() {
    assert!(valid_ingest_id("a1b2:eu"));
    assert!(valid_ingest_id("ckx1abc.FIN-2_x"));
    assert!(!valid_ingest_id(""));
    assert!(!valid_ingest_id("a/b"));
    assert!(!valid_ingest_id("srt://host?streamid=play/x"));
    assert!(!valid_ingest_id("a b"));
    assert!(!valid_ingest_id(&"a".repeat(129)));
}

#[test]
fn the_list_parses_and_optional_fields_default() {
    let list = parse_ingest_list(
        r#"{"ingests":[
            {"id":"a1:eu","name":"Main phone","detail":"Europe","online":true,"bitrate_kbps":4200},
            {"id":"c3:eu","name":"Backup phone"}
        ]}"#,
    )
    .unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].detail.as_deref(), Some("Europe"));
    assert_eq!(list[0].online, Some(true));
    assert_eq!(list[0].bitrate_kbps, Some(4200));
    assert_eq!(list[1].detail, None);
    assert_eq!(list[1].online, None);
    assert_eq!(list[1].bitrate_kbps, None);
}

#[test]
fn entries_outside_the_contract_are_dropped_not_fatal() {
    let list = parse_ingest_list(
        r#"{"ingests":[
            {"id":"ok","name":"Fine"},
            {"id":"has/slash","name":"Bad id"},
            {"id":"blank","name":"   "},
            {"id":"also-ok","name":"Also fine","extra":"ignored"}
        ]}"#,
    )
    .unwrap();
    let ids: Vec<&str> = list.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["ok", "also-ok"]);
}

#[test]
fn an_empty_or_missing_list_is_empty() {
    assert!(parse_ingest_list(r#"{"ingests":[]}"#).unwrap().is_empty());
    assert!(parse_ingest_list("{}").unwrap().is_empty());
    assert!(parse_ingest_list("null").is_err());
}

#[test]
fn the_resolve_endpoint_hangs_off_the_list_endpoint() {
    assert_eq!(
        resolve_endpoint("https://api.provider.example/irl-source/ingests", "a1:eu"),
        "https://api.provider.example/irl-source/ingests/a1:eu/url"
    );
    assert_eq!(
        resolve_endpoint("https://api.provider.example/ingests/", "x"),
        "https://api.provider.example/ingests/x/url"
    );
}
