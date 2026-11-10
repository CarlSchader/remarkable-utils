//! The `rmu hub` operations behind the CLI: `init`, `clone` (with
//! `--reattach`), `sync`, `backup`, `status`, `--dry-run`, `remote` and
//! `detach`, composing
//! the local git cache ([`crate::git`]), the data model, the merge engine
//! and a [`Tablet`] (see "The sync cycle" in `docs/hub-design.md`).
//!
//! [`Hub::sync`] runs the 16-step cycle:
//!
//! 1. preflight: read the tablet state, `ls-remote` origin, take the
//!    local cache's `flock`, fetch origin `main` and the device ref, read
//!    the mirror rules, check the repo id and that every base is an
//!    ancestor of `main` (before the tablet is touched); pre-read the
//!    tablet while xochitl runs;
//! 2. guard: take the tablet lock, arm the watchdog, refuse a
//!    cloud-paired tablet, re-read the state and fetch again under the
//!    lock;
//! 3. stop xochitl (watchdog armed);
//! 4. reconcile a journal left by an interrupted run, while stopped,
//!    writing its result before anything else;
//! 5. read the tablet: stat pass, download of changed files streamed into
//!    `fast-import` (step 5 reads before step 4 decides, because
//!    reconciliation judges the files it read; a document reconciliation
//!    finishes is read again);
//! 6. start xochitl; the watchdog stays armed and is re-armed before each
//!    network step (a crash then leaves a lock that goes stale when it
//!    fires); validate, carry forward, build the snapshot;
//! 7. backup: commit the snapshot (parents: base, device ref), push the
//!    device ref with a lease, record `device_ref` on the tablet;
//! 8. fetch, identity and ancestry checks;
//! 9. merge, deletion guard;
//! 10. journal, written durably before each push attempt (and before the
//!     apply of a pull-only run, which pushes nothing);
//! 11. push `main` (fast-forward only), back to 8 on rejection, a bounded
//!     number of times; nothing when the tablet already matches origin or
//!     has no changes of its own;
//! 12. stage (in batches, re-arming the watchdog);
//! 13. stop (watchdog armed);
//! 14. re-read, verify, live-tablet tree checks, apply;
//! 15. record;
//! 16. start, disarm, release; then `git maintenance run --auto` on the
//!     cache, still under its `flock`.
//!
//! Every error path brings xochitl back (`reset-failed` + `start`) when
//! this run may have stopped it, then releases the lock; if the
//! connection is gone, the watchdog brings it back. The library never
//! prints: progress goes to a [`Progress`], results come back as reports.

mod admin;
mod history;
mod inspect;
mod message;
mod plan;
mod snapshot;
mod steps;
mod store;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use self::plan::JsonBlobs;
use self::snapshot::{Snapshot, TabletRead};
use self::store::{Cache, Remote, Store, StoreHistory};
use crate::error::{Error, Result};
use crate::git::{CommitMeta, CommitWriter, Expected, FileMode, Git, Oid, Signature};
use crate::hub::device::{
    DeviceError, DeviceInfo, HeldLock, LockOwner, LockRequest, LockState, StaleReason, Tablet,
};
use crate::hub::merge::{
    self, DeletionGuard, Fork, GuardThresholds, MergeInput, MergeOutcome, MergeParams, MergedEntry,
    Note, Summary, TreeItem,
};
use crate::hub::model::{
    DeviceId, DeviceName, DocFile, DocId, DocVersion, ListingEntry, MirrorRules, RepoConfig,
    RepoId, RepoTree, TabletState, XochitlPath,
};
use crate::progress::Progress;

/// Errors of the sync cycle itself.
#[derive(Debug, Error)]
pub enum CycleError {
    /// The tablet has no hub state.
    #[error("the tablet is not attached to a hub; run `rmu hub clone` first")]
    NotAttached,

    /// `clone --reattach` to the same repository while an interrupted
    /// sync's journal, whose merge origin has, is pending: `sync`
    /// reconciles it first.
    #[error(
        "an interrupted sync left a journal on the tablet that origin can still finish; run `rmu hub sync` first (`rmu hub detach` then `rmu hub clone` would attach the tablet afresh, without it)"
    )]
    JournalPending,

    /// `clone` on a tablet that already has hub state.
    #[error(
        "the tablet is already attached to hub {repo_id}; `rmu hub clone --reattach` attaches it again"
    )]
    AlreadyAttached { repo_id: RepoId },

    /// The pairing guard: rmu hub and the reMarkable cloud would fight.
    #[error("the tablet is paired with the reMarkable cloud; unpair it before using rmu hub")]
    CloudPaired,

    /// `clone` from an origin without `main`.
    #[error("origin {url} has no main branch; run `rmu hub init` first")]
    NotInitialised { url: String },

    /// `init` on an origin that already has `main`.
    #[error("origin {url} already has a main branch")]
    AlreadyInitialised { url: String },

    /// The tablet belongs to another origin.
    #[error(
        "origin's repo id {origin} does not match the tablet's {tablet}: wrong or replaced origin; stopping (`rmu hub clone --reattach` attaches the tablet to it deliberately)"
    )]
    RepoMismatch { tablet: RepoId, origin: RepoId },

    /// Origin lost its `main`.
    #[error("origin {url} no longer has a main branch: it was emptied or replaced; stopping")]
    OriginEmpty { url: String },

    /// A base commit that is not in origin's history: origin was rewound
    /// or replaced. Nothing is inferred (a rewound origin would read as
    /// "everything since was deleted").
    #[error(
        "base commit {base} is not in origin's history: origin was rewound or replaced; stopping (`rmu hub clone --reattach`, or `rmu hub detach` then `rmu hub clone`, attaches the tablet to it deliberately)"
    )]
    OriginRewound { base: Oid },

    /// The device ref was rewritten or replaced: it does not continue
    /// the snapshots this tablet recorded.
    #[error(
        "origin's device ref {reference} ({remote}) does not continue the snapshot history recorded on the tablet ({recorded}): it was rewritten or replaced; stopping (`rmu hub clone --reattach` starts the tablet's snapshot history afresh)"
    )]
    DeviceRefConflict {
        reference: String,
        recorded: String,
        remote: String,
    },

    /// Origin lacks the device ref the tablet recorded pushing.
    #[error(
        "origin has no device ref {reference}, though the tablet recorded pushing snapshot {recorded} to it: origin was probably moved or copied without its `refs/rmu/*` (copy them too, e.g. `git push <new origin> 'refs/rmu/*:refs/rmu/*'`); stopping (`rmu hub clone --reattach` starts the tablet's snapshot history afresh)"
    )]
    DeviceRefMissing { reference: String, recorded: Oid },

    /// The deletion guard.
    #[error(
        "this sync would delete {} document(s) from origin and {} from the tablet{}; nothing was pushed. Rerun with --allow-deletions if that is intended",
        .from_origin.len(),
        .from_tablet.len(),
        if *.empty_snapshot { " (the tablet looks empty)" } else { "" }
    )]
    DeletionsNeedConsent {
        from_origin: Vec<DocId>,
        from_tablet: Vec<DocId>,
        empty_snapshot: bool,
    },

    /// Origin kept moving.
    #[error("origin kept moving: the push was rejected {0} times; try again")]
    PushRetriesExhausted(usize),

    /// Another run on this computer holds the local cache.
    #[error("the local cache is in use by another rmu hub run on this computer ({0})")]
    CacheBusy(PathBuf),

    /// Origin's `.rmu/hub.json` (repo id, format, mirror rules) changed
    /// during the run: the snapshot was classified under the old rules.
    #[error("origin's repository configuration changed during the run; try again")]
    RepoConfigChanged,

    /// An origin URL with a password in it.
    #[error(
        "refusing an origin URL with embedded credentials; use an ssh key or a credential helper"
    )]
    CredentialsInUrl,

    /// An object the run needs is not in the local cache.
    #[error("object {0} is missing from the local cache")]
    MissingObject(Oid),

    /// The tablet state changed between the preflight and the lock.
    #[error("the tablet's hub state changed while this run was starting; try again")]
    StateChanged,

    /// A file whose size changed between the stat pass and its download.
    #[error("{path} changed while it was read ({expected} bytes expected, {actual} read)")]
    FileChanged {
        path: String,
        expected: u64,
        actual: u64,
    },

    /// Origin's `.rmu/hub.json` is missing or invalid.
    #[error("origin's repository configuration is unusable: {0}")]
    InvalidRepoConfig(String),
}

/// How a run behaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubOptions {
    /// Watchdog timeout: xochitl comes back at most this long after a
    /// stop if rmu disappears. Re-armed before every step that may take
    /// long, so it bounds each step, not the run.
    pub watchdog: Duration,
    /// Push attempts before giving up on an origin that keeps moving.
    pub push_attempts: usize,
    /// `--allow-deletions`.
    pub allow_deletions: bool,
    /// `--break-lock`.
    pub break_lock: bool,
    pub thresholds: GuardThresholds,
    /// Origin URL for this invocation (`--origin`); the tablet's stored
    /// one otherwise. `sync`, `backup`, `status`, `dry_run`, `remote` and
    /// `detach` use it; `init`, `clone_tablet` and `set_remote_url` take
    /// their origin as an argument and ignore it.
    pub origin: Option<String>,
    /// The date in fork names, `yyyy-mm-dd`; today (UTC) if `None`.
    pub date: Option<String>,
    /// The pre-read checkpoints its local writes about every this many
    /// bytes, so a connection cut loses at most that much progress.
    pub pre_read_checkpoint: u64,
}

impl Default for HubOptions {
    fn default() -> Self {
        Self {
            watchdog: Duration::from_secs(600),
            push_attempts: 5,
            allow_deletions: false,
            break_lock: false,
            thresholds: GuardThresholds::default(),
            origin: None,
            date: None,
            pre_read_checkpoint: 64 * 1024 * 1024,
        }
    }
}

/// `$XDG_CACHE_HOME/rmu/hub` (or `~/.cache/rmu/hub`): where local caches
/// live by default.
pub fn default_cache_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|dir| !dir.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .map(|dir| dir.join("rmu").join("hub"))
}

/// The hub operations, for one computer (its git, its cache directory)
/// and one rmu process (its lock request).
pub struct Hub<'a> {
    git: Git,
    cache_dir: PathBuf,
    request: LockRequest,
    progress: &'a dyn Progress,
    options: HubOptions,
}

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
    /// The snapshot commit (device ref). For [`MainUpdate::Pulled`] it is
    /// a local commit only.
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
    /// Documents the sync would delete from the tablet.
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

// ---- helpers ------------------------------------------------------------------

/// Whether `origin` names a local path rather than a remote URL, by git's
/// own rule: no `scheme://`, and no `host:` before the first `/` (the
/// scp-like `host:path` form). `init` creates a local path that does not
/// exist yet.
pub fn is_local_path(origin: &str) -> bool {
    if origin.contains("://") {
        return false;
    }
    match (origin.find(':'), origin.find('/')) {
        (None, _) => true,
        (Some(colon), Some(slash)) => slash < colon,
        (Some(_), None) => false,
    }
}

/// Refuse URLs that carry a password (`scheme://user:pass@host`).
fn check_url(url: &str) -> Result<()> {
    let embedded = url.split_once("://").is_some_and(|(_, rest)| {
        rest.split(['/', '?', '#'])
            .next()
            .and_then(|authority| authority.rsplit_once('@'))
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
    });
    if embedded {
        Err(CycleError::CredentialsInUrl.into())
    } else {
        Ok(())
    }
}

/// Days since the epoch to a civil `yyyy-mm-dd` (proleptic Gregorian).
fn civil_date(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// rmu as committer (and as the author of `init`).
fn rmu_signature(time: i64) -> Signature {
    Signature {
        name: "rmu".to_string(),
        email: "rmu@rmu.invalid".to_string(),
        time,
        offset_minutes: 0,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A device name from the model and the device id (`--name` absent).
fn default_name(info: &DeviceInfo, id: &DeviceId) -> DeviceName {
    let model: String = info
        .model
        .as_deref()
        .unwrap_or("reMarkable")
        .chars()
        .filter(|c| !c.is_control() && *c != '<' && *c != '>')
        .take(50)
        .collect();
    let model = match model.trim() {
        "" => "reMarkable".to_string(),
        trimmed => trimmed.to_string(),
    };
    DeviceName::new(format!("{model} {}", &id.as_str()[..4]))
        .expect("a model name of at most 50 characters plus an id is a valid name")
}

fn is_transport(error: &Error) -> bool {
    !matches!(
        error,
        Error::HubDevice(
            DeviceError::Locked(_)
                | DeviceError::LockContended
                | DeviceError::Busy
                | DeviceError::InvalidRequest(_)
        )
    )
}

/// JSON blobs the cache does not have yet (read in this run, before the
/// snapshot is committed; new in a merge not committed yet) in front of
/// the cache.
struct Overlay<'a> {
    bytes: HashMap<&'a Oid, &'a [u8]>,
    below: &'a dyn JsonBlobs,
}

impl<'a> Overlay<'a> {
    /// The `.metadata`/`.content` a tablet read holds.
    fn read(read: &'a TabletRead, below: &'a dyn JsonBlobs) -> Self {
        Self {
            bytes: read
                .files
                .iter()
                .filter_map(|(path, file)| Some((&file.blob, read.json.get(path)?.as_slice())))
                .collect(),
            below,
        }
    }

    /// Blobs a merge wrote that are not committed yet.
    fn new_blobs(blobs: &'a HashMap<Oid, Vec<u8>>, below: &'a dyn JsonBlobs) -> Self {
        Self {
            bytes: blobs
                .iter()
                .map(|(oid, bytes)| (oid, bytes.as_slice()))
                .collect(),
            below,
        }
    }
}

impl JsonBlobs for Overlay<'_> {
    fn json(&self, id: &Oid) -> Result<Option<Rc<Value>>> {
        match self.bytes.get(id) {
            Some(bytes) => Ok(serde_json::from_slice::<Value>(bytes).ok().map(Rc::new)),
            None => self.below.json(id),
        }
    }
}

fn read_summary(read: &TabletRead) -> ReadSummary {
    ReadSummary {
        downloaded: read.downloaded,
        unmirrored: read.excluded.len() + read.irregular.len() + read.unlisted,
    }
}

/// Parent and kind of every present document of a tree.
fn tree_items(tree: &RepoTree, json: &dyn JsonBlobs) -> Result<BTreeMap<DocId, TreeItem>> {
    tree.docs
        .iter()
        .filter_map(|(doc, version)| Some((doc, version.files.get(&DocFile::Metadata)?)))
        .map(|(doc, entry)| {
            Ok(json
                .json(&entry.id)?
                .filter(|metadata| metadata.is_object())
                .map(|metadata| (doc.clone(), TreeItem::from_metadata(&metadata))))
        })
        .filter_map(Result::transpose)
        .collect()
}

/// The tablet's documents as versions (from the read blobs).
fn tablet_version(files: &BTreeMap<DocFile, Oid>) -> DocVersion {
    DocVersion {
        files: files
            .iter()
            .map(|(file, blob)| (file.clone(), ListingEntry::regular(blob.clone())))
            .collect(),
        other: BTreeMap::new(),
    }
}

/// The tablet's files as read, per document, as versions.
fn tablet_versions(read: &TabletRead) -> BTreeMap<DocId, DocVersion> {
    read.doc_files()
        .iter()
        .map(|(doc, files)| (doc.clone(), tablet_version(files)))
        .collect()
}

fn quarantine_report(snapshot: &Snapshot) -> BTreeMap<DocId, String> {
    snapshot
        .quarantined
        .iter()
        .map(|(doc, reason)| (doc.clone(), reason.to_string()))
        .collect()
}

/// The mirrored path of a document's file.
fn xochitl_path(doc: &DocId, file: &DocFile) -> Result<XochitlPath> {
    Ok(XochitlPath::new(file.xochitl_path(doc))?)
}

/// Everything known about origin during a run.
struct OriginCtx {
    url: String,
    cache: Cache,
    store: Store,
    remote: Remote,
    main: Oid,
    config: RepoConfig,
    rules: &'static MirrorRules,
}

impl OriginCtx {
    /// Fetch and read origin's configuration; `advertised` is an
    /// `ls-remote` just made (by [`store::open_cache`]), reused.
    fn new(
        cache: Cache,
        url: String,
        device: Option<&DeviceId>,
        advertised: Option<Vec<crate::git::RemoteRef>>,
    ) -> Result<Self> {
        let remote = store::fetch(&cache.repo, &url, device, advertised)?;
        let main = remote
            .main
            .clone()
            .ok_or_else(|| CycleError::OriginEmpty { url: url.clone() })?;
        let store = Store::new(cache.repo.clone())?;
        let config = store.repo_config(&main)?;
        let rules = config.rules()?;
        Ok(Self {
            url,
            cache,
            store,
            remote,
            main,
            config,
            rules,
        })
    }

    /// Fetch again (under the lock, at step 8, and to resolve a lost
    /// push acknowledgement): `main` may have moved, its `.rmu/hub.json`
    /// must not have changed.
    fn refresh(&mut self, device: &DeviceId) -> Result<()> {
        let remote = store::fetch(&self.cache.repo, &self.url, Some(device), None)?;
        let main = remote.main.clone().ok_or_else(|| CycleError::OriginEmpty {
            url: self.url.clone(),
        })?;
        // Pinned for the whole run: the snapshot was classified under
        // these rules.
        if self.store.repo_config(&main)? != self.config {
            return Err(CycleError::RepoConfigChanged.into());
        }
        self.remote = remote;
        self.main = main;
        Ok(())
    }

    /// The tablet must belong to this origin, and every base must be in
    /// its history.
    fn check(&self, state: &TabletState) -> Result<()> {
        if state.repo_id != self.config.repo_id {
            return Err(CycleError::RepoMismatch {
                tablet: state.repo_id.clone(),
                origin: self.config.repo_id.clone(),
            }
            .into());
        }
        bases(state)
            .into_iter()
            .flatten()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .try_for_each(|base| {
                if self.store.is_ancestor(base, &self.main)? {
                    Ok(())
                } else {
                    Err(CycleError::OriginRewound { base: base.clone() }.into())
                }
            })
    }
}

/// The default base and every override (`None` = the empty base).
fn bases(state: &TabletState) -> Vec<Option<&Oid>> {
    std::iter::once(state.base.as_ref())
        .chain(state.base_overrides.values().map(Option::as_ref))
        .collect()
}

/// The virtual base: the default base's tree with every override
/// document at its own base version.
fn virtual_base(store: &Store, rules: &MirrorRules, state: &TabletState) -> Result<RepoTree> {
    let tree = match &state.base {
        Some(base) => store.tree_of(rules, base)?,
        None => RepoTree::default(),
    };
    state
        .base_overrides
        .iter()
        .try_fold(tree, |mut tree, (doc, base)| {
            match base
                .as_ref()
                .map(|base| store.version_at(rules, base, doc))
                .transpose()?
                .flatten()
            {
                Some(version) => tree.docs.insert(doc.clone(), version),
                None => tree.docs.remove(doc),
            };
            Ok(tree)
        })
}

/// The tablet's documents that origin deleted with recorded consent:
/// present in `ours`, absent from `theirs`, and named by an
/// `Rmu-Deletion-Consent` trailer of a commit since that document's own
/// base. A document with the empty base never counts (the merge never
/// deletes it from the tablet: it reads as new).
fn consented_deletions(
    store: &Store,
    main: &Oid,
    state: &TabletState,
    ours: &RepoTree,
    theirs: &RepoTree,
) -> Result<BTreeSet<DocId>> {
    let present =
        |tree: &RepoTree, doc: &DocId| tree.docs.get(doc).is_some_and(DocVersion::is_present);
    let by_base = ours
        .docs
        .keys()
        .filter(|doc| present(ours, doc) && !present(theirs, doc))
        .filter_map(|doc| Some((state.base_for(doc)?.clone(), doc.clone())))
        .fold(
            BTreeMap::<Oid, Vec<DocId>>::new(),
            |mut groups, (base, doc)| {
                groups.entry(base).or_default().push(doc);
                groups
            },
        );
    by_base
        .into_iter()
        .try_fold(BTreeSet::new(), |mut consented, (base, docs)| {
            let since = format!("^{base}");
            let recorded: BTreeSet<DocId> = store
                .messages(&[main.as_str(), &since], false)?
                .iter()
                .flat_map(|text| {
                    let trailers = message::trailers(text);
                    message::doc_ids(message::values(&trailers, message::CONSENT))
                })
                .collect();
            consented.extend(docs.into_iter().filter(|doc| recorded.contains(doc)));
            Ok(consented)
        })
}

/// Every document of either tree has equivalent versions in both.
fn trees_equivalent(
    rules: &MirrorRules,
    a: &RepoTree,
    b: &RepoTree,
    json: &dyn JsonBlobs,
    except: &BTreeSet<DocId>,
) -> Result<bool> {
    a.docs
        .keys()
        .chain(b.docs.keys())
        .filter(|doc| !except.contains(*doc))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .try_fold(true, |same, doc| {
            Ok(same && plan::versions_equivalent(rules, a.docs.get(doc), b.docs.get(doc), json)?)
        })
}

/// Write every new blob of a merge outcome; its tree, and the new
/// blobs' contents (the cache has them only once the commit is written).
fn realise(
    writer: &mut CommitWriter,
    outcome: &MergeOutcome,
) -> Result<(RepoTree, HashMap<Oid, Vec<u8>>)> {
    let mut written = HashMap::new();
    let docs = outcome
        .docs
        .iter()
        .map(|(doc, version)| {
            let files = version
                .files
                .iter()
                .map(|(file, entry)| {
                    let entry = match entry {
                        MergedEntry::Existing(entry) => entry.clone(),
                        MergedEntry::New(bytes) => {
                            let oid = writer.write_blob(
                                &format!("docs/{doc}/{}", file.repo_name()),
                                bytes.len() as u64,
                                &mut &bytes[..],
                            )?;
                            written.insert(oid.clone(), bytes.clone());
                            ListingEntry::regular(oid)
                        }
                    };
                    Ok((file.clone(), entry))
                })
                .collect::<Result<_>>()?;
            Ok((
                doc.clone(),
                DocVersion {
                    files,
                    other: version.other.clone(),
                },
            ))
        })
        .collect::<Result<_>>()?;
    Ok((
        RepoTree {
            docs,
            extra: outcome.extra.clone(),
        },
        written,
    ))
}

/// Add a whole tree to a commit and write it.
fn commit_tree(mut writer: CommitWriter, tree: &RepoTree, meta: &CommitMeta) -> Result<Oid> {
    tree.to_listing()
        .iter()
        .try_for_each(|(path, entry)| writer.add_existing(path.as_str(), entry.mode, &entry.id))?;
    writer.commit(meta)
}

// ---- the tablet session -----------------------------------------------------------

/// The tablet side of one run: the lock, whether xochitl may be stopped,
/// and the error path that brings it back.
struct Session<'t> {
    tablet: &'t dyn Tablet,
    watchdog: Duration,
    lock: Option<HeldLock>,
    /// Set before a stop is attempted (its reply may be lost), cleared by
    /// a start that succeeded.
    maybe_stopped: bool,
}

impl<'t> Session<'t> {
    fn new(tablet: &'t dyn Tablet, watchdog: Duration) -> Self {
        Self {
            tablet,
            watchdog,
            lock: None,
            maybe_stopped: false,
        }
    }

    /// Take the lock, retrying once with the same request after a
    /// failure that may have been a lost acknowledgement (the retry then
    /// adopts the lock).
    fn acquire(&mut self, request: &LockRequest, break_lock: bool) -> Result<Option<TakeOver>> {
        let lock = match self.tablet.acquire_lock(request, break_lock) {
            Err(error) if is_transport(&error) => self.tablet.acquire_lock(request, break_lock)?,
            other => other?,
        };
        let took_over = lock.took_over().map(|(owner, reason)| TakeOver {
            previous: owner.cloned(),
            reason,
        });
        self.lock = Some(lock);
        Ok(took_over)
    }

    fn lock(&self) -> &HeldLock {
        self.lock.as_ref().expect("the lock is taken first")
    }

    /// Arm (or re-arm) the watchdog.
    fn arm(&self) -> Result<()> {
        self.tablet.arm_watchdog(self.lock(), self.watchdog)
    }

    fn stop(&mut self) -> Result<()> {
        self.arm()?;
        self.maybe_stopped = true;
        self.tablet.stop_xochitl(self.lock())
    }

    /// Start xochitl; the device time just before the start.
    fn start(&mut self) -> Result<i64> {
        let time = self.tablet.start_xochitl(self.lock())?;
        self.maybe_stopped = false;
        Ok(time)
    }

    fn write_state(&self, state: &TabletState) -> Result<()> {
        self.tablet.write_state(self.lock(), state)
    }

    /// Step 16 after success: disarm and release. Failures here leave
    /// nothing unsafe (a pending watchdog only restarts a running
    /// xochitl; a left lock goes stale when it fires), so they are
    /// warnings.
    fn finish(&mut self) -> Vec<Warning> {
        let Some(lock) = self.lock.take() else {
            return Vec::new();
        };
        let disarm = self.tablet.disarm_watchdog(&lock).err();
        let release = self.tablet.release_lock(&lock).err();
        [
            disarm.map(|e| Warning::DisarmFailed {
                error: e.to_string(),
            }),
            release.map(|e| Warning::ReleaseFailed {
                error: e.to_string(),
            }),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The error path: bring xochitl back if this run may have stopped
    /// it, disarm only once it runs, release the lock. Everything is best
    /// effort; with the connection gone the watchdog brings xochitl back.
    fn abort(&mut self) {
        let Some(lock) = self.lock.take() else {
            return;
        };
        let running = !self.maybe_stopped || self.tablet.start_xochitl(&lock).is_ok();
        if running {
            let _ = self.tablet.disarm_watchdog(&lock);
        }
        let _ = self.tablet.release_lock(&lock);
    }
}

/// What steps 1–6 produced.
struct Prepared<'p> {
    origin: OriginCtx,
    state: TabletState,
    read: TabletRead,
    /// Holds every blob the reads streamed; it becomes the snapshot.
    writer: Option<CommitWriter<'p>>,
    /// Device time of the step-6 start (racy-entry reference).
    started_at: i64,
    info: DeviceInfo,
    took_over: Option<TakeOver>,
    reconciled: Option<Reconciled>,
}

/// A merge against origin `main` as a sync runs it (and `--dry-run`
/// previews it): consent, the merge itself, its new blobs streamed into a
/// commit writer not committed yet, and what it means for pushing.
struct MergeRun<'p> {
    theirs: RepoTree,
    outcome: MergeOutcome,
    merged: RepoTree,
    /// Holds the merge's new blobs, until it commits.
    writer: CommitWriter<'p>,
    /// The tablet already agrees with origin: nothing to push or apply.
    /// Quarantined documents carry their base version, and documents the
    /// apply leaves alone keep their previous base: neither counts.
    up_to_date: bool,
    /// The tablet has no changes of its own (every document equals its
    /// base version) and the merge is origin's tree: nothing to push.
    pull_only: bool,
}

/// What steps 8–11 decided.
struct Pushed {
    update: MainUpdate,
    base: Oid,
    merged: RepoTree,
    outcome: MergeOutcome,
    attempts: usize,
}

impl<'a> Hub<'a> {
    /// A hub for this computer: `git` (its environment), the directory
    /// holding local caches ([`default_cache_dir`]), this process's lock
    /// request, and where progress goes.
    pub fn new(
        git: Git,
        cache_dir: impl Into<PathBuf>,
        request: LockRequest,
        progress: &'a dyn Progress,
    ) -> Self {
        Self {
            git,
            cache_dir: cache_dir.into(),
            request,
            progress,
            options: HubOptions::default(),
        }
    }

    pub fn with_options(mut self, options: HubOptions) -> Self {
        self.options = options;
        self
    }

    pub fn options(&self) -> &HubOptions {
        &self.options
    }

    fn date(&self) -> String {
        self.options
            .date
            .clone()
            .unwrap_or_else(|| civil_date(unix_now()))
    }

    /// A commit's metadata: author = the device, committer = rmu, both
    /// explicit (never auto-detected, which would record a hostname).
    fn meta(state: &TabletState, parents: Vec<Oid>, message: String) -> CommitMeta {
        let time = unix_now();
        CommitMeta {
            parents,
            author: Signature {
                name: state.device_name.to_string(),
                email: format!("{}@rmu.invalid", state.device_id),
                time,
                offset_minutes: 0,
            },
            committer: rmu_signature(time),
            message,
        }
    }

    fn origin_url(&self, stored: &str) -> Result<String> {
        let url = self
            .options
            .origin
            .clone()
            .unwrap_or_else(|| stored.to_string());
        check_url(&url)?;
        Ok(url)
    }

    /// Run `body` on a tablet session: on error, the error path; on
    /// success, disarm and release, then (if `body` hands its origin back,
    /// whose cache `flock` is still held) `git maintenance run --auto` on
    /// the cache, with the tablet free again. Warnings returned.
    fn session<T>(
        &self,
        tablet: &dyn Tablet,
        body: impl FnOnce(&mut Session) -> Result<(T, Option<OriginCtx>)>,
    ) -> Result<(T, Vec<Warning>)> {
        let mut session = Session::new(tablet, self.options.watchdog);
        let result = match body(&mut session) {
            Ok((value, origin)) => {
                let mut warnings = session.finish();
                if let Some(origin) = origin {
                    warnings.extend(self.maintain(&origin));
                }
                Ok((value, warnings))
            }
            Err(error) => {
                session.abort();
                Err(error)
            }
        };
        self.progress.finished();
        result
    }

    /// `git maintenance run --auto` on the cache (the user's thresholds
    /// decide whether anything happens); a failure is only a warning.
    fn maintain(&self, origin: &OriginCtx) -> Option<Warning> {
        self.progress.step("Maintaining the local cache");
        origin
            .cache
            .repo
            .maintenance_auto()
            .err()
            .map(|error| Warning::MaintenanceFailed {
                error: error.to_string(),
            })
    }

    /// The merge a sync runs against origin's current `main`.
    fn run_merge<'p>(
        &'p self,
        origin: &OriginCtx,
        state: &TabletState,
        base_tree: &RepoTree,
        snapshot: &Snapshot,
        read: &TabletRead,
    ) -> Result<MergeRun<'p>> {
        let (store, rules, main) = (&origin.store, origin.rules, &origin.main);
        let theirs = store.tree_of(rules, main)?;
        let consented = consented_deletions(store, main, state, &snapshot.tree, &theirs)?;
        let date = self.date();
        let outcome = merge::merge(
            &MergeInput {
                base: base_tree,
                ours: &snapshot.tree,
                theirs: &theirs,
            },
            store,
            &StoreHistory {
                store,
                rules,
                tip: main,
            },
            &MergeParams {
                rules,
                device_name: &state.device_name,
                date: &date,
                allow_deletions: self.options.allow_deletions,
                thresholds: self.options.thresholds,
                consented_deletions: &consented,
            },
        )?;
        let mut writer = origin.cache.repo.commit_writer(self.progress, None)?;
        let (merged, new_blobs) = realise(&mut writer, &outcome)?;
        let merged_json = Overlay::new_blobs(&new_blobs, store);
        let json = Overlay::read(read, &merged_json);
        let left_alone = plan::left_alone(rules, &merged, &tablet_versions(read), &json)?;
        let except: BTreeSet<DocId> = snapshot
            .quarantined
            .keys()
            .cloned()
            .chain(left_alone)
            .collect();
        let none = BTreeSet::new();
        let merge_is_theirs = trees_equivalent(rules, &merged, &theirs, &json, &none)?;
        let up_to_date =
            merge_is_theirs && trees_equivalent(rules, &snapshot.tree, &theirs, &json, &except)?;
        let pull_only =
            merge_is_theirs && trees_equivalent(rules, &snapshot.tree, base_tree, &json, &none)?;
        drop(json);
        Ok(MergeRun {
            theirs,
            outcome,
            merged,
            writer,
            up_to_date,
            pull_only,
        })
    }

    // ---- init ---------------------------------------------------------------------

    /// `rmu hub init`: give an empty origin its `.rmu/hub.json` (a new repo
    /// id, the newest mirror rules) as the first commit of `main`. A local
    /// path ([`is_local_path`]) that does not exist yet becomes a new bare
    /// repository. `origin` is used as given: [`HubOptions::origin`] does
    /// not apply. On failure, whatever this call created (that new
    /// repository, the cache) is removed again.
    pub fn init(&self, origin: &str) -> Result<InitReport> {
        self.progress.step("Initialising origin");
        let result = self.init_inner(origin);
        self.progress.finished();
        result
    }

    fn init_inner(&self, origin: &str) -> Result<InitReport> {
        check_url(origin)?;
        let local = Path::new(origin);
        let created = is_local_path(origin) && !local.exists();
        if created {
            self.git.init_bare(local, crate::git::ObjectFormat::Sha1)?;
        }
        let config = RepoConfig::new(RepoId::random());
        let cache_path = self.cache_dir.join(format!("{}.git", config.repo_id));
        let lock_path = self.cache_dir.join(format!("{}.lock", config.repo_id));
        let attempt = || -> Result<Oid> {
            let (cache, refs) =
                store::open_cache(&self.git, &self.cache_dir, &config.repo_id, origin)?;
            if refs.iter().any(|reference| reference.name == store::MAIN) {
                return Err(CycleError::AlreadyInitialised {
                    url: origin.to_string(),
                }
                .into());
            }
            let signature = rmu_signature(unix_now());
            let mut writer = cache.repo.commit_writer(self.progress, None)?;
            writer.add_bytes(
                crate::hub::model::REPO_CONFIG_PATH,
                FileMode::Regular,
                config.to_json().as_bytes(),
            )?;
            let commit = writer.commit(&CommitMeta {
                parents: Vec::new(),
                author: signature.clone(),
                committer: signature,
                message: "rmu hub: init\n".to_string(),
            })?;
            store::push(
                &cache.repo,
                origin,
                &commit,
                store::MAIN,
                Some(&Expected::Absent),
                store::LOCAL_MAIN,
            )?;
            Ok(commit)
        };
        attempt()
            .map(|commit| InitReport {
                config: config.clone(),
                commit,
            })
            .inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&cache_path);
                let _ = std::fs::remove_file(&lock_path);
                if created {
                    let _ = std::fs::remove_dir_all(local);
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(951_782_400), "2000-02-29");
        assert_eq!(civil_date(1_794_182_400), "2026-11-09");
        assert_eq!(civil_date(1_794_268_799), "2026-11-09");
        assert_eq!(civil_date(-86_400), "1969-12-31");
    }

    #[test]
    fn local_paths_follow_gits_rule() {
        [
            "/srv/git/library.git",
            "library.git",
            "./a:b",
            "../x/y.git",
            "dir/with:colon",
        ]
        .iter()
        .for_each(|origin| assert!(is_local_path(origin), "{origin}"));
        [
            "ssh://git@example.invalid/library.git",
            "file:///srv/git/library.git",
            "git@example.invalid:library.git",
            "host:library.git",
            "host:dir/library.git",
        ]
        .iter()
        .for_each(|origin| assert!(!is_local_path(origin), "{origin}"));
    }

    #[test]
    fn urls_with_passwords_are_refused() {
        [
            "ssh://git@example.invalid/library.git",
            "git@example.invalid:library.git",
            "/srv/git/library.git",
            "https://example.invalid/a@b:c",
            "https://token@example.invalid/x.git",
        ]
        .iter()
        .for_each(|url| assert!(check_url(url).is_ok(), "{url}"));
        [
            "https://user:secret@example.invalid/x.git",
            "ssh://user:pw@example.invalid/x",
        ]
        .iter()
        .for_each(|url| {
            assert!(
                matches!(
                    check_url(url),
                    Err(Error::HubCycle(CycleError::CredentialsInUrl))
                ),
                "{url}"
            )
        });
    }

    #[test]
    fn default_names_come_from_the_model_and_id() {
        let id = DeviceId::new("0f0e0d0c-0000-4000-8000-00000000c001").unwrap();
        let info = |model: Option<&str>| DeviceInfo {
            model: model.map(str::to_string),
            firmware: None,
        };
        assert_eq!(
            default_name(&info(Some("reMarkable Paper Pro")), &id).as_str(),
            "reMarkable Paper Pro 0f0e"
        );
        assert_eq!(default_name(&info(None), &id).as_str(), "reMarkable 0f0e");
        assert_eq!(
            default_name(&info(Some("<\n>")), &id).as_str(),
            "reMarkable 0f0e"
        );
        let long = "m".repeat(200);
        assert!(
            default_name(&info(Some(&long)), &id)
                .as_str()
                .chars()
                .count()
                <= 64
        );
    }
}
