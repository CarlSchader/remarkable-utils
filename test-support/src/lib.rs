//! Test-only helpers shared by the workspace's unit and integration tests.
#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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
