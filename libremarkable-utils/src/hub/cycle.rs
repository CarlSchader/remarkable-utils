//! The `rmu hub` operations behind the CLI: `init`, `clone` (with
//! `--reattach`), `sync`, `backup`, `status`, `--dry-run`, `remote` and
//! `detach`, composing
//! the local git cache ([`crate::git`]), the data model, the merge engine
//! and a [`Tablet`](crate::hub::device::Tablet) (see "The sync cycle" in `docs/hub-design.md`).
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
//!    fires); validate, carry forward, build the snapshot. A small sync
//!    keeps xochitl stopped through step 16 instead ([`SingleStop`]),
//!    for one outage and one start, falling back to the two windows when
//!    it turns out large or slow;
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
//! 13. stop (watchdog armed), unless still stopped;
//! 14. re-read, verify, live-tablet tree checks, apply;
//! 15. record;
//! 16. start, disarm, release; then `git maintenance run --auto` on the
//!     cache, still under its `flock`.
//!
//! Every error path brings xochitl back (`reset-failed` + `start`) when
//! this run may have stopped it, then releases the lock; if the
//! connection is gone, the watchdog brings it back. The library never
//! prints: progress goes to a [`Progress`], results come back as reports.

mod add;
mod admin;
mod history;
mod init;
mod inspect;
mod library;
mod log;
mod merge_run;
mod message;
mod origin;
mod plan;
mod report;
mod restore;
mod session;
mod snapshot;
mod steps;
mod store;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use thiserror::Error;

use self::plan::JsonBlobs;
use self::snapshot::{Snapshot, TabletRead};
use crate::error::Result;
use crate::git::{CommitMeta, CommitWriter, Git, Oid, Signature};
use crate::hub::device::{DeviceInfo, LockRequest};
use crate::hub::merge::{GuardThresholds, TreeItem};
use crate::hub::model::{
    DeviceId, DeviceName, DocFile, DocId, DocVersion, ListingEntry, RepoId, RepoTree, TabletState,
    XochitlPath,
};
use crate::progress::Progress;
use crate::xdg;

pub use self::report::{
    AddReport, BackupReport, CommitDevice, CommitKind, DetachReport, DocChange, DocEvent,
    DocLogEntry, DocLogReport, Downloaded, DryRunReport, ForkRecord, InitReport, LogEntry,
    LogReport, MainUpdate, OpenConflict, Place, ReadSummary, Reconciled, RemoteReport,
    RestoreRecord, RestoreReport, SetUrlReport, StatusReport, SyncReport, TabletBase, TakeOver,
    Warning,
};

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
        "an interrupted sync left a journal on the tablet that origin can still finish; run `rmu hub sync` first (`rmu hub detach --force` then `rmu hub clone` would attach the tablet afresh, discarding it)"
    )]
    JournalPending,

    /// `remote set-url` to an origin that lacks the merge of a pending
    /// journal (where `sync` could no longer finish it).
    #[error(
        "the new origin does not have merge {merge}, which an interrupted sync on this tablet is waiting to finish; run `rmu hub sync` against the current origin first, then set the URL"
    )]
    JournalMergeMissing { merge: Oid },

    /// `detach` while an interrupted sync's journal is pending (without
    /// `force`).
    #[error(
        "an interrupted sync left a journal on the tablet: documents its apply was writing may be half-updated there (both versions stay in origin's history); run `rmu hub sync` first, or detach with --force to discard it"
    )]
    DetachJournalPending,

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
        /// The visible name of each document listed, from origin's or the
        /// tablet's history (the tablet may no longer have it).
        names: BTreeMap<DocId, String>,
    },

    /// Origin kept moving.
    #[error("origin kept moving: the push was rejected {0} times; try again")]
    PushRetriesExhausted(usize),

    /// The local cache of this repository has another object format than
    /// origin advertises: another repository answers at that URL (or a
    /// converted one). The cache, which may hold the last copy of a
    /// replaced origin's history, is kept.
    #[error(
        "origin {url} serves another object format than the local cache {cache}: a different (or converted) repository answers there; the cache is kept, move it away if that origin is intended",
        cache = .cache.display()
    )]
    CacheFormatMismatch { cache: PathBuf, url: String },

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

    /// A tree nested deeper than git's own default bound
    /// (`core.maxTreeDepth`): refused rather than walked.
    #[error(
        "tree {tree} in the local cache is nested deeper than {max} levels; refusing to read it"
    )]
    TreeTooDeep { tree: Oid, max: usize },

    /// `log`, `restore` or `add` with neither an origin URL nor a tablet.
    #[error("no origin: give --origin <url>, or reach a tablet attached to the hub")]
    NoOrigin,

    /// A UUID or path that names no document in `main`'s history.
    #[error("no document '{0}' in origin's history")]
    NotInHistory(String),

    /// A `--at` that names no commit in the local cache.
    #[error(
        "unknown revision '{0}': give a commit id as `rmu hub log` prints it (abbreviated is fine), or main, main~N"
    )]
    UnknownRevision(String),

    /// A `--at` commit that is not `main` or one of its ancestors (a
    /// commit only on a device ref, say).
    #[error("commit {0} is not in the history of origin's main")]
    NotOnMain(Oid),

    /// The document has no version at or before the `--at` commit.
    #[error("document {doc} has no version at or before commit {at}")]
    NoVersionAt { doc: DocId, at: Oid },

    /// `add` of a file that is not a PDF or an EPUB (by its name).
    #[error("rmu hub add takes .pdf and .epub files, not '{0}'")]
    NotAddable(String),

    /// `add` of a `.pdf` that does not start like one.
    #[error("'{0}' is not a PDF: it does not start with %PDF-")]
    NotAPdf(String),

    /// `add` into a folder in the trash, or one whose parent chain does
    /// not reach the root.
    #[error("folder '{0}' is in the trash, or its parent chain does not reach the root")]
    FolderUnusable(String),

    /// The new document's id is on origin already (never expected: ids
    /// are random).
    #[error("document {0} already exists on origin")]
    DocExists(DocId),

    /// `add`: the folder went away (deleted, or moved to the trash) while
    /// retrying the push.
    #[error("folder '{0}' was deleted or moved to the trash on origin meanwhile; try again")]
    FolderGone(String),
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
    /// A small sync keeps xochitl stopped from step 3 through step 16,
    /// for one outage and one start (see [`SingleStop`]); `None` always
    /// runs the two-window cycle.
    pub single_stop: Option<SingleStop>,
}

/// When a sync keeps xochitl stopped through its network steps: only if
/// what it would send is small (an estimate before step 7, the actual
/// plan before staging), and only for so long. Past either bound it
/// starts xochitl and goes on as the two-window cycle. Defaults: 1 MiB,
/// 32 documents, a 10 s budget, a 2.5 s slow fetch, a 60 s watchdog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SingleStop {
    /// At most this many bytes for the device-ref push and the staging
    /// together (about a second at the 1 MB/s measured over Wi-Fi and a
    /// VPN).
    pub max_bytes: u64,
    /// At most this many documents to apply.
    pub max_docs: usize,
    /// Checked between steps: once xochitl has been stopped this long,
    /// it is started and the run goes on with two windows.
    pub budget: Duration,
    /// A fetch under the lock (step 2) slower than this means a slow
    /// origin, or git asking for credentials: the later network steps
    /// would be slow too, so xochitl is started at step 6.
    pub slow_fetch: Duration,
    /// The watchdog timeout while xochitl stays stopped past step 6 (at
    /// most [`HubOptions::watchdog`]): the bound on one step that hangs
    /// between two budget checks. When it fires, xochitl comes back and
    /// the run ends, as after a crash.
    pub watchdog: Duration,
}

impl Default for SingleStop {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            max_docs: 32,
            budget: Duration::from_secs(10),
            slow_fetch: Duration::from_millis(2500),
            watchdog: Duration::from_secs(60),
        }
    }
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
            single_stop: Some(SingleStop::default()),
        }
    }
}

/// `$XDG_CACHE_HOME/rmu/hub` (or `~/.cache/rmu/hub`): where local caches
/// live by default.
pub fn default_cache_dir() -> Option<PathBuf> {
    xdg::base_dir(
        xdg::non_empty_var("XDG_CACHE_HOME"),
        xdg::non_empty_var("HOME"),
        ".cache",
    )
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

/// How `detach` goes about it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetachOptions {
    /// Also delete the tablet's device ref on origin (`--prune-ref`).
    pub prune_device_ref: bool,
    /// Detach even while an interrupted sync's journal is pending,
    /// discarding it (`--force`).
    pub force: bool,
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

/// The UTC date of a time in seconds since the epoch, `yyyy-mm-dd`
/// (proleptic Gregorian).
pub fn civil_date(unix_secs: i64) -> String {
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

/// Add a whole tree to a commit and write it.
fn commit_tree(mut writer: CommitWriter, tree: &RepoTree, meta: &CommitMeta) -> Result<Oid> {
    tree.to_listing()
        .iter()
        .try_for_each(|(path, entry)| writer.add_existing(path.as_str(), entry.mode, &entry.id))?;
    writer.commit(meta)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

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
