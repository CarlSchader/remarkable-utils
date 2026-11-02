//! Exercise the real planner/executor with SSH replaced by a local shell.
//! All document bytes, metadata, archives, and commands stay in a scratch dir.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use libremarkable_utils::sync::{SyncState, hash_bytes};
use serde_json::json;

const ORIGINAL: &[u8] = b"synthetic original PDF payload";
const CHANGED: &[u8] = b"synthetic changed PDF payload, with a different size";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("rmu-copy-sources-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        ["bin", "local", "device", "state"].iter().for_each(|dir| {
            fs::create_dir(root.join(dir)).unwrap();
        });
        let ssh = root.join("bin/ssh");
        fs::write(
            &ssh,
            "#!/usr/bin/env sh\n\
             while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
             [ \"$#\" -eq 3 ] || exit 97\n\
             shift\n\
             [ \"$1\" = mock-tablet ] || exit 98\n\
             shift\n\
             exec sh -c \"$1\"\n",
        )
        .unwrap();
        fs::set_permissions(ssh, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn device_doc(&self, uuid: &str, name: &str, bytes: &[u8], modified: &str) {
        let device = self.root.join("device");
        fs::write(device.join(format!("{uuid}.pdf")), bytes).unwrap();
        fs::write(
            device.join(format!("{uuid}.content")),
            json!({"fileType": "pdf"}).to_string(),
        )
        .unwrap();
        fs::write(
            device.join(format!("{uuid}.metadata")),
            json!({
                "type": "DocumentType", "visibleName": name, "parent": "",
                "lastModified": modified
            })
            .to_string(),
        )
        .unwrap();
    }

    fn sync(&self, push: bool, dry_run: bool) -> String {
        let paths = std::iter::once(self.root.join("bin"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))
            .collect::<Vec<_>>();
        let mut command = Command::new(env!("CARGO_BIN_EXE_rmu"));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOME", &self.root)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env_remove("RMU_SSH_PASSWORD")
            .args(["--no-multiplex", "--no-restart", "--quiet", "--xochitl-dir"])
            .arg(self.root.join("device"))
            .args(["sync", "--remote-kind", "remarkable"]);
        let local = self.root.join("local");
        if push {
            command.arg(local).arg("mock-tablet:/");
        } else {
            command.arg("mock-tablet:/").arg(local);
        }
        if dry_run {
            command.arg("--dry-run");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "rmu failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn assert_agreed_bytes(&self) {
        let archive = fs::read_dir(self.root.join("state/rmu"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("sync-")
            })
            .unwrap();
        let state = SyncState::load(&archive).unwrap();
        assert_eq!(state.entries.len(), 2);
        [("a.pdf", CHANGED), ("z.pdf", ORIGINAL)]
            .iter()
            .for_each(|(path, expected)| {
                let entry = &state.entries[*path];
                let local = fs::read(self.root.join("local").join(path)).unwrap();
                let device =
                    fs::read(self.root.join("device").join(format!("{}.pdf", entry.uuid))).unwrap();
                assert_eq!(local, *expected, "local {path}");
                assert_eq!(device, *expected, "device {path}");
                assert_eq!(
                    entry.local_hash.as_deref(),
                    Some(hash_bytes(&local).as_str())
                );
                assert_eq!(
                    entry.payload_hash.as_deref(),
                    Some(hash_bytes(&device).as_str())
                );
            });
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn push_update_does_not_corrupt_later_fingerprint_copy() {
    let fixture = Fixture::new();
    fixture.device_doc("a-source", "a", ORIGINAL, "10");
    fs::write(fixture.root.join("local/a.pdf"), ORIGINAL).unwrap();
    fixture.sync(true, false); // Establish the agreed H1 archive by adoption.

    fs::write(fixture.root.join("local/a.pdf"), CHANGED).unwrap();
    fs::write(fixture.root.join("local/z.pdf"), ORIGINAL).unwrap();
    fixture.sync(true, false);
    fixture.assert_agreed_bytes();
    assert!(fixture.sync(true, true).is_empty());
    fixture.sync(true, false);
    fixture.assert_agreed_bytes();
}

#[test]
fn pull_download_does_not_corrupt_later_fingerprint_copy() {
    let fixture = Fixture::new();
    fixture.device_doc("a-source", "a", ORIGINAL, "10");
    fs::write(fixture.root.join("local/a.pdf"), ORIGINAL).unwrap();
    fixture.sync(false, false);

    // Change size as well as lastModified, avoiding same-second stat-cache
    // ambiguity (which is independent of copy-source dependencies).
    fixture.device_doc("a-source", "a", CHANGED, "20000");
    fixture.device_doc("z-source", "z", ORIGINAL, "30000");
    fixture.sync(false, false);
    fixture.assert_agreed_bytes();
    assert!(fixture.sync(false, true).is_empty());
    fixture.sync(false, false);
    fixture.assert_agreed_bytes();
}
