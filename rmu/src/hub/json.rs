//! `--json` output: serialisable mirrors of the library's reports. The
//! field names are a stable interface for scripts; add fields, never
//! rename or remove them. Documents are listed by id (`rmu ls --json`
//! maps ids to names); names appear only where the report carries them
//! (conflict copies).

use std::collections::BTreeMap;

use libremarkable_utils::Error;
use libremarkable_utils::git::Oid;
use libremarkable_utils::hub::cycle::{
    BackupReport, CycleError, DocChange, DryRunReport, MainUpdate, OpenConflict, ReadSummary,
    Reconciled, StatusReport, SyncReport,
};
use libremarkable_utils::hub::device::{LockOwner, LockState, StaleReason};
use libremarkable_utils::hub::merge::{DeletionGuard, Fork, Note, NoteKind, Side, Summary};
use libremarkable_utils::hub::model::{DeviceId, DeviceName, DocId, RepoId};
use serde::Serialize;
use serde_json::Value;

use super::{Failure, Report};

/// The JSON for a report.
pub fn report(report: &Report) -> Value {
    let value = match report {
        Report::Init { origin, report } => serde_json::to_value(InitJson {
            command: "init",
            origin,
            repo_id: &report.config.repo_id,
            format: report.config.format,
            mirror_rules: report.config.mirror_rules,
            commit: &report.commit,
        }),
        Report::Clone(report) => serde_json::to_value(SyncJson::new("clone", report)),
        Report::Sync(report) => serde_json::to_value(SyncJson::new("sync", report)),
        Report::DryRun(report) => serde_json::to_value(DryRunJson::new(report)),
        Report::Backup(report) => serde_json::to_value(BackupJson::new(report)),
        Report::Status(report) => serde_json::to_value(StatusJson::new("status", report)),
    };
    value.expect("reports serialise")
}

/// The JSON for an error: `{"error": {"kind", "exit_code", "message", …}}`,
/// with the documents for the deletion guard.
pub fn error(error: &Error, failure: Failure) -> Value {
    let details = match error {
        Error::HubCycle(CycleError::DeletionsNeedConsent {
            from_origin,
            from_tablet,
            empty_snapshot,
        }) => Some(GuardRefusal {
            from_origin,
            from_tablet,
            empty_snapshot: *empty_snapshot,
        }),
        _ => None,
    };
    serde_json::to_value(ErrorJson {
        error: ErrorBody {
            kind: failure.kind(),
            exit_code: failure.code(),
            message: error.to_string(),
            details,
        },
    })
    .expect("errors serialise")
}

#[derive(Serialize)]
struct ErrorJson<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    kind: &'static str,
    exit_code: u8,
    message: String,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    details: Option<GuardRefusal<'a>>,
}

#[derive(Serialize)]
struct GuardRefusal<'a> {
    from_origin: &'a [DocId],
    from_tablet: &'a [DocId],
    empty_snapshot: bool,
}

#[derive(Serialize)]
struct InitJson<'a> {
    command: &'static str,
    origin: &'a str,
    repo_id: &'a RepoId,
    format: u32,
    mirror_rules: u32,
    commit: &'a Oid,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum UpdateJson<'a> {
    UpToDate,
    FastForward { commit: &'a Oid },
    Merge { commit: &'a Oid },
}

impl<'a> From<&'a MainUpdate> for UpdateJson<'a> {
    fn from(update: &'a MainUpdate) -> Self {
        match update {
            MainUpdate::UpToDate => Self::UpToDate,
            MainUpdate::FastForward(commit) => Self::FastForward { commit },
            MainUpdate::Merge(commit) => Self::Merge { commit },
        }
    }
}

#[derive(Serialize)]
struct ReadJson {
    downloaded_files: usize,
    downloaded_bytes: u64,
    unmirrored_files: usize,
}

impl From<&ReadSummary> for ReadJson {
    fn from(read: &ReadSummary) -> Self {
        Self {
            downloaded_files: read.downloaded.0,
            downloaded_bytes: read.downloaded.1,
            unmirrored_files: read.unmirrored,
        }
    }
}

#[derive(Serialize)]
struct QuarantinedJson<'a> {
    doc: &'a DocId,
    reason: &'a str,
}

fn quarantined(docs: &BTreeMap<DocId, String>) -> Vec<QuarantinedJson<'_>> {
    docs.iter()
        .map(|(doc, reason)| QuarantinedJson { doc, reason })
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SideJson {
    Tablet,
    Origin,
}

impl From<&Side> for SideJson {
    fn from(side: &Side) -> Self {
        match side {
            Side::Ours => Self::Tablet,
            Side::Theirs => Self::Origin,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum NoteKindJson<'a> {
    MetadataConflict { field: &'a str },
    KeptOverDeletion { deleted_by: SideJson },
    Forked { copy: &'a DocId },
    ForkAlreadyPresent { copy: &'a DocId },
    Opaque,
    RestoredFromTrash { parent: &'a str },
    Resurrected,
    OrphanToRoot { missing_parent: &'a str },
    ParentNotFolder { parent: &'a str },
    CycleBroken { parent: &'a str },
}

impl<'a> From<&'a NoteKind> for NoteKindJson<'a> {
    fn from(kind: &'a NoteKind) -> Self {
        match kind {
            NoteKind::MetadataConflict { field } => Self::MetadataConflict { field },
            NoteKind::KeptOverDeletion { deleted_by } => Self::KeptOverDeletion {
                deleted_by: deleted_by.into(),
            },
            NoteKind::Forked { copy } => Self::Forked { copy },
            NoteKind::ForkAlreadyPresent { copy } => Self::ForkAlreadyPresent { copy },
            NoteKind::Opaque => Self::Opaque,
            NoteKind::RestoredFromTrash { parent } => Self::RestoredFromTrash { parent },
            NoteKind::Resurrected => Self::Resurrected,
            NoteKind::OrphanToRoot { missing_parent } => Self::OrphanToRoot { missing_parent },
            NoteKind::ParentNotFolder { parent } => Self::ParentNotFolder { parent },
            NoteKind::CycleBroken { parent } => Self::CycleBroken { parent },
        }
    }
}

#[derive(Serialize)]
struct NoteJson<'a> {
    doc: &'a DocId,
    #[serde(flatten)]
    kind: NoteKindJson<'a>,
    message: String,
}

fn notes(notes: &[Note]) -> Vec<NoteJson<'_>> {
    notes
        .iter()
        .map(|note| NoteJson {
            doc: &note.doc,
            kind: (&note.kind).into(),
            message: note.to_string(),
        })
        .collect()
}

#[derive(Serialize)]
struct ForkJson<'a> {
    original: &'a DocId,
    copy: &'a DocId,
    name: &'a str,
}

fn forks(forks: &[Fork]) -> Vec<ForkJson<'_>> {
    forks
        .iter()
        .map(|fork| ForkJson {
            original: &fork.original,
            copy: &fork.copy,
            name: &fork.name,
        })
        .collect()
}

#[derive(Serialize)]
struct DeletionsJson<'a> {
    from_origin: Vec<&'a DocId>,
    from_tablet: Vec<&'a DocId>,
    exempt: Vec<&'a DocId>,
    empty_snapshot: bool,
    needs_consent: bool,
    allowed: bool,
}

impl<'a> From<&'a DeletionGuard> for DeletionsJson<'a> {
    fn from(guard: &'a DeletionGuard) -> Self {
        Self {
            from_origin: guard.from_origin.iter().collect(),
            from_tablet: guard.from_tablet.iter().collect(),
            exempt: guard.exempt.iter().collect(),
            empty_snapshot: guard.empty_snapshot,
            needs_consent: guard.needs_consent,
            allowed: guard.allowed,
        }
    }
}

#[derive(Serialize)]
struct SummaryJson {
    unchanged: usize,
    changed_on_tablet: usize,
    changed_on_origin: usize,
    identical: usize,
    added: usize,
    deleted: usize,
    kept: usize,
    merged: usize,
    forked: usize,
    resurrected: usize,
}

impl From<&Summary> for SummaryJson {
    fn from(summary: &Summary) -> Self {
        Self {
            unchanged: summary.unchanged,
            changed_on_tablet: summary.changed_ours,
            changed_on_origin: summary.changed_theirs,
            identical: summary.identical,
            added: summary.added,
            deleted: summary.deleted,
            kept: summary.kept,
            merged: summary.merged,
            forked: summary.forked,
            resurrected: summary.resurrected,
        }
    }
}

#[derive(Serialize)]
struct ReconciledJson<'a> {
    dropped: bool,
    finished: &'a [DocId],
    kept: &'a [DocId],
}

impl<'a> From<&'a Reconciled> for ReconciledJson<'a> {
    fn from(reconciled: &'a Reconciled) -> Self {
        Self {
            dropped: reconciled.dropped,
            finished: &reconciled.finished,
            kept: &reconciled.kept,
        }
    }
}

/// A lock owner, without the run and boot ids (internal to the lock).
#[derive(Serialize)]
struct OwnerJson<'a> {
    host: &'a str,
    pid: u32,
    started_unix: u64,
}

impl<'a> From<&'a LockOwner> for OwnerJson<'a> {
    fn from(owner: &'a LockOwner) -> Self {
        Self {
            host: &owner.host,
            pid: owner.pid,
            started_unix: owner.started_unix,
        }
    }
}

#[derive(Serialize)]
struct TookOverJson<'a> {
    reason: &'static str,
    previous_owner: Option<OwnerJson<'a>>,
}

pub(super) fn stale_reason(reason: StaleReason) -> &'static str {
    match reason {
        StaleReason::BootChanged => "boot_changed",
        StaleReason::WatchdogFired => "watchdog_fired",
        StaleReason::Broken => "broken",
    }
}

fn took_over(took_over: &Option<(Option<LockOwner>, StaleReason)>) -> Option<TookOverJson<'_>> {
    took_over.as_ref().map(|(owner, reason)| TookOverJson {
        reason: stale_reason(*reason),
        previous_owner: owner.as_ref().map(OwnerJson::from),
    })
}

#[derive(Serialize)]
struct SyncJson<'a> {
    command: &'static str,
    update: UpdateJson<'a>,
    snapshot: &'a Oid,
    base: &'a Oid,
    applied: &'a [DocId],
    skipped: &'a [DocId],
    quarantined: Vec<QuarantinedJson<'a>>,
    orphans: &'a [DocId],
    read: ReadJson,
    notes: Vec<NoteJson<'a>>,
    forks: Vec<ForkJson<'a>>,
    deletions: DeletionsJson<'a>,
    summary: SummaryJson,
    reconciled: Option<ReconciledJson<'a>>,
    push_attempts: usize,
    took_over: Option<TookOverJson<'a>>,
    warnings: &'a [String],
}

impl<'a> SyncJson<'a> {
    fn new(command: &'static str, report: &'a SyncReport) -> Self {
        Self {
            command,
            update: (&report.update).into(),
            snapshot: &report.snapshot,
            base: &report.base,
            applied: &report.applied,
            skipped: &report.skipped,
            quarantined: quarantined(&report.quarantined),
            orphans: &report.orphans,
            read: (&report.read).into(),
            notes: notes(&report.notes),
            forks: forks(&report.forks),
            deletions: (&report.deletions).into(),
            summary: (&report.summary).into(),
            reconciled: report.reconciled.as_ref().map(ReconciledJson::from),
            push_attempts: report.push_attempts,
            took_over: took_over(&report.took_over),
            warnings: &report.warnings,
        }
    }
}

#[derive(Serialize)]
struct BackupJson<'a> {
    command: &'static str,
    snapshot: &'a Oid,
    pushed: bool,
    quarantined: Vec<QuarantinedJson<'a>>,
    orphans: &'a [DocId],
    read: ReadJson,
    took_over: Option<TookOverJson<'a>>,
    warnings: &'a [String],
}

impl<'a> BackupJson<'a> {
    fn new(report: &'a BackupReport) -> Self {
        Self {
            command: "backup",
            snapshot: &report.snapshot,
            pushed: report.pushed,
            quarantined: quarantined(&report.quarantined),
            orphans: &report.orphans,
            read: (&report.read).into(),
            took_over: took_over(&report.took_over),
            warnings: &report.warnings,
        }
    }
}

#[derive(Serialize)]
struct LockJson<'a> {
    held: bool,
    owner: Option<OwnerJson<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ChangeJson {
    Added,
    Modified,
    Deleted,
}

#[derive(Serialize)]
struct DocChangeJson<'a> {
    doc: &'a DocId,
    change: ChangeJson,
}

#[derive(Serialize)]
struct OpenConflictJson<'a> {
    original: &'a DocId,
    copy: &'a DocId,
    name: &'a str,
}

impl<'a> From<&'a OpenConflict> for OpenConflictJson<'a> {
    fn from(conflict: &'a OpenConflict) -> Self {
        Self {
            original: &conflict.original,
            copy: &conflict.copy,
            name: &conflict.name,
        }
    }
}

#[derive(Serialize)]
struct StatusJson<'a> {
    command: &'static str,
    repo_id: &'a RepoId,
    device_id: &'a DeviceId,
    device_name: &'a DeviceName,
    origin: &'a str,
    lock: LockJson<'a>,
    journal_pending: bool,
    quarantined: Vec<QuarantinedJson<'a>>,
    orphans: &'a [DocId],
    read: ReadJson,
    changes: Vec<DocChangeJson<'a>>,
    behind: usize,
    open_conflicts: Vec<OpenConflictJson<'a>>,
    deletions: DeletionsJson<'a>,
    summary: SummaryJson,
    notes: Vec<NoteJson<'a>>,
    forks: Vec<ForkJson<'a>>,
}

impl<'a> StatusJson<'a> {
    fn new(command: &'static str, report: &'a StatusReport) -> Self {
        Self {
            command,
            repo_id: &report.repo_id,
            device_id: &report.device_id,
            device_name: &report.device_name,
            origin: &report.origin,
            lock: match &report.lock {
                LockState::Free => LockJson {
                    held: false,
                    owner: None,
                },
                LockState::Held(owner) => LockJson {
                    held: true,
                    owner: owner.as_ref().map(OwnerJson::from),
                },
            },
            journal_pending: report.journal_pending,
            quarantined: quarantined(&report.quarantined),
            orphans: &report.orphans,
            read: (&report.read).into(),
            changes: report
                .changes
                .iter()
                .map(|(doc, change)| DocChangeJson {
                    doc,
                    change: match change {
                        DocChange::Added => ChangeJson::Added,
                        DocChange::Modified => ChangeJson::Modified,
                        DocChange::Deleted => ChangeJson::Deleted,
                    },
                })
                .collect(),
            behind: report.behind,
            open_conflicts: report.open_conflicts.iter().map(Into::into).collect(),
            deletions: (&report.guard).into(),
            summary: (&report.summary).into(),
            notes: notes(&report.notes),
            forks: forks(&report.forks),
        }
    }
}

#[derive(Serialize)]
struct DryRunJson<'a> {
    #[serde(flatten)]
    status: StatusJson<'a>,
    writes: &'a [DocId],
    deletes: &'a [DocId],
    would_push: bool,
}

impl<'a> DryRunJson<'a> {
    fn new(report: &'a DryRunReport) -> Self {
        Self {
            status: StatusJson::new("sync --dry-run", &report.status),
            writes: &report.writes,
            deletes: &report.deletes,
            would_push: report.would_push,
        }
    }
}
