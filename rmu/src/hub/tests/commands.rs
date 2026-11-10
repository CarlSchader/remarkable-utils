//! The commands against the library's fake tablet and a local bare
//! origin (hermetic git).

use super::*;
use libremarkable_utils::hub::model::{Journal, TabletState};

/// A local origin, fake tablets and a cache, hermetic.
struct World {
    scratch: Scratch,
    git: Git,
}

impl World {
    fn new() -> Self {
        let scratch = Scratch::new("hub-cli");
        let home = scratch.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let git = [
            ("GIT_CONFIG_GLOBAL", PathBuf::from("/dev/null")),
            ("GIT_CONFIG_NOSYSTEM", PathBuf::from("1")),
            ("GIT_CONFIG_COUNT", PathBuf::from("0")),
            ("GIT_DEFAULT_HASH", PathBuf::from("sha1")),
            ("HOME", home.clone()),
            ("XDG_CONFIG_HOME", home),
        ]
        .into_iter()
        .fold(Git::new(), |git, (key, value)| git.env(key, value));
        Self { scratch, git }
    }

    fn origin(&self) -> String {
        self.scratch
            .join("origin.git")
            .to_string_lossy()
            .into_owned()
    }

    fn tablet(
        &self,
        name: &str,
        configure: impl FnOnce(FakeTabletBuilder) -> FakeTabletBuilder,
    ) -> FakeTablet {
        let builder =
            FakeTablet::builder(self.scratch.join(name)).device_info("reMarkable Fake", "3.28.0");
        configure(builder).build().unwrap()
    }

    /// Run `args` (after `rmu hub`) on `tablet`; the result as `finish`
    /// writes it, and the exit code.
    fn run(&self, tablet: &FakeTablet, args: &[&str], output: Output) -> (String, String, u8) {
        static PROGRESS: NoProgress = NoProgress;
        let args = hub_args(args);
        let hub = Hub::new(
            self.git.clone(),
            self.scratch.join("cache"),
            LockRequest::for_this_process("test-host"),
            &PROGRESS,
        )
        .with_options(HubOptions {
            date: Some(DATE.to_string()),
            ..options(&args.command)
        });
        let result = execute(&hub, Some(tablet), &args.command);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let cache = self.scratch.join("cache");
        let context = OutputContext {
            tablet: Some(tablet),
            cache_dir: Some(&cache),
            output,
            quiet: false,
        };
        let code = finish(result, &context, &mut stdout, &mut stderr)
            .map_or_else(|error| exit_code(&error), |()| 0);
        (
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
            code,
        )
    }

    fn json(&self, tablet: &FakeTablet, args: &[&str]) -> (Value, u8) {
        let (stdout, _, code) = self.run(tablet, args, Output::Json);
        (serde_json::from_str(&stdout).unwrap(), code)
    }
}

use libremarkable_utils::hub::device::FakeTabletBuilder;

/// A PDF on the tablet: metadata, content and payload.
fn with_pdf(builder: FakeTabletBuilder, n: u64, name: &str) -> FakeTabletBuilder {
    let id = doc(n);
    let metadata = json!({
        "createdTime": "1767225600000",
        "lastModified": "1767312000000",
        "parent": "",
        "pinned": false,
        "type": "DocumentType",
        "visibleName": name,
    });
    builder
        .file(format!("{id}.metadata"), metadata.to_string())
        .file(
            format!("{id}.content"),
            libremarkable_utils::xochitl::document_content_json("pdf"),
        )
        .file(format!("{id}.pdf"), format!("%PDF-1.4 fabricated {n}\n"))
}

fn ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn init_clone_sync_status_backup() {
    let world = World::new();
    let origin = world.origin();
    let a = world.tablet("a", |b| {
        with_pdf(with_pdf(b, 1, "Paper one"), 2, "Paper two")
    });

    let (value, code) = world.json(&a, &["init", &origin]);
    assert_eq!(code, 0);
    assert_eq!(value["command"], "init");
    assert!(Path::new(&origin).join("HEAD").exists());

    let (value, code) = world.json(&a, &["clone", &origin, "--name", "Tablet A"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["command"], "clone");
    assert_eq!(value["summary"]["added"], 2);

    let b = world.tablet("b", |b| b);
    let (stdout, stderr, code) =
        world.run(&b, &["clone", &origin, "--name", "Tablet B"], Output::Human);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.starts_with("Cloned:"), "{stdout}");
    assert!(
        stdout.contains("Updated on the tablet (2):\n  \"Paper one\""),
        "{stdout}"
    );
    assert!(b.read_file(&format!("{}.pdf", doc(1))).is_some());

    // A new document on A reaches B.
    let id = doc(3);
    a.write_file(
        &format!("{id}.metadata"),
        json!({"parent": "", "type": "DocumentType", "visibleName": "Paper three"})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    a.write_file(
        &format!("{id}.content"),
        libremarkable_utils::xochitl::document_content_json("pdf").as_bytes(),
    )
    .unwrap();
    a.write_file(&format!("{id}.pdf"), b"%PDF-1.4 fabricated 3\n")
        .unwrap();

    let (stdout, _, code) = world.run(&a, &["status"], Output::Plain);
    assert_eq!(code, 0);
    assert_eq!(stdout, format!("A {id}\n"));
    let (stdout, _, code) = world.run(&a, &["status"], Output::Human);
    assert_eq!(code, 0);
    assert!(
        stdout.contains(&format!("added    \"Paper three\" ({id})")),
        "{stdout}"
    );

    let (value, code) = world.json(&a, &["sync"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["command"], "sync");

    let (value, code) = world.json(&b, &["sync", "--dry-run"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(ids(&value["writes"]), vec![id.to_string()]);
    assert_eq!(value["status"]["behind"], 1);
    assert!(
        b.read_file(&format!("{id}.pdf")).is_none(),
        "dry run wrote nothing"
    );

    let (stdout, stderr, code) = world.run(&b, &["sync"], Output::Plain);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.trim().parse::<Oid>().is_ok(),
        "the commit B is at: {stdout}"
    );
    assert!(
        stderr.starts_with("Synced:"),
        "the summary goes to stderr: {stderr}"
    );
    assert!(
        stderr.contains("Updated on the tablet (1):\n  \"Paper three\""),
        "{stderr}"
    );
    assert!(b.read_file(&format!("{id}.pdf")).is_some());
    let (value, code) = world.json(&b, &["sync"]);
    assert_eq!(code, 0);
    assert_eq!(value["update"], json!({"kind": "up_to_date"}));
    assert_eq!(value["base"], stdout.trim());

    let (value, code) = world.json(&b, &["backup"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["command"], "backup");
    let (stdout, _, code) = world.run(&b, &["backup"], Output::Human);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Origin already had this backup"),
        "{stdout}"
    );

    let (value, code) = world.json(&b, &["status"]);
    assert_eq!(code, 0);
    assert_eq!(value["device_name"], "Tablet B");
    assert_eq!(value["changes"], json!({}));
    assert_eq!(value["behind"], 0);
}

#[test]
fn the_deletion_guard_exits_3_and_lists_the_documents() {
    let world = World::new();
    let origin = world.origin();
    let a = world.tablet("a", |b| {
        (1..=12).fold(b, |b, n| with_pdf(b, n, &format!("Paper {n}")))
    });
    assert_eq!(world.json(&a, &["init", &origin]).1, 0);
    assert_eq!(world.json(&a, &["clone", &origin]).1, 0);
    (1..=12).for_each(|n| {
        ["metadata", "content", "pdf"]
            .iter()
            .for_each(|ext| a.remove_file(&format!("{}.{ext}", doc(n))).unwrap());
    });

    let (stdout, stderr, code) = world.run(&a, &["sync"], Output::Json);
    assert_eq!(code, 3, "{stderr}");
    let value: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["error"]["kind"], "deletions_need_consent");
    assert_eq!(value["error"]["from_origin"].as_array().unwrap().len(), 12);
    // The tablet no longer has them: their names come from history.
    assert_eq!(value["error"]["names"][doc(3).to_string()], "Paper 3");
    assert!(
        stderr.contains(&format!("  \"Paper 3\" ({})", doc(3))),
        "{stderr}"
    );
    assert_eq!(value["error"]["empty_snapshot"], true);
    assert!(
        stderr.contains("Would delete from origin (12):"),
        "{stderr}"
    );
    assert!(stderr.contains("rerun with --allow-deletions"), "{stderr}");

    let (value, code) = world.json(&a, &["sync", "--dry-run"]);
    assert_eq!(code, 0);
    assert_eq!(value["status"]["guard"]["needs_consent"], true);

    let (value, code) = world.json(&a, &["sync", "--allow-deletions"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(
        value["deletions"]["from_origin"].as_array().unwrap().len(),
        12
    );
}

#[test]
fn a_held_lock_exits_4() {
    let world = World::new();
    let origin = world.origin();
    let a = world.tablet("a", |b| with_pdf(b, 1, "Paper"));
    assert_eq!(world.json(&a, &["init", &origin]).1, 0);
    assert_eq!(world.json(&a, &["clone", &origin]).1, 0);
    a.acquire_lock(&LockRequest::for_this_process("other-computer"), false)
        .unwrap();

    let (stdout, stderr, code) = world.run(&a, &["sync"], Output::Json);
    assert_eq!(code, 4, "{stderr}");
    let value: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["error"]["kind"], "lock_held");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("on other-computer, started ") && message.contains(" s ago"),
        "an age, not a unix time: {message}"
    );

    let (value, code) = world.json(&a, &["status"]);
    assert_eq!(code, 0, "status reads without the lock");
    assert_eq!(value["lock"]["state"], "held");
    assert_eq!(value["lock"]["owner"]["host"], "other-computer");

    assert_eq!(world.json(&a, &["sync", "--break-lock"]).1, 0);
}

#[test]
fn a_replaced_origin_exits_5() {
    let world = World::new();
    let origin = world.origin();
    let other = world
        .scratch
        .join("other.git")
        .to_string_lossy()
        .into_owned();
    let a = world.tablet("a", |b| with_pdf(b, 1, "Paper"));
    assert_eq!(world.json(&a, &["init", &origin]).1, 0);
    assert_eq!(world.json(&a, &["init", &other]).1, 0);
    assert_eq!(world.json(&a, &["clone", &origin]).1, 0);

    let (value, code) = world.json(&a, &["sync", "--origin", &other]);
    assert_eq!(code, 5);
    assert_eq!(value["error"]["kind"], "origin_mismatch");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--reattach")
    );

    let (_, code) = world.json(&a, &["clone", &other, "--reattach"]);
    assert_eq!(code, 0);
    assert_eq!(world.json(&a, &["sync"]).1, 0);
}

#[test]
fn a_cloud_paired_tablet_exits_6() {
    let world = World::new();
    let origin = world.origin();
    let a = world.tablet("a", |b| with_pdf(b, 1, "Paper").cloud_paired(true));
    assert_eq!(world.json(&a, &["init", &origin]).1, 0);
    let (value, code) = world.json(&a, &["clone", &origin]);
    assert_eq!(code, 6);
    assert_eq!(value["error"]["kind"], "cloud_paired");
    assert!(a.is_running(), "xochitl is back");
}

#[test]
fn other_errors_exit_1() {
    let world = World::new();
    let a = world.tablet("a", |b| b);
    let (stdout, stderr, code) = world.run(&a, &["status"], Output::Human);
    assert_eq!(code, 1);
    assert!(
        stdout.is_empty() && stderr.is_empty(),
        "main prints the error"
    );
    let (value, code) = world.json(&a, &["sync"]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["kind"], "error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rmu hub clone")
    );
}

#[test]
fn remote_set_url_and_detach() {
    let world = World::new();
    let origin = world.origin();
    let a = world.tablet("a", |b| with_pdf(b, 1, "Paper"));
    assert_eq!(world.json(&a, &["init", &origin]).1, 0);
    assert_eq!(
        world.json(&a, &["clone", &origin, "--name", "Tablet A"]).1,
        0
    );

    let (value, code) = world.json(&a, &["remote"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["origin"], origin.as_str());
    assert_eq!(value["effective"], origin.as_str());
    assert_eq!(value["device_name"], "Tablet A");
    let (value, _) = world.json(&a, &["remote", "--origin", "/elsewhere.git"]);
    assert_eq!(value["origin"], origin.as_str());
    assert_eq!(value["effective"], "/elsewhere.git");

    // A copy of origin (moved to another path) is accepted.
    let moved = world.scratch.join("moved.git").display().to_string();
    let copied = std::process::Command::new("cp")
        .args(["-R", &origin, &moved])
        .status()
        .unwrap();
    assert!(copied.success());
    let (stdout, stderr, code) = world.run(&a, &["remote", "set-url", &moved], Output::Plain);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, format!("{moved}\n"));
    assert!(
        stderr.contains(&format!("changed from {origin} to {moved}")),
        "{stderr}"
    );
    assert_eq!(world.json(&a, &["sync"]).1, 0);

    // Another repository is refused before anything is stored.
    let other = world.scratch.join("other.git").display().to_string();
    assert_eq!(world.json(&a, &["init", &other]).1, 0);
    let (value, code) = world.json(&a, &["remote", "set-url", &other]);
    assert_eq!(code, 5, "{value}");
    assert_eq!(world.json(&a, &["remote"]).0["origin"], moved.as_str());

    let (value, code) = world.json(&a, &["detach", "--prune-ref"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["command"], "detach");
    assert!(
        value["pruned"].as_str().unwrap().contains("rmu/devices/"),
        "{value}"
    );
    assert!(
        a.read_file(&format!("{}.pdf", doc(1))).is_some(),
        "documents stay"
    );
    let (value, code) = world.json(&a, &["status"]);
    assert_eq!(code, 1);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not attached")
    );
    let (value, code) = world.json(&a, &["clone", &moved]);
    assert_eq!(code, 0, "{value}");
}

#[test]
fn names_are_looked_up_on_the_tablet() {
    let world = World::new();
    let a = world.tablet("a", |b| {
        with_pdf(b, 1, "Paper one").file(format!("{}.metadata", doc(2)), "not json")
    });
    let names = lookup_names(&a, &[doc(1), doc(2), doc(3), doc(1)]);
    assert_eq!(names, BTreeMap::from([(doc(1), "Paper one".to_string())]));
    assert!(lookup_names(&a, &[]).is_empty());
}

/// Delete a PDF's files on a fake tablet, as the user would.
fn delete_pdf(tablet: &FakeTablet, n: u64) {
    ["metadata", "content", "pdf"]
        .iter()
        .for_each(|ext| tablet.remove_file(&format!("{}.{ext}", doc(n))).unwrap());
}

#[test]
fn deletions_from_the_tablet_are_named() {
    let world = World::new();
    let origin = world.origin();
    let a = world.tablet("a", |b| {
        (1..=3).fold(b, |b, n| with_pdf(b, n, &format!("Paper {n}")))
    });
    let b = world.tablet("b", |b| b);
    assert_eq!(world.json(&a, &["init", &origin]).1, 0);
    assert_eq!(world.json(&a, &["clone", &origin]).1, 0);
    assert_eq!(world.json(&b, &["clone", &origin]).1, 0);
    delete_pdf(&a, 1);
    delete_pdf(&a, 2);
    let (value, code) = world.json(&a, &["sync"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["names"][doc(1).to_string()], "Paper 1");

    // B loses them in its sync; the summary still names them.
    let (value, code) = world.json(&b, &["sync", "--dry-run"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["status"]["names"][doc(2).to_string()], "Paper 2");
    let (stdout, stderr, code) = world.run(&b, &["sync"], Output::Human);
    assert_eq!(code, 0, "{stderr}");
    assert!(b.read_file(&format!("{}.pdf", doc(1))).is_none());
    assert!(
        stdout.contains(&format!(
            "Deleted from the tablet (2):\n  \"Paper 1\" ({})\n  \"Paper 2\" ({})",
            doc(1),
            doc(2)
        )),
        "{stdout}"
    );
}

/// Give a fake tablet's hub state a journal, as an interrupted sync
/// leaves it.
fn leave_journal(tablet: &FakeTablet, merge: &str) {
    let path = tablet.paths().state_file();
    let mut state = TabletState::parse(&std::fs::read(&path).unwrap()).unwrap();
    state.journal = Some(Journal {
        merge: merge.parse().unwrap(),
        snapshot: state.device_ref.clone().expect("synced once"),
    });
    std::fs::write(&path, state.to_json()).unwrap();
}

#[test]
fn detach_refuses_a_pending_journal_without_force() {
    let world = World::new();
    let origin = world.origin();
    let a = world.tablet("a", |b| with_pdf(b, 1, "Paper"));
    assert_eq!(world.json(&a, &["init", &origin]).1, 0);
    let (value, _) = world.json(&a, &["clone", &origin]);
    leave_journal(&a, value["base"].as_str().unwrap());

    let (value, code) = world.json(&a, &["detach"]);
    assert_eq!(code, 7, "{value}");
    assert_eq!(value["error"]["kind"], "journal_pending");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("rmu hub sync") && message.contains("--force"),
        "{message}"
    );
    assert_eq!(world.json(&a, &["status"]).1, 0, "still attached");

    let (stdout, stderr, code) = world.run(&a, &["detach", "--force"], Output::Plain);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("warning: the journal of an interrupted sync was discarded"),
        "{stderr}"
    );
    assert_eq!(world.json(&a, &["status"]).1, 1, "detached");
}

#[test]
fn set_url_refuses_an_origin_without_the_journaled_merge() {
    let world = World::new();
    let origin = world.origin();
    let a = world.tablet("a", |b| with_pdf(b, 1, "Paper one"));
    let b = world.tablet("b", |b| b);
    assert_eq!(world.json(&a, &["init", &origin]).1, 0);
    assert_eq!(world.json(&a, &["clone", &origin]).1, 0);
    assert_eq!(world.json(&b, &["clone", &origin]).1, 0);
    // A copy of origin made now, then origin moves on.
    let moved = world.scratch.join("moved.git").display().to_string();
    let copied = std::process::Command::new("cp")
        .args(["-R", &origin, &moved])
        .status()
        .unwrap();
    assert!(copied.success());
    let id = doc(2);
    b.write_file(
        &format!("{id}.metadata"),
        json!({"parent": "", "type": "DocumentType", "visibleName": "Paper two"})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    b.write_file(
        &format!("{id}.content"),
        libremarkable_utils::xochitl::document_content_json("pdf").as_bytes(),
    )
    .unwrap();
    b.write_file(&format!("{id}.pdf"), b"%PDF-1.4 fabricated 2\n")
        .unwrap();
    let (value, code) = world.json(&b, &["sync"]);
    assert_eq!(code, 0, "{value}");
    // A was interrupted applying that merge, which the copy lacks.
    leave_journal(&a, value["base"].as_str().unwrap());

    let (value, code) = world.json(&a, &["remote", "set-url", &moved]);
    assert_eq!(code, 7, "{value}");
    assert_eq!(value["error"]["kind"], "journal_pending");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("run `rmu hub sync` against the current origin first"),
        "{value}"
    );
    assert_eq!(world.json(&a, &["remote"]).0["origin"], origin.as_str());
}
