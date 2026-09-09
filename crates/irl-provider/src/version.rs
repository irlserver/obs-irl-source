//! The `min_plugin_version` comparison.
//!
//! Only `major.minor.patch` is compared. A pre-release or build suffix is
//! ignored, so `2.1.0-rc.1` counts as `2.1.0`: a provider that wants to shut
//! out release candidates raises the floor to the next patch instead.

/// `major.minor.patch` from the front of `s`; `None` if that is not what it
/// starts with.
#[must_use]
pub fn parse(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next()??;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Whether plugin version `have` satisfies a provider's `need`.
///
/// A `need` the plugin cannot parse is ignored (`true`): a provider that
/// mistypes its floor should not lock every user out. A `have` that does not
/// parse is treated as too old (`false`), which can only happen to a
/// development build.
#[must_use]
pub fn at_least(have: &str, need: &str) -> bool {
    match (parse(have), parse(need)) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(h), Some(n)) => h >= n,
    }
}
