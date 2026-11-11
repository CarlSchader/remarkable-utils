//! XDG base directories for rmu's files on this computer.

use std::ffi::OsString;
use std::path::PathBuf;

/// `explicit` (an `$XDG_*_HOME` value) if given, else `home` joined with
/// `under_home` (the spec's default, e.g. `.cache`); `None` without
/// either. An empty value counts as not given, as the XDG Base Directory
/// specification says: it never means the current directory.
pub(crate) fn base_dir(
    explicit: Option<OsString>,
    home: Option<OsString>,
    under_home: &str,
) -> Option<PathBuf> {
    let given = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    given(explicit)
        .map(PathBuf::from)
        .or_else(|| given(home).map(|home| PathBuf::from(home).join(under_home)))
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
    }

    #[test]
    fn empty_values_count_as_unset() {
        assert_eq!(
            base_dir(os(""), os("/home/u"), ".local/state"),
            Some(PathBuf::from("/home/u/.local/state"))
        );
        assert_eq!(
            base_dir(os("/x/state"), os(""), ".local/state"),
            Some(PathBuf::from("/x/state"))
        );
        assert_eq!(base_dir(os(""), os(""), ".local/state"), None);
        assert_eq!(base_dir(None, os(""), ".cache"), None);
        assert_eq!(base_dir(os(""), None, ".cache"), None);
    }
}
