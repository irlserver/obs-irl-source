//! The loopback redirect: parsing, the state check, and one real round-trip
//! over a socket.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;

use irl_provider::loopback::{
    Handoff, Outcome, PORTS, all_redirect_uris, classify, parse_query, percent_decode,
    redirect_uri_for,
};

fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn percent_decoding_leaves_plus_alone() {
    // An authorization code may be base64, and base64 contains `+`. A form
    // decoder would turn it into a space and the code exchange would fail.
    assert_eq!(percent_decode("ab%2Bcd+ef"), "ab+cd+ef");
    assert_eq!(percent_decode("%41%42%43"), "ABC");
    assert_eq!(percent_decode("%4"), "%4");
    assert_eq!(percent_decode("%zz"), "%zz");
}

#[test]
fn query_parsing_reads_the_request_line() {
    let q = parse_query("GET /callback?code=abc%2B1&state=xyz HTTP/1.1");
    assert_eq!(q.get("code").map(String::as_str), Some("abc+1"));
    assert_eq!(q.get("state").map(String::as_str), Some("xyz"));
    assert!(parse_query("GET /callback HTTP/1.1").is_empty());
    assert!(parse_query("").is_empty());
}

#[test]
fn a_redirect_is_only_ours_if_the_state_matches() {
    assert_eq!(classify(&params(&[("code", "c")]), "s"), None);
    assert_eq!(
        classify(&params(&[("code", "c"), ("state", "other")]), "s"),
        None
    );
    assert_eq!(
        classify(&params(&[("code", "c"), ("state", "s")]), "s"),
        Some(Outcome::Code("c".to_owned()))
    );
    assert_eq!(
        classify(&params(&[("error", "access_denied"), ("state", "s")]), "s"),
        Some(Outcome::Denied("access_denied".to_owned()))
    );
    // A matching state with neither code nor error is a malformed redirect,
    // not a success.
    assert_eq!(classify(&params(&[("state", "s")]), "s"), None);
    assert_eq!(
        classify(&params(&[("code", ""), ("state", "s")]), "s"),
        None
    );
}

#[test]
fn every_port_has_a_registered_redirect_uri() {
    let uris = all_redirect_uris();
    assert_eq!(uris.len(), PORTS.count());
    assert_eq!(uris[0], "http://127.0.0.1:47420/callback");
    assert_eq!(redirect_uri_for(47429), "http://127.0.0.1:47429/callback");
}

#[test]
fn a_real_redirect_is_answered_and_returns_the_code() {
    let handoff = Handoff::bind("nonce-1".to_owned()).expect("a loopback port is free");
    let port = handoff.port();
    assert!(PORTS.contains(&port));
    assert_eq!(handoff.redirect_uri(), redirect_uri_for(port));

    let client = std::thread::spawn(move || {
        let mut bodies = Vec::new();
        // First a redirect with the wrong state, which must be answered and
        // ignored, then the right one.
        for target in [
            "/callback?code=stolen&state=someone-else",
            "/callback?code=the-code&state=nonce-1",
        ] {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(s, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut body = String::new();
            s.read_to_string(&mut body).unwrap();
            bodies.push(body);
        }
        bodies
    });

    assert_eq!(handoff.wait(), Outcome::Code("the-code".to_owned()));
    let bodies = client.join().unwrap();
    assert!(bodies[0].starts_with("HTTP/1.1 200 OK"));
    assert!(bodies[0].contains("Sign-in failed"));
    assert!(bodies[1].contains("Signed in"));
    assert!(bodies[1].contains("history.replaceState"));
}
