//! Local-only SSH subprocess tests. Each test re-executes itself with a private
//! PATH, so no process-global environment changes or real SSH connections occur.
#![cfg(unix)]

use std::fs::{self, File, FileTimes};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use libremarkable_utils::client::Client;
use libremarkable_utils::error::Error;
use libremarkable_utils::ssh::{SshOptions, SshSession};
use serde_json::{Value, json};

struct Device {
    root: PathBuf,
}

impl Device {
    fn client(&self) -> Client {
        Client::new(
            SshSession::new(SshOptions {
                destination: "mock-device".into(),
                multiplex: false,
                ..SshOptions::default()
            })
            .unwrap(),
            self.root.join("data").to_str().unwrap(),
        )
        .with_listing_cache(self.root.join("cache.json"))
    }

    fn metadata_path(&self, uuid: &str) -> PathBuf {
        self.root.join("data").join(format!("{uuid}.metadata"))
    }

    fn put(&self, uuid: &str, name: &str, parent: &str, folder: bool) {
        let metadata = json!({
            "type": if folder { "CollectionType" } else { "DocumentType" },
            "visibleName": name,
            "parent": parent,
            "lastModified": "1000000000000",
            "unknownField": {"preserve": true},
        });
        fs::write(
            self.metadata_path(uuid),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();
        File::options()
            .write(true)
            .open(self.metadata_path(uuid))
            .unwrap()
            .set_times(fixed_times())
            .unwrap();
    }

    fn signature(&self, uuid: &str) -> (u64, u64) {
        let stat = fs::metadata(self.metadata_path(uuid)).unwrap();
        (
            stat.modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            stat.len(),
        )
    }

    fn metadata(&self, uuid: &str) -> Value {
        serde_json::from_slice(&fs::read(self.metadata_path(uuid)).unwrap()).unwrap()
    }

    fn calls(&self) -> usize {
        fs::read_to_string(self.root.join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

fn fixed_times() -> FileTimes {
    FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
}

fn with_device(test_name: &str, test: impl FnOnce(Device)) {
    if let Some(root) = std::env::var_os("RMU_CACHE_TEST_ROOT") {
        test(Device { root: root.into() });
        return;
    }

    let root = std::env::temp_dir().join(format!("rmu-cache-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir(root.join("data")).unwrap();
    File::create(root.join("stamp"))
        .unwrap()
        .set_times(fixed_times())
        .unwrap();
    let ssh = root.join("bin/ssh");
    fs::write(
        &ssh,
        r##"#!/bin/sh
[ "$#" = 3 ] && [ "$1" = -- ] && [ "$2" = mock-device ] || exit 90
printf 'call\n' >> "$RMU_CACHE_TEST_ROOT/calls"
sh -c "$3"
status=$?
case "$3" in
    *'cat > '*)
        # Match staged uploads too: their cat follows setup/trap commands.
        # Force every metadata write to keep exactly the cached mtime.
        touch -r "$RMU_CACHE_TEST_ROOT/stamp" "$RMU_CACHE_TEST_ROOT"/data/*.metadata
        # Simulate a lost success response after the remote write completed.
        [ ! -e "$RMU_CACHE_TEST_ROOT/fail-write" ] || exit 91
        ;;
esac
exit "$status"
"##,
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(root.join("bin"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env("RMU_CACHE_TEST_ROOT", &root)
        .env("PATH", path)
        .output()
        .unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn same_stamp_rename_invalidates_persistent_listing() {
    with_device(
        "same_stamp_rename_invalidates_persistent_listing",
        |device| {
            device.put("doc", "AAAA", "", false);
            // Repeated mutations and new Client instances must not reuse old bodies.
            ["BBBB", "AAAA", "BBBB"].into_iter().for_each(|new_name| {
                let old_name = device.client().list_items().unwrap()[0]
                    .visible_name
                    .clone();
                let before = device.signature("doc");
                device.client().rename(&old_name, new_name).unwrap();
                assert_eq!(device.signature("doc"), before);
                assert!(!device.root.join("cache.json").exists());
                assert_eq!(
                    device.client().list_items().unwrap()[0].visible_name,
                    new_name
                );
                assert!(matches!(
                    device.client().delete(&old_name, false),
                    Err(Error::PathNotFound(_))
                ));
                assert!(device.metadata_path("doc").exists());
                assert_eq!(device.metadata("doc")["unknownField"]["preserve"], true);
            });
            device.client().delete("BBBB", false).unwrap();
            assert!(!device.root.join("cache.json").exists());
            assert!(device.client().list_items().unwrap().is_empty());
        },
    );
}

#[test]
fn external_same_stamp_names_are_not_destructive_targets() {
    with_device(
        "external_same_stamp_names_are_not_destructive_targets",
        |device| {
            device.put("doc", "AAAA", "", false);
            device.put("dest", "Folder", "", true);
            device.client().list_items().unwrap();
            let before = device.signature("doc");
            device.put("doc", "BBBB", "", false);
            assert_eq!(device.signature("doc"), before);
            // Ordinary listings retain the documented stat-fastcheck limitation.
            assert_eq!(
                device.client().list_items().unwrap()[1].visible_name,
                "AAAA"
            );
            let calls = device.calls();
            assert!(matches!(
                device.client().delete("AAAA", false),
                Err(Error::PathNotFound(_))
            ));
            // Authoritative resolution still uses one batched SSH round trip.
            assert_eq!(device.calls() - calls, 1);
            assert!(matches!(
                device.client().rename("AAAA", "CCCC"),
                Err(Error::PathNotFound(_))
            ));
            assert!(matches!(
                device.client().move_item("AAAA", "Folder"),
                Err(Error::PathNotFound(_))
            ));
            assert_eq!(device.metadata("doc")["visibleName"], "BBBB");
            let deleted = device.client().delete("BBB*", false).unwrap();
            assert_eq!(deleted.len(), 1);
            assert_eq!(deleted[0].uuid, "doc");
        },
    );
}

#[test]
fn external_same_stamp_destinations_and_conflicts_are_authoritative() {
    with_device(
        "external_same_stamp_destinations_and_conflicts_are_authoritative",
        |device| {
            device.put("dest", "AAAA", "", true);
            device.put("doc", "Target", "", false);
            device.put("sibling", "CCCC", "dest", false);
            device.client().list_items().unwrap();
            let before = device.signature("dest");
            device.put("dest", "BBBB", "", true);
            assert_eq!(device.signature("dest"), before);
            assert!(matches!(
                device.client().move_item("Target", "AAAA"),
                Err(Error::PathNotFound(_))
            ));
            device.client().move_item("Target", "BBBB").unwrap();
            assert!(!device.root.join("cache.json").exists());
            assert_eq!(device.metadata("doc")["parent"], "dest");
            device.client().list_items().unwrap();
            let before = device.signature("sibling");
            device.put("sibling", "DDDD", "dest", false);
            assert_eq!(device.signature("sibling"), before);
            assert!(matches!(
                device.client().rename("BBBB/Target", "DDDD"),
                Err(Error::NameConflict { .. })
            ));
            assert_eq!(device.metadata("doc")["visibleName"], "Target");
        },
    );
}

#[test]
fn external_same_stamp_tree_checks_are_authoritative() {
    with_device(
        "external_same_stamp_tree_checks_are_authoritative",
        |device| {
            device.put("fold", "Folder", "", true);
            device.put("else", "Elsewhere", "", true);
            device.put("doc", "Child", "else", false);
            device.client().list_items().unwrap();
            let before = device.signature("doc");
            device.put("doc", "Child", "fold", false);
            assert_eq!(device.signature("doc"), before);
            assert!(matches!(
                device.client().delete("Folder", false),
                Err(Error::FolderNotEmpty(_))
            ));
            let deleted = device.client().delete("Folder", true).unwrap();
            assert_eq!(
                deleted
                    .iter()
                    .map(|item| item.uuid.as_str())
                    .collect::<Vec<_>>(),
                ["doc", "fold"]
            );
            assert!(device.metadata_path("else").exists());
        },
    );
}

#[test]
fn external_same_stamp_trash_restore_is_not_deleted() {
    with_device(
        "external_same_stamp_trash_restore_is_not_deleted",
        |device| {
            device.put("alive", "Folder", "", true);
            device.put("doc", "Saved", "trash", false);
            device.client().list_items().unwrap();
            let before = device.signature("doc");
            device.put("doc", "Saved", "alive", false);
            assert_eq!(device.signature("doc"), before);
            assert!(device.client().empty_trash().unwrap().is_empty());
            assert!(device.metadata_path("doc").exists());
        },
    );
}

#[test]
fn failed_write_response_does_not_preserve_stale_cache() {
    with_device(
        "failed_write_response_does_not_preserve_stale_cache",
        |device| {
            device.put("doc", "AAAA", "", false);
            device.client().list_items().unwrap();
            let before = device.signature("doc");
            fs::write(device.root.join("fail-write"), "").unwrap();
            assert!(device.client().rename("AAAA", "BBBB").is_err());
            assert_eq!(device.signature("doc"), before);
            assert!(!device.root.join("cache.json").exists());
            assert_eq!(
                device.client().list_items().unwrap()[0].visible_name,
                "BBBB"
            );
            assert!(matches!(
                device.client().delete("AAAA", false),
                Err(Error::PathNotFound(_))
            ));
        },
    );
}

#[test]
fn unremovable_cache_blocks_mutation_before_writing() {
    with_device(
        "unremovable_cache_blocks_mutation_before_writing",
        |device| {
            device.put("doc", "AAAA", "", false);
            // A directory at the cache path deterministically rejects remove_file,
            // including when the tests run with elevated filesystem permissions.
            fs::create_dir(device.root.join("cache.json")).unwrap();
            assert!(device.client().rename("AAAA", "BBBB").is_err());
            assert_eq!(device.metadata("doc")["visibleName"], "AAAA");
            assert!(device.client().delete("AAAA", false).is_err());
            assert!(device.metadata_path("doc").exists());
        },
    );
}
