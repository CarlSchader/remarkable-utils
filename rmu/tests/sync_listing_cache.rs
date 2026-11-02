//! Destructive sync regressions through the real CLI, with two local synthetic
//! tablets. The SSH stand-in rejects every destination except the fixtures.
#![cfg(unix)]

use std::fs::{self, File, FileTimes};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

const ROOT: &str = "00000000-0000-4000-8000-000000000001";
const OTHER: &str = "00000000-0000-4000-8000-000000000002";
const DOC: &str = "00000000-0000-4000-8000-000000000003";
const ORIGINAL: &[u8] = b"synthetic original PDF";
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "rmu-sync-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fixture = Self(root);
        for dir in ["bin", "a/device", "b/device", "local", "state"] {
            fs::create_dir_all(fixture.0.join(dir)).unwrap();
        }
        let ssh = fixture.0.join("bin/ssh");
        fs::write(
            &ssh,
            r#"#!/bin/sh
[ "$#" = 3 ] && [ "$1" = -- ] || exit 90
case "$2" in
    root@a.invalid) cd "$MOCK_ROOT/a" || exit 91;;
    root@b.invalid) cd "$MOCK_ROOT/b" || exit 91;;
    *) exit 92;;
esac
case "$3" in
    *'__sizes__'*)
        # A failing full read must not be replaced by the warm cache.
        [ "$MOCK_FAIL_LISTING" != "$2" ] || exit 93
        ;;
esac
exec sh -c "$3"
"#,
        )
        .unwrap();
        fs::set_permissions(ssh, fs::Permissions::from_mode(0o755)).unwrap();
        fixture
    }

    fn file(&self, side: &str, uuid: &str, extension: &str) -> PathBuf {
        self.0.join(format!("{side}/device/{uuid}.{extension}"))
    }

    fn metadata(&self, side: &str, uuid: &str) -> Value {
        serde_json::from_slice(&fs::read(self.file(side, uuid, "metadata")).unwrap()).unwrap()
    }

    fn put_metadata(&self, side: &str, uuid: &str, metadata: &Value) {
        let path = self.file(side, uuid, "metadata");
        fs::write(&path, serde_json::to_vec_pretty(metadata).unwrap()).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            )
            .unwrap();
    }

    fn folder(&self, side: &str, uuid: &str, name: &str) {
        self.put_metadata(
            side,
            uuid,
            &json!({
                "type": "CollectionType", "visibleName": name, "parent": "",
                "lastModified": "1000000000000"
            }),
        );
    }

    fn document(&self, side: &str) {
        self.put_metadata(
            side,
            DOC,
            &json!({
                "type": "DocumentType", "visibleName": "keep", "parent": ROOT,
                "lastModified": "1000000000000"
            }),
        );
        fs::write(self.file(side, DOC, "content"), br#"{"fileType":"pdf"}"#).unwrap();
        fs::write(self.file(side, DOC, "pdf"), ORIGINAL).unwrap();
    }

    fn command(&self) -> Command {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rmu"));
        cmd.env(
            "PATH",
            std::env::join_paths(
                std::iter::once(self.0.join("bin")).chain(std::env::split_paths(&path)),
            )
            .unwrap(),
        )
        .env("XDG_STATE_HOME", self.0.join("state"))
        .env("MOCK_ROOT", &self.0)
        .env("MOCK_FAIL_LISTING", "")
        .args([
            "--no-multiplex",
            "--no-restart",
            "--quiet",
            "--xochitl-dir",
            "device",
        ]);
        cmd
    }

    fn sync(&self, src: &str, dst: &str, delete: bool, fail: &str) -> Output {
        let mut cmd = self.command();
        cmd.env("MOCK_FAIL_LISTING", fail)
            .args(["sync", "--remote-kind", "remarkable", src, dst]);
        if delete {
            cmd.arg("--delete");
        }
        cmd.output().unwrap()
    }

    fn prime_cache(&self, side: &str) {
        assert_success(
            self.command()
                .args([
                    "--host",
                    &format!("{side}.invalid"),
                    "--user",
                    "root",
                    "ls",
                    "--json",
                ])
                .output()
                .unwrap(),
        );
    }

    fn assert_archive(&self, prefix: &str) {
        assert!(
            fs::read_dir(self.0.join("state/rmu"))
                .unwrap()
                .any(|entry| {
                    let path = entry.unwrap().path();
                    path.file_name().unwrap().to_str().unwrap().starts_with(prefix)
                && serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap()["entries"]
                    .as_object().is_some_and(|entries| !entries.is_empty())
                })
        );
    }

    fn child(&self, side: &str) -> String {
        fs::read_dir(self.0.join(format!("{side}/device")))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                if path.extension()?.to_str()? != "metadata" {
                    return None;
                }
                let uuid = path.file_stem()?.to_str()?.to_string();
                (self.metadata(side, &uuid)["type"] == "DocumentType").then_some(uuid)
            })
            .next()
            .expect("copied document")
    }

    fn same_stamp_change(&self, side: &str, uuid: &str, field: &str, value: &str) {
        let path = self.file(side, uuid, "metadata");
        let before = fs::metadata(&path).unwrap();
        let mut metadata = self.metadata(side, uuid);
        metadata[field] = json!(value);
        fs::write(&path, serde_json::to_vec_pretty(&metadata).unwrap()).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        let after = fs::metadata(path).unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    }

    fn delete_source_document(&self) {
        ["metadata", "content", "pdf"].into_iter().for_each(|ext| {
            fs::remove_file(self.file("a", DOC, ext)).unwrap();
        });
    }

    fn assert_preserved(&self, side: &str, uuid: &str, metadata: &Value) {
        assert_eq!(self.metadata(side, uuid), *metadata);
        assert_eq!(fs::read(self.file(side, uuid, "pdf")).unwrap(), ORIGINAL);
        assert!(self.file(side, uuid, "content").exists());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn local_path(fixture: &Fixture) -> String {
    fixture.0.join("local").to_str().unwrap().to_string()
}

fn remove_or_replace_local(path: &Path, delete: bool) {
    if delete {
        fs::remove_file(path).unwrap();
    } else {
        fs::write(path, b"synthetic replacement with different size").unwrap();
    }
}

#[test]
fn filesystem_sync_does_not_mutate_renamed_roots_or_moved_children() {
    for by_uuid in [false, true] {
        for delete in [false, true] {
            let fixture = Fixture::new();
            fixture.folder("b", ROOT, "AAAA");
            fixture.folder("b", OTHER, "Elsewhere");
            let local = local_path(&fixture);
            fs::write(fixture.0.join("local/keep.pdf"), ORIGINAL).unwrap();
            let dst = format!("root@b.invalid:{}", if by_uuid { ROOT } else { "/AAAA" });
            assert_success(fixture.sync(&local, &dst, false, ""));
            fixture.assert_archive("sync-");
            let child = fixture.child("b");
            fixture.prime_cache("b");
            if by_uuid {
                // Even a UUID root needs a fresh subtree: archived child UUIDs
                // must not authorize mutation of a document moved outside it.
                fixture.same_stamp_change("b", &child, "parent", OTHER);
            } else {
                fixture.same_stamp_change("b", ROOT, "visibleName", "BBBB");
            }
            let metadata = fixture.metadata("b", &child);
            remove_or_replace_local(&fixture.0.join("local/keep.pdf"), delete);
            for _ in 0..2 {
                assert_success(fixture.sync(&local, &dst, delete, ""));
                fixture.assert_preserved("b", &child, &metadata);
            }
            if !by_uuid {
                assert_eq!(fixture.metadata("b", ROOT)["visibleName"], "BBBB");
            }
        }
    }
}

#[test]
fn tablet_sync_does_not_mutate_renamed_roots_or_moved_children() {
    for by_uuid in [false, true] {
        for delete in [false, true] {
            let fixture = Fixture::new();
            fixture.folder("a", ROOT, "Source");
            fixture.document("a");
            fixture.folder("b", ROOT, "AAAA");
            fixture.folder("b", OTHER, "Elsewhere");
            let src = "root@a.invalid:/Source";
            let dst = format!("root@b.invalid:{}", if by_uuid { ROOT } else { "/AAAA" });
            assert_success(fixture.sync(src, &dst, false, ""));
            fixture.assert_archive("sync-pair-");
            let child = fixture.child("b");
            fixture.prime_cache("b");
            if by_uuid {
                fixture.same_stamp_change("b", &child, "parent", OTHER);
            } else {
                fixture.same_stamp_change("b", ROOT, "visibleName", "BBBB");
            }
            let metadata = fixture.metadata("b", &child);
            if delete {
                fixture.delete_source_document();
            } else {
                let mut changed = fixture.metadata("a", DOC);
                changed["lastModified"] = json!("1000000000001");
                fixture.put_metadata("a", DOC, &changed);
                fs::write(fixture.file("a", DOC, "pdf"), b"synthetic replacement").unwrap();
            }
            for _ in 0..2 {
                assert_success(fixture.sync(src, &dst, delete, ""));
                fixture.assert_preserved("b", &child, &metadata);
            }
            if !by_uuid {
                assert_eq!(fixture.metadata("b", ROOT)["visibleName"], "BBBB");
            }
        }
    }
}

#[test]
fn renamed_read_only_sources_abort_before_propagating_deletions() {
    for tablet_destination in [false, true] {
        let fixture = Fixture::new();
        fixture.folder("a", ROOT, "AAAA");
        fixture.document("a");
        let dst = if tablet_destination {
            "root@b.invalid:/Target".to_string()
        } else {
            local_path(&fixture)
        };
        let src = "root@a.invalid:/AAAA";
        assert_success(fixture.sync(src, &dst, false, ""));
        fixture.assert_archive(if tablet_destination {
            "sync-pair-"
        } else {
            "sync-"
        });
        let child = tablet_destination.then(|| fixture.child("b"));
        fixture.prime_cache("a");
        fixture.same_stamp_change("a", ROOT, "visibleName", "BBBB");
        fixture.delete_source_document();
        for _ in 0..2 {
            let output = fixture.sync(src, &dst, true, "");
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("AAAA"));
            let payload = child.as_ref().map_or_else(
                || fixture.0.join("local/keep.pdf"),
                |uuid| fixture.file("b", uuid, "pdf"),
            );
            assert_eq!(fs::read(payload).unwrap(), ORIGINAL);
        }
    }
}

#[test]
fn dry_run_uses_authoritative_roots_without_creating_renamed_targets() {
    fn tree(path: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        fs::read_dir(path)
            .unwrap()
            .fold(std::collections::BTreeMap::new(), |mut files, entry| {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    files.extend(tree(&path));
                    files.insert(path, Vec::new());
                } else {
                    files.insert(path.clone(), fs::read(path).unwrap());
                }
                files
            })
    }

    for tablet_source in [false, true] {
        let fixture = Fixture::new();
        fixture.folder("a", ROOT, "Source");
        fixture.document("a");
        fixture.folder("b", ROOT, "AAAA");
        fs::write(fixture.0.join("local/keep.pdf"), ORIGINAL).unwrap();
        let src = if tablet_source {
            "root@a.invalid:/Source".to_string()
        } else {
            local_path(&fixture)
        };
        let dst = "root@b.invalid:/AAAA";
        assert_success(fixture.sync(&src, dst, false, ""));
        fixture.prime_cache("b");
        fixture.same_stamp_change("b", ROOT, "visibleName", "BBBB");
        let files = || tree(&fixture.0.join("b/device"));
        let before = files();
        for _ in 0..2 {
            let output = fixture
                .command()
                .args([
                    "sync",
                    "--remote-kind",
                    "remarkable",
                    "--dry-run",
                    "--delete",
                    &src,
                    dst,
                ])
                .output()
                .unwrap();
            assert!(String::from_utf8_lossy(&output.stdout).contains("mkdir root"));
            assert_success(output);
            assert_eq!(files(), before);
        }
    }
}

#[test]
fn failed_authoritative_sync_listing_never_uses_warm_cache() {
    for tablet_source in [false, true] {
        let fixture = Fixture::new();
        fixture.folder("a", ROOT, "Source");
        fixture.document("a");
        fs::write(fixture.0.join("local/keep.pdf"), ORIGINAL).unwrap();
        let src = if tablet_source {
            "root@a.invalid:/Source".to_string()
        } else {
            local_path(&fixture)
        };
        let dst = "root@b.invalid:/Target";
        assert_success(fixture.sync(&src, dst, false, ""));
        let child = fixture.child("b");
        let metadata = fixture.metadata("b", &child);
        fixture.prime_cache("b");
        if tablet_source {
            fixture.delete_source_document();
        } else {
            fs::remove_file(fixture.0.join("local/keep.pdf")).unwrap();
        }
        for _ in 0..2 {
            assert!(
                !fixture
                    .sync(&src, dst, true, "root@b.invalid")
                    .status
                    .success()
            );
            fixture.assert_preserved("b", &child, &metadata);
        }
    }
}
