//! Every `module_text` key has a string in `data/locale/en-US.ini`.
//!
//! The lookup falls back to returning the key, so a missing entry does not
//! fail: it renders the dialog as bare identifiers like `AudioBufferHelp`,
//! which is only noticed by opening the properties window. This test is the
//! mechanical half of the "a new string belongs in two places" convention.
//!
//! Both sides are `include_str!`d, so nothing here reads the filesystem or
//! calls into libobs.

const LOCALE: &str = include_str!("../../../data/locale/en-US.ini");
const SOURCES: [(&str, &str); 2] = [
    ("settings.rs", include_str!("../src/settings.rs")),
    ("source.rs", include_str!("../src/source.rs")),
];

/// Every `module_text(c"…")` argument in a source file.
fn keys(src: &str) -> Vec<&str> {
    src.match_indices("module_text(c\"")
        .filter_map(|(at, pat)| {
            let rest = &src[at + pat.len()..];
            rest.find('"').map(|end| &rest[..end])
        })
        .collect()
}

/// Every `Key=` at the start of a line in the ini.
fn defined(locale: &str) -> Vec<&str> {
    locale
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim()))
        .filter(|key| !key.is_empty())
        .collect()
}

#[test]
fn every_ui_string_is_in_the_locale_file() {
    let defined = defined(LOCALE);
    let mut missing = Vec::new();
    for (file, src) in SOURCES {
        for key in keys(src) {
            if !defined.contains(&key) {
                missing.push(format!("{file}: {key}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "keys with no en-US.ini entry (they would render as the key itself):\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn the_locale_file_carries_no_dead_strings() {
    let used: Vec<&str> = SOURCES.iter().flat_map(|(_, src)| keys(src)).collect();
    let dead: Vec<&str> = defined(LOCALE)
        .into_iter()
        .filter(|key| !used.contains(key))
        .collect();
    assert!(dead.is_empty(), "unused locale keys: {dead:?}");
}

#[test]
fn the_scan_finds_the_keys_it_should() {
    // A guard on the test itself: a refactor that changes how `module_text`
    // is called would otherwise turn both tests above into no-ops.
    let found = keys(SOURCES[0].1);
    assert!(found.len() > 10, "only found {found:?}");
    assert!(found.contains(&"CatchUpSpeed"));
    assert!(found.contains(&"TargetBuffer"));
    assert!(keys(SOURCES[1].1).contains(&"SourceName"));
}
