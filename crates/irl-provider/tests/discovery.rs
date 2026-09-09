//! The provider document is the one thing a provider writes by hand, so the
//! validation is where their typos land.

use irl_provider::discovery::{
    DiscoveryError, normalize_base_url, oidc_url, parse_doc, parse_oidc, valid_provider_id,
    well_known_url,
};

const DOC: &str = r#"{
    "protocol_version": 1,
    "id": "example",
    "name": "Example Relays",
    "issuer": "https://auth.provider.example",
    "client_id": "obs-irl-source",
    "scope": "openid",
    "ingests_endpoint": "https://api.provider.example/irl-source/ingests",
    "min_plugin_version": "2.1.0"
}"#;

#[test]
fn a_complete_document_parses() {
    let doc = parse_doc(DOC).unwrap();
    assert_eq!(doc.id, "example");
    assert_eq!(doc.name, "Example Relays");
    assert_eq!(doc.client_id.as_deref(), Some("obs-irl-source"));
    assert_eq!(doc.min_plugin_version.as_deref(), Some("2.1.0"));
}

#[test]
fn optional_fields_default() {
    let doc = parse_doc(
        r#"{"protocol_version":1,"id":"x","name":"X","issuer":"https://a.example","ingests_endpoint":"https://b.example/i"}"#,
    )
    .unwrap();
    assert_eq!(doc.client_id, None);
    assert_eq!(doc.scope, "openid");
    assert_eq!(doc.min_plugin_version, None);
}

#[test]
fn an_unknown_protocol_version_is_refused() {
    let err =
        parse_doc(&DOC.replace("\"protocol_version\": 1", "\"protocol_version\": 2")).unwrap_err();
    assert!(matches!(err, DiscoveryError::UnsupportedVersion(2)));
}

#[test]
fn ids_are_file_and_settings_safe() {
    assert!(valid_provider_id("irlserver"));
    assert!(valid_provider_id("go-irl-2"));
    assert!(!valid_provider_id(""));
    assert!(!valid_provider_id("IRLServer"));
    assert!(!valid_provider_id("../etc"));
    assert!(!valid_provider_id("a b"));
    assert!(!valid_provider_id(&"a".repeat(33)));
    assert!(matches!(
        parse_doc(&DOC.replace("\"id\": \"example\"", "\"id\": \"Bad Id\"")).unwrap_err(),
        DiscoveryError::Invalid(_)
    ));
}

#[test]
fn endpoints_must_be_https() {
    let plain = DOC.replace(
        "https://api.provider.example",
        "http://api.provider.example",
    );
    assert!(matches!(
        parse_doc(&plain).unwrap_err(),
        DiscoveryError::Invalid(_)
    ));
    let plain_issuer = DOC.replace(
        "https://auth.provider.example",
        "http://auth.provider.example",
    );
    assert!(matches!(
        parse_doc(&plain_issuer).unwrap_err(),
        DiscoveryError::Invalid(_)
    ));
}

#[test]
fn base_urls_are_normalized_and_gated() {
    assert_eq!(
        normalize_base_url("  https://provider.example/ "),
        Some("https://provider.example".to_owned())
    );
    assert_eq!(
        normalize_base_url("https://provider.example/dash//"),
        Some("https://provider.example/dash".to_owned())
    );
    // A provider under development may run plain http on the loopback
    // interface, and nowhere else.
    assert!(normalize_base_url("http://127.0.0.1:3000").is_some());
    assert!(normalize_base_url("http://localhost:3000/api").is_some());
    assert!(normalize_base_url("http://provider.example").is_none());
    assert!(normalize_base_url("http://127.0.0.1.evil.example").is_none());
    assert!(normalize_base_url("provider.example").is_none());
    assert!(normalize_base_url("https://pro vider.example").is_none());
    assert!(normalize_base_url("").is_none());
}

#[test]
fn well_known_paths_tolerate_a_trailing_slash() {
    assert_eq!(
        well_known_url("https://provider.example/"),
        "https://provider.example/.well-known/irl-source-provider.json"
    );
    assert_eq!(
        oidc_url("https://auth.provider.example/api/auth"),
        "https://auth.provider.example/api/auth/.well-known/openid-configuration"
    );
}

#[test]
fn oidc_configuration_needs_only_two_endpoints() {
    let oidc = parse_oidc(
        r#"{"issuer":"https://a.example","authorization_endpoint":"https://a.example/authorize","token_endpoint":"https://a.example/token","jwks_uri":"https://a.example/jwks"}"#,
    )
    .unwrap();
    assert_eq!(oidc.registration_endpoint, None);
    assert_eq!(oidc.revocation_endpoint, None);
    assert!(parse_oidc(r#"{"authorization_endpoint":"https://a.example/authorize"}"#).is_err());
    assert!(matches!(
        parse_oidc(
            r#"{"authorization_endpoint":"https://a.example/authorize","token_endpoint":"http://a.example/token"}"#
        )
        .unwrap_err(),
        DiscoveryError::Invalid(_)
    ));
}
