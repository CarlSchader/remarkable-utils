//! XDG base directories for rmu's files on this computer.

use std::ffi::OsString;
use std::path::PathBuf;

/// `explicit` (an `$XDG_*_HOME` value) if given, else `home` joined with
/// `under_home` (the spec's default, e.g. `.cache`); `None` without
/// either. Callers decide which values count as given (an empty one, by
/// the spec, should not).
pub(crate) fn base_dir(
    explicit: Option<OsString>,
    home: Option<OsString>,
    under_home: &str,
) -> Option<PathBuf> {
    explicit
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(under_home)))
}

/// An environment variable, unless it is unset or empty.
pub(crate) fn non_empty_var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(text: &str) -> Option<OsString> {
        Some(OsString::from(text))
    }

    #[test]
    fn explicit_wins_then_home() {
        assert_eq!(
            base_dir(os("/x/cache"), os("/home/u"), ".cache"),
            Some(PathBuf::from("/x/cache"))
        );
        assert_eq!(
            base_dir(None, os("/home/u"), ".local/state"),
            Some(PathBuf::from("/home/u/.local/state"))
        );
        assert_eq!(base_dir(None, None, ".cache"), None);
        // Empty values are the caller's to filter out.
        assert_eq!(
            base_dir(os(""), os("/home/u"), ".cache"),
            Some(PathBuf::new())
        );
    }
}
