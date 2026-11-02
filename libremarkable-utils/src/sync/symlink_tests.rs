//! Destination collision regressions: no device or SSH server is used.
use super::*;
use crate::progress::NoProgress;
use crate::ssh::SshOptions;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rmu-symlink-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("root")).unwrap();
        fs::create_dir_all(path.join("outside")).unwrap();
        fs::write(path.join("outside/doc.pdf"), b"outside original").unwrap();
        fs::write(path.join("root/source.pdf"), b"source original").unwrap();
        Self(path)
    }

    fn root(&self) -> PathBuf {
        self.0.join("root")
    }

    fn collide(&self, kind: &str) -> &str {
        match kind {
            "leaf" => {
                symlink(self.0.join("outside/doc.pdf"), self.root().join("doc.pdf")).unwrap();
                "doc.pdf"
            }
            "dangling" => {
                symlink(
                    self.0.join("outside/missing.pdf"),
                    self.root().join("doc.pdf"),
                )
                .unwrap();
                "doc.pdf"
            }
            "parent" => {
                symlink(self.0.join("outside"), self.root().join("linked")).unwrap();
                "linked/doc.pdf"
            }
            "deep-parent" => {
                symlink(self.0.join("outside"), self.root().join("linked")).unwrap();
                "linked/new/doc.pdf"
            }
            "dangling-parent" => {
                symlink(self.0.join("outside/missing"), self.root().join("linked")).unwrap();
                "linked/doc.pdf"
            }
            _ => unreachable!(),
        }
    }

    fn assert_untouched(&self) {
        assert_eq!(
            fs::read(self.0.join("outside/doc.pdf")).unwrap(),
            b"outside original"
        );
        assert_eq!(
            fs::read(self.root().join("source.pdf")).unwrap(),
            b"source original"
        );
        assert_eq!(fs::read_dir(self.0.join("outside")).unwrap().count(), 1);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

const COLLISIONS: [&str; 5] = [
    "leaf",
    "dangling",
    "parent",
    "deep-parent",
    "dangling-parent",
];

fn no_network_session() -> SshSession {
    SshSession::new(SshOptions {
        destination: "synthetic.invalid".into(),
        extra_options: vec!["ProxyCommand=false".into()],
        multiplex: false,
        ..SshOptions::default()
    })
    .unwrap()
}

fn ssh_fs(root: &Path) -> SshFs {
    SshFs::new(
        no_network_session(),
        root.to_str().unwrap(),
        Arc::new(NoProgress),
    )
}

/// Execute the exact script sent by SshFs, but locally, without mutating PATH
/// or other process-wide environment shared with concurrently running tests.
fn run_script(script: &str, data: &[u8]) -> std::process::Output {
    let mut child = Command::new("sh")
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A rejected destination may exit before consuming stdin.
    let _ = child.stdin.take().unwrap().write_all(data);
    child.wait_with_output().unwrap()
}

#[test]
fn local_writes_and_renames_reject_symlink_collisions() {
    COLLISIONS.iter().for_each(|kind| {
        let f = Fixture::new();
        let target = f.collide(kind);
        let endpoint = LocalFs::new(f.root());
        assert!(endpoint.write(target, b"replacement").is_err(), "{kind}");
        assert!(endpoint.rename("source.pdf", target).is_err(), "{kind}");
        f.assert_untouched();
    });
}

#[test]
fn ssh_writes_and_renames_reject_symlink_collisions() {
    COLLISIONS.iter().for_each(|kind| {
        let f = Fixture::new();
        let target = f.collide(kind);
        let endpoint = ssh_fs(&f.root());
        [
            endpoint.write_command(target).unwrap(),
            endpoint.rename_command("source.pdf", target).unwrap(),
        ]
        .iter()
        .for_each(|script| {
            let output = run_script(script, b"replacement");
            assert!(!output.status.success(), "{kind}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("unsafe sync path"));
            f.assert_untouched();
        });
    });
}

#[test]
fn rename_cannot_remove_a_source_through_a_symlinked_parent() {
    let f = Fixture::new();
    let source = f.collide("parent");
    assert!(LocalFs::new(f.root()).rename(source, "moved.pdf").is_err());
    let output = run_script(
        &ssh_fs(&f.root())
            .rename_command(source, "moved.pdf")
            .unwrap(),
        b"",
    );
    assert!(!output.status.success());
    assert!(!f.root().join("moved.pdf").exists());
    f.assert_untouched();
}

#[test]
fn symlinked_roots_and_ordinary_nested_writes_remain_supported() {
    [false, true].into_iter().for_each(|remote| {
        let f = Fixture::new();
        let alias = f.0.join("root alias's");
        symlink(f.root(), &alias).unwrap();
        let target = "nested dir's/-doc.pdf";
        if remote {
            let endpoint = ssh_fs(&alias);
            assert!(
                run_script(&endpoint.write_command(target).unwrap(), b"first")
                    .status
                    .success()
            );
            assert!(
                run_script(&endpoint.write_command(target).unwrap(), b"second")
                    .status
                    .success()
            );
            assert!(
                run_script(
                    &endpoint.rename_command(target, "new/renamed.pdf").unwrap(),
                    b""
                )
                .status
                .success()
            );
        } else {
            let endpoint = LocalFs::new(&alias);
            endpoint.write(target, b"first").unwrap();
            endpoint.write(target, b"second").unwrap();
            endpoint.rename(target, "new/renamed.pdf").unwrap();
        }
        assert_eq!(
            fs::read(f.root().join("new/renamed.pdf")).unwrap(),
            b"second"
        );
        assert!(!f.root().join(target).exists());
        f.assert_untouched();
    });
}

#[test]
fn invalid_relative_paths_and_directory_leaves_are_rejected() {
    let f = Fixture::new();
    let local = LocalFs::new(f.root());
    let remote = ssh_fs(&f.root());
    [
        "",
        "/doc.pdf",
        "../outside/doc.pdf",
        "nested/../../outside/doc.pdf",
        "./doc.pdf",
    ]
    .into_iter()
    .for_each(|path| {
        assert!(local.write(path, b"replacement").is_err());
        assert!(remote.write_command(path).is_err());
    });
    fs::create_dir(f.root().join("dir.pdf")).unwrap();
    assert!(local.rename("source.pdf", "dir.pdf").is_err());
    assert!(
        !run_script(
            &remote.rename_command("source.pdf", "dir.pdf").unwrap(),
            b""
        )
        .status
        .success()
    );
    assert_eq!(fs::read_dir(f.root().join("dir.pdf")).unwrap().count(), 0);
    f.assert_untouched();
}

#[test]
fn filesystem_sync_rechecks_collisions_after_snapshot_and_on_retry() {
    COLLISIONS.iter().for_each(|kind| {
        let f = Fixture::new();
        let a = LocalFs::new(f.0.join("source"));
        a.ensure_root().unwrap();
        let b = LocalFs::new(f.root());
        // Snapshot before injecting the collision: execution must not trust it.
        let before = b.snapshot().unwrap().0;
        let path = f.collide(kind);
        a.write(path, b"new data").unwrap();
        let mut state = SyncState::default();
        let state_path = f.0.join("archive.json");
        let options = SyncOptions {
            mode: Mode::Push,
            delete: false,
            conflict: ConflictPolicy::Skip,
        };
        let plan = plan_files(options, &a.snapshot().unwrap().0, &before, &state);
        assert!(plan.contains(&FileAction::CopyToB { path: path.into() }));
        (0..2).for_each(|_| {
            assert!(execute_files(&a, &b, &NoProgress, &plan, &mut state, &state_path).is_err());
            assert!(state.entries.is_empty());
            assert!(!state_path.exists());
            f.assert_untouched();
        });
        // Removing the collision permits a retry; no poisoned archive entry.
        fs::remove_file(f.root().join(if kind.contains("parent") {
            "linked"
        } else {
            "doc.pdf"
        }))
        .unwrap();
        execute_files(&a, &b, &NoProgress, &plan, &mut state, &state_path).unwrap();
        assert_eq!(b.read(path).unwrap(), b"new data");
        assert!(state.entries.contains_key(path));
        f.assert_untouched();
    });
}

#[test]
fn tablet_sync_fast_paths_reject_collisions_before_copy_move_or_download() {
    COLLISIONS.iter().for_each(|kind| {
        let f = Fixture::new();
        let target = f.collide(kind);
        let endpoint = LocalFs::new(f.root());
        let client = Client::new(no_network_session(), "/synthetic");
        let mut actions = vec![
            SyncAction::CopyLocal {
                from: "source.pdf".into(),
                to: target.into(),
                kind: DocKind::Pdf,
                uuid: "synthetic".into(),
                last_modified: 1,
                hash: hash_bytes(b"source original"),
            },
            SyncAction::MoveLocal {
                from: "source.pdf".into(),
                to: target.into(),
                kind: DocKind::Pdf,
                uuid: "synthetic".into(),
                last_modified: 1,
                local_hash: None,
                payload_hash: None,
            },
        ];
        actions.extend(
            [RemoteType::Pdf, RemoteType::Epub, RemoteType::Notebook]
                .into_iter()
                .map(|doc_type| SyncAction::Download {
                    local: target.into(),
                    uuid: "synthetic".into(),
                    doc_type,
                    size_bytes: Some(1),
                    last_modified: 1,
                }),
        );
        actions.into_iter().for_each(|action| {
            let mut state = SyncState::default();
            // MoveLocal operates on an existing mapping. Seed a real agreed
            // baseline so the executor reaches the symlink guard, rather than
            // rejecting an invalid unmapped move before attempting the rename.
            if let SyncAction::MoveLocal { from, uuid, .. } = &action {
                let (size, mtime) = endpoint.stat(from).unwrap();
                let hash = endpoint.content_hash(from).unwrap();
                state.entries.insert(
                    from.clone(),
                    StateEntry {
                        uuid: uuid.clone(),
                        kind: DocKind::Pdf,
                        local_size: size,
                        local_mtime_ms: mtime,
                        remote_last_modified: 0,
                        remote_pending: false,
                        remote_size: None,
                        local_hash: hash.clone(),
                        payload_hash: hash,
                    },
                );
            }
            let state_path = f.0.join("archive.json");
            state.save(&state_path).unwrap();
            let baseline = state.serialize().unwrap();
            let archive_before = fs::read(&state_path).unwrap();
            let error = execute(
                &client,
                &NoProgress,
                &endpoint,
                &Plan {
                    actions: vec![action],
                },
                &mut BTreeMap::new(),
                &mut state,
                &state_path,
            )
            .unwrap_err();
            // Must fail locally, not by attempting the intentionally disabled SSH.
            assert!(error.to_string().contains("unsafe sync path"), "{error}");
            assert_eq!(state.serialize().unwrap(), baseline);
            assert_eq!(fs::read(&state_path).unwrap(), archive_before);
            f.assert_untouched();
        });
    });
}
