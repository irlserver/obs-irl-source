//! PKCE and the authorization URL. The code challenge is what stops another
//! process on the machine from redeeming an intercepted code, so it is pinned
//! to the RFC's own test vector rather than to our own output.

use irl_provider::oauth::{AuthorizeRequest, challenge_for, new_pkce, nonce, percent_encode};

#[test]
fn challenge_matches_rfc_7636_appendix_b() {
    assert_eq!(
        challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn a_fresh_pkce_pair_is_well_formed_and_unique() {
    let a = new_pkce();
    let b = new_pkce();
    // 32 bytes base64url without padding is exactly 43 characters, the RFC's
    // minimum verifier length.
    assert_eq!(a.verifier.len(), 43);
    assert!(
        a.verifier
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    );
    assert_eq!(a.challenge, challenge_for(&a.verifier));
    assert_ne!(a.verifier, b.verifier);
}

#[test]
fn nonce_is_128_bits_of_hex() {
    let n = nonce();
    assert_eq!(n.len(), 32);
    assert!(n.bytes().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(n, nonce());
}

#[test]
fn percent_encoding_keeps_only_unreserved_characters() {
    assert_eq!(percent_encode("abcXYZ019-._~"), "abcXYZ019-._~");
    assert_eq!(
        percent_encode("http://127.0.0.1:47420/callback"),
        "http%3A%2F%2F127.0.0.1%3A47420%2Fcallback"
    );
    assert_eq!(percent_encode("openid profile"), "openid%20profile");
    assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
}

#[test]
fn authorize_url_carries_every_pkce_parameter_encoded() {
    let url = AuthorizeRequest {
        endpoint: "https://auth.provider.example/oauth2/authorize",
        client_id: "obs-irl-source",
        redirect_uri: "http://127.0.0.1:47420/callback",
        scope: "openid profile",
        state: "abc123",
        code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
    }
    .url();
    assert_eq!(
        url,
        "https://auth.provider.example/oauth2/authorize?response_type=code\
         &client_id=obs-irl-source\
         &redirect_uri=http%3A%2F%2F127.0.0.1%3A47420%2Fcallback\
         &scope=openid%20profile\
         &state=abc123\
         &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
         &code_challenge_method=S256"
    );
}

#[test]
fn authorize_url_appends_to_an_existing_query() {
    let url = AuthorizeRequest {
        endpoint: "https://auth.provider.example/authorize?tenant=x",
        client_id: "c",
        redirect_uri: "http://127.0.0.1:47420/callback",
        scope: "openid",
        state: "s",
        code_challenge: "ch",
    }
    .url();
    assert!(
        url.starts_with("https://auth.provider.example/authorize?tenant=x&response_type=code&")
    );
}
