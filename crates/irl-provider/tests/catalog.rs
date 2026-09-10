//! The `providers.json` format: what a deployment may write, what is refused,
//! and the order the dropdown gets.

use irl_provider::{Catalog, CatalogEntry, CatalogError};

fn entry(name: &str, base_url: &str, priority: u32) -> CatalogEntry {
    CatalogEntry {
        name: name.to_owned(),
        base_url: base_url.to_owned(),
        priority,
    }
}

#[test]
fn a_single_provider_deployment() {
    let catalog = Catalog::parse(
        r#"{
            "providers": [
                { "name": "Example", "base_url": "https://example.com", "priority": 100 }
            ],
            "allow_custom": false
        }"#,
    )
    .unwrap();
    assert_eq!(
        catalog.providers(),
        &[entry("Example", "https://example.com", 100)]
    );
    assert!(!catalog.allow_custom());
}

#[test]
fn priority_orders_the_dropdown_and_ties_keep_file_order() {
    let catalog = Catalog::parse(
        r#"{
            "providers": [
                { "name": "Low", "base_url": "https://low.example" },
                { "name": "First tie", "base_url": "https://a.example", "priority": 10 },
                { "name": "Top", "base_url": "https://top.example", "priority": 100 },
                { "name": "Second tie", "base_url": "https://b.example", "priority": 10 }
            ]
        }"#,
    )
    .unwrap();
    let names: Vec<&str> = catalog
        .providers()
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["Top", "First tie", "Second tie", "Low"]);
}

#[test]
fn custom_is_off_unless_asked_for() {
    let catalog = Catalog::parse(r#"{ "providers": [] }"#).unwrap();
    assert!(catalog.providers().is_empty());
    assert!(!catalog.allow_custom());

    let catalog = Catalog::parse(r#"{ "providers": [], "allow_custom": true }"#).unwrap();
    assert!(catalog.allow_custom());
}

#[test]
fn base_urls_are_normalized_like_the_custom_field() {
    let catalog = Catalog::parse(
        r#"{ "providers": [ { "name": "Example", "base_url": " https://example.com/ " } ] }"#,
    )
    .unwrap();
    assert_eq!(catalog.providers()[0].base_url, "https://example.com");
}

#[test]
fn a_duplicate_base_url_is_refused_even_when_spelt_differently() {
    let err = Catalog::parse(
        r#"{ "providers": [
            { "name": "A", "base_url": "https://example.com" },
            { "name": "B", "base_url": "https://example.com/" }
        ] }"#,
    )
    .unwrap_err();
    assert!(
        matches!(err, CatalogError::DuplicateBaseUrl { .. }),
        "{err}"
    );
}

#[test]
fn an_invalid_base_url_names_the_provider() {
    let err =
        Catalog::parse(r#"{ "providers": [ { "name": "Broken", "base_url": "not a url" } ] }"#)
            .unwrap_err();
    assert!(matches!(err, CatalogError::BadBaseUrl { .. }), "{err}");
    assert!(err.to_string().contains("Broken"), "{err}");
}

#[test]
fn a_blank_name_is_refused() {
    let err = Catalog::parse(
        r#"{ "providers": [ { "name": "  ", "base_url": "https://example.com" } ] }"#,
    )
    .unwrap_err();
    assert!(matches!(err, CatalogError::EmptyName { index: 0 }), "{err}");
}

#[test]
fn a_misspelt_field_is_an_error_not_a_default() {
    let err = Catalog::parse(r#"{ "providers": [], "allow_costum": true }"#).unwrap_err();
    assert!(matches!(err, CatalogError::Json(_)), "{err}");
    assert!(err.to_string().contains("allow_costum"), "{err}");

    let err = Catalog::parse(
        r#"{ "providers": [ { "name": "A", "base_url": "https://example.com", "prio": 1 } ] }"#,
    )
    .unwrap_err();
    assert!(matches!(err, CatalogError::Json(_)), "{err}");
}

#[test]
fn the_built_in_constructor_sorts_too() {
    let catalog = Catalog::new(
        vec![
            entry("Low", "https://low.example", 1),
            entry("High", "https://high.example", 100),
        ],
        true,
    );
    assert_eq!(catalog.providers()[0].name, "High");
    assert!(catalog.allow_custom());
}
