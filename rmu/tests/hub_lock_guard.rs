//! The other `rmu` commands and the hub lock, over a mock `ssh`: the
//! "tablet" is a scratch directory; the mock runs every remote command
//! locally after rewriting the hub's fixed paths (`/home/root/…`, the boot
//! id) into it. Writes refuse while a live hub lock is held, a stale one
//! does not count, reads are unaffected, and `rmu sync` warns before
//! writing to a hub-attached tablet. Messages are pinned. Every name and
//! id is fabricated.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use test_support::{Scratch, assert_success};

const MOCK_SSH: &str = r#"while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
[ "$#" -ge 3 ] || { echo "mock ssh: unexpected arguments" >&2; exit 90; }
shift 2
dev=$(printf '%s\n' "$MOCK_DEVICE" | sed 's/[\\&#]/\\&/g')
rewrite() {
    sed -e "s#/home/root/#$dev/home/root/#g" \
        -e "s#/proc/sys/kernel/random/boot_id#$dev/boot_id#g"
}
command=$(printf '%s\n' "$1" | rewrite)
printf '%s\n----\n' "$command" >> "$MOCK_DEVICE/ssh.log"
# A hub run taking the lock right before the restart (after the writes).
case "$command" in
*'systemctl reset-failed'*)
    if [ -n "$MOCK_LOCK_BEFORE_RESTART" ]; then
        mkdir -p "$MOCK_DEVICE/home/root/.local/share/rmu/lock"
        printf '%s' "$MOCK_LOCK_BEFORE_RESTART" > "$MOCK_DEVICE/home/root/.local/share/rmu/lock/owner"
    fi ;;
esac
if [ "$command" = "sh -s" ]; then
    rewrite | sh -s
    exit $?
fi
exec sh -c "$command"
"#;

const BOOT_ID: &str = "0f0e0d0c-0000-4000-8000-0000000000b1";
const DOC: &str = "8c1f0b9e-0000-4000-8000-000000000001";

struct Tablet {
    scratch: Scratch,
}

impl Tablet {
    fn new() -> Self {
        let scratch = Scratch::new("hub-lock-guard");
        fs::create_dir_all(scratch.join("xochitl")).unwrap();
        fs::create_dir_all(scratch.join("device/home/root/.local/share/rmu")).unwrap();
        fs::write(scratch.join("device/boot_id"), format!("{BOOT_ID}\n")).unwrap();
        scratch.mock_command("ssh", MOCK_SSH);
        scratch.mock_command(
            "systemctl",
            "printf '%s\\n' \"$*\" >> \"$MOCK_DEVICE/systemctl.log\"\n",
        );
        let tablet = Self { scratch };
        tablet.put_document(DOC, "Paper one");
        tablet
    }

    fn state_dir(&self) -> PathBuf {
        self.scratch.join("device/home/root/.local/share/rmu")
    }

    fn put_document(&self, id: &str, name: &str) {
        let dir = self.scratch.join("xochitl");
        fs::write(
            dir.join(format!("{id}.metadata")),
            format!(
                "{{\"parent\": \"\", \"type\": \"DocumentType\", \"visibleName\": \"{name}\"}}"
            ),
        )
        .unwrap();
        fs::write(dir.join(format!("{id}.content")), "{\"fileType\": \"pdf\"}").unwrap();
        fs::write(dir.join(format!("{id}.pdf")), "%PDF-1.4 fabricated\n").unwrap();
    }

    /// Another computer's `rmu hub` run holds the lock, taken five minutes
    /// ago in boot `boot`.
    fn lock(&self, boot: &str) {
        let lock = self.state_dir().join("lock");
        fs::create_dir_all(&lock).unwrap();
        fs::write(lock.join("owner"), owner_record(boot)).unwrap();
    }

    fn attach(&self) {
        fs::write(self.state_dir().join("hub.json"), "{}").unwrap();
    }

    fn rmu(&self, args: &[&str]) -> Output {
        self.rmu_env(args, &[])
    }

    fn rmu_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let xochitl = self.scratch.join("xochitl");
        Command::new(env!("CARGO_BIN_EXE_rmu"))
            .envs(env.iter().copied())
            .current_dir(self.scratch.path())
            .args(["--host", "tablet", "--no-multiplex", "--xochitl-dir"])
            .arg(&xochitl)
            .args(args)
            .env("PATH", self.scratch.mock_path())
            .env("MOCK_DEVICE", self.scratch.join("device"))
            .env("XDG_STATE_HOME", self.scratch.join("state"))
            .env("XDG_CACHE_HOME", self.scratch.join("cache"))
            .env("HOME", self.scratch.join("home"))
            .env_remove("RMU_SSH_PASSWORD")
            .output()
            .unwrap()
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.scratch.join("xochitl"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn restarted(&self) -> bool {
        fs::read_to_string(self.scratch.join("device/systemctl.log"))
            .is_ok_and(|log| log.contains("restart xochitl.service"))
    }
}

/// The owner record of a run started five minutes ago in boot `boot`.
fn owner_record(boot: &str) -> String {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 300;
    format!(
        "run=0f0e0d0c00004000800000000000d001\nhost=other-computer\npid=7\nstarted={started}\nboot_id={boot}\n"
    )
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The refusal, as `main` prints it.
const REFUSAL: &str = "Error: the tablet is busy: an rmu hub run holds its lock (pid 7 on \
     other-computer, started 5 min ago); stopping before changing anything else. Try again once \
     that run has finished; if it is gone (its computer crashed or was closed), `rmu hub sync \
     --break-lock` frees the tablet\n";

#[test]
fn writes_wait_for_a_live_hub_lock() {
    let tablet = Tablet::new();
    tablet.lock(BOOT_ID);
    let before = tablet.files();

    let output = tablet.rmu(&["mkdir", "Books"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), REFUSAL);
    let output = tablet.rmu(&["rm", "Paper one"]);
    assert_eq!(stderr(&output), REFUSAL);
    let output = tablet.rmu(&["restart"]);
    assert_eq!(stderr(&output), REFUSAL);
    assert_eq!(tablet.files(), before, "nothing written");
    assert!(!tablet.restarted(), "xochitl left alone");

    // Reads are unaffected.
    let output = tablet.rmu(&["ls"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Paper one"));
    assert_success(&tablet.rmu(&["download", "Paper one", "out.pdf"]));
}

#[test]
fn an_unreadable_owner_record_also_holds() {
    let tablet = Tablet::new();
    fs::create_dir_all(tablet.state_dir().join("lock")).unwrap();
    let output = tablet.rmu(&["mkdir", "Books"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with(
            "Error: the tablet is busy: an rmu hub run holds its lock (its owner record is \
             missing, as after a power cut while the lock was taken)"
        ),
        "{}",
        stderr(&output)
    );
}

/// A lock taken after the writes (between the listing's look and the
/// restart) still stops the restart: it always looks afresh.
#[test]
fn the_restart_looks_afresh() {
    let tablet = Tablet::new();
    let output = tablet.rmu_env(
        &["mkdir", "Books"],
        &[("MOCK_LOCK_BEFORE_RESTART", &owner_record(BOOT_ID))],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "Created folder: Books/\nError: changes were written, but restarting xochitl failed\n\nCaused by:\n    {}",
            REFUSAL.strip_prefix("Error: ").unwrap()
        )
    );
    assert_eq!(
        tablet.files().len(),
        6,
        "the folder was written before the lock"
    );
    let log = fs::read_to_string(tablet.scratch.join("device/systemctl.log")).unwrap();
    assert_eq!(log, "reset-failed xochitl.service\n", "never restarted");
}

/// The look at the hub rides along in the listing a write starts from,
/// and in the restart's `reset-failed`: no round trip of its own.
#[test]
fn the_check_costs_no_extra_round_trip() {
    let tablet = Tablet::new();
    assert_success(&tablet.rmu(&["mkdir", "Books"]));
    let log = fs::read_to_string(tablet.scratch.join("device/ssh.log")).unwrap();
    let commands: Vec<&str> = log.split("\n----\n").filter(|c| !c.is_empty()).collect();
    let probing: Vec<&&str> = commands
        .iter()
        .filter(|command| command.contains("__hub_lock__"))
        .collect();
    assert_eq!(probing.len(), 2, "{commands:#?}");
    assert!(
        probing[0].contains("*.metadata"),
        "in the listing: {}",
        probing[0]
    );
    assert!(
        probing[1].ends_with("systemctl reset-failed xochitl.service"),
        "in the restart: {}",
        probing[1]
    );
    assert!(tablet.restarted());
}

#[test]
fn a_stale_lock_does_not_count() {
    let tablet = Tablet::new();
    tablet.lock("an-earlier-boot");
    let output = tablet.rmu(&["mkdir", "Books"]);
    assert_success(&output);
    assert!(tablet.restarted());
    assert_eq!(
        tablet.files().len(),
        3 + 3,
        "a folder: metadata, content, pagedata"
    );
}

/// The warning, as `rmu sync` prints it before writing to `tablet:/Books`.
const ATTACHED: &str = "warning: tablet:/Books is attached to an rmu hub: documents rmu sync \
     copies onto it get new ids, so its next `rmu hub sync` adds them to the library as new \
     documents (duplicates of any copies already there); `rmu hub` is the way to share documents \
     between hub tablets\n";

fn local_books(tablet: &Tablet) -> String {
    let books = tablet.scratch.join("books");
    fs::create_dir_all(&books).unwrap();
    fs::write(books.join("notes.pdf"), "%PDF-1.4 fabricated local\n").unwrap();
    books.display().to_string()
}

#[test]
fn sync_warns_before_writing_to_a_hub_tablet() {
    let tablet = Tablet::new();
    tablet.attach();
    let books = local_books(&tablet);

    let output = tablet.rmu(&[
        "sync",
        "--remote-kind",
        "remarkable",
        &books,
        "tablet:/Books",
    ]);
    assert_success(&output);
    assert!(stderr(&output).starts_with(ATTACHED), "{}", stderr(&output));
    assert!(tablet.files().len() > 3, "the sync wrote");

    // Pulling from it writes nothing there: no warning.
    let pulled = tablet.scratch.join("pulled").display().to_string();
    let output = tablet.rmu(&[
        "sync",
        "--remote-kind",
        "remarkable",
        "tablet:/Books",
        &pulled,
    ]);
    assert_success(&output);
    assert!(
        !stderr(&output).contains("attached to an rmu hub"),
        "{}",
        stderr(&output)
    );

    // --quiet silences it with the rest.
    let output = tablet.rmu(&[
        "--quiet",
        "sync",
        "--remote-kind",
        "remarkable",
        "--dry-run",
        &books,
        "tablet:/Books",
    ]);
    assert_success(&output);
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
}

#[test]
fn sync_waits_for_a_live_hub_lock_but_a_dry_run_does_not() {
    let tablet = Tablet::new();
    tablet.attach();
    tablet.lock(BOOT_ID);
    let books = local_books(&tablet);
    let before = tablet.files();

    let output = tablet.rmu(&[
        "sync",
        "--remote-kind",
        "remarkable",
        &books,
        "tablet:/Books",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), format!("{ATTACHED}{REFUSAL}"));
    assert_eq!(tablet.files(), before);
    assert!(!tablet.restarted());

    let output = tablet.rmu(&[
        "sync",
        "--remote-kind",
        "remarkable",
        "--dry-run",
        &books,
        "tablet:/Books",
    ]);
    assert_success(&output);
    assert!(stderr(&output).starts_with(ATTACHED), "{}", stderr(&output));
}
