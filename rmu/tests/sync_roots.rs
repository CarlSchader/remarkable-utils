//! CLI-level sync tests. SSH commands run only against synthetic local trees.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use test_support::Scratch;

const PAYLOAD: &[u8] = b"synthetic PDF payload\n";

#[derive(Clone, Copy, Debug)]
enum Kind {
    Local,
    Ssh,
    Tablet,
}
const KINDS: [Kind; 3] = [Kind::Local, Kind::Ssh, Kind::Tablet];

struct Fixture(Scratch);

impl Fixture {
    fn new() -> Self {
        let fixture = Self(Scratch::new("sync-roots"));
        fixture.0.mock_command(
            "ssh",
            r#"while [ "$1" != -- ]; do
    [ "$#" -gt 0 ] || exit 90
    shift
done
shift
host=$1
shift
command=$1
printf '%s: %s\n' "$host" "$command" >> "$MOCK_ROOT/commands"
cd "$MOCK_ROOT/trees/$host" || exit 91
case "$command" in
    *'/usr/bin/xochitl'*) test -d xochitl; exit $? ;;
esac
case "$command" in
    # 'sh -s': a new item's files, created by one script over stdin.
    *'cat >'*|*'mkdir '*|*'systemctl '*|*'tar -x'*|*'rm '*|*'mv '*|*'cp '*|'sh -s')
        if [ "$MOCK_READ_ONLY" = 1 ]; then
            printf 'unexpected write during dry-run\n' >&2
            exit 92
        fi
        if [ "$MOCK_FAIL_WRITES" = 1 ]; then
            printf 'injected write failure\n' >&2
            exit 93
        fi
        ;;
esac
exec sh -c "$command"
"#,
        );
        fixture.0.mock_command(
            "systemctl",
            "printf '%s: %s\\n' \"$PWD\" \"$*\" >> \"$MOCK_ROOT/restarts\"\n",
        );
        fixture
    }

    fn endpoint(&self, kind: Kind, side: &str, exists: bool, populated: bool) -> String {
        let host = self.0.join("trees").join(side);
        fs::create_dir_all(&host).unwrap();
        match kind {
            Kind::Local | Kind::Ssh => {
                let path = host.join("parent/root");
                if exists {
                    fs::create_dir_all(&path).unwrap();
                    if populated {
                        fs::create_dir(path.join("nested")).unwrap();
                        fs::write(path.join("nested/book.pdf"), PAYLOAD).unwrap();
                    }
                }
                match kind {
                    Kind::Local => path.display().to_string(),
                    _ => format!("{side}:parent/root"),
                }
            }
            Kind::Tablet => {
                let data = host.join("xochitl");
                fs::create_dir(&data).unwrap();
                // An unrelated root document must never enter a virtual subtree.
                Self::item(&data, "outside", "outside", "", false);
                Self::item(&data, "parent", "parent", "", true);
                if exists {
                    Self::item(&data, "root", "root", "parent", true);
                    if populated {
                        Self::item(&data, "nested", "nested", "root", true);
                        Self::item(&data, "book", "book", "nested", false);
                    }
                }
                format!("{side}:/parent/root")
            }
        }
    }

    fn item(data: &Path, uuid: &str, name: &str, parent: &str, folder: bool) {
        fs::write(
            data.join(format!("{uuid}.metadata")),
            serde_json::json!({
                "visibleName": name, "parent": parent, "lastModified": "1000",
                "type": if folder { "CollectionType" } else { "DocumentType" }
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            data.join(format!("{uuid}.content")),
            if folder {
                "[]"
            } else {
                r#"{"fileType":"pdf"}"#
            },
        )
        .unwrap();
        if !folder {
            fs::write(data.join(format!("{uuid}.pdf")), PAYLOAD).unwrap();
        }
    }

    fn run(&self, src: &str, dst: &str, flags: &[&str], fail_writes: bool) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rmu"))
            .args(["--no-multiplex", "--xochitl-dir", "xochitl", "sync"])
            .args(flags)
            .args([src, dst])
            .env("PATH", self.0.mock_path())
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("MOCK_ROOT", self.0.path())
            .env(
                "MOCK_READ_ONLY",
                if flags.contains(&"--dry-run") {
                    "1"
                } else {
                    "0"
                },
            )
            .env("MOCK_FAIL_WRITES", if fail_writes { "1" } else { "0" })
            .output()
            .unwrap()
    }

    fn trees(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
            fs::read_dir(path).unwrap().for_each(|entry| {
                let path = entry.unwrap().path();
                let key = path.strip_prefix(root).unwrap().to_path_buf();
                if path.is_symlink() {
                    result.insert(
                        key,
                        fs::read_link(path)
                            .unwrap()
                            .as_os_str()
                            .as_encoded_bytes()
                            .to_vec(),
                    );
                } else if path.is_dir() {
                    result.insert(key, Vec::new());
                    walk(root, &path, result);
                } else {
                    result.insert(key, fs::read(path).unwrap());
                }
            });
        }
        let mut result = BTreeMap::new();
        walk(self.0.path(), &self.0.join("trees"), &mut result);
        result
    }

    fn assert_nested_copy(&self, kind: Kind, side: &str) {
        let host = self.0.join("trees").join(side);
        match kind {
            Kind::Local | Kind::Ssh => {
                assert_eq!(
                    fs::read(host.join("parent/root/nested/book.pdf")).unwrap(),
                    PAYLOAD
                );
            }
            Kind::Tablet => {
                let data = host.join("xochitl");
                let items: Vec<(String, serde_json::Value)> = fs::read_dir(&data)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "metadata"))
                    .map(|path| {
                        (
                            path.file_stem().unwrap().to_str().unwrap().to_string(),
                            serde_json::from_slice(&fs::read(path).unwrap()).unwrap(),
                        )
                    })
                    .collect();
                let root = items
                    .iter()
                    .find(|(_, item)| item["visibleName"] == "root")
                    .unwrap();
                assert_eq!(root.1["parent"], "parent");
                let nested = items
                    .iter()
                    .find(|(_, item)| item["visibleName"] == "nested")
                    .unwrap();
                assert_eq!(nested.1["parent"], root.0);
                let book = items
                    .iter()
                    .find(|(_, item)| item["visibleName"] == "book")
                    .unwrap();
                assert_eq!(book.1["parent"], nested.0);
                assert_eq!(
                    fs::read(data.join(format!("{}.pdf", book.0))).unwrap(),
                    PAYLOAD
                );
            }
        }
    }
}

fn success(output: &Output) -> String {
    test_support::assert_success(output);
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn missing_destinations_are_virtual_for_every_pairing() {
    KINDS.into_iter().for_each(|src_kind| {
        KINDS.into_iter().for_each(|dst_kind| {
            let fixture = Fixture::new();
            let src = fixture.endpoint(src_kind, "a", true, true);
            let dst = fixture.endpoint(dst_kind, "b", false, false);
            let before = fixture.trees();
            let first = success(&fixture.run(&src, &dst, &["--dry-run"], false));
            assert!(
                first.contains("mkdir root"),
                "{src_kind:?} -> {dst_kind:?}: {first}"
            );
            assert!(first.contains("nested/book"), "{first}");
            assert!(!first.contains("outside"), "{first}");
            assert_eq!(before, fixture.trees());
            let second = success(&fixture.run(&src, &dst, &["--dry-run"], false));
            assert_eq!(first, second);
            assert_eq!(before, fixture.trees());
            assert!(!fixture.0.join("restarts").exists());

            success(&fixture.run(&src, &dst, &[], false));
            fixture.assert_nested_copy(dst_kind, "b");
            // A successful real run must leave no pending root creation.
            let after = success(&fixture.run(&src, &dst, &["--dry-run"], false));
            assert!(!after.contains("mkdir root"), "{after}");
        });
    });
}

#[test]
fn missing_sources_error_unless_writable_for_every_pairing() {
    KINDS.into_iter().for_each(|src_kind| {
        KINDS.into_iter().for_each(|dst_kind| {
            let fixture = Fixture::new();
            let src = fixture.endpoint(src_kind, "z", false, false);
            let dst = fixture.endpoint(dst_kind, "a", true, true);
            let before = fixture.trees();
            assert!(
                !fixture
                    .run(&src, &dst, &["--dry-run"], false)
                    .status
                    .success()
            );
            assert!(!fixture.run(&src, &dst, &[], false).status.success());
            assert_eq!(before, fixture.trees());
            let plan = success(&fixture.run(&src, &dst, &["--dry-run", "--two-way"], false));
            assert!(plan.contains("mkdir root"), "{plan}");
            assert!(plan.contains("nested/book"), "{plan}");
            assert_eq!(before, fixture.trees());
            success(&fixture.run(&src, &dst, &["--two-way"], false));
            fixture.assert_nested_copy(src_kind, "z");
        });
    });
}

#[test]
fn both_missing_roots_are_virtual_in_two_way_sync() {
    KINDS.into_iter().for_each(|src_kind| {
        KINDS.into_iter().for_each(|dst_kind| {
            let fixture = Fixture::new();
            let src = fixture.endpoint(src_kind, "z", false, false);
            let dst = fixture.endpoint(dst_kind, "a", false, false);
            let before = fixture.trees();
            let plan = success(&fixture.run(&src, &dst, &["--dry-run", "--two-way"], false));
            assert_eq!(
                plan.lines()
                    .filter(|line| line.starts_with("mkdir root"))
                    .count(),
                2
            );
            assert!(!plan.contains("outside"), "{plan}");
            assert_eq!(before, fixture.trees());
        });
    });
}

#[test]
fn root_only_tablet_creation_restarts_once_and_respects_no_restart() {
    [Kind::Local, Kind::Tablet]
        .into_iter()
        .for_each(|src_kind| {
            [false, true].into_iter().for_each(|no_restart| {
                let fixture = Fixture::new();
                let src = fixture.endpoint(src_kind, "z", true, false);
                let dst = fixture.endpoint(Kind::Tablet, "a", false, false);
                let flags: &[&str] = if no_restart { &["--no-restart"] } else { &[] };
                success(&fixture.run(&src, &dst, flags, false));
                let restarts = fs::read_to_string(fixture.0.join("restarts")).unwrap_or_default();
                assert_eq!(
                    restarts.matches("restart xochitl.service").count(),
                    usize::from(!no_restart)
                );
                if !no_restart {
                    assert!(restarts.contains("reset-failed xochitl.service"));
                    assert!(!restarts.contains("/z:"));
                }
                success(&fixture.run(&src, &dst, flags, false));
                assert_eq!(
                    restarts,
                    fs::read_to_string(fixture.0.join("restarts")).unwrap_or_default()
                );
            });
        });
}

#[test]
fn two_way_root_only_creation_restarts_both_tablets() {
    let fixture = Fixture::new();
    let src = fixture.endpoint(Kind::Tablet, "z", false, false);
    let dst = fixture.endpoint(Kind::Tablet, "a", false, false);
    success(&fixture.run(&src, &dst, &["--two-way"], false));
    let restarts = fs::read_to_string(fixture.0.join("restarts")).unwrap();
    assert_eq!(restarts.matches("restart xochitl.service").count(), 2);
    assert_eq!(restarts.matches("reset-failed xochitl.service").count(), 2);
    success(&fixture.run(&src, &dst, &["--two-way"], false));
    assert_eq!(
        restarts,
        fs::read_to_string(fixture.0.join("restarts")).unwrap()
    );
}

#[test]
fn invalid_existing_roots_are_not_treated_as_empty() {
    KINDS.into_iter().for_each(|kind| {
        let fixture = Fixture::new();
        let src = fixture.endpoint(Kind::Local, "a", true, true);
        let dst = fixture.endpoint(kind, "b", false, false);
        let host = fixture.0.join("trees/b");
        match kind {
            Kind::Local | Kind::Ssh => {
                fs::create_dir(host.join("parent")).unwrap();
                fs::write(host.join("parent/root"), b"not a directory").unwrap();
            }
            Kind::Tablet => {
                Fixture::item(&host.join("xochitl"), "root", "root", "parent", false);
            }
        }
        let before = fixture.trees();
        assert!(
            !fixture
                .run(&src, &dst, &["--dry-run"], false)
                .status
                .success()
        );
        assert_eq!(before, fixture.trees());
    });

    [false, true].into_iter().for_each(|ambiguous| {
        let fixture = Fixture::new();
        let src = fixture.endpoint(Kind::Local, "a", true, true);
        let dst = fixture.endpoint(Kind::Tablet, "b", false, false);
        let data = fixture.0.join("trees/b/xochitl");
        if ambiguous {
            Fixture::item(&data, "duplicate", "parent", "", true);
        } else {
            Fixture::item(&data, "parent", "parent", "", false);
        }
        let before = fixture.trees();
        assert!(
            !fixture
                .run(&src, &dst, &["--dry-run"], false)
                .status
                .success()
        );
        assert_eq!(before, fixture.trees());
    });
}

#[test]
fn invalid_filesystem_ancestors_are_not_treated_as_missing() {
    [Kind::Local, Kind::Ssh].into_iter().for_each(|kind| {
        [false, true].into_iter().for_each(|inaccessible| {
            let fixture = Fixture::new();
            let src = fixture.endpoint(Kind::Local, "a", true, true);
            let dst = fixture.endpoint(kind, "b", false, false);
            let parent = fixture.0.join("trees/b/parent");
            if inaccessible {
                fs::create_dir(&parent).unwrap();
            } else {
                fs::write(&parent, b"not a directory").unwrap();
            }
            let before = fixture.trees();
            if inaccessible {
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o600)).unwrap();
                // Privileged test runners may bypass directory permissions.
                let searchable = Command::new("sh")
                    .args(["-c", "test -x \"$1\"", "sh"])
                    .arg(&parent)
                    .status()
                    .unwrap()
                    .success();
                if searchable {
                    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
                    eprintln!("skipping inaccessible-prefix case: runner bypasses permissions");
                    return;
                }
            }
            let output = fixture.run(&src, &dst, &["--dry-run"], false);
            if inaccessible {
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
            }
            assert!(
                !output.status.success(),
                "{kind:?}, inaccessible={inaccessible}"
            );
            assert_eq!(before, fixture.trees());
        });
    });
}

#[test]
fn dangling_local_roots_and_ancestors_are_errors_in_preview_and_execution() {
    KINDS.into_iter().for_each(|src_kind| {
        [
            "root",
            "root-slash",
            "root-dot",
            "ancestor",
            "deep-ancestor",
        ]
        .into_iter()
        .for_each(|location| {
            let fixture = Fixture::new();
            let src = fixture.endpoint(src_kind, "a", true, true);
            let mut dst = fixture.endpoint(Kind::Local, "b", false, false);
            let host = fixture.0.join("trees/b");
            if location.starts_with("root") {
                fs::create_dir(host.join("parent")).unwrap();
                symlink("absent", host.join("parent/root")).unwrap();
                if location == "root-slash" {
                    dst.push('/');
                } else if location == "root-dot" {
                    dst.push_str("/.");
                }
            } else {
                symlink("absent", host.join("parent")).unwrap();
                if location == "deep-ancestor" {
                    dst.push_str("/deep/root");
                }
            }
            let before = fixture.trees();
            [&["--dry-run"][..], &["--dry-run"][..], &[][..]]
                .into_iter()
                .for_each(|flags| {
                    let output = fixture.run(&src, &dst, flags, false);
                    assert!(
                        !output.status.success(),
                        "{src_kind:?}, {location}, {flags:?}: {}",
                        String::from_utf8_lossy(&output.stdout)
                    );
                    assert!(!String::from_utf8_lossy(&output.stdout).contains("mkdir root"));
                    assert_eq!(before, fixture.trees());
                });
        });
    });
}

#[test]
fn valid_local_root_and_ancestor_symlinks_remain_supported() {
    KINDS.into_iter().for_each(|src_kind| {
        [false, true].into_iter().for_each(|ancestor| {
            let fixture = Fixture::new();
            let src = fixture.endpoint(src_kind, "a", true, true);
            let dst = fixture.endpoint(Kind::Local, "b", false, false);
            let host = fixture.0.join("trees/b");
            fs::create_dir(host.join("target")).unwrap();
            if ancestor {
                // The root is genuinely missing beneath a valid directory link.
                symlink("target", host.join("parent")).unwrap();
            } else {
                fs::create_dir(host.join("parent")).unwrap();
                symlink("../target", host.join("parent/root")).unwrap();
            }
            let before = fixture.trees();
            let plan = success(&fixture.run(&src, &dst, &["--dry-run"], false));
            assert_eq!(plan.contains("mkdir root"), ancestor, "{plan}");
            assert!(plan.contains("nested/book"), "{plan}");
            assert_eq!(before, fixture.trees());
            success(&fixture.run(&src, &dst, &[], false));
            fixture.assert_nested_copy(Kind::Local, "b");
            let after = fixture.trees();
            let plan = success(&fixture.run(&src, &dst, &["--dry-run"], false));
            assert!(!plan.contains("mkdir root"), "{plan}");
            assert_eq!(after, fixture.trees());
        });
    });
}

#[test]
fn failed_root_creation_is_reported_and_retryable() {
    [Kind::Ssh, Kind::Tablet].into_iter().for_each(|dst_kind| {
        let fixture = Fixture::new();
        let src = fixture.endpoint(Kind::Local, "a", true, true);
        let dst = fixture.endpoint(dst_kind, "b", false, false);
        let before = fixture.trees();
        let output = fixture.run(&src, &dst, &[], true);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("injected write failure"));
        assert_eq!(before, fixture.trees());
        success(&fixture.run(&src, &dst, &[], false));
        fixture.assert_nested_copy(dst_kind, "b");
    });
}
