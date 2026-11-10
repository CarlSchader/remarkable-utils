//! Test-only helpers shared by the workspace's unit and integration tests:
//! scratch directories, mock executables, and running test subprocesses.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::SystemTime;

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
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::join_paths(std::iter::once(self.join("bin")).chain(std::env::split_paths(&path)))
            .unwrap()
    }
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
