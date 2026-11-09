//! The `rmu hub` operations behind the CLI: `init`, `clone` (with
//! `--reattach`), `sync`, `backup`, `status` and `--dry-run`, composing
//! the local git cache ([`crate::git`]), the data model, the merge engine
//! and a [`Tablet`] (see "The sync cycle" in `docs/hub-design.md`).
//!
//! [`Hub::sync`] runs the 16-step cycle:
//!
//! 1. preflight: read the tablet state, `ls-remote` origin, take the
//!    local cache's `flock`, fetch origin `main` and the device ref, read
//!    the mirror rules, check the repo id and that every base is an
//!    ancestor of `main` (before the tablet is touched);
//! 2. guard: take the tablet lock, arm the watchdog, refuse a
//!    cloud-paired tablet, re-read the state under the lock;
//! 3. stop xochitl (watchdog armed);
//! 4. reconcile a journal left by an interrupted run ([`Hub::reconcile`]),
//!    while stopped, writing its result before anything else;
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
//! 10. journal, written durably before each push attempt;
//! 11. push `main` (fast-forward only), back to 8 on rejection, a bounded
//!     number of times;
//! 12. stage (in batches, re-arming the watchdog);
//! 13. stop (watchdog armed);
//! 14. re-read, verify, live-tablet tree checks, apply;
//! 15. record;
//! 16. start, disarm, release.
//!
//! Every error path brings xochitl back (`reset-failed` + `start`) when
//! this run may have stopped it, then releases the lock; if the
//! connection is gone, the watchdog brings it back. The library never
//! prints: progress goes to a [`Progress`], results come back as reports.

mod inspect;
mod message;
mod plan;
mod snapshot;
mod steps;
mod store;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use thiserror::Error;

use self::message::{Author, MergeRecord};
use self::plan::{Change, Content, JsonBlobs, Planned, Reread, Verdict};
use self::snapshot::{Known, Snapshot, TabletRead};
use self::store::{Cache, Remote, Store, StoreHistory};
use crate::error::{Error, Result};
use crate::git::{CommitMeta, CommitWriter, Expected, FileMode, Git, Oid, Signature};
use crate::hub::device::{
    ApplyPlan, DeviceError, DeviceInfo, DocApply, HeldLock, LockOwner, LockRequest, LockState,
    StagedFile, StaleReason, StatEntry, Tablet,
};
use crate::hub::merge::{
    self, DeletionGuard, Fork, GuardThresholds, MergeInput, MergeOutcome, MergeParams, MergedEntry,
    Note, Summary, TreeItem,
};
use crate::hub::model::{
    DeviceId, DeviceName, DocFile, DocId, DocVersion, Journal, JsonFile, ListingEntry, MirrorRules,
    RepoConfig, RepoId, RepoTree, StatSignature, TabletState, XochitlPath,
};
use crate::progress::Progress;

/// Errors of the sync cycle itself.
#[derive(Debug, Error)]
pub enum CycleError {
    /// The tablet has no hub state.
    #[error("the tablet is not attached to a hub; run `rmu hub clone` first")]
    NotAttached,

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
        "base commit {base} is not in origin's history: origin was rewound or replaced; stopping (`rmu hub clone --reattach` is the deliberate recovery)"
    )]
    OriginRewound { base: Oid },

    /// The device ref moved in a way this tablet did not.
    #[error(
        "origin's device ref {reference} ({remote}) does not continue the snapshot history recorded on the tablet ({recorded}): another tablet may use this device id; stopping"
    )]
    DeviceRefConflict {
        reference: String,
        recorded: String,
        remote: String,
    },

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
    /// one otherwise.
    pub origin: Option<String>,
    /// The date in fork names, `yyyy-mm-dd`; today (UTC) if `None`.
    pub date: Option<String>,
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
#[derive(Debug, Clone, PartialEq)]
pub struct InitReport {
    pub config: RepoConfig,
    /// The first commit of `main`.
    pub commit: Oid,
}

/// How `main` was updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MainUpdate {
    /// The tablet already matched origin: nothing pushed.
    UpToDate,
    /// Origin had not moved: the snapshot became `main`.
    FastForward(Oid),
    /// A merge commit (first parent: origin `main`, second: the snapshot).
    Merge(Oid),
}

/// What reconciling an interrupted run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// The journaled merge never reached origin: journal dropped, bases
    /// unchanged.
    pub dropped: bool,
    /// Documents whose interrupted apply was finished.
    pub finished: Vec<DocId>,
    /// Documents that keep their snapshot version as a base override.
    pub kept: Vec<DocId>,
}

/// What reading the tablet found besides documents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadSummary {
    /// Files downloaded and their bytes (the stopped read; files whose
    /// stat matched a recorded one are not read again).
    pub downloaded: (usize, u64),
    /// Files the mirror leaves on the tablet (thumbnails, `.local`,
    /// `.tree`, unknown files, symlinks, unlistable names).
    pub unmirrored: usize,
}

/// What one sync (or clone) did.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncReport {
    pub update: MainUpdate,
    /// The snapshot commit (device ref).
    pub snapshot: Oid,
    /// The commit the tablet now agrees with.
    pub base: Oid,
    /// Documents written to or deleted from the tablet.
    pub applied: Vec<DocId>,
    /// Documents not applied because they changed during the sync or
    /// would break the tablet's tree; the next sync picks them up.
    pub skipped: Vec<DocId>,
    /// Malformed documents (neither deleted nor changed), with why.
    pub quarantined: BTreeMap<DocId, String>,
    /// Files without a `.metadata` for documents origin does not know.
    pub orphans: Vec<DocId>,
    pub read: ReadSummary,
    pub notes: Vec<Note>,
    pub forks: Vec<Fork>,
    pub deletions: DeletionGuard,
    pub summary: Summary,
    pub reconciled: Option<Reconciled>,
    pub push_attempts: usize,
    /// The previous lock owner, if this run took a stale lock over.
    pub took_over: Option<(Option<LockOwner>, StaleReason)>,
    pub warnings: Vec<String>,
}

/// What `backup` did.
#[derive(Debug, Clone, PartialEq)]
pub struct BackupReport {
    pub snapshot: Oid,
    /// `false` if the device ref already held this snapshot.
    pub pushed: bool,
    pub quarantined: BTreeMap<DocId, String>,
    pub orphans: Vec<DocId>,
    pub read: ReadSummary,
    pub took_over: Option<(Option<LockOwner>, StaleReason)>,
    pub warnings: Vec<String>,
}

/// One document's change on the tablet since its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocChange {
    Added,
    Modified,
    Deleted,
}

/// A conflict copy that still has its generated name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenConflict {
    pub original: DocId,
    pub copy: DocId,
    pub name: String,
}

/// `status`: read-only, best effort (xochitl keeps running).
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct DryRunReport {
    pub status: StatusReport,
    /// Documents the sync would write on the tablet.
    pub writes: Vec<DocId>,
    /// Documents the sync would delete from the tablet.
    pub deletes: Vec<DocId>,
    /// How `main` would be updated (merge commits are not created: `Merge`
    /// carries no id meaning).
    pub would_push: bool,
}

// ---- helpers ------------------------------------------------------------------

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

/// JSON blobs read in this run (not in the cache until the snapshot is
/// committed) in front of the cache.
struct ReadJson<'a> {
    read: HashMap<&'a Oid, &'a Vec<u8>>,
    store: &'a Store,
}

impl<'a> ReadJson<'a> {
    fn new(read: &'a TabletRead, store: &'a Store) -> Self {
        Self {
            read: read
                .files
                .iter()
                .filter_map(|(path, file)| Some((&file.blob, read.json.get(path)?)))
                .collect(),
            store,
        }
    }
}

impl JsonBlobs for ReadJson<'_> {
    fn json(&self, id: &Oid) -> Result<Option<Rc<Value>>> {
        match self.read.get(id) {
            Some(bytes) => Ok(serde_json::from_slice::<Value>(bytes).ok().map(Rc::new)),
            None => self.store.json(id),
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
    fn new(cache: Cache, url: String, device: Option<&DeviceId>) -> Result<Self> {
        let remote = store::fetch(&cache.repo, &url, device)?;
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

    /// Fetch again (steps 4 and 8): `main` may have moved, its
    /// `.rmu/hub.json` must not have changed.
    fn refresh(&mut self, device: &DeviceId) -> Result<()> {
        let remote = store::fetch(&self.cache.repo, &self.url, Some(device))?;
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
                if self.store.has_commit(base)? && self.cache.repo.is_ancestor(base, &self.main)? {
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
) -> Result<bool> {
    a.docs
        .keys()
        .chain(b.docs.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .try_fold(true, |same, doc| {
            Ok(same && plan::versions_equivalent(rules, a.docs.get(doc), b.docs.get(doc), json)?)
        })
}

/// Write every new blob of a merge outcome and return its tree.
fn realise(writer: &mut CommitWriter, outcome: &MergeOutcome) -> Result<RepoTree> {
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
                        MergedEntry::New(bytes) => ListingEntry::regular(writer.write_blob(
                            &format!("docs/{doc}/{}", file.repo_name()),
                            bytes.len() as u64,
                            &mut &bytes[..],
                        )?),
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
    Ok(RepoTree {
        docs,
        extra: outcome.extra.clone(),
    })
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
    fn acquire(
        &mut self,
        request: &LockRequest,
        break_lock: bool,
    ) -> Result<Option<(Option<LockOwner>, StaleReason)>> {
        let lock = match self.tablet.acquire_lock(request, break_lock) {
            Err(error) if is_transport(&error) => self.tablet.acquire_lock(request, break_lock)?,
            other => other?,
        };
        let took_over = lock
            .took_over()
            .map(|(owner, reason)| (owner.cloned(), reason));
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
    fn finish(&mut self) -> Vec<String> {
        let Some(lock) = self.lock.take() else {
            return Vec::new();
        };
        let disarm = self.tablet.disarm_watchdog(&lock).err();
        let release = self.tablet.release_lock(&lock).err();
        [
            disarm.map(|e| format!("could not disarm the watchdog: {e}")),
            release.map(|e| format!("could not release the tablet lock: {e}")),
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
        let running = match self.maybe_stopped {
            true => self.tablet.start_xochitl(&lock).is_ok(),
            false => true,
        };
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
    took_over: Option<(Option<LockOwner>, StaleReason)>,
    reconciled: Option<Reconciled>,
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
    /// success, disarm and release (warnings returned).
    fn session<T>(
        &self,
        tablet: &dyn Tablet,
        body: impl FnOnce(&mut Session) -> Result<T>,
    ) -> Result<(T, Vec<String>)> {
        let mut session = Session::new(tablet, self.options.watchdog);
        let result = body(&mut session);
        let result = match result {
            Ok(value) => Ok((value, session.finish())),
            Err(error) => {
                session.abort();
                Err(error)
            }
        };
        self.progress.finished();
        result
    }

    // ---- init ---------------------------------------------------------------------

    /// `rmu hub init`: give an empty origin its `.rmu/hub.json` (a new repo
    /// id, the newest mirror rules) as the first commit of `main`. A local
    /// path that does not exist yet becomes a new bare repository.
    pub fn init(&self, origin: &str) -> Result<InitReport> {
        check_url(origin)?;
        let local = Path::new(origin);
        if !origin.contains("://") && !local.exists() && !origin.contains(':') {
            self.git.init_bare(local, crate::git::ObjectFormat::Sha1)?;
        }
        let config = RepoConfig::new(RepoId::random());
        let cache = store::open_cache(&self.git, &self.cache_dir, &config.repo_id, origin)?;
        let remote = store::fetch(&cache.repo, origin, None)?;
        if remote.main.is_some() {
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
        self.progress.finished();
        Ok(InitReport { config, commit })
    }
}
