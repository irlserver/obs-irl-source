//! `min_plugin_version`: a provider's floor must never lock users out because
//! it was mistyped, and must not be dodged by a pre-release suffix.

use irl_provider::version::{at_least, parse};

#[test]
fn parses_the_numeric_core_only() {
    assert_eq!(parse("2.1.0"), Some((2, 1, 0)));
    assert_eq!(parse("v2.1.0"), Some((2, 1, 0)));
    assert_eq!(parse("2.1.0-rc.1"), Some((2, 1, 0)));
    assert_eq!(parse("2.1.0+build.7"), Some((2, 1, 0)));
    assert_eq!(parse(" 10.20.30 "), Some((10, 20, 30)));
    assert_eq!(parse("2.1"), None);
    assert_eq!(parse("2.1.0.4"), None);
    assert_eq!(parse("two"), None);
    assert_eq!(parse(""), None);
}

#[test]
fn comparison_is_numeric_per_component() {
    assert!(at_least("2.1.0", "2.1.0"));
    assert!(at_least("2.10.0", "2.9.9"));
    assert!(at_least("3.0.0", "2.99.99"));
    assert!(!at_least("2.0.2", "2.1.0"));
    assert!(!at_least("2.1.0-rc.1", "2.1.1"));
    assert!(at_least("2.1.0-rc.1", "2.1.0"));
}

#[test]
fn an_unparseable_floor_is_ignored_and_an_unparseable_plugin_is_too_old() {
    assert!(at_least("2.1.0", "soon"));
    assert!(at_least("2.1.0", ""));
    assert!(!at_least("dev", "2.1.0"));
}
