//! Tests of the `rmu hub` command layer: argument parsing, exit codes,
//! rendering of fabricated reports, and the commands against the
//! library's fake tablet and a local bare origin (hermetic git). Every
//! id, name and document is fabricated.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Parser;
use libremarkable_utils::git::Oid;
use libremarkable_utils::hub::cycle::{
    BackupReport, DetachReport, DocChange, Downloaded, DryRunReport, MainUpdate, OpenConflict,
    ReadSummary, Reconciled, RemoteReport, SetUrlReport, StatusReport, SyncReport, TakeOver,
    Warning,
};
use libremarkable_utils::hub::device::{FakeTablet, LockOwner, LockState, StaleReason};
use libremarkable_utils::hub::merge::{DeletionGuard, Fork, Note, NoteKind, Side, Summary};
use libremarkable_utils::hub::model::{DeviceId, RepoConfig, RepoId};
use libremarkable_utils::progress::NoProgress;
use serde_json::{Value, json};
use test_support::Scratch;

use super::*;

const DATE: &str = "2026-11-09";

fn doc(n: u64) -> DocId {
    DocId::new(format!("8c1f0b9e-0000-4000-8000-{n:012x}")).unwrap()
}

fn oid(c: char) -> Oid {
    c.to_string().repeat(40).parse().unwrap()
}

fn hub_args(args: &[&str]) -> HubArgs {
    let argv = ["rmu", "hub"].iter().chain(args).copied();
    match crate::Cli::try_parse_from(argv).unwrap().command {
        crate::Command::Hub(args) => args,
        _ => unreachable!(),
    }
}

fn parse_error(args: &[&str]) -> clap::error::ErrorKind {
    let argv = ["rmu", "hub"].iter().chain(args).copied();
    match crate::Cli::try_parse_from(argv) {
        Ok(_) => panic!("{args:?} parsed"),
        Err(error) => error.kind(),
    }
}

// ---- arguments ------------------------------------------------------------

#[test]
fn parses_every_subcommand() {
    let init = hub_args(&["init", "/srv/library.git"]);
    assert!(!init.json);
    assert!(matches!(&init.command, HubCommand::Init { origin } if origin == "/srv/library.git"));

    let clone = hub_args(&[
        "clone",
        "ssh://git@example.invalid/library.git",
        "--name",
        "Desk tablet",
        "--reattach",
        "--allow-deletions",
        "--break-lock",
        "--watchdog",
        "120",
        "--json",
    ]);
    assert!(clone.json);
    match &clone.command {
        HubCommand::Clone {
            origin,
            name,
            reattach,
            allow_deletions,
            run,
        } => {
            assert_eq!(origin, "ssh://git@example.invalid/library.git");
            assert_eq!(name.as_ref().unwrap().as_str(), "Desk tablet");
            assert!(*reattach && *allow_deletions);
            assert_eq!(
                run,
                &RunArgs {
                    break_lock: true,
                    watchdog: 120
                }
            );
        }
        other => panic!("{other:?}"),
    }

    // `--json` is accepted before the subcommand too.
    let sync = hub_args(&[
        "--json",
        "sync",
        "--allow-deletions",
        "--origin",
        "laptop:lib.git",
    ]);
    assert!(sync.json);
    match &sync.command {
        HubCommand::Sync {
            dry_run,
            allow_deletions,
            run,
            origin,
        } => {
            assert!(!dry_run);
            assert!(allow_deletions);
            assert_eq!(run, &RunArgs::default());
            assert_eq!(origin.origin.as_deref(), Some("laptop:lib.git"));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        hub_args(&["sync", "--dry-run"]).command,
        HubCommand::Sync { dry_run: true, .. }
    ));
    assert!(matches!(
        hub_args(&["backup", "--break-lock"]).command,
        HubCommand::Backup {
            run: RunArgs {
                break_lock: true,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        hub_args(&["remote"]).command,
        HubCommand::Remote {
            action: None,
            origin: OriginArg { origin: None }
        }
    ));
    match hub_args(&["remote", "set-url", "laptop:lib.git", "--break-lock"]).command {
        HubCommand::Remote {
            action: Some(RemoteAction::SetUrl { url, run }),
            ..
        } => {
            assert_eq!(url, "laptop:lib.git");
            assert!(run.break_lock);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        hub_args(&["detach", "--prune-ref", "--origin", "/x.git"]).command,
        HubCommand::Detach {
            prune_ref: true,
            origin: OriginArg { origin: Some(_) },
            ..
        }
    ));
    assert!(matches!(
        hub_args(&["status", "--origin", "/x.git"]).command,
        HubCommand::Status {
            origin: OriginArg { origin: Some(_) }
        }
    ));
}

#[test]
fn rejects_bad_arguments() {
    use clap::error::ErrorKind;
    assert_eq!(
        parse_error(&["sync", "--dry-run", "--break-lock"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse_error(&["clone", "/x.git", "--name", "a<b"]),
        ErrorKind::ValueValidation
    );
    assert_eq!(
        parse_error(&["clone", "/x.git", "--name", ""]),
        ErrorKind::ValueValidation
    );
    assert_eq!(
        parse_error(&["sync", "--watchdog", "5"]),
        ErrorKind::ValueValidation
    );
    assert_eq!(
        parse_error(&["status", "--break-lock"]),
        ErrorKind::UnknownArgument
    );
    assert_eq!(parse_error(&["init"]), ErrorKind::MissingRequiredArgument);
    assert_eq!(
        parse_error(&["remote", "set-url"]),
        ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
        parse_error(&["remote", "--origin", "/a.git", "set-url", "/b.git"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse_error(&[]),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}

#[test]
fn local_origins_become_absolute() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        absolute_origin("library.git"),
        cwd.join("library.git").display().to_string()
    );
    assert_eq!(absolute_origin("/srv/library.git"), "/srv/library.git");
    [
        "ssh://git@example.invalid/library.git",
        "server.example:library.git",
        "https://example.invalid/library.git",
    ]
    .iter()
    .for_each(|url| assert_eq!(absolute_origin(url), *url));
    let clone = options(&hub_args(&["clone", "/o.git", "--allow-deletions"]).command);
    assert!(clone.allow_deletions);
    assert_eq!(
        options(&hub_args(&["status", "--origin", "rel.git"]).command).origin,
        Some(cwd.join("rel.git").display().to_string())
    );
}

#[test]
fn options_follow_the_flags() {
    let sync = options(
        &hub_args(&[
            "sync",
            "--allow-deletions",
            "--break-lock",
            "--watchdog",
            "90",
            "--origin",
            "/o.git",
        ])
        .command,
    );
    assert!(sync.allow_deletions && sync.break_lock);
    assert_eq!(sync.watchdog, Duration::from_secs(90));
    assert_eq!(sync.origin.as_deref(), Some("/o.git"));

    let status = options(&hub_args(&["status"]).command);
    assert_eq!(status, HubOptions::default());
    assert_eq!(
        options(&hub_args(&["clone", "/o.git"]).command).watchdog,
        HubOptions::default().watchdog
    );
}

// ---- exit codes -----------------------------------------------------------

#[test]
fn exit_codes() {
    let code = |error: Error| exit_code(&anyhow::Error::from(error));
    let consent = || {
        Error::HubCycle(CycleError::DeletionsNeedConsent {
            from_origin: vec![doc(1)],
            from_tablet: Vec::new(),
            empty_snapshot: false,
        })
    };
    assert_eq!(code(consent()), 3);
    assert_eq!(
        code(Error::HubDevice(DeviceError::Locked("pid 1 on h".into()))),
        4
    );
    assert_eq!(
        code(Error::HubCycle(CycleError::OriginRewound {
            base: oid('a')
        })),
        5
    );
    assert_eq!(
        code(Error::HubCycle(CycleError::RepoMismatch {
            tablet: RepoId::new("0f0e0d0c-0000-4000-8000-0000000000a1").unwrap(),
            origin: RepoId::new("0f0e0d0c-0000-4000-8000-0000000000a2").unwrap(),
        })),
        5
    );
    assert_eq!(
        code(Error::HubCycle(CycleError::OriginEmpty {
            url: "/o".into()
        })),
        5
    );
    assert_eq!(
        code(Error::HubCycle(CycleError::DeviceRefConflict {
            reference: "refs/rmu/devices/x".into(),
            recorded: "a".into(),
            remote: "b".into(),
        })),
        5
    );
    assert_eq!(code(Error::HubCycle(CycleError::CloudPaired)), 6);
    assert_eq!(code(Error::HubCycle(CycleError::JournalPending)), 7);
    assert!(
        CycleError::JournalPending
            .to_string()
            .contains("rmu hub sync")
    );
    assert_eq!(code(Error::HubCycle(CycleError::NotAttached)), 1);
    assert_eq!(code(Error::HubDevice(DeviceError::Busy)), 4);
    assert_eq!(code(Error::HubDevice(DeviceError::LockContended)), 4);
    assert_eq!(code(Error::HubDevice(DeviceError::LockLost)), 1);
    assert_eq!(code(Error::PathNotFound("x".into())), 1);
    assert_eq!(exit_code(&anyhow::anyhow!("not a library error")), 1);
    // Context does not hide the cause.
    assert_eq!(
        exit_code(&anyhow::Error::from(consent()).context("while syncing")),
        3
    );
}

// ---- rendering --------------------------------------------------------------

fn owner() -> LockOwner {
    LockOwner {
        run: "run-1".into(),
        host: "laptop".into(),
        pid: 4242,
        started_unix: 1_790_000_000,
        boot_id: "0f0e0d0c-0000-4000-8000-0000000000b1".into(),
    }
}

fn guard() -> DeletionGuard {
    DeletionGuard {
        from_origin: BTreeSet::from([doc(20)]),
        from_tablet: BTreeSet::from([doc(21), doc(22)]),
        exempt: BTreeSet::from([doc(23)]),
        empty_snapshot: false,
        needs_consent: true,
        allowed: true,
    }
}

fn sync_report() -> SyncReport {
    SyncReport {
        update: MainUpdate::Merge(oid('c')),
        snapshot: oid('b'),
        base: oid('c'),
        inferred: BTreeMap::new(),
        applied: vec![doc(1), doc(2), doc(3), doc(21), doc(23)],
        skipped: vec![doc(4), doc(22)],
        quarantined: BTreeMap::from([(doc(5), "unparsable .metadata".to_string())]),
        orphans: vec![doc(6)],
        read: ReadSummary {
            downloaded: Downloaded {
                files: 7,
                bytes: 2048,
            },
            unmirrored: 3,
        },
        notes: vec![
            Note {
                doc: doc(7),
                kind: NoteKind::KeptOverDeletion {
                    deleted_by: Side::Theirs,
                },
            },
            Note {
                doc: doc(8),
                kind: NoteKind::RestoredFromTrash {
                    parent: String::new(),
                },
            },
        ],
        forks: vec![Fork {
            original: doc(9),
            copy: doc(10),
            name: "Lecture notes (conflict Desk tablet 2026-11-09)".into(),
        }],
        deletions: guard(),
        summary: Summary {
            unchanged: 40,
            changed_ours: 2,
            changed_theirs: 3,
            forked: 1,
            ..Summary::default()
        },
        reconciled: Some(Reconciled {
            dropped: false,
            finished: vec![doc(11)],
            kept: vec![doc(12)],
        }),
        push_attempts: 2,
        took_over: Some(TakeOver {
            previous: Some(owner()),
            reason: StaleReason::BootChanged,
        }),
        warnings: vec![Warning::MixedModel {
            seen: "reMarkable 2".into(),
            model: "reMarkable Paper Pro".into(),
        }],
    }
}

fn status_report() -> StatusReport {
    StatusReport {
        repo_id: RepoId::new("0f0e0d0c-0000-4000-8000-0000000000a1").unwrap(),
        device_id: DeviceId::new("0f0e0d0c-0000-4000-8000-0000000000c1").unwrap(),
        device_name: DeviceName::new("Desk tablet").unwrap(),
        origin: "ssh://git@example.invalid/library.git".into(),
        lock: LockState::Held(Some(owner())),
        journal_pending: true,
        quarantined: BTreeMap::from([(doc(5), "zero-length page".to_string())]),
        orphans: Vec::new(),
        read: ReadSummary::default(),
        changes: BTreeMap::from([
            (doc(1), DocChange::Added),
            (doc(2), DocChange::Modified),
            (doc(3), DocChange::Deleted),
        ]),
        behind: 4,
        open_conflicts: vec![OpenConflict {
            original: doc(9),
            copy: doc(10),
            name: "Lecture notes (conflict Desk tablet 2026-11-09)".into(),
        }],
        guard: DeletionGuard {
            allowed: false,
            ..guard()
        },
        summary: Summary {
            added: 1,
            ..Summary::default()
        },
        notes: vec![Note {
            doc: doc(2),
            kind: NoteKind::MetadataConflict {
                field: "visibleName".into(),
            },
        }],
        forks: Vec::new(),
    }
}

fn names() -> render::Names {
    BTreeMap::from([
        (doc(4), "Edited meanwhile".to_string()),
        (doc(7), "Kept book".to_string()),
        (doc(9), "Lecture notes".to_string()),
        (doc(21), "Old draft".to_string()),
    ])
}

fn text(lines: &[String]) -> String {
    lines.join("\n")
}

#[test]
fn renders_sync_reports() {
    let report = Report::Sync(sync_report());
    let human = text(&render::human(&report, &names()));
    [
        "Synced: merged with origin and pushed cccccccccccc (after 2 push attempts)",
        "The tablet is at cccccccccccc (its backup: bbbbbbbbbbbb)",
        "Updated 5 documents on the tablet",
        "Read 7 files (2.00 KiB) from the tablet; 3 files stay not mirrored",
        "Documents: 2 changed on the tablet, 3 changed on origin, 1 forked",
        "Deletions: 1 from origin, 2 from the tablet",
        "Conflict copies (1):\n  \"Lecture notes (conflict Desk tablet 2026-11-09)\" (8c1f0b9e-0000-4000-8000-00000000000a), the tablet's version of \"Lecture notes\" (8c1f0b9e-0000-4000-8000-000000000009)",
        "Skipped: changed on the tablet during the sync, or would break its folder tree (the next sync picks them up) (2):\n  \"Edited meanwhile\" (8c1f0b9e-0000-4000-8000-000000000004)",
        "Quarantined, malformed on the tablet (left untouched until it parses again) (1):\n  8c1f0b9e-0000-4000-8000-000000000005: unparsable .metadata",
        "Files without a .metadata",
        "  \"Kept book\" (8c1f0b9e-0000-4000-8000-000000000007): kept over a deletion: deleted on origin but changed on the other side",
        "  8c1f0b9e-0000-4000-8000-000000000008: restored from the trash to the root, so an edit is not lost",
        "Deleted from the tablet (1):\n  \"Old draft\" (8c1f0b9e-0000-4000-8000-000000000015)\n",
        "1 document also deleted from the tablet: origin deleted them with recorded consent",
        "These deletions are allowed by --allow-deletions",
        "Reconciled an interrupted sync: finished 1 document, 1 left for the next sync",
        "Took over the tablet's lock (pid 4242 on laptop, started at unix time 1790000000): the tablet rebooted since it was taken",
        "warning: this repository has only seen reMarkable 2 tablets",
    ]
    .iter()
    .for_each(|expected| assert!(human.contains(expected), "{expected:?} missing from:\n{human}"));

    let Report::Sync(sync) = &report else {
        unreachable!()
    };
    let clone = text(&render::human(
        &Report::Clone(sync.clone()),
        &BTreeMap::new(),
    ));
    assert!(clone.starts_with("Cloned: merged with origin"), "{clone}");

    let up_to_date = SyncReport {
        update: MainUpdate::UpToDate,
        applied: Vec::new(),
        push_attempts: 0,
        ..sync.clone()
    };
    let human = text(&render::human(&Report::Sync(up_to_date), &BTreeMap::new()));
    assert!(human.contains("Synced: already in sync with origin; nothing pushed"));
    assert!(human.contains("Nothing to change on the tablet"));
    assert!(human.contains("Deletions: 1 from origin, 0 from the tablet"));
    assert!(
        !human.contains("Deleted from the tablet"),
        "nothing was applied"
    );

    assert_eq!(render::plain(&report), vec!["c".repeat(40)]);
    let warnings = render::warnings(&report);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with("warning: this repository has only seen reMarkable 2 tablets"),
        "{warnings:?}"
    );
    // Names are looked up only for the listed documents.
    let named = render::named_docs(&report);
    assert!(named.contains(&doc(4)) && named.contains(&doc(9)) && named.contains(&doc(21)));
    assert!(
        !named.contains(&doc(1)),
        "applied documents are only counted"
    );
}

/// The top-level fields of a JSON object, sorted.
fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[test]
fn sync_json_is_stable() {
    let value = json::report(&Report::Sync(sync_report()));
    assert_eq!(
        keys(&value),
        vec![
            "applied",
            "base",
            "command",
            "deletions",
            "forks",
            "inferred",
            "notes",
            "orphans",
            "push_attempts",
            "quarantined",
            "read",
            "reconciled",
            "skipped",
            "snapshot",
            "summary",
            "took_over",
            "update",
            "warnings",
        ]
    );
    assert_eq!(value["command"], "sync");
    assert_eq!(
        value["update"],
        json!({"kind": "merge", "commit": "c".repeat(40)})
    );
    assert_eq!(value["base"], "c".repeat(40));
    assert_eq!(value["snapshot"], "b".repeat(40));
    assert_eq!(value["inferred"], json!({}));
    assert_eq!(value["applied"].as_array().unwrap().len(), 5);
    assert_eq!(
        value["skipped"],
        json!([doc(4).to_string(), doc(22).to_string()])
    );
    assert_eq!(
        value["quarantined"],
        json!({doc(5).to_string(): "unparsable .metadata"})
    );
    assert_eq!(value["orphans"], json!([doc(6).to_string()]));
    assert_eq!(
        value["read"],
        json!({"downloaded": {"files": 7, "bytes": 2048}, "unmirrored": 3})
    );
    assert_eq!(
        value["notes"][0],
        json!({"doc": doc(7).to_string(), "kind": "kept_over_deletion", "deleted_by": "theirs"})
    );
    assert_eq!(value["notes"][1]["kind"], "restored_from_trash");
    assert_eq!(value["notes"][1]["parent"], "");
    assert_eq!(
        value["forks"],
        json!([{
            "original": doc(9).to_string(),
            "copy": doc(10).to_string(),
            "name": "Lecture notes (conflict Desk tablet 2026-11-09)",
        }])
    );
    assert_eq!(
        value["deletions"],
        json!({
            "from_origin": [doc(20).to_string()],
            "from_tablet": [doc(21).to_string(), doc(22).to_string()],
            "exempt": [doc(23).to_string()],
            "empty_snapshot": false,
            "needs_consent": true,
            "allowed": true,
        })
    );
    assert_eq!(value["summary"]["unchanged"], 40);
    assert_eq!(value["summary"]["changed_ours"], 2);
    assert_eq!(value["summary"]["changed_theirs"], 3);
    assert_eq!(
        value["reconciled"],
        json!({"dropped": false, "finished": [doc(11).to_string()], "kept": [doc(12).to_string()]})
    );
    assert_eq!(value["push_attempts"], 2);
    assert_eq!(value["took_over"]["reason"], "boot_changed");
    assert_eq!(value["took_over"]["previous"]["host"], "laptop");
    assert_eq!(value["took_over"]["previous"]["pid"], 4242);
    assert_eq!(
        value["warnings"],
        json!([{"kind": "mixed_model", "seen": "reMarkable 2", "model": "reMarkable Paper Pro"}])
    );

    let fast_forward = SyncReport {
        update: MainUpdate::FastForward(oid('d')),
        inferred: BTreeMap::from([(doc(1), oid('a'))]),
        reconciled: None,
        took_over: None,
        ..sync_report()
    };
    let value = json::report(&Report::Clone(fast_forward));
    assert_eq!(value["command"], "clone");
    assert_eq!(
        value["update"],
        json!({"kind": "fast_forward", "commit": "d".repeat(40)})
    );
    assert_eq!(
        value["inferred"],
        json!({doc(1).to_string(): "a".repeat(40)})
    );
    assert_eq!(value["reconciled"], Value::Null);
    assert_eq!(value["took_over"], Value::Null);
    [
        (MainUpdate::UpToDate, "up_to_date"),
        (MainUpdate::Pulled, "pulled"),
    ]
    .into_iter()
    .for_each(|(update, kind)| {
        let report = SyncReport {
            update,
            ..sync_report()
        };
        assert_eq!(
            json::report(&Report::Sync(report))["update"],
            json!({"kind": kind})
        );
    });
}

#[test]
fn renders_status_and_dry_run_reports() {
    let names = BTreeMap::from([(doc(1), "New notebook".to_string())]);
    let report = Report::Status(status_report());
    let human = text(&render::human(&report, &names));
    [
        "Tablet \"Desk tablet\" (0f0e0d0c-0000-4000-8000-0000000000c1)",
        "Origin ssh://git@example.invalid/library.git (repository 0f0e0d0c-0000-4000-8000-0000000000a1)",
        "Lock: held by pid 4242 on laptop",
        "An interrupted sync left a journal; the next sync reconciles it",
        "3 changes on the tablet since its last sync",
        "  added    \"New notebook\" (8c1f0b9e-0000-4000-8000-000000000001)",
        "  modified 8c1f0b9e-0000-4000-8000-000000000002",
        "  deleted  8c1f0b9e-0000-4000-8000-000000000003",
        "Origin has 4 commits the tablet has not seen",
        "Open conflict copies (rename or delete a copy to close it) (1):",
        "Quarantined, malformed on the tablet",
        "A sync would merge: Documents: 1 added",
        "\"visibleName\" changed on both sides; kept origin's value",
        "Would delete from origin (1):",
        "Would delete from the tablet (2):",
        "1 document also would be deleted from the tablet",
        "A sync would refuse these deletions without --allow-deletions",
        "(read while xochitl runs: best effort)",
    ]
    .iter()
    .for_each(|expected| assert!(human.contains(expected), "{expected:?} missing from:\n{human}"));
    assert_eq!(
        render::plain(&report),
        vec![
            format!("A {}", doc(1)),
            format!("M {}", doc(2)),
            format!("D {}", doc(3)),
        ]
    );

    let value = json::report(&report);
    assert_eq!(
        keys(&value),
        vec![
            "behind",
            "changes",
            "command",
            "device_id",
            "device_name",
            "forks",
            "guard",
            "journal_pending",
            "lock",
            "notes",
            "open_conflicts",
            "origin",
            "orphans",
            "quarantined",
            "read",
            "repo_id",
            "summary",
        ]
    );
    assert_eq!(value["command"], "status");
    assert_eq!(value["device_name"], "Desk tablet");
    assert_eq!(value["lock"]["state"], "held");
    assert_eq!(value["lock"]["owner"]["host"], "laptop");
    assert_eq!(value["journal_pending"], true);
    assert_eq!(
        value["changes"],
        json!({
            doc(1).to_string(): "added",
            doc(2).to_string(): "modified",
            doc(3).to_string(): "deleted",
        })
    );
    assert_eq!(value["behind"], 4);
    assert_eq!(value["open_conflicts"][0]["copy"], doc(10).to_string());
    assert_eq!(value["guard"]["allowed"], false);
    assert_eq!(value["notes"][0]["kind"], "metadata_conflict");
    assert_eq!(value["notes"][0]["field"], "visibleName");

    let clean = StatusReport {
        lock: LockState::Free,
        journal_pending: false,
        quarantined: BTreeMap::new(),
        changes: BTreeMap::new(),
        behind: 0,
        open_conflicts: Vec::new(),
        guard: DeletionGuard::default(),
        summary: Summary::default(),
        notes: Vec::new(),
        ..status_report()
    };
    let human = text(&render::human(
        &Report::Status(clean.clone()),
        &BTreeMap::new(),
    ));
    assert!(human.contains("Lock: free"));
    assert!(human.contains("No changes on the tablet since its last sync"));
    assert!(human.contains("The tablet has everything on origin"));
    assert!(!human.contains("Would delete"));
    assert_eq!(
        json::report(&Report::Status(clean.clone()))["lock"],
        json!({"state": "free"})
    );

    let dry_run = Report::DryRun(DryRunReport {
        status: clean,
        writes: vec![doc(30), doc(31)],
        deletes: vec![doc(32)],
        would_push: true,
    });
    let human = text(&render::human(&dry_run, &BTreeMap::new()));
    assert!(human.contains("A sync would write 2 documents to the tablet and delete 1 from it"));
    assert!(human.contains(&format!("  write   {}", doc(30))));
    assert!(human.contains(&format!("  delete  {}", doc(32))));
    assert!(human.contains("It would push to origin"));
    assert!(
        human.ends_with("(dry run: nothing was changed; read while xochitl runs: best effort)")
    );
    assert!(!human.contains("\n(read while xochitl runs"));
    assert_eq!(
        render::plain(&dry_run),
        vec![
            format!("write {}", doc(30)),
            format!("write {}", doc(31)),
            format!("delete {}", doc(32)),
        ]
    );
    let value = json::report(&dry_run);
    assert_eq!(
        keys(&value),
        vec!["command", "deletes", "status", "would_push", "writes"]
    );
    assert_eq!(value["command"], "sync --dry-run");
    assert_eq!(
        value["writes"],
        json!([doc(30).to_string(), doc(31).to_string()])
    );
    assert_eq!(value["deletes"], json!([doc(32).to_string()]));
    assert_eq!(value["would_push"], true);
    assert_eq!(value["status"]["behind"], 0);
}

#[test]
fn renders_backup_and_init_reports() {
    let backup = BackupReport {
        snapshot: oid('e'),
        pushed: true,
        quarantined: BTreeMap::new(),
        orphans: vec![doc(6)],
        read: ReadSummary {
            downloaded: Downloaded {
                files: 1,
                bytes: 10,
            },
            unmirrored: 0,
        },
        took_over: Some(TakeOver {
            previous: None,
            reason: StaleReason::Broken,
        }),
        warnings: Vec::new(),
    };
    let report = Report::Backup(backup.clone());
    let human = text(&render::human(&report, &BTreeMap::new()));
    assert!(human.contains("Backed up the tablet to origin (eeeeeeeeeeee)"));
    assert!(human.contains("Read 1 file (10 B) from the tablet"));
    assert!(!human.contains("not mirrored"));
    assert!(human.contains("Took over the tablet's lock (unknown owner): --break-lock"));
    assert_eq!(render::plain(&report), vec!["e".repeat(40)]);
    let value = json::report(&report);
    assert_eq!(value["command"], "backup");
    assert_eq!(value["pushed"], true);
    assert_eq!(
        value["took_over"],
        json!({"reason": "broken", "previous": null})
    );
    assert_eq!(
        keys(&value),
        vec![
            "command",
            "orphans",
            "pushed",
            "quarantined",
            "read",
            "snapshot",
            "took_over",
            "warnings",
        ]
    );
    let again = Report::Backup(BackupReport {
        pushed: false,
        took_over: None,
        ..backup
    });
    assert!(
        text(&render::human(&again, &BTreeMap::new())).contains("Origin already had this backup")
    );

    let repo_id = RepoId::new("0f0e0d0c-0000-4000-8000-0000000000a1").unwrap();
    let init = Report::Init {
        origin: "/srv/library.git".into(),
        report: InitReport {
            config: RepoConfig::new(repo_id.clone()),
            commit: oid('f'),
        },
    };
    let human = text(&render::human(&init, &BTreeMap::new()));
    assert!(human.contains("Initialised origin /srv/library.git"));
    assert!(human.contains("rmu hub clone /srv/library.git --name <name>"));
    assert_eq!(render::plain(&init), vec![repo_id.to_string()]);
    let value = json::report(&init);
    assert_eq!(keys(&value), vec!["command", "commit", "config", "origin"]);
    assert_eq!(value["command"], "init");
    assert_eq!(value["origin"], "/srv/library.git");
    assert_eq!(value["config"]["repo_id"], repo_id.to_string());
    assert_eq!(value["config"]["format"], RepoConfig::FORMAT);
    assert_eq!(value["commit"], "f".repeat(40));
}

#[test]
fn renders_pulled_and_inferred_syncs() {
    let report = SyncReport {
        update: MainUpdate::Pulled,
        inferred: BTreeMap::from([(doc(1), oid('a')), (doc(2), oid('a'))]),
        ..sync_report()
    };
    let human = text(&render::human(&Report::Clone(report), &BTreeMap::new()));
    assert!(
        human.starts_with("Cloned: took origin's changes; the tablet had none of its own to push"),
        "{human}"
    );
    assert!(human.contains("2 documents matched a version in origin's history"));
    let human = text(&render::human(
        &Report::Sync(sync_report()),
        &BTreeMap::new(),
    ));
    assert!(!human.contains("matched a version"), "{human}");
}

fn remote_report() -> RemoteReport {
    RemoteReport {
        repo_id: RepoId::new("0f0e0d0c-0000-4000-8000-0000000000a1").unwrap(),
        device_id: DeviceId::new("0f0e0d0c-0000-4000-8000-0000000000c1").unwrap(),
        device_name: DeviceName::new("Desk tablet").unwrap(),
        origin: "ssh://git@example.invalid/library.git".into(),
        effective: "ssh://git@example.invalid/library.git".into(),
    }
}

#[test]
fn renders_remote_set_url_and_detach_reports() {
    let report = Report::Remote(remote_report());
    let human = text(&render::human(&report, &BTreeMap::new()));
    assert!(human.contains("Tablet \"Desk tablet\" (0f0e0d0c-0000-4000-8000-0000000000c1)"));
    assert!(human.contains("Origin ssh://git@example.invalid/library.git"));
    assert!(!human.contains("This run uses"));
    assert_eq!(
        render::plain(&report),
        vec!["ssh://git@example.invalid/library.git"]
    );
    let value = json::report(&report);
    assert_eq!(
        keys(&value),
        vec![
            "command",
            "device_id",
            "device_name",
            "effective",
            "origin",
            "repo_id"
        ]
    );
    assert_eq!(value["command"], "remote");
    let overridden = Report::Remote(RemoteReport {
        effective: "laptop:library.git".into(),
        ..remote_report()
    });
    assert!(
        text(&render::human(&overridden, &BTreeMap::new()))
            .contains("This run uses laptop:library.git (--origin)")
    );

    let set_url = Report::SetUrl(SetUrlReport {
        previous: "/old/library.git".into(),
        remote: remote_report(),
        took_over: None,
        warnings: vec![Warning::ReleaseFailed {
            error: "connection lost".into(),
        }],
    });
    let human = text(&render::human(&set_url, &BTreeMap::new()));
    assert!(human.contains(
        "Origin of \"Desk tablet\" changed from /old/library.git to ssh://git@example.invalid/library.git"
    ));
    assert!(human.contains("warning: could not release the tablet lock: connection lost"));
    assert_eq!(
        render::plain(&set_url),
        vec!["ssh://git@example.invalid/library.git"]
    );
    let value = json::report(&set_url);
    assert_eq!(
        keys(&value),
        vec!["command", "previous", "remote", "took_over", "warnings"]
    );
    assert_eq!(value["command"], "remote set-url");
    assert_eq!(value["warnings"][0]["kind"], "release_failed");

    let detach = DetachReport {
        repo_id: RepoId::new("0f0e0d0c-0000-4000-8000-0000000000a1").unwrap(),
        device_id: DeviceId::new("0f0e0d0c-0000-4000-8000-0000000000c1").unwrap(),
        pruned: Some("refs/rmu/devices/0f0e0d0c-0000-4000-8000-0000000000c1".into()),
        took_over: None,
        warnings: vec![Warning::JournalDiscarded],
    };
    let report = Report::Detach(detach.clone());
    let human = text(&render::human(&report, &BTreeMap::new()));
    assert!(human.contains(
        "Detached the tablet from repository 0f0e0d0c-0000-4000-8000-0000000000a1; its documents are untouched"
    ));
    assert!(human.contains("Deleted its device ref refs/rmu/devices/"));
    assert!(human.contains("warning: the journal of an interrupted sync was discarded"));
    assert_eq!(
        render::plain(&report),
        vec!["refs/rmu/devices/0f0e0d0c-0000-4000-8000-0000000000c1"]
    );
    let value = json::report(&report);
    assert_eq!(
        keys(&value),
        vec![
            "command",
            "device_id",
            "pruned",
            "repo_id",
            "took_over",
            "warnings"
        ]
    );
    assert_eq!(value["warnings"], json!([{"kind": "journal_discarded"}]));
    let kept = Report::Detach(DetachReport {
        pruned: None,
        warnings: Vec::new(),
        ..detach
    });
    assert!(render::plain(&kept).is_empty());
    assert!(!text(&render::human(&kept, &BTreeMap::new())).contains("device ref"));
}

#[test]
fn long_lists_are_capped() {
    let report = SyncReport {
        skipped: (100..130).map(doc).collect(),
        ..sync_report()
    };
    let human = render::human(&Report::Sync(report.clone()), &BTreeMap::new());
    assert!(human.iter().any(|line| line == "  ... and 10 more"));
    let named = render::named_docs(&Report::Sync(report));
    assert_eq!(
        named.iter().filter(|d| **d >= doc(100)).count(),
        render::LIST_LIMIT
    );
}

#[test]
fn renders_errors() {
    let error = Error::HubCycle(CycleError::DeletionsNeedConsent {
        from_origin: vec![doc(1), doc(2)],
        from_tablet: vec![doc(3)],
        empty_snapshot: true,
    });
    let names = BTreeMap::from([(doc(3), "Old draft".to_string())]);
    let details = text(&render::error_details(&error, &names));
    assert!(details.contains("The tablet looks empty"));
    assert!(details.contains(&format!(
        "Would delete from origin (2):\n  {}\n  {}",
        doc(1),
        doc(2)
    )));
    assert!(details.contains("Would delete from the tablet (1):\n  \"Old draft\""));
    assert!(details.contains("--allow-deletions"));
    assert_eq!(render::error_docs(&error), vec![doc(3), doc(1), doc(2)]);

    let value = json::error(Failure::of(&error), &error.to_string(), Some(&error));
    assert_eq!(value["error"]["kind"], "deletions_need_consent");
    assert_eq!(value["error"]["exit_code"], 3);
    assert_eq!(
        value["error"]["from_origin"],
        json!([doc(1).to_string(), doc(2).to_string()])
    );
    assert_eq!(value["error"]["from_tablet"], json!([doc(3).to_string()]));
    assert_eq!(value["error"]["empty_snapshot"], true);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--allow-deletions")
    );

    let setup = json::error(Failure::Error, "ssh: connection refused", None);
    assert_eq!(
        setup,
        json!({"error": {"kind": "error", "exit_code": 1, "message": "ssh: connection refused"}})
    );

    let locked = Error::HubDevice(DeviceError::Locked("pid 1 on laptop".into()));
    assert!(render::error_details(&locked, &BTreeMap::new()).is_empty());
    let value = json::error(Failure::of(&locked), &locked.to_string(), Some(&locked));
    assert_eq!(
        value,
        json!({"error": {
            "kind": "lock_held",
            "exit_code": 4,
            "message": locked.to_string(),
        }})
    );
    assert!(locked.to_string().contains("--break-lock"));
}

// ---- the commands against the fake tablet -----------------------------------

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
        let code = finish(
            result,
            Some(tablet),
            output,
            false,
            &mut stdout,
            &mut stderr,
        )
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
        stdout.contains("Updated 2 documents on the tablet"),
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
        stderr.contains("Updated 1 document on the tablet"),
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
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["error"]["kind"],
        "lock_held"
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
