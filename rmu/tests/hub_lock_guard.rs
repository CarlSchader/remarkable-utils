//! The other `rmu` commands and the hub lock, over a mock `ssh`: each
//! host ("tablet", "other") is a scratch directory; the mock runs every
//! remote command locally after rewriting the tablet's fixed paths
//! (`/home/root/…`, the boot id) into that host's directory. Writes
//! refuse while a live hub lock is held (also one taken between the look
//! and the write: each write's own command checks again), a stale one
//! does not count, reads are unaffected, and `rmu sync` warns before its
//! plan writes to a hub-attached tablet. Messages are pinned. Every name
//! and id is fabricated.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use test_support::{Scratch, assert_success};

/// The mock `ssh`. `MOCK_LOCK_WHEN` (a substring of a command) and
/// `MOCK_LOCK_RECORD` simulate a hub run taking the lock right before
/// that command runs (once).
const MOCK_SSH: &str = r#"while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
[ "$#" -ge 3 ] || { echo "mock ssh: unexpected arguments" >&2; exit 90; }
host=${2#*@}
shift 2
MOCK_DEVICE=$MOCK_ROOT/$host
export MOCK_DEVICE
dev=$(printf '%s\n' "$MOCK_DEVICE" | sed 's/[\\&#]/\\&/g')
rewrite() {
    sed -e "s#/home/root/#$dev/home/root/#g" \
        -e "s#/proc/sys/kernel/random/boot_id#$dev/boot_id#g"
}
command=$(printf '%s\n' "$1" | rewrite)
printf '%s\n----\n' "$command" >> "$MOCK_DEVICE/ssh.log"
lock=$MOCK_DEVICE/home/root/.local/share/rmu/lock
case "$command" in
*"${MOCK_LOCK_WHEN:-no command contains this}"*)
    if [ ! -e "$lock" ]; then
        mkdir -p "$lock"
        printf '%s' "$MOCK_LOCK_RECORD" > "$lock/owner"
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
const FOLDER: &str = "8c1f0b9e-0000-4000-8000-0000000000f1";
const XOCHITL: &str = "home/root/.local/share/remarkable/xochitl";
const STATE: &str = "home/root/.local/share/rmu";

struct Tablets {
    scratch: Scratch,
}

impl Tablets {
    /// Hosts "tablet" and "other", each with one PDF.
    fn new() -> Self {
        let scratch = Scratch::new("hub-lock-guard");
        scratch.mock_command("ssh", MOCK_SSH);
        scratch.mock_command(
            "systemctl",
            "printf '%s\\n' \"$*\" >> \"$MOCK_DEVICE/systemctl.log\"\n",
        );
        let tablets = Self { scratch };
        ["tablet", "other"].iter().for_each(|host| {
            fs::create_dir_all(tablets.xochitl(host)).unwrap();
            fs::create_dir_all(tablets.state_dir(host)).unwrap();
            fs::write(tablets.device(host).join("boot_id"), format!("{BOOT_ID}\n")).unwrap();
            tablets.put_document(host, DOC, "Paper one");
        });
        tablets
    }

    fn device(&self, host: &str) -> PathBuf {
        self.scratch.join("hosts").join(host)
    }

    fn xochitl(&self, host: &str) -> PathBuf {
        self.device(host).join(XOCHITL)
    }

    fn state_dir(&self, host: &str) -> PathBuf {
        self.device(host).join(STATE)
    }

    fn put_document(&self, host: &str, id: &str, name: &str) {
        self.put_document_in(host, id, name, "");
    }

    fn put_document_in(&self, host: &str, id: &str, name: &str, parent: &str) {
        let dir = self.xochitl(host);
        fs::write(
            dir.join(format!("{id}.metadata")),
            format!(
                "{{\"parent\": \"{parent}\", \"type\": \"DocumentType\", \"visibleName\": \"{name}\"}}"
            ),
        )
        .unwrap();
        fs::write(dir.join(format!("{id}.content")), "{\"fileType\": \"pdf\"}").unwrap();
        fs::write(dir.join(format!("{id}.pdf")), "%PDF-1.4 fabricated\n").unwrap();
    }

    fn put_folder(&self, host: &str, id: &str, name: &str) {
        let dir = self.xochitl(host);
        fs::write(
            dir.join(format!("{id}.metadata")),
            format!(
                "{{\"parent\": \"\", \"type\": \"CollectionType\", \"visibleName\": \"{name}\"}}"
            ),
        )
        .unwrap();
        fs::write(dir.join(format!("{id}.content")), "{}").unwrap();
    }

    /// Another computer's `rmu hub` run holds the lock on "tablet".
    fn lock(&self, boot: &str) {
        let lock = self.state_dir("tablet").join("lock");
        fs::create_dir_all(&lock).unwrap();
        fs::write(lock.join("owner"), owner_record(boot)).unwrap();
    }

    fn attach(&self, host: &str) {
        fs::write(self.state_dir(host).join("hub.json"), "{}").unwrap();
    }

    fn rmu(&self, args: &[&str]) -> Output {
        self.rmu_env(args, &[])
    }

    fn rmu_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rmu"))
            .current_dir(self.scratch.path())
            .args(["--host", "tablet", "--no-multiplex"])
            .args(args)
            .envs(env.iter().copied())
            .env("PATH", self.scratch.mock_path())
            .env("MOCK_ROOT", self.scratch.join("hosts"))
            .env("XDG_STATE_HOME", self.scratch.join("state"))
            .env("XDG_CACHE_HOME", self.scratch.join("cache"))
            .env("HOME", self.scratch.join("home"))
            .env_remove("RMU_SSH_PASSWORD")
            .output()
            .unwrap()
    }

    /// Every file in "tablet"'s data dir with its bytes.
    fn files(&self) -> BTreeMap<String, Vec<u8>> {
        let dir = self.xochitl("tablet");
        fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_file())
            .map(|path| {
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read(&path).unwrap(),
                )
            })
            .collect()
    }

    /// Whether xochitl was restarted on "tablet" (not just reset).
    fn restarted(&self) -> bool {
        fs::read_to_string(self.device("tablet").join("systemctl.log"))
            .is_ok_and(|log| log.contains("restart xochitl.service"))
    }

    fn local_books(&self) -> String {
        let books = self.scratch.join("books");
        fs::create_dir_all(&books).unwrap();
        fs::write(books.join("notes.pdf"), "%PDF-1.4 fabricated local\n").unwrap();
        books.display().to_string()
    }

    fn sync(&self, args: &[&str]) -> Output {
        let args: Vec<&str> = ["sync", "--remote-kind", "remarkable"]
            .iter()
            .chain(args)
            .copied()
            .collect();
        self.rmu(&args)
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
     other-computer, started 5 min ago); nothing more was written. Try again once that run has \
     finished; if it is gone (its computer crashed or was closed), `rmu hub sync --break-lock` \
     frees the tablet\n";

#[test]
fn writes_wait_for_a_live_hub_lock() {
    let tablets = Tablets::new();
    tablets.put_folder("tablet", FOLDER, "Archive");
    tablets.lock(BOOT_ID);
    let pdf = tablets.scratch.join("new.pdf");
    fs::write(&pdf, "%PDF-1.4 fabricated upload\n").unwrap();
    let before = tablets.files();

    let refused = |args: &[&str]| {
        let output = tablets.rmu(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stderr(&output), REFUSAL, "{args:?}");
    };
    refused(&["mkdir", "Books"]);
    refused(&["upload", &pdf.display().to_string()]);
    refused(&["rm", "Paper one"]);
    refused(&["mv", "Paper one", "Archive"]);
    refused(&["rename", "Paper one", "Paper two"]);
    refused(&["restart"]);
    assert_eq!(tablets.files(), before, "nothing written");
    assert!(!tablets.restarted(), "xochitl left alone");

    // Reads are unaffected.
    let output = tablets.rmu(&["ls"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Paper one"));
    assert_success(&tablets.rmu(&["download", "Paper one", "out.pdf"]));
}

#[test]
fn an_unreadable_owner_record_also_holds() {
    let tablets = Tablets::new();
    fs::create_dir_all(tablets.state_dir("tablet").join("lock")).unwrap();
    let output = tablets.rmu(&["mkdir", "Books"]);
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

#[test]
fn a_stale_lock_does_not_count() {
    let tablets = Tablets::new();
    tablets.lock("an-earlier-boot");
    let output = tablets.rmu(&["mkdir", "Books"]);
    assert_success(&output);
    assert!(tablets.restarted());
    assert_eq!(
        tablets.files().len(),
        3 + 3,
        "a folder: metadata, content, pagedata"
    );
}

/// A hub run taking the lock after the listing's look, right before a
/// write: the write's own command checks again and writes nothing.
#[test]
fn a_lock_taken_after_the_look_stops_the_write() {
    let tablets = Tablets::new();
    let before = tablets.files();
    let record = owner_record(BOOT_ID);
    let output = tablets.rmu_env(
        &["mkdir", "Books"],
        &[("MOCK_LOCK_WHEN", "mv -fT"), ("MOCK_LOCK_RECORD", &record)],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), REFUSAL);
    assert_eq!(tablets.files(), before, "nothing written");
    assert!(!tablets.restarted());

    // The same for a deletion.
    fs::remove_dir_all(tablets.state_dir("tablet").join("lock")).unwrap();
    let output = tablets.rmu_env(
        &["rm", "Paper one"],
        &[("MOCK_LOCK_WHEN", "rm -rf"), ("MOCK_LOCK_RECORD", &record)],
    );
    assert_eq!(stderr(&output), REFUSAL);
    assert_eq!(tablets.files(), before, "nothing deleted");
}

/// A lock taken after the writes, between the restart's fresh look and
/// the restart itself, still stops the restart, and that stays an error
/// (xochitl must reload what was written), even with --quiet.
#[test]
fn the_restart_checks_too() {
    let tablets = Tablets::new();
    let record = owner_record(BOOT_ID);
    let output = tablets.rmu_env(
        &["--quiet", "mkdir", "Books"],
        &[
            ("MOCK_LOCK_WHEN", "systemctl restart"),
            ("MOCK_LOCK_RECORD", &record),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), NOT_RESTARTED);
    assert_eq!(
        tablets.files().len(),
        6,
        "the folder was written before the lock"
    );
    let log = fs::read_to_string(tablets.device("tablet").join("systemctl.log")).unwrap();
    assert_eq!(log, "reset-failed xochitl.service\n", "never restarted");

    // The lock was already there at the restart's fresh look.
    fs::remove_dir_all(tablets.state_dir("tablet").join("lock")).unwrap();
    let output = tablets.rmu_env(
        &["mkdir", "Notes"],
        &[
            ("MOCK_LOCK_WHEN", "systemctl reset-failed"),
            ("MOCK_LOCK_RECORD", &record),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!("Created folder: Notes/\n{NOT_RESTARTED}")
    );
    assert!(!tablets.restarted());
}

/// A refused restart after writes, as `main` prints it.
const NOT_RESTARTED: &str = "Error: changes were written, but xochitl was not restarted: an rmu \
     hub run now holds the tablet (pid 7 on other-computer, started 5 min ago). A hub sync \
     restarts xochitl when it finishes; otherwise run `rmu restart` once the lock is free\n";

/// The look at the hub rides along in the listing a write starts from,
/// and in the restart's `reset-failed`: no round trip of its own.
#[test]
fn the_check_costs_no_extra_round_trip() {
    let tablets = Tablets::new();
    assert_success(&tablets.rmu(&["mkdir", "Books"]));
    let log = fs::read_to_string(tablets.device("tablet").join("ssh.log")).unwrap();
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
    assert!(tablets.restarted());
}

/// The warning `rmu sync` prints before writing to `tablet:/Books`.
const ATTACHED: &str = "warning: tablet:/Books is attached to an rmu hub: documents rmu sync \
     copies onto it get new ids, so its next `rmu hub sync` adds them to the library as new \
     documents (duplicates of any copies already there); `rmu hub` is the way to share documents \
     between hub tablets\n";

#[test]
fn sync_warns_when_it_writes_to_a_hub_tablet() {
    let tablets = Tablets::new();
    tablets.attach("tablet");
    let books = tablets.local_books();

    let output = tablets.sync(&[&books, "tablet:/Books"]);
    assert_success(&output);
    assert!(stderr(&output).starts_with(ATTACHED), "{}", stderr(&output));
    assert!(tablets.files().len() > 3, "the sync wrote");

    // Nothing to write: no warning.
    let output = tablets.sync(&[&books, "tablet:/Books"]);
    assert_success(&output);
    assert!(
        !stderr(&output).contains("attached to an rmu hub"),
        "{}",
        stderr(&output)
    );

    // Pulling from it writes nothing there: no warning.
    let pulled = tablets.scratch.join("pulled").display().to_string();
    let output = tablets.sync(&["tablet:/Books", &pulled]);
    assert_success(&output);
    assert!(
        !stderr(&output).contains("attached to an rmu hub"),
        "{}",
        stderr(&output)
    );

    // Deletions spread to every hub tablet: said so.
    fs::remove_file(format!("{books}/notes.pdf")).unwrap();
    let output = tablets.sync(&["--delete", "--dry-run", &books, "tablet:/Books"]);
    assert_success(&output);
    assert!(
        stderr(&output).starts_with(
            "warning: tablet:/Books is attached to an rmu hub: documents rmu sync copies onto it \
             get new ids, so its next `rmu hub sync` adds them to the library as new documents \
             (duplicates of any copies already there), and the documents it deletes there are \
             deleted from the library, on every hub tablet; `rmu hub` is the way to share \
             documents between hub tablets\n"
        ),
        "{}",
        stderr(&output)
    );

    // --quiet silences it with the rest.
    let output = tablets.rmu(&[
        "--quiet",
        "sync",
        "--remote-kind",
        "remarkable",
        "--delete",
        "--dry-run",
        &books,
        "tablet:/Books",
    ]);
    assert_success(&output);
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
}

/// Tablet to tablet: only the tablet the plan writes to is warned about
/// ("other:/Books" is side A, "tablet:/Books" side B).
#[test]
fn tablet_to_tablet_warns_about_the_written_side() {
    let tablets = Tablets::new();
    tablets.attach("other");
    ["tablet", "other"].iter().for_each(|host| {
        tablets.put_folder(host, FOLDER, "Books");
    });
    tablets.put_document_in(
        "tablet",
        "8c1f0b9e-0000-4000-8000-000000000002",
        "From tablet",
        FOLDER,
    );
    tablets.put_document_in(
        "other",
        "8c1f0b9e-0000-4000-8000-000000000003",
        "From other",
        FOLDER,
    );
    let warned = |output: &Output| -> Vec<String> {
        stderr(output)
            .lines()
            .filter_map(|line| line.strip_prefix("warning: "))
            .filter_map(|line| line.split(" is attached").next())
            .map(str::to_string)
            .collect()
    };

    // tablet -> other writes to "other" (attached).
    let output = tablets.sync(&["--dry-run", "tablet:/Books", "other:/Books"]);
    assert_success(&output);
    assert_eq!(warned(&output), vec!["other:/Books"], "{}", stderr(&output));

    // other -> tablet writes to "tablet" (not attached): no warning.
    let output = tablets.sync(&["--dry-run", "other:/Books", "tablet:/Books"]);
    assert_success(&output);
    assert!(warned(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn sync_waits_for_a_live_hub_lock_but_a_dry_run_does_not() {
    let tablets = Tablets::new();
    tablets.attach("tablet");
    tablets.lock(BOOT_ID);
    let books = tablets.local_books();
    let before = tablets.files();

    let output = tablets.sync(&[&books, "tablet:/Books"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), format!("{ATTACHED}{REFUSAL}"));
    assert_eq!(tablets.files(), before);
    assert!(!tablets.restarted());

    let output = tablets.sync(&["--dry-run", &books, "tablet:/Books"]);
    assert_success(&output);
    assert!(stderr(&output).starts_with(ATTACHED), "{}", stderr(&output));
}

/// A sync that would update an existing document's PDF refuses under the
/// lock before any byte lands: the payload write checks too.
#[test]
fn a_payload_update_waits_for_the_lock() {
    let tablets = Tablets::new();
    let books = tablets.local_books();
    assert_success(&tablets.sync(&[&books, "tablet:/Books"]));
    let synced = tablets.files();
    fs::write(
        format!("{books}/notes.pdf"),
        "%PDF-1.4 fabricated local, edited longer\n",
    )
    .unwrap();
    tablets.lock(BOOT_ID);
    let output = tablets.sync(&[&books, "tablet:/Books"]);
    assert_eq!(stderr(&output), REFUSAL);
    assert_eq!(tablets.files(), synced, "the PDF is unchanged");
}
