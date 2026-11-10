//! What the operations report: every report derives `Serialize` (the
//! CLI's `--json`); warnings are the typed [`Warning`].

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

use crate::git::Oid;
use crate::hub::device::{LockOwner, LockState, StaleReason};
use crate::hub::merge::{DeletionGuard, Fork, Note, Summary};
use crate::hub::model::{DeviceId, DeviceName, DocId, RepoConfig, RepoId};

/// What `init` created.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InitReport {
    pub config: RepoConfig,
    /// The first commit of `main`.
    pub commit: Oid,
}

/// How `main` was updated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "commit", rename_all = "snake_case")]
pub enum MainUpdate {
    /// The tablet already matched origin: nothing pushed.
    UpToDate,
    /// The tablet had no changes of its own (its documents equal their
    /// base versions): nothing pushed, origin's changes applied.
    Pulled,
    /// Origin had not moved: the snapshot became `main`.
    FastForward(Oid),
    /// A merge commit (first parent: origin `main`, second: the snapshot).
    Merge(Oid),
}

/// What reconciling an interrupted run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Reconciled {
    /// The journaled merge never reached origin: journal dropped, bases
    /// unchanged.
    pub dropped: bool,
    /// Documents whose interrupted apply was finished.
    pub finished: Vec<DocId>,
    /// Documents that keep their snapshot version as a base override.
    pub kept: Vec<DocId>,
}

/// Files read from the tablet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Downloaded {
    pub files: usize,
    pub bytes: u64,
}

/// What reading the tablet found besides documents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReadSummary {
    /// Files downloaded while xochitl was stopped (files whose stat
    /// matched a recorded one, or that the pre-read already read, are not
    /// read again).
    pub downloaded: Downloaded,
    /// Files the mirror leaves on the tablet (thumbnails, `.local`,
    /// `.tree`, unknown files, symlinks, unlistable names).
    pub unmirrored: usize,
}

/// A stale tablet lock this run took over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TakeOver {
    /// The previous owner (`None` if its record was unreadable).
    pub previous: Option<LockOwner>,
    pub reason: StaleReason,
}

/// Something the user should know about a run that otherwise succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Warning {
    /// `clone`: the repository has only seen tablets of another model.
    MixedModel { seen: String, model: String },
    /// The watchdog could not be disarmed: it will restart the (running)
    /// xochitl when it fires, which also makes the lock stale.
    DisarmFailed { error: String },
    /// The tablet lock could not be released: it goes stale only when a
    /// watchdog fires, else `--break-lock` frees it.
    ReleaseFailed { error: String },
    /// The staging directory could not be cleared (the next run clears
    /// it).
    StagingNotCleared { error: String },
    /// `git maintenance run --auto` on the local cache failed.
    MaintenanceFailed { error: String },
    /// `detach`, or `clone --reattach` to an origin that no longer has
    /// the journaled merge: the journal of an interrupted sync was
    /// discarded.
    JournalDiscarded,
    /// `detach`: the device ref on origin could not be deleted.
    DeviceRefNotPruned { error: String },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Warning::MixedModel { seen, model } => write!(
                f,
                "this repository has only seen {seen} tablets so far; {model} is untested with it (documents are merged per format, but nothing has been validated across models)"
            ),
            Warning::DisarmFailed { error } => write!(f, "could not disarm the watchdog: {error}"),
            Warning::ReleaseFailed { error } => {
                write!(f, "could not release the tablet lock: {error}")
            }
            Warning::StagingNotCleared { error } => {
                write!(f, "could not clear the staging directory: {error}")
            }
            Warning::MaintenanceFailed { error } => {
                write!(f, "cache maintenance failed: {error}")
            }
            Warning::JournalDiscarded => {
                f.write_str("the journal of an interrupted sync was discarded")
            }
            Warning::DeviceRefNotPruned { error } => {
                write!(f, "could not delete the device ref on origin: {error}")
            }
        }
    }
}

/// What one sync (or clone) did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncReport {
    pub update: MainUpdate,
    /// The snapshot commit, on the device ref (pushed at step 7, unless
    /// the device ref already held the same tree and its head is reused).
    /// For [`MainUpdate::Pulled`] and [`MainUpdate::UpToDate`] this run
    /// did not merge it into `main` (a reused head may be there from an
    /// earlier merge).
    pub snapshot: Oid,
    /// The commit the tablet now agrees with.
    pub base: Oid,
    /// `clone`: the documents whose version matched one in origin's
    /// history, with the commit taken as their base (empty for `sync`).
    pub inferred: BTreeMap<DocId, Oid>,
    /// Documents written to or deleted from the tablet.
    pub applied: Vec<DocId>,
    /// Documents not applied because they changed during the sync or
    /// would break the tablet's tree; the next sync picks them up.
    pub skipped: Vec<DocId>,
    /// Malformed documents (neither deleted nor changed), with why.
    pub quarantined: BTreeMap<DocId, String>,
    /// Files without a `.metadata` that are not rmu's own leftovers
    /// (left alone).
    pub orphans: Vec<DocId>,
    pub read: ReadSummary,
    pub notes: Vec<Note>,
    pub forks: Vec<Fork>,
    pub deletions: DeletionGuard,
    /// The visible name of every document `deletions` lists, from
    /// origin's or the tablet's history: the tablet no longer has (or is
    /// about to lose) these.
    pub names: BTreeMap<DocId, String>,
    pub summary: Summary,
    pub reconciled: Option<Reconciled>,
    pub push_attempts: usize,
    pub took_over: Option<TakeOver>,
    pub warnings: Vec<Warning>,
}

/// What `backup` did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BackupReport {
    pub snapshot: Oid,
    /// `false` if the device ref already held this snapshot.
    pub pushed: bool,
    pub quarantined: BTreeMap<DocId, String>,
    pub orphans: Vec<DocId>,
    pub read: ReadSummary,
    pub took_over: Option<TakeOver>,
    pub warnings: Vec<Warning>,
}

/// One document's change on the tablet since its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocChange {
    Added,
    Modified,
    Deleted,
}

/// A conflict copy that still has its generated name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenConflict {
    pub original: DocId,
    pub copy: DocId,
    pub name: String,
}

/// `status`: read-only, best effort (xochitl keeps running).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusReport {
    pub repo_id: RepoId,
    pub device_id: DeviceId,
    pub device_name: DeviceName,
    pub origin: String,
    pub lock: LockState,
    /// An interrupted sync left a journal; the next sync reconciles it.
    pub journal_pending: bool,
    pub quarantined: BTreeMap<DocId, String>,
    pub orphans: Vec<DocId>,
    pub read: ReadSummary,
    /// The tablet's changes since its base.
    pub changes: BTreeMap<DocId, DocChange>,
    /// Commits on origin `main` the tablet has not seen.
    pub behind: usize,
    pub open_conflicts: Vec<OpenConflict>,
    /// What a sync would decide about deletions.
    pub guard: DeletionGuard,
    /// The visible name of every document `guard` lists, of every
    /// [`DocChange::Deleted`] document, and (for `--dry-run`) of every
    /// document the sync would delete, from origin's or the tablet's
    /// history: the tablet no longer has these. [`DryRunReport`] has its
    /// names here.
    pub names: BTreeMap<DocId, String>,
    pub summary: Summary,
    pub notes: Vec<Note>,
    pub forks: Vec<Fork>,
}

/// `sync --dry-run`: the status plus the planned tablet writes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DryRunReport {
    pub status: StatusReport,
    /// Documents the sync would write on the tablet.
    pub writes: Vec<DocId>,
    /// Documents the sync would delete from the tablet (named in
    /// `status.names`).
    pub deletes: Vec<DocId>,
    /// Whether the sync would push to origin `main` (it would not when
    /// the tablet already agrees with origin, or has no changes of its
    /// own).
    pub would_push: bool,
}

/// `remote`: the tablet's hub attachment and its origin URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteReport {
    pub repo_id: RepoId,
    pub device_id: DeviceId,
    pub device_name: DeviceName,
    /// The URL stored on the tablet.
    pub origin: String,
    /// The URL this invocation uses ([`HubOptions::origin`] if set).
    ///
    /// [`HubOptions::origin`]: super::HubOptions::origin
    pub effective: String,
}

/// What `remote set-url` changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetUrlReport {
    /// The URL the tablet stored before.
    pub previous: String,
    pub remote: RemoteReport,
    pub took_over: Option<TakeOver>,
    pub warnings: Vec<Warning>,
}

/// What `detach` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetachReport {
    pub repo_id: RepoId,
    pub device_id: DeviceId,
    /// The device ref deleted from origin, if asked and it existed.
    pub pruned: Option<String>,
    pub took_over: Option<TakeOver>,
    pub warnings: Vec<Warning>,
}

/// What kind of commit a history entry is, from its subject, parents
/// and trailers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitKind {
    /// `rmu hub init`.
    Init,
    /// A sync's merge commit.
    Sync,
    /// A sync's snapshot that fast-forwarded `main`.
    Snapshot,
    /// `rmu hub restore`.
    Restore,
    /// `rmu hub add`.
    Add,
    /// Anything else (a commit made by hand or by another tool).
    Other,
}

/// The device trailers of a commit (the tablet that synced, or the one
/// given to `restore`/`add`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommitDevice {
    pub name: Option<String>,
    pub id: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
}

/// A conflict copy recorded in a merge commit's trailers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForkRecord {
    pub original: DocId,
    pub copy: DocId,
}

/// A restore recorded in a commit's trailers: `copy` is `original`'s
/// version from commit `from`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreRecord {
    pub copy: DocId,
    pub original: DocId,
    pub from: Oid,
}

/// One entry of the library's history: a commit on `main`'s first-parent
/// chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogEntry {
    pub commit: Oid,
    pub kind: CommitKind,
    /// The commit's author (a device name, or `rmu`).
    pub author: String,
    /// The author time, in seconds since the epoch.
    pub time: i64,
    pub subject: String,
    /// The device trailers, if the commit has any.
    pub device: Option<CommitDevice>,
    /// A merge's notes (its body).
    pub notes: Vec<String>,
    pub forks: Vec<ForkRecord>,
    /// Deletions from origin made with `--allow-deletions`.
    pub consented: Vec<DocId>,
    pub restored: Vec<RestoreRecord>,
    /// Documents this commit added, changed and deleted against its
    /// first parent (folders included).
    pub added: Vec<DocId>,
    pub changed: Vec<DocId>,
    pub deleted: Vec<DocId>,
    /// The visible name of each of those documents (after the commit, or
    /// before it for a deletion) and of each restore's original (in the
    /// version restored).
    pub names: BTreeMap<DocId, String>,
}

/// The tablet a history was read with, and where it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TabletBase {
    pub device_id: DeviceId,
    pub device_name: DeviceName,
    /// Its default base (`log`), or the document's own base (a
    /// document's log); `None` = the empty base.
    pub base: Option<Oid>,
    /// Documents with a base of their own.
    pub overrides: usize,
}

/// `log`: the library's history, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogReport {
    pub repo_id: RepoId,
    pub origin: String,
    pub main: Oid,
    pub tablet: Option<TabletBase>,
    pub entries: Vec<LogEntry>,
    /// Older entries left out by the limit.
    pub more: usize,
}

/// Where an item is: its parent (`""` root, `"trash"`, or a folder's
/// UUID) and that folder's logical path (`/`, `/Books/Math`, `trash`,
/// `trash/Old`; `?/...` where the chain breaks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Place {
    pub parent: String,
    pub path: String,
}

/// What one commit did to one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocEvent {
    /// It appeared (again, after a deletion, if earlier entries say so).
    Added {
        name: Option<String>,
        place: Place,
    },
    /// Its content changed (anything but `.metadata`).
    Changed,
    Renamed {
        from: Option<String>,
        to: Option<String>,
    },
    /// Moved to another folder (the trash included).
    Moved {
        from: Place,
        to: Place,
    },
    Deleted,
    /// A conflict copy was forked from it.
    ForkedInto {
        copy: DocId,
    },
    /// It is a conflict copy of `original`.
    ForkOf {
        original: DocId,
    },
    /// `restore` made a copy of one of its versions.
    RestoredAs {
        copy: DocId,
        from: Oid,
    },
    /// `restore` made it from `original`'s version in commit `from`.
    RestoredFrom {
        original: DocId,
        from: Oid,
    },
}

/// One entry of a document's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocLogEntry {
    pub commit: Oid,
    pub kind: CommitKind,
    pub author: String,
    pub time: i64,
    pub subject: String,
    pub events: Vec<DocEvent>,
    /// The document's subtree after the commit (`None`: absent): the
    /// version `restore --at <commit>` brings back (after a deletion, it
    /// brings back the version before it).
    pub version: Option<Oid>,
}

/// A document's history, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocLogReport {
    pub repo_id: RepoId,
    pub origin: String,
    pub main: Oid,
    pub doc: DocId,
    /// Its newest known name.
    pub name: Option<String>,
    /// Where it is on `main` (`None`: deleted).
    pub place: Option<Place>,
    pub tablet: Option<TabletBase>,
    pub entries: Vec<DocLogEntry>,
}

/// What `restore` committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreReport {
    pub original: DocId,
    /// The name of the version restored.
    pub original_name: String,
    /// The new document.
    pub copy: DocId,
    pub name: String,
    pub place: Place,
    /// The commit `--at` named.
    pub at: Oid,
    /// The commit holding the version restored (`at` itself, or for a
    /// document deleted by then, the last commit before the deletion).
    pub from: Oid,
    /// The commit pushed to `main`.
    pub commit: Oid,
    pub push_attempts: usize,
}

/// What `add` committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddReport {
    pub doc: DocId,
    pub name: String,
    pub place: Place,
    /// `pdf` or `epub`.
    pub file_type: String,
    pub bytes: u64,
    /// The pages of the materialised page list; `None`: unmaterialised
    /// (EPUBs, and PDFs whose page count could not be read), the first
    /// open materialises it.
    pub pages: Option<usize>,
    /// The commit pushed to `main`.
    pub commit: Oid,
    pub push_attempts: usize,
}
