//! Splitting of safe relative paths, shared by `sync` (local tree
//! paths) and `hub::model` (tablet and repository paths). Pure.

/// Split a `/`-separated relative path into its components, or `None`
/// when it is unsafe: empty, absolute (leading `/`), with a trailing or
/// doubled `/`, a `.` or `..` component, or a NUL byte anywhere. Callers
/// map `None` to their own error and may add stricter checks.
pub(crate) fn split_safe_relative(path: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = path.split('/').collect();
    parts
        .iter()
        .all(|part| !part.is_empty() && *part != "." && *part != ".." && !part.contains('\0'))
        .then_some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_safe_paths() {
        assert_eq!(split_safe_relative("a"), Some(vec!["a"]));
        assert_eq!(
            split_safe_relative("a/b c/.d"),
            Some(vec!["a", "b c", ".d"])
        );
        assert_eq!(split_safe_relative("..a/b..").map(|p| p.len()), Some(2));
        assert_eq!(split_safe_relative("a\\..\\b"), Some(vec!["a\\..\\b"]));
    }

    #[test]
    fn rejects_unsafe_paths() {
        [
            "", "/", "/a", "a/", "a//b", ".", "..", "./a", "a/.", "a/../b", "a\0b", "a/\0",
        ]
        .iter()
        .for_each(|path| assert_eq!(split_safe_relative(path), None, "{path:?}"));
    }
}
