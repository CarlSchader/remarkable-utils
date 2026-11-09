#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use libremarkable_utils::client::Client;
use libremarkable_utils::ssh::{SshOptions, SshSession};

const MOCK_SSH: &str = r#"[ "$#" -eq 3 ] && [ "$1" = -- ] && [ "$2" = mock-device ] || exit 99
case "$3" in
  *'stat -c'*) phase=stat;;
  *'__sizes__'*) phase=full;;
  *) phase=fetch;;
esac
printf '%s\n' "$phase" >> "$RMU_LISTING_TEST_ROOT/trips"
stat() {
  printf 'stat\n' >> "$RMU_LISTING_TEST_ROOT/stat-calls"
  if [ -f "$RMU_LISTING_TEST_ROOT/stat-total" ]; then
    echo 'injected stat I/O error' >&2
    return 1
  fi
  if [ -f "$RMU_LISTING_TEST_ROOT/stat-partial" ]; then
    shift 3
    failed=0
    for file do
      if [ "$file" = bbb.metadata ]; then
        echo 'injected stat I/O error for bbb.metadata' >&2
        failed=1
      else
        command stat -c '%Y %s %n' -- "$file" || return 1
      fi
    done
    return "$failed"
  fi
  command stat "$@" || return 1
  if [ -f "$RMU_LISTING_TEST_ROOT/vanish-after-stat" ]; then
    rm -- bbb.metadata
  fi
}
cat() {
  if [ "$1" = bbb.metadata ] && {
    [ -f "$RMU_LISTING_TEST_ROOT/read-failure" ] || {
      [ "$phase" = fetch ] && [ -f "$RMU_LISTING_TEST_ROOT/fetch-failure" ]
    }
  }; then
    printf '{"type":'
    echo 'injected metadata read error' >&2
    return 1
  fi
  command cat "$@"
}
eval "$3"
"#;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn data(&self) -> PathBuf {
        // Exercise remote shell quoting as well as error handling.
        self.root.join("device's data")
    }

    fn cache(&self) -> PathBuf {
        self.root.join("listing.json")
    }

    fn client(&self, cached: bool) -> Client {
        let session = SshSession::new(SshOptions {
            destination: "mock-device".into(),
            multiplex: false,
            ..SshOptions::default()
        })
        .unwrap();
        let client = Client::new(session, self.data().to_str().unwrap());
        if cached {
            client.with_listing_cache(self.cache())
        } else {
            client
        }
    }

    fn document(&self, id: &str, name: &str) {
        fs::write(
            self.data().join(format!("{id}.metadata")),
            serde_json::json!({
                "type": "DocumentType", "visibleName": name, "parent": ""
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            self.data().join(format!("{id}.content")),
            r#"{"fileType":"pdf"}"#,
        )
        .unwrap();
        fs::write(self.data().join(format!("{id}.pdf")), b"synthetic payload").unwrap();
    }

    fn fail(&self, mode: &str) {
        fs::write(self.root.join(mode), b"").unwrap();
    }

    fn recover(&self, mode: &str) {
        fs::remove_file(self.root.join(mode)).unwrap();
    }

    fn trips(&self) -> String {
        fs::read_to_string(self.root.join("trips")).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.root.exists() {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
}

// Re-execute only this test with a private PATH: no process-global environment
// mutation, cross-test races, SSH connection, or production transport changes.
fn with_mock_ssh(name: &str, test: impl FnOnce(&Fixture)) {
    if let Some(root) = std::env::var_os("RMU_LISTING_TEST_ROOT") {
        test(&Fixture { root: root.into() });
        return;
    }
    let root = std::env::temp_dir().join(format!("rmu-listing-{}", uuid::Uuid::new_v4()));
    let fixture = Fixture { root };
    fs::create_dir_all(fixture.data()).unwrap();
    let bin = fixture.root.join("bin");
    fs::create_dir(&bin).unwrap();
    test_support::write_script(&bin.join("ssh"), MOCK_SSH);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(bin).chain(std::env::split_paths(&path));
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("RMU_LISTING_TEST_ROOT", &fixture.root)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mock listing test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn check_stat_failure(fixture: &Fixture, mode: &str) {
    fixture.document("aaa", "First");
    fixture.document("bbb", "Second");
    let client = fixture.client(true);
    fixture.fail(mode);
    assert_eq!(client.list_items().unwrap().len(), 2);
    assert!(!fixture.cache().exists(), "failed stat must not seed cache");
    assert_eq!(fixture.trips(), "stat\nfull\n");

    fixture.recover(mode);
    assert_eq!(client.list_items().unwrap().len(), 2);
    let cache = fs::read(fixture.cache()).unwrap();
    fixture.document("ccc", "New document");
    fixture.fail(mode);
    (0..2).for_each(|_| {
        assert_eq!(client.list_items().unwrap().len(), 3);
        assert_eq!(fs::read(fixture.cache()).unwrap(), cache);
        assert!(fixture.trips().ends_with("stat\nfull\n"));
    });
    fixture.recover(mode);
    assert_eq!(client.list_items().unwrap().len(), 3);
    assert_ne!(fs::read(fixture.cache()).unwrap(), cache);
    assert_eq!(client.list_items().unwrap().len(), 3);
    assert!(fixture.trips().ends_with("stat\nfetch\nstat\n"));
}

#[test]
fn total_stat_failure_falls_back_without_poisoning_cache() {
    with_mock_ssh(
        "total_stat_failure_falls_back_without_poisoning_cache",
        |fixture| check_stat_failure(fixture, "stat-total"),
    );
}

#[test]
fn partial_stat_failure_falls_back_without_poisoning_cache() {
    with_mock_ssh(
        "partial_stat_failure_falls_back_without_poisoning_cache",
        |fixture| check_stat_failure(fixture, "stat-partial"),
    );
}

#[test]
fn metadata_read_failures_reject_incremental_and_fallback_listings() {
    with_mock_ssh(
        "metadata_read_failures_reject_incremental_and_fallback_listings",
        |fixture| {
            fixture.document("aaa", "First");
            fixture.document("bbb", "Second");
            let client = fixture.client(true);
            assert_eq!(client.list_items().unwrap().len(), 2);
            let cache = fs::read(fixture.cache()).unwrap();
            fixture.document("bbb", "Second with a changed size");
            fixture.fail("read-failure");
            [None, Some("stat-total"), Some("stat-partial")]
                .into_iter()
                .for_each(|mode| {
                    if let Some(mode) = mode {
                        fixture.fail(mode);
                    }
                    (0..2).for_each(|_| {
                        assert!(client.list_items().is_err());
                        assert_eq!(fs::read(fixture.cache()).unwrap(), cache);
                    });
                    // Mutation planning must stop before creating anything.
                    assert!(client.mkdir_path("New folder", "/").is_err());
                    if let Some(mode) = mode {
                        fixture.recover(mode);
                    }
                });
            assert!(fixture.client(false).list_items().is_err());
            fixture.recover("read-failure");
            let items = client.list_items().unwrap();
            assert_eq!(items.len(), 2);
            assert_eq!(items[1].visible_name, "Second with a changed size");
        },
    );
}

#[test]
fn transient_fetch_failure_uses_full_listing_without_saving_partial_cache() {
    with_mock_ssh(
        "transient_fetch_failure_uses_full_listing_without_saving_partial_cache",
        |fixture| {
            fixture.document("aaa", "First");
            fixture.document("bbb", "Second");
            fixture.fail("fetch-failure");
            assert_eq!(fixture.client(true).list_items().unwrap().len(), 2);
            assert!(!fixture.cache().exists());
            assert_eq!(fixture.trips(), "stat\nfetch\nfull\n");
            fixture.recover("fetch-failure");
            assert_eq!(fixture.client(true).list_items().unwrap().len(), 2);
            assert!(fixture.cache().exists());
        },
    );
}

#[test]
fn metadata_disappearing_after_stat_triggers_fallback_without_caching() {
    with_mock_ssh(
        "metadata_disappearing_after_stat_triggers_fallback_without_caching",
        |fixture| {
            fixture.document("aaa", "First");
            fixture.document("bbb", "Second");
            fixture.fail("vanish-after-stat");
            let items = fixture.client(true).list_items().unwrap();
            // The full listing observes the deletion afresh; the old stat
            // snapshot must not be saved with a missing fetched body.
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].uuid, "aaa");
            assert!(!fixture.cache().exists());
            assert_eq!(fixture.trips(), "stat\nfetch\nfull\n");
            fixture.recover("vanish-after-stat");
            fixture.document("bbb", "Recreated");
            assert_eq!(fixture.client(true).list_items().unwrap().len(), 2);
        },
    );
}

#[test]
fn empty_directory_is_a_valid_cached_and_full_listing() {
    with_mock_ssh(
        "empty_directory_is_a_valid_cached_and_full_listing",
        |fixture| {
            (0..2).for_each(|_| {
                assert!(fixture.client(true).list_items().unwrap().is_empty());
            });
            assert!(fixture.cache().exists());
            assert!(!fixture.root.join("stat-calls").exists());
            assert!(fixture.client(false).list_items().unwrap().is_empty());
            assert_eq!(fixture.trips(), "stat\nstat\nfull\n");
        },
    );
}

#[test]
fn fallback_rejects_payload_size_read_failure() {
    with_mock_ssh("fallback_rejects_payload_size_read_failure", |fixture| {
        fixture.document("aaa", "First");
        fs::remove_file(fixture.data().join("aaa.pdf")).unwrap();
        symlink(Path::new("missing-target"), fixture.data().join("aaa.pdf")).unwrap();
        fixture.fail("stat-total");
        assert!(fixture.client(true).list_items().is_err());
        assert!(fixture.client(false).list_items().is_err());
        assert!(!fixture.cache().exists());
    });
}

#[test]
fn fallback_does_not_skip_unreadable_actual_metadata() {
    with_mock_ssh(
        "fallback_does_not_skip_unreadable_actual_metadata",
        |fixture| {
            fixture.document("aaa", "First");
            // A dangling symlink is an actual glob match, but `test -f`
            // would silently omit it rather than let cat report the error.
            symlink(
                Path::new("missing-target"),
                fixture.data().join("bbb.metadata"),
            )
            .unwrap();
            fixture.fail("stat-total");
            assert!(fixture.client(true).list_items().is_err());
            assert!(fixture.client(false).list_items().is_err());
            assert!(!fixture.cache().exists());
        },
    );
}
