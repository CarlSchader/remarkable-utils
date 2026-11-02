//! CLI regressions for stable filesystem archive roles (review finding 14).
//! SSH endpoints run shell commands against synthetic local trees, never hosts.
#![cfg(unix)]

use std::fs::{self, File, FileTimes};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::Value;

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
enum Pair {
    Local,
    Ssh,
    SshHomeRoot,
    Mixed,
}

struct Fixture {
    root: PathBuf,
    paths: [PathBuf; 2],
    endpoints: [String; 2],
}

impl Fixture {
    fn new(pair: Pair) -> Self {
        let root = std::env::temp_dir().join(format!(
            "rmu-archive-roles-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        let paths = [root.join("a"), root.join("b")];
        paths.iter().for_each(|path| fs::create_dir(path).unwrap());
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        fs::write(
            &ssh,
            "#!/bin/sh\n\
             [ \"$#\" = 3 ] && [ \"$1\" = -- ] || exit 98\n\
             case \"$2\" in mock-a|mock-b) ;; *) exit 99 ;; esac\n\
             exec sh -c \"$3\"\n",
        )
        .unwrap();
        if matches!(pair, Pair::SshHomeRoot) {
            // Never execute the supplied shell command here: absolute paths
            // must address the synthetic root, not the machine running tests.
            fs::write(
                &ssh,
                r#"#!/bin/sh
[ "$#" = 3 ] && [ "$1" = -- ] && [ "$2" = mock-same ] || exit 98
stamp() {
    stat -c "$1" -- "$3" 2>/dev/null || stat -f "$2" "$3"
}
case "$3" in
    'mkdir -p .'|'mkdir -p /') exit 0 ;;
    'cat book.pdf') exec cat "$MOCK_HOME/book.pdf" ;;
    'cat /book.pdf') exec cat "$MOCK_ROOT/book.pdf" ;;
    'mkdir -p . && cat > book.pdf') cat > "$MOCK_HOME/book.pdf"; exit $? ;;
    "stat -c '%s %Y' -- book.pdf "*)
        stamp '%s %Y' '%z %m' "$MOCK_HOME/book.pdf"; exit $? ;;
    "stat -c '%s %Y' -- /book.pdf "*)
        stamp '%s %Y' '%z %m' "$MOCK_ROOT/book.pdf"; exit $? ;;
    'cd . || exit 9'*) cd "$MOCK_HOME" || exit 9 ;;
    'cd / || exit 9'*) cd "$MOCK_ROOT" || exit 9 ;;
    *) printf 'unexpected mock command: %s\n' "$3" >&2; exit 99 ;;
esac
if [ -f book.pdf ]; then
    stamp '%s %Y ./book.pdf' '%z %m ./book.pdf' book.pdf
fi
"#,
            )
            .unwrap();
        }
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        let local = |i: usize| paths[i].display().to_string();
        let remote = |i: usize| format!("mock-{}:{}", ["a", "b"][i], paths[i].display());
        let endpoints = match pair {
            Pair::Local => [local(0), local(1)],
            Pair::Ssh => [remote(0), remote(1)],
            Pair::SshHomeRoot => ["mock-same:".to_string(), "mock-same:/".to_string()],
            Pair::Mixed => [local(0), remote(1)],
        };
        Self {
            root,
            paths,
            endpoints,
        }
    }

    fn sync(&self, src: usize, flags: &[&str]) -> Output {
        let path = std::env::join_paths(std::iter::once(self.root.join("bin")).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_rmu"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("HOME", &self.root)
            .env("MOCK_HOME", &self.paths[0])
            .env("MOCK_ROOT", &self.paths[1])
            .env("PATH", path)
            .env_remove("RMU_SSH_PASSWORD")
            .env_remove("RMU_ASKPASS_MODE")
            .env_remove("RMU_ASKPASS_PASSWORD")
            .args(["--no-multiplex", "sync", "--remote-kind", "fs"])
            .args(flags)
            .arg(&self.endpoints[src])
            .arg(&self.endpoints[1 - src])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }

    fn write(&self, side: usize, name: &str, bytes: &[u8], seconds: u64) {
        let path = self.paths[side].join(name);
        fs::write(&path, bytes).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
            .unwrap();
    }

    fn assert_bytes(&self, side: usize, name: &str, expected: &[u8]) {
        assert_eq!(fs::read(self.paths[side].join(name)).unwrap(), expected);
    }

    fn archive(&self) -> PathBuf {
        let paths: Vec<_> = fs::read_dir(self.root.join("state/rmu"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(paths.len(), 1, "expected exactly one shared archive");
        paths[0].clone()
    }

    fn assert_noop(&self, src: usize) {
        let output = self.sync(src, &["--two-way", "--dry-run"]);
        assert!(
            output.stdout.is_empty(),
            "unexpected plan: {}",
            String::from_utf8_lossy(&output.stdout),
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn read_state(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn exercise_reversal(pair: Pair) {
    [0, 1].into_iter().for_each(|initial_src| {
        let fixture = Fixture::new(pair);
        fixture.write(initial_src, "book.pdf", b"original", 1_000_000_000);
        fixture.sync(initial_src, &[]);
        fixture.assert_bytes(1 - initial_src, "book.pdf", b"original");
        let archive = fixture.archive();
        let state = read_state(&archive);
        let entry = &state["entries"]["book.pdf"];
        assert_ne!(entry["local_mtime_ms"], entry["remote_last_modified"]);
        let source_stamp = if initial_src == 0 {
            "local_mtime_ms"
        } else {
            "remote_last_modified"
        };
        assert_eq!(entry[source_stamp], 1_000_000_000_000_i64);
        let stable_namespace = archive.to_string_lossy().ends_with(".files-v1.json");
        assert_eq!(stable_namespace, !matches!(pair, Pair::Mixed));

        // Both argument orders use the same baseline, including an actual
        // reversed run between the dry runs, not just a planner invocation.
        fixture.assert_noop(initial_src);
        fixture.assert_noop(1 - initial_src);
        let before = fs::read(&archive).unwrap();
        fixture.sync(1 - initial_src, &["--two-way"]);
        assert_eq!(fs::read(&archive).unwrap(), before);

        // One-way mode must still follow CLI source, not the sorted A role.
        let next_src = 1 - initial_src;
        fixture.write(next_src, "book.pdf", b"reversed source edit", 1_100_000_000);
        fixture.sync(next_src, &[]);
        fixture.assert_bytes(initial_src, "book.pdf", b"reversed source edit");
        fixture.assert_noop(initial_src);
        fixture.assert_noop(next_src);
        assert_eq!(fixture.archive(), archive);
    });
}

#[test]
fn local_pair_reversal_preserves_stamps_and_one_way_direction() {
    exercise_reversal(Pair::Local);
}

#[test]
fn ssh_pair_reversal_preserves_stamps_and_one_way_direction() {
    exercise_reversal(Pair::Ssh);
}

#[test]
fn same_host_home_and_root_keep_roles_on_actual_reversed_second_run() {
    [0, 1].into_iter().for_each(|src| {
        let fixture = Fixture::new(Pair::SshHomeRoot);
        fixture.write(1, "book.pdf", b"original", 1_000_000_000);
        fixture.sync(src, &["--two-way"]);
        fixture.assert_bytes(0, "book.pdf", b"original");
        let archive = fixture.archive();
        let state = read_state(&archive);
        let entry = &state["entries"]["book.pdf"];
        assert_ne!(entry["local_mtime_ms"], entry["remote_last_modified"]);
        let before = fs::read(&archive).unwrap();

        // The second invocation really executes sync, with arguments reversed.
        let output = fixture.sync(1 - src, &["--two-way"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("0 conflict(s)"), "{stderr}");
        assert_eq!(fs::read(&archive).unwrap(), before);
        fixture.assert_noop(src);
        fixture.assert_noop(1 - src);
        assert_eq!(fixture.archive(), archive);
    });
}

#[test]
fn mixed_pair_keeps_existing_archive_and_local_a_role() {
    exercise_reversal(Pair::Mixed);
}

#[test]
fn conflict_src_and_dst_follow_arguments_not_archive_roles() {
    [Pair::Local, Pair::Ssh, Pair::Mixed]
        .into_iter()
        .for_each(|pair| {
            [0, 1].into_iter().for_each(|src| {
                ["src", "dst"].into_iter().for_each(|policy| {
                    let fixture = Fixture::new(pair);
                    fixture.write(0, "book.pdf", b"baseline", 1_000_000_000);
                    fixture.sync(0, &[]);
                    fixture.write(0, "book.pdf", b"edit A", 1_100_000_000);
                    fixture.write(1, "book.pdf", b"edit B differs", 1_200_000_000);
                    let winner = if policy == "src" { src } else { 1 - src };
                    let expected = fs::read(fixture.paths[winner].join("book.pdf")).unwrap();
                    fixture.sync(src, &["--two-way", "--conflict", policy]);
                    fixture.assert_bytes(0, "book.pdf", &expected);
                    fixture.assert_bytes(1, "book.pdf", &expected);
                    fixture.assert_noop(1 - src);
                });
            });
        });
}

#[test]
fn directional_legacy_archives_are_not_reinterpreted_or_deleted() {
    [Pair::Local, Pair::Ssh].into_iter().for_each(|pair| {
        let fixture = Fixture::new(pair);
        fixture.write(1, "book.pdf", b"book", 1_000_000_000);
        fixture.write(1, "removed.pdf", b"keep me", 1_100_000_000);
        fixture.sync(1, &[]);
        let archive = fixture.archive();
        let mut state = read_state(&archive);
        // Simulate an older B -> A run: its first argument was recorded
        // as archive A, the opposite of the new stable ordering.
        state["entries"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .for_each(|entry| {
                let a_mtime = entry["local_mtime_ms"].clone();
                entry["local_mtime_ms"] = entry["remote_last_modified"].clone();
                entry["remote_last_modified"] = a_mtime;
                let a_size = entry["local_size"].clone();
                entry["local_size"] = entry["remote_size"].clone();
                entry["remote_size"] = a_size;
            });
        let old_path = archive.with_file_name(
            archive
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .replace(".files-v1.json", ".json"),
        );
        let old_bytes = serde_json::to_vec_pretty(&state).unwrap();
        fs::write(&old_path, &old_bytes).unwrap();
        fs::remove_file(&archive).unwrap();
        fs::remove_file(fixture.paths[1].join("removed.pdf")).unwrap();

        [0, 1].into_iter().for_each(|src| {
            let output = fixture.sync(src, &["--two-way", "--delete", "--dry-run"]);
            let plan = String::from_utf8(output.stdout).unwrap();
            assert!(plan.contains("no sync state to match them"), "{plan}");
            assert!(plan.contains("copy → B  removed.pdf"), "{plan}");
            assert!(!plan.contains("delete"), "{plan}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("unknown side order"));
            assert!(!archive.exists());
            assert_eq!(fs::read(&old_path).unwrap(), old_bytes);
        });
        fixture.sync(0, &["--two-way", "--delete"]);
        fixture.assert_bytes(0, "removed.pdf", b"keep me");
        fixture.assert_bytes(1, "removed.pdf", b"keep me");
        let state = read_state(&archive);
        assert!(state["entries"].get("book.pdf").is_none());
        assert!(state["entries"].get("removed.pdf").is_some());
        // An explicit conflict choice establishes the remaining baseline.
        fixture.sync(1, &["--two-way", "--conflict", "src"]);
        fixture.assert_noop(0);
        fixture.assert_noop(1);
        assert_eq!(fs::read(&old_path).unwrap(), old_bytes);
    });
}
