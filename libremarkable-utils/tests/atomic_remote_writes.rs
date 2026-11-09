//! Exercise the actual SSH subprocess boundary without a device. Each scenario
//! runs in a separate test process so its mock PATH never races other tests.
//! The remote-script harness needs GNU/BusyBox utilities, not BSD variants.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use libremarkable_utils::client::Client;
use libremarkable_utils::progress::{NoProgress, Progress};
use libremarkable_utils::ssh::{SshOptions, SshSession};
use test_support::Scratch;

const UUID: &str = "11111111-1111-4111-8111-111111111111";
const METADATA: &[u8] = br#"{"type":"DocumentType","visibleName":"Original","parent":"","lastModified":"1","unknown":{"keep":true}}"#;
const ORIGINAL: &[u8] = b"original synthetic PDF bytes";

#[test]
fn mock_remote_write_scenarios() {
    let sandbox = Scratch::new("atomic");
    sandbox.mock_command(
        "ssh",
        r#"for cmd do :; done
case "$cmd" in
    *'cat >'*) ;;
    *) exec sh -c "$cmd" ;;
esac
case "$RMU_TEST_MODE" in
    truncated)
        # Drain the sender but deliver a clean, short EOF to the real command.
        # Remote cat succeeds: only the expected-length check detects this.
        { head -c 3; cat >/dev/null; } | sh -c "$cmd"
        ;;
    cat-failure)
        sh -c 'cat() { command head -c 3; return 1; }; '"$cmd"
        ;;
    signal)
        sh -c 'cat() { command head -c 3; kill -TERM $$; return 1; }; '"$cmd"
        ;;
    promotion-failure)
        sh -c 'mv() { return 1; }; '"$cmd"
        ;;
    disconnected)
        exit 255
        ;;
    *)
        # Check that even after cat receives all bytes, the original has not
        # changed. Also record each reserved sibling to test uniqueness.
        sh -c 'cat() {
            command cat "$@" || return 1
            if [ -n "$tmp" ]; then
                [ "$(stat -c %a -- "$tmp")" = 600 ] || return 1
                printf "%s\n" "$tmp" >> "$RMU_TEST_ROOT/staging-paths"
                if [ -f "$dest" ]; then
                    cmp "$dest" "$dest.expected" || return 1
                fi
            fi
        }; '"$cmd"
        ;;
esac
"#,
    );
    let path = sandbox.mock_path();
    [
        "truncated",
        "cat-failure",
        "signal",
        "promotion-failure",
        "disconnected",
        "success",
    ]
    .into_iter()
    .for_each(|mode| {
        let root = sandbox.join(format!("{mode} 'quoted $directory"));
        fs::create_dir(&root).unwrap();
        let output = test_support::rerun_test("mock_remote_write_child")
            .env("PATH", &path)
            .env("RMU_TEST_ROOT", &root)
            .env("RMU_TEST_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    });
}

fn session() -> SshSession {
    SshSession::new(SshOptions {
        destination: "mock-only".to_string(),
        multiplex: false,
        ..SshOptions::default()
    })
    .unwrap()
}

fn put(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o640)).unwrap();
    fs::write(format!("{}.expected", path.display()), bytes).unwrap();
}

fn assert_unchanged(path: &Path, bytes: &[u8]) {
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o640);
}

fn assert_no_staging(root: &Path) {
    assert!(fs::read_dir(root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".rmu-upload-")
    }));
}

#[test]
fn mock_remote_write_child() {
    let Some(root) = std::env::var_os("RMU_TEST_ROOT").map(PathBuf::from) else {
        return;
    };
    let mode = std::env::var("RMU_TEST_MODE").unwrap();
    let success = mode == "success";
    let metadata = root.join(format!("{UUID}.metadata"));
    let payload = root.join(format!("{UUID}.pdf"));
    let local = root.join("local.pdf");
    let replacement = vec![0x5a; 256 * 1024 + 7]; // exceeds the pipe buffer
    fs::write(&local, &replacement).unwrap();
    fs::write(
        root.join(format!("{UUID}.content")),
        br#"{"fileType":"pdf"}"#,
    )
    .unwrap();
    let client = Client::new(session(), root.to_str().unwrap());

    // Repeat failures to catch fixed staging names, stale files, and damaged
    // metadata that would make the second rename/listing behave differently.
    (0..3).for_each(|_| {
        put(&metadata, METADATA);
        put(&payload, ORIGINAL);
        let attrs = fs::metadata(&metadata).unwrap();
        let result = client.rename(UUID, "Renamed");
        assert_eq!(result.is_ok(), success, "rename: {result:?}");
        if mode == "truncated" {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("unexpected byte count")
            );
        }
        if success {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
            assert_eq!(value["visibleName"], "Renamed");
            assert_eq!(value["unknown"]["keep"], true);
            let updated = fs::metadata(&metadata).unwrap();
            assert_eq!(updated.mode(), attrs.mode());
            assert_eq!(updated.uid(), attrs.uid());
            assert_eq!(updated.gid(), attrs.gid());
        } else {
            assert_unchanged(&metadata, METADATA);
            assert_eq!(client.list_items().unwrap().len(), 1);
        }
        assert_no_staging(&root);

        [false, true].into_iter().for_each(|from_file| {
            put(&metadata, METADATA);
            put(&payload, ORIGINAL);
            let result = if from_file {
                client.update_payload_from_file(UUID, "pdf", &local)
            } else {
                client.update_payload_bytes(UUID, "pdf", &replacement)
            };
            assert_eq!(result.is_ok(), success, "payload: {result:?}");
            if mode == "truncated" {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("unexpected byte count")
                );
            }
            if success {
                assert_unchanged(&payload, &replacement);
                let value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
                assert_ne!(value["lastModified"], "1");
                assert_eq!(value["unknown"]["keep"], true);
            } else {
                assert_unchanged(&metadata, METADATA);
                assert_unchanged(&payload, ORIGINAL);
            }
            assert_no_staging(&root);
        });
    });

    // Absent destinations must not become visible after a partial write.
    let new_path = root.join("new 'quoted $.metadata");
    let result = session().write_remote_file(new_path.to_str().unwrap(), METADATA);
    assert_eq!(result.is_ok(), success);
    assert_eq!(new_path.exists(), success);
    assert_no_staging(&root);

    let before_store = client.list_items().unwrap().len();
    let result = client.store_payload_bytes(&replacement, "", "New document", "pdf");
    assert_eq!(result.is_ok(), success, "new payload: {result:?}");
    if let Ok(item) = result {
        assert_eq!(
            fs::read(root.join(format!("{}.pdf", item.uuid))).unwrap(),
            replacement
        );
    }
    assert_eq!(
        client.list_items().unwrap().len(),
        before_store + usize::from(success)
    );
    assert_no_staging(&root);

    if success {
        assert_eq!(fs::read(&new_path).unwrap(), METADATA);
        // New files retain normal umask-derived permissions, not mktemp's 0600.
        assert_eq!(
            fs::metadata(&new_path).unwrap().mode() & 0o777,
            fs::metadata(&local).unwrap().mode() & 0o777
        );

        // A local source shrinking after the size was recorded also produces
        // a clean EOF, not an I/O error. It must not replace the remote file.
        struct ShrinkSource<'a>(&'a Path);
        impl Progress for ShrinkSource<'_> {
            fn step(&self, _: &str) {}
            fn bytes(&self, _: u64, _: Option<u64>) {
                fs::OpenOptions::new()
                    .write(true)
                    .open(self.0)
                    .unwrap()
                    .set_len(64 * 1024)
                    .unwrap();
            }
            fn bytes_done(&self) {
                panic!("short upload must not report completion");
            }
            fn finished(&self) {}
        }
        put(&payload, ORIGINAL);
        let error = session()
            .upload_local_file(&local, payload.to_str().unwrap(), &ShrinkSource(&local))
            .unwrap_err();
        assert!(error.to_string().contains("unexpected byte count"));
        assert_unchanged(&payload, ORIGINAL);
        assert_no_staging(&root);

        // Empty files are complete uploads too; preserve existing mode.
        put(&payload, ORIGINAL);
        session()
            .write_remote_file(payload.to_str().unwrap(), b"")
            .unwrap();
        assert_unchanged(&payload, b"");
        fs::write(&local, b"").unwrap();
        put(&payload, ORIGINAL);
        session()
            .upload_local_file(&local, payload.to_str().unwrap(), &NoProgress)
            .unwrap();
        assert_unchanged(&payload, b"");
        let paths = fs::read_to_string(root.join("staging-paths")).unwrap();
        let unique: std::collections::HashSet<_> = paths.lines().collect();
        assert_eq!(unique.len(), paths.lines().count());
        assert_no_staging(&root);
    }
}
