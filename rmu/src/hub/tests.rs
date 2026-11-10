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

/// The names the library carries for `guard()`'s documents (from
/// history: the tablet no longer has them).
fn guard_names() -> BTreeMap<DocId, String> {
    BTreeMap::from([
        (doc(20), "Removed on this tablet".to_string()),
        (doc(21), "Old draft".to_string()),
        (doc(22), "Second draft".to_string()),
        (doc(23), "Cleaned up elsewhere".to_string()),
    ])
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
        names: guard_names(),
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
        names: guard_names()
            .into_iter()
            .chain([(doc(3), "Gone notebook".to_string())])
            .collect(),
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
        names: BTreeMap::from([
            (doc(1), "Deleted paper".to_string()),
            (doc(2), "Deleted notes".to_string()),
            (doc(3), "Old draft".to_string()),
        ]),
    })
}

// ---- history reports -----------------------------------------------------------

use libremarkable_utils::hub::cycle::{
    AddReport, CommitDevice, CommitKind, DocEvent, DocLogEntry, DocLogReport, ForkRecord, LogEntry,
    LogReport, Place, RestoreRecord, RestoreReport, TabletBase,
};

/// 2026-11-09 14:03:20 UTC.
const TIME: i64 = 1_794_232_800 + 3 * 60 + 20;

fn tablet_base(base: Option<Oid>) -> TabletBase {
    TabletBase {
        device_id: device_id(),
        device_name: DeviceName::new("Desk tablet").unwrap(),
        base,
        overrides: 2,
    }
}

fn root() -> Place {
    Place {
        parent: String::new(),
        path: "/".into(),
    }
}

fn projects() -> Place {
    Place {
        parent: doc(40).to_string(),
        path: "/Projects".into(),
    }
}

fn log_entry(commit: char, kind: CommitKind) -> LogEntry {
    LogEntry {
        commit: oid(commit),
        kind,
        author: "rmu".into(),
        time: TIME,
        subject: "rmu hub: init".into(),
        device: None,
        notes: Vec::new(),
        forks: Vec::new(),
        consented: Vec::new(),
        restored: Vec::new(),
        added: Vec::new(),
        changed: Vec::new(),
        deleted: Vec::new(),
        names: BTreeMap::new(),
    }
}

fn log_report() -> LogReport {
    let sync = LogEntry {
        author: "Desk tablet".into(),
        subject: "rmu hub: sync Desk tablet".into(),
        device: Some(CommitDevice {
            name: Some("Desk tablet".into()),
            id: Some(device_id().to_string()),
            model: Some("reMarkable Fake".into()),
            firmware: Some("3.28.0".into()),
        }),
        notes: vec![
            "8c1f0b9e-0000-4000-8000-000000000001: \"visibleName\" changed on both sides".into(),
        ],
        forks: vec![ForkRecord {
            original: doc(1),
            copy: doc(2),
        }],
        consented: vec![doc(3)],
        added: vec![doc(2)],
        changed: vec![doc(1)],
        deleted: vec![doc(3)],
        names: BTreeMap::from([
            (doc(1), "Notes".to_string()),
            (
                doc(2),
                "Notes (conflict Desk tablet 2026-11-09)".to_string(),
            ),
            (doc(3), "Old".to_string()),
        ]),
        ..log_entry('c', CommitKind::Sync)
    };
    let restore = LogEntry {
        subject: "rmu hub: restore \"Old (restored 2026-11-09)\"".into(),
        restored: vec![RestoreRecord {
            copy: doc(4),
            original: doc(3),
            from: oid('b'),
        }],
        added: vec![doc(4)],
        names: BTreeMap::from([(doc(4), "Old (restored 2026-11-09)".to_string())]),
        ..log_entry('d', CommitKind::Restore)
    };
    LogReport {
        repo_id: repo_id(),
        origin: "ssh://git@example.invalid/library.git".into(),
        main: oid('d'),
        tablet: Some(tablet_base(Some(oid('c')))),
        entries: vec![restore, sync, log_entry('a', CommitKind::Init)],
        more: 5,
    }
}

fn doc_log_report() -> DocLogReport {
    let entry = |commit: char, events: Vec<DocEvent>| DocLogEntry {
        commit: oid(commit),
        kind: CommitKind::Sync,
        author: "Desk tablet".into(),
        time: TIME,
        subject: "rmu hub: sync Desk tablet".into(),
        events,
        version: Some(oid('9')),
    };
    DocLogReport {
        repo_id: repo_id(),
        origin: "ssh://git@example.invalid/library.git".into(),
        main: oid('d'),
        doc: doc(1),
        name: Some("Notes".into()),
        place: Some(projects()),
        tablet: Some(tablet_base(Some(oid('c')))),
        entries: vec![
            entry(
                'd',
                vec![DocEvent::Changed, DocEvent::ForkedInto { copy: doc(2) }],
            ),
            entry(
                'c',
                vec![
                    DocEvent::Renamed {
                        from: Some("Draft".into()),
                        to: Some("Notes".into()),
                    },
                    DocEvent::Moved {
                        from: root(),
                        to: projects(),
                    },
                ],
            ),
            entry(
                'b',
                vec![DocEvent::RestoredAs {
                    copy: doc(4),
                    from: oid('a'),
                }],
            ),
            entry(
                'a',
                vec![DocEvent::Added {
                    name: Some("Draft".into()),
                    place: root(),
                }],
            ),
        ],
    }
}

fn restore_report() -> RestoreReport {
    RestoreReport {
        original: doc(3),
        copy: doc(4),
        name: "Old (restored 2026-11-09)".into(),
        place: projects(),
        at: oid('d'),
        from: oid('b'),
        commit: oid('e'),
        push_attempts: 2,
    }
}

fn add_report(pages: Option<usize>) -> AddReport {
    AddReport {
        doc: doc(5),
        name: "Report".into(),
        place: root(),
        file_type: "pdf".into(),
        bytes: 2048,
        pages,
        commit: oid('f'),
        push_attempts: 1,
    }
}
