//! Tests of the `rmu hub` command layer: argument parsing, exit codes,
//! rendering of fabricated reports, and the commands against the
//! library's fake tablet and a local bare origin (hermetic git). Every
//! id, name and document is fabricated.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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

mod commands;
mod golden;
mod output;
mod parsing;

const DATE: &str = "2026-11-09";

fn doc(n: u64) -> DocId {
    DocId::new(format!("8c1f0b9e-0000-4000-8000-{n:012x}")).unwrap()
}

fn oid(c: char) -> Oid {
    c.to_string().repeat(40).parse().unwrap()
}

fn hub_args(args: &[&str]) -> HubArgs {
    let argv = ["rmu", "hub"].iter().chain(args).copied();
    match crate::parse_cli(argv).unwrap().command {
        crate::Command::Hub(args) => args,
        _ => unreachable!(),
    }
}

fn parse_error(args: &[&str]) -> clap::error::ErrorKind {
    parse_error_with(&[], args)
}

/// The error kind for `rmu <globals> hub <args>`.
fn parse_error_with(globals: &[&str], args: &[&str]) -> clap::error::ErrorKind {
    let argv = ["rmu"]
        .iter()
        .chain(globals)
        .chain(&["hub"])
        .chain(args)
        .copied();
    match crate::parse_cli(argv) {
        Ok(_) => panic!("{args:?} parsed"),
        Err(error) => error.kind(),
    }
}

// ---- fabricated reports ------------------------------------------------------

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

/// Names as the tablet read finds them after `sync_report()`: only for
/// documents still on the tablet (not the deleted `doc(21)`, `doc(23)`).
fn names() -> render::Names {
    BTreeMap::from([
        (doc(1), "Paper one".to_string()),
        (doc(4), "Edited meanwhile".to_string()),
        (doc(7), "Kept book".to_string()),
        (doc(9), "Lecture notes".to_string()),
    ])
}

fn text(lines: &[String]) -> String {
    lines.join("\n")
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

fn repo_id() -> RepoId {
    RepoId::new("0f0e0d0c-0000-4000-8000-0000000000a1").unwrap()
}

fn device_id() -> DeviceId {
    DeviceId::new("0f0e0d0c-0000-4000-8000-0000000000c1").unwrap()
}

fn dry_run_report() -> DryRunReport {
    DryRunReport {
        status: status_report(),
        writes: vec![doc(30), doc(31)],
        deletes: vec![doc(32)],
        would_push: true,
    }
}

fn backup_report() -> BackupReport {
    BackupReport {
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
    }
}

fn init_report() -> Report {
    Report::Init {
        origin: "/srv/library.git".into(),
        report: InitReport {
            config: RepoConfig::new(repo_id()),
            commit: oid('f'),
        },
    }
}

fn set_url_report() -> SetUrlReport {
    SetUrlReport {
        previous: "/old/library.git".into(),
        remote: remote_report(),
        took_over: None,
        warnings: vec![Warning::ReleaseFailed {
            error: "connection lost".into(),
        }],
    }
}

fn detach_report() -> DetachReport {
    DetachReport {
        repo_id: repo_id(),
        device_id: device_id(),
        pruned: Some("refs/rmu/devices/0f0e0d0c-0000-4000-8000-0000000000c1".into()),
        took_over: None,
        warnings: vec![Warning::JournalDiscarded],
    }
}

fn guard_refusal() -> Error {
    Error::HubCycle(CycleError::DeletionsNeedConsent {
        from_origin: vec![doc(1), doc(2)],
        from_tablet: vec![doc(3)],
        empty_snapshot: true,
    })
}
