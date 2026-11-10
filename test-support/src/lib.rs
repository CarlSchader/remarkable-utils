//! Test-only helpers shared by the workspace's unit and integration tests:
//! scratch directories, mock executables, and running test subprocesses.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::SystemTime;

pub mod tablet;

/// A uniquely named directory under the system temporary directory,
/// removed together with its contents on drop.
pub struct Scratch(PathBuf);

impl Scratch {
    /// Create `rmu-<prefix>-<uuid>` in the system temporary directory.
    pub fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("rmu-{prefix}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.0.join(path)
    }

    /// Install `body` as the mock executable `bin/<name>` (see
    /// [`write_script`]), creating `bin/` if needed.
    pub fn mock_command(&self, name: &str, body: &str) {
        let bin = self.join("bin");
        fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join(name), body);
    }

    /// The current `PATH` with this directory's `bin/` first, so the
    /// [mock commands](Self::mock_command) shadow the real ones. Pass it to
    /// child processes only; tests never change their own environment.
    pub fn mock_path(&self) -> OsString {
        path_with_first(self.join("bin"), std::env::var_os("PATH"))
    }
}

/// `PATH` for a child process with `first` ahead of `path` (this
/// process's `PATH` in [`Scratch::mock_path`]). An unset or empty
/// `path` adds nothing: an empty entry would mean the current directory.
fn path_with_first(first: PathBuf, path: Option<OsString>) -> OsString {
    let rest = path.filter(|path| !path.is_empty());
    std::env::join_paths(std::iter::once(first).chain(rest.iter().flat_map(std::env::split_paths)))
        .unwrap()
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A failed cleanup is a test bug, but panicking while a failing test
        // unwinds would abort the process and hide the original failure.
        if let Err(error) = fs::remove_dir_all(&self.0)
            && !std::thread::panicking()
        {
            panic!("failed to remove {}: {error}", self.0.display());
        }
    }
}

/// A command that re-runs only the test `name` of the current test binary.
/// Tests use it to give one scenario a private environment (such as a
/// [mock `PATH`](Scratch::mock_path)) without mutating the environment the
/// rest of the test process shares.
pub fn rerun_test(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", name, "--nocapture"]);
    command
}

/// Panic with a process's captured output unless it exited successfully.
#[track_caller]
pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Set the modification time of the existing file at `path`.
pub fn set_modified(path: &Path, time: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(time))
        .unwrap();
}

/// Install an executable `/bin/sh` script at `path`, replacing any existing
/// file, such that tests running concurrently in this process can exec it.
///
/// Exec fails with ETXTBSY ("Text file busy") while any process holds the
/// file open for writing. A test thread writing the script itself races with
/// every other thread's `Command::spawn`: the forked child inherits the open
/// descriptor until it execs (close-on-exec only applies at exec), so a mock
/// exec'd in that window fails. Here a short-lived `sh` child creates the
/// file, so a writable descriptor for it never exists in this (forking)
/// process. Writing to a temporary sibling and renaming it publishes the
/// script atomically, so replacing one never truncates a running script.
pub fn write_script(path: &Path, body: &str) {
    let mut writer = Command::new("sh")
        .args([
            "-c",
            r#"tmp=$(mktemp "$1.XXXXXX") && cat > "$tmp" && chmod 700 "$tmp" && mv -f "$tmp" "$1""#,
            "write-script",
        ])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn the mock script writer");
    let mut stdin = writer.stdin.take().expect("stdin was piped");
    stdin
        .write_all(format!("#!/bin/sh\n{body}").as_bytes())
        .expect("failed to send the mock script body");
    drop(stdin);
    let status = writer
        .wait()
        .expect("failed to wait for the mock script writer");
    assert!(
        status.success(),
        "writing mock script {} failed: {status}",
        path.display()
    );
}

/// A fabricated, valid PDF of `pages` blank pages (a classic
/// cross-reference table, no streams): what `rmu hub add` tests feed the
/// page counter.
pub fn pdf(pages: usize) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
    let objects: Vec<String> = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} >>",
            kids.join(" ")
        ),
    ]
    .into_iter()
    .chain(
        (0..pages).map(|_| "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string()),
    )
    .collect();
    let offsets: Vec<usize> = objects
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let at = out.len();
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
            at
        })
        .collect();
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f\r\n", objects.len() + 1).as_bytes(),
    );
    offsets
        .iter()
        .for_each(|at| out.extend_from_slice(format!("{at:010} 00000 n\r\n").as_bytes()));
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    fn os(text: &str) -> Option<OsString> {
        Some(OsString::from(text))
    }

    #[test]
    fn mock_path_puts_bin_first_and_keeps_the_rest_in_order() {
        let bin = PathBuf::from("/mock/bin");
        assert_eq!(
            path_with_first(bin.clone(), os("/usr/bin:/bin")),
            "/mock/bin:/usr/bin:/bin"
        );
        // Empty entries inside a PATH are the caller's, kept as they were.
        assert_eq!(
            path_with_first(bin.clone(), os("/a::/b")),
            "/mock/bin:/a::/b"
        );
        let scratch = Scratch::new("mock-path");
        let path = scratch.mock_path();
        assert_eq!(
            std::env::split_paths(&path).next(),
            Some(scratch.join("bin"))
        );
    }

    #[test]
    fn mock_path_adds_no_empty_entry_for_an_empty_path() {
        // An empty entry would put the current directory on the PATH.
        let bin = PathBuf::from("/mock/bin");
        assert_eq!(path_with_first(bin.clone(), os("")), "/mock/bin");
        assert_eq!(path_with_first(bin, None), "/mock/bin");
    }

    fn running_as_root() -> bool {
        let scratch = Scratch::new("uid");
        fs::write(scratch.join("probe"), b"").unwrap();
        fs::metadata(scratch.join("probe")).unwrap().uid() == 0
    }

    /// A scratch dir its owner cannot empty: a file in a read-only
    /// subdirectory. Returns the dir's path and the subdirectory.
    fn undeletable() -> (Scratch, PathBuf) {
        let scratch = Scratch::new("undeletable");
        let locked = scratch.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("file"), b"").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        (scratch, locked)
    }

    fn clean_up(path: &Path, locked: &Path) {
        fs::set_permissions(locked, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn scratch_dirs_are_removed_on_drop() {
        let scratch = Scratch::new("drop");
        fs::create_dir_all(scratch.join("a/b")).unwrap();
        fs::write(scratch.join("a/b/c"), b"x").unwrap();
        let path = scratch.path().to_path_buf();
        assert!(path.starts_with(std::env::temp_dir()));
        drop(scratch);
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_cleanup_panics_but_never_during_another_panic() {
        if running_as_root() {
            eprintln!("running as root: permissions do not stop removal; skipping");
            return;
        }
        let (scratch, locked) = undeletable();
        let path = scratch.path().to_path_buf();
        let error = catch_unwind(AssertUnwindSafe(|| drop(scratch))).unwrap_err();
        let message = error.downcast_ref::<String>().unwrap();
        assert!(message.starts_with("failed to remove"), "{message}");
        clean_up(&path, &locked);
        // Unwinding from another panic: that panic is what comes out.
        let (scratch, locked) = undeletable();
        let path = scratch.path().to_path_buf();
        let error = catch_unwind(AssertUnwindSafe(|| {
            let _scratch = scratch;
            panic!("the original failure");
        }))
        .unwrap_err();
        assert_eq!(error.downcast_ref::<&str>(), Some(&"the original failure"));
        clean_up(&path, &locked);
    }

    #[test]
    fn scripts_are_executable_and_replaced_whole() {
        let scratch = Scratch::new("script");
        scratch.mock_command("hello", "echo one\n");
        let run = || Command::new(scratch.join("bin/hello")).output().unwrap();
        assert_eq!(run().stdout, b"one\n");
        scratch.mock_command("hello", "echo two\n");
        assert_eq!(run().stdout, b"two\n");
        let mode = fs::metadata(scratch.join("bin/hello"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        // No temporary siblings are left behind.
        assert_eq!(fs::read_dir(scratch.join("bin")).unwrap().count(), 1);
    }

    #[test]
    fn modification_times_are_set() {
        let scratch = Scratch::new("mtime");
        let file = scratch.join("f");
        fs::write(&file, b"").unwrap();
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        set_modified(&file, time);
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), time);
    }
}
