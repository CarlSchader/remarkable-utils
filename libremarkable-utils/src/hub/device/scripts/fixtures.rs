//! Helpers shared by the script tests.

use crate::hub::device::TabletPaths;
use crate::hub::device::lock::LockRequest;
use crate::ssh::shell_quote;

pub(super) const RUN: &str = "0123456789abcdef0123456789abcdef";
pub(super) const DOC: &str = "aaaaaaaa-0000-4000-8000-000000000001";
pub(super) const VERSION: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Directories with every character a shell might trip over.
pub(super) fn hostile() -> TabletPaths {
    TabletPaths {
        xochitl_dir: "/data/it's $(rm -rf /) `x` \"q\" dir".into(),
        state_dir: "/state/a b;c&d|e\nf".into(),
        xochitl_conf: "/conf/x'y.conf".into(),
        boot_id: "/proc/boot id".into(),
    }
}

pub(super) fn request() -> LockRequest {
    LockRequest::new("laptop", 1, 2).with_run(RUN)
}

/// Every path a script may mention, longest first.
pub(super) fn known_paths(paths: &TabletPaths, extra: &[&str]) -> Vec<String> {
    let mut known: Vec<String> = [
        paths.xochitl_dir.clone(),
        paths.state_dir.clone(),
        paths.xochitl_conf.clone(),
        paths.boot_id.clone(),
        paths.state_file(),
        paths.lock_dir(),
        paths.lock_owner(),
        paths.guard_flock(),
        paths.watchdog_marker(),
        paths.watchdog_unit(),
        paths.watchdog_token(),
        paths.watchdog_pid(),
        paths.staging_root(),
        format!("{}/.rmu-state-XXXXXX", paths.state_dir),
    ]
    .into_iter()
    .chain(extra.iter().map(|path| path.to_string()))
    .collect();
    known.sort_by_key(|path| std::cmp::Reverse(path.len()));
    known
}

/// No hostile fragment survives once every quoted path is removed:
/// paths only ever appear in their quoted form.
pub(super) fn assert_quoted_with(script: &str, paths: &TabletPaths, extra: &[&str]) {
    let stripped = known_paths(paths, extra)
        .iter()
        .fold(script.to_string(), |text, path| {
            text.replace(&shell_quote(path), "<path>")
        });
    ["$(rm -rf /)", "a b;c&d|e\nf", "x'y", "boot id", "`x`"]
        .into_iter()
        .for_each(|fragment| {
            assert!(
                !stripped.contains(fragment),
                "unquoted {fragment:?} in:\n{script}"
            );
        });
}

pub(super) fn assert_quoted(script: &str, paths: &TabletPaths) {
    assert_quoted_with(script, paths, &[]);
}

pub(super) fn position(script: &str, needle: &str) -> usize {
    script
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not in:\n{script}"))
}
