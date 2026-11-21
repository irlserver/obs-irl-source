//! Every `module_text` key has a string in `data/locale/en-US.ini`.
//!
//! The lookup falls back to returning the key, so a missing entry does not
//! fail: it renders the dialog as bare identifiers like `AudioBufferHelp`,
//! which is only noticed by opening the properties window. This test is the
//! mechanical half of the "a new string belongs in two places" convention.
//!
//! The translations next to it are checked against en-US: they may lag behind
//! (a missing key falls back to the English string at runtime), but a key
//! en-US no longer has, or a dropped `%1`, is a bug.
//!
//! The strings are `include_str!`d and nothing calls into libobs. The only
//! filesystem read is the listing of `data/locale/`, so a new translation
//! cannot ship without being added to `TRANSLATIONS`.

/// `(file name, contents)` of `data/locale/<code>.ini`.
macro_rules! locale {
    ($code:literal) => {
        (
            concat!($code, ".ini"),
            include_str!(concat!("../../../data/locale/", $code, ".ini")),
        )
    };
}

/// The `default_locale` of `declare_module!`: every key lives here, and every
/// other locale falls back to it.
const DEFAULT: (&str, &str) = locale!("en-US");

/// Every translation shipped next to the default. A new language is one line.
const TRANSLATIONS: &[(&str, &str)] = &[locale!("de-DE"), locale!("tr-TR")];

const SOURCES: [(&str, &str); 3] = [
    ("settings.rs", include_str!("../src/settings.rs")),
    ("source.rs", include_str!("../src/source.rs")),
    ("providers.rs", include_str!("../src/providers.rs")),
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

/// Every `Key=value` pair in the ini, comments skipped.
fn entries(locale: &str) -> Vec<(&str, &str)> {
    locale
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

/// Every `Key=` at the start of a line in the ini.
fn defined(locale: &str) -> Vec<&str> {
    entries(locale).into_iter().map(|(key, _)| key).collect()
}

#[test]
fn every_ui_string_is_in_the_locale_file() {
    let (default_file, default) = DEFAULT;
    let defined = defined(default);
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
        "keys with no {default_file} entry (they would render as the key itself):\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn the_locale_file_carries_no_dead_strings() {
    let used: Vec<&str> = SOURCES.iter().flat_map(|(_, src)| keys(src)).collect();
    let dead: Vec<&str> = defined(DEFAULT.1)
        .into_iter()
        .filter(|key| !used.contains(key))
        .collect();
    assert!(dead.is_empty(), "unused locale keys: {dead:?}");
}

#[test]
fn translations_carry_only_default_keys() {
    let (default_file, default) = DEFAULT;
    let reference = defined(default);
    for &(file, src) in TRANSLATIONS {
        let stale: Vec<&str> = defined(src)
            .into_iter()
            .filter(|key| !reference.contains(key))
            .collect();
        assert!(
            stale.is_empty(),
            "{file}: keys {default_file} does not have: {stale:?}"
        );
    }
}

#[test]
fn every_shipped_translation_is_checked() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/locale");
    let mut unchecked: Vec<String> = std::fs::read_dir(dir)
        .expect("data/locale is readable")
        .map(|entry| entry.expect("data/locale entry").file_name())
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name.ends_with(".ini") && name != DEFAULT.0)
        .filter(|name| !TRANSLATIONS.iter().any(|(file, _)| file == name))
        .collect();
    unchecked.sort();
    assert!(
        unchecked.is_empty(),
        "translations missing from TRANSLATIONS: {unchecked:?}"
    );
}

#[test]
fn translations_keep_the_placeholders() {
    let (default_file, default) = DEFAULT;
    let reference = entries(default);
    for &(file, src) in TRANSLATIONS {
        for (key, value) in entries(src) {
            let Some((_, english)) = reference.iter().find(|(k, _)| *k == key) else {
                continue;
            };
            assert_eq!(
                value.matches("%1").count(),
                english.matches("%1").count(),
                "{file}: {key} changes the number of %1 placeholders"
            );
            assert!(
                value.starts_with('"') && value.ends_with('"'),
                "{file}: {key} is not quoted like {default_file}"
            );
        }
    }
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
    assert!(keys(SOURCES[2].1).contains(&"Provider"));
}
