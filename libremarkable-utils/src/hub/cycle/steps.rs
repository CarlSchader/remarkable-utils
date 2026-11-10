//! The cycle's steps: `sync`, `clone` and `backup` (see the parent
//! module for the step list).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;

use serde_json::Value;

use super::message::{self, Author, MergeRecord};
use super::plan::{self, Change, Content, JsonBlobs, LocalJson, Planned, Reread};
use super::snapshot::{self, Known, ReadMode, Snapshot, TabletRead};
use super::store::{self, Store};
use super::{
    BackupReport, CycleError, Hub, MainUpdate, OriginCtx, Overlay, Prepared, Pushed, Reconciled,
    Session, SyncReport, Warning, check_url, commit_tree, default_name, quarantine_report,
    read_summary, rmu_signature, tablet_versions, tree_items, unix_now, virtual_base, xochitl_path,
};
use crate::error::{Error, Result};
use crate::git::{CommitMeta, Expected, Oid, Repo};
use crate::hub::device::{
    ApplyPlan, DeviceError, DeviceInfo, DocApply, LockState, StagedFile, StatEntry, Tablet,
};
use crate::hub::merge::{self, BlobSource, TreeItem};
use crate::hub::model::{
    Classification, DeviceId, DeviceName, DocFile, DocId, DocOutcome, DocVersion, Journal,
    JsonFile, ManifestEntry, MirrorRules, MirroredFile, RepoId, RepoTree, StatSignature,
    TabletState, XochitlPath,
};

/// Staging uploads go in batches of about this many bytes, with the
/// watchdog re-armed before each: a timeout then bounds one batch, not a
/// whole (first-clone sized) upload, which would otherwise fire it
/// mid-transfer on every attempt.
const STAGE_BATCH_BYTES: u64 = 64 * 1024 * 1024;

/// The device ref to push to, its current head (the snapshot's second
/// parent) and the lease. A head that does not continue the recorded
/// one (rewritten or replaced), or none where one was recorded (origin
/// moved without `refs/rmu/*`), stops the run (see "Device ref
/// bookkeeping"); `clone --reattach` is the way out.
pub(super) fn device_ref(
    origin: &OriginCtx,
    state: &TabletState,
) -> Result<(String, Option<Oid>, Expected)> {
    let [primary, _] = store::device_ref_names(&state.device_id);
    let conflict = |reference: &str, remote: String| -> Error {
        CycleError::DeviceRefConflict {
            reference: reference.to_string(),
            recorded: state
                .device_ref
                .as_ref()
                .map_or_else(|| "none".to_string(), Oid::to_string),
            remote,
        }
        .into()
    };
    match (&state.device_ref, &origin.remote.device) {
        (None, None) => Ok((primary, None, Expected::Absent)),
        (Some(recorded), None) => Err(CycleError::DeviceRefMissing {
            reference: primary,
            recorded: recorded.clone(),
        }
        .into()),
        // A first push whose recording was lost (device ids are random
        // per clone, and a clone never reuses another tablet's state).
        (None, Some(remote)) => Ok((
            remote.name.clone(),
            Some(remote.oid.clone()),
            Expected::At(remote.oid.clone()),
        )),
        (Some(recorded), Some(remote)) => {
            let continues = *recorded == remote.oid
                || (origin.store.has_commit(recorded)?
                    && origin.cache.repo.is_ancestor(recorded, &remote.oid)?);
            if continues {
                Ok((
                    remote.name.clone(),
                    Some(remote.oid.clone()),
                    Expected::At(remote.oid.clone()),
                ))
            } else {
                Err(conflict(&remote.name, remote.oid.to_string()))
            }
        }
    }
}

/// The manifest after a run that wrote nothing.
fn read_manifest(read: &TabletRead, started_at: i64) -> BTreeMap<XochitlPath, ManifestEntry> {
    plan::manifest(&plan::ManifestInput {
        read: &read.stats(),
        read_reference_ns: started_at,
        before_apply: None,
        written: &BTreeMap::new(),
        removed: &BTreeSet::new(),
        written_reference_ns: 0,
    })
}

/// This tablet as the author of its commits.
fn author<'a>(state: &'a TabletState, info: &'a DeviceInfo) -> Author<'a> {
    Author {
        name: &state.device_name,
        id: &state.device_id,
        model: info.model.as_deref(),
        firmware: info.firmware.as_deref(),
    }
}

/// The tablet's `.metadata`/`.content` bytes as read (not committed to
/// the cache yet) in front of the cache.
struct ReadBlobs<'a> {
    bytes: HashMap<&'a Oid, &'a [u8]>,
    store: &'a Store,
}

impl<'a> ReadBlobs<'a> {
    fn new(read: &'a TabletRead, store: &'a Store) -> Self {
        Self {
            bytes: read
                .files
                .iter()
                .filter_map(|(path, file)| Some((&file.blob, read.json.get(path)?.as_slice())))
                .collect(),
            store,
        }
    }
}

impl BlobSource for ReadBlobs<'_> {
    fn read(&self, id: &Oid) -> Result<Vec<u8>> {
        match self.bytes.get(id) {
            Some(bytes) => Ok(bytes.to_vec()),
            None => self.store.blob(id),
        }
    }
}

/// `(original, copy)` of the forks recorded on `main` since `snapshot`
/// (this merge's own and other tablets'): fork groups stay units even
/// when a later merge re-derives one as two unrelated documents.
fn recorded_forks(store: &Store, tip: &Oid, snapshot: &Oid) -> Result<Vec<(DocId, DocId)>> {
    let since = format!("^{snapshot}");
    Ok(store
        .messages(&[tip.as_str(), &since], false)?
        .iter()
        .flat_map(|text| message::forks(&message::trailers(text)))
        .collect())
}

/// What `clone --reattach` does with an interrupted sync's journal. A
/// merge origin's `main` (`main`, of repository `repo_id`) has: refused
/// (`JournalPending`), since only that journal's reconciliation knows
/// which documents its apply stopped inside, and `sync` runs it. Any
/// other journal (origin was rewound or replaced, or the merge never
/// landed) can never be reconciled: discarded, with a warning.
fn pending_journal(
    existing: Option<&TabletState>,
    repo_id: &RepoId,
    store: &Store,
    main: &Oid,
) -> Result<Option<Warning>> {
    let Some((state, journal)) =
        existing.and_then(|state| state.journal.as_ref().map(|journal| (state, journal)))
    else {
        return Ok(None);
    };
    if state.repo_id == *repo_id && store.is_ancestor(&journal.merge, main)? {
        return Err(CycleError::JournalPending.into());
    }
    Ok(Some(Warning::JournalDiscarded))
}

/// String paths (from the pure helpers) as tablet paths.
fn tablet_paths(paths: BTreeSet<String>) -> Result<BTreeSet<XochitlPath>> {
    paths
        .into_iter()
        .map(|path| Ok(XochitlPath::new(path)?))
        .collect()
}

/// The step-14 view of the tablet.
struct Recheck {
    /// Mirrored regular files and their stats now.
    now: BTreeMap<XochitlPath, StatSignature>,
    /// Mirrored files per document now.
    files_now: BTreeMap<DocId, BTreeSet<DocFile>>,
    /// Re-read files of planned documents whose stat moved.
    moved: BTreeMap<DocId, BTreeMap<DocFile, Reread>>,
    /// Re-read `.metadata`/`.content` (moved or new) of any document;
    /// `None`: gone or unparsable.
    json: BTreeMap<(DocId, JsonFile), Option<Value>>,
    /// Documents with any file moved, new or gone since the read.
    touched: BTreeSet<DocId>,
}

/// What an apply did, for the record.
#[derive(Default)]
struct Applied {
    applied: Vec<DocId>,
    skipped: BTreeSet<DocId>,
    /// The step-14 stat pass.
    before: Option<BTreeMap<XochitlPath, StatSignature>>,
    written: BTreeMap<XochitlPath, (StatSignature, Option<Oid>)>,
    removed: BTreeSet<XochitlPath>,
    written_at: i64,
}

/// Everything step 12 onwards works on.
struct ApplyInput<'a> {
    snapshot: &'a Snapshot,
    snapshot_commit: &'a Oid,
    pushed: &'a Pushed,
    planned: Vec<Planned>,
    json: &'a dyn JsonBlobs,
    local: BTreeMap<DocId, LocalJson>,
    tablet: &'a BTreeMap<DocId, DocVersion>,
    protect: &'a BTreeSet<DocId>,
}

impl Hub<'_> {
    /// `rmu hub sync`: the full cycle.
    pub fn sync(&self, tablet: &dyn Tablet) -> Result<SyncReport> {
        let (mut report, warnings) = self.session(tablet, |s| {
            let prepared = self.prepare(s, true)?;
            let (report, origin) = self.integrate(s, prepared)?;
            Ok((report, Some(origin)))
        })?;
        report.warnings.extend(warnings);
        Ok(report)
    }

    /// `rmu hub backup`: steps 1–7 only. Skips reconciliation (it may
    /// write files) and never writes documents; records the device ref,
    /// the manifest and the quarantine.
    pub fn backup(&self, tablet: &dyn Tablet) -> Result<BackupReport> {
        let (mut report, warnings) = self.session(tablet, |s| {
            let mut p = self.prepare(s, false)?;
            let base_tree = virtual_base(&p.origin.store, p.origin.rules, &p.state)?;
            let snapshot = snapshot::build(p.origin.rules, &p.read, &base_tree)?;
            self.progress.step("Backing up the tablet to origin");
            let (commit, pushed) = self.backup_snapshot(s, &mut p, &snapshot)?;
            p.state.manifest = read_manifest(&p.read, p.started_at);
            p.state.quarantine = snapshot.quarantined.keys().cloned().collect();
            s.write_state(&p.state)?;
            let report = BackupReport {
                snapshot: commit,
                pushed,
                quarantined: quarantine_report(&snapshot),
                orphans: snapshot.orphans.iter().cloned().collect(),
                read: read_summary(&p.read),
                took_over: p.took_over.take(),
                warnings: Vec::new(),
            };
            Ok((report, Some(p.origin)))
        })?;
        report.warnings.extend(warnings);
        Ok(report)
    }

    /// `rmu hub clone`: attach the tablet to `origin` and sync. Documents
    /// whose version is, by the merge's own rules ([`merge::unchanged`]),
    /// one in origin's history (ancestors of `main`, newest first, never
    /// device refs) take that commit as their base and fast-forward; the
    /// others have the empty base ("new on both"). `reattach` does the
    /// same for a tablet that already has hub state (lost, or for a
    /// replaced origin), keeping its device id and name; it refuses while
    /// a journal for the same repository is pending (`sync` reconciles
    /// it). `origin` is used as given: [`super::HubOptions::origin`] does
    /// not apply.
    pub fn clone_tablet(
        &self,
        tablet: &dyn Tablet,
        origin: &str,
        name: Option<DeviceName>,
        reattach: bool,
    ) -> Result<SyncReport> {
        check_url(origin)?;
        let (mut report, warnings) = self.session(tablet, |s| {
            self.progress.step("Checking origin");
            let (cache, main) = store::bootstrap_cache(&self.git, &self.cache_dir, origin)?;
            let probe = Store::new(cache.repo.clone())?;
            let config = probe.repo_config(&main)?;
            let rules = config.rules()?;
            // Read-only refusals first, so a tablet that would be refused
            // under the lock gets no (full) pre-read; checked again there.
            if s.tablet.cloud_paired()? {
                return Err(CycleError::CloudPaired.into());
            }
            let before = s.tablet.read_state()?;
            if let Some(existing) = before.as_ref().filter(|_| !reattach) {
                return Err(CycleError::AlreadyAttached {
                    repo_id: existing.repo_id.clone(),
                }
                .into());
            }
            pending_journal(before.as_ref(), &config.repo_id, &probe, &main)?;
            let pre = if self.may_pre_read(s.tablet)? {
                let saved = store::pre_read_path(
                    &self.cache_dir,
                    &config.repo_id,
                    before.as_ref().map(|state| &state.device_id),
                );
                self.pre_read(
                    s.tablet,
                    rules,
                    &BTreeMap::new(),
                    &cache.repo,
                    &probe,
                    &saved,
                )
            } else {
                TabletRead::default()
            };
            drop(probe);
            self.progress.step("Locking the tablet");
            let took_over = s.acquire(&self.request, self.options.break_lock)?;
            s.arm()?;
            if s.tablet.cloud_paired()? {
                return Err(CycleError::CloudPaired.into());
            }
            let existing = s.tablet.read_state()?;
            if let Some(existing) = existing.as_ref().filter(|_| !reattach) {
                return Err(CycleError::AlreadyAttached {
                    repo_id: existing.repo_id.clone(),
                }
                .into());
            }
            let info = s.tablet.device_info()?;
            let device_id = existing
                .as_ref()
                .map_or_else(DeviceId::random, |state| state.device_id.clone());
            let name = name
                .or_else(|| existing.as_ref().map(|state| state.device_name.clone()))
                .unwrap_or_else(|| default_name(&info, &device_id));
            let ctx = OriginCtx::new(cache, origin.to_string(), Some(&device_id), None)?;
            // Pinned from the first fetch: the pre-read was classified
            // under these rules.
            if ctx.config != config {
                return Err(CycleError::RepoConfigChanged.into());
            }
            let discarded =
                pending_journal(existing.as_ref(), &config.repo_id, &ctx.store, &ctx.main)?;
            self.progress.step("Stopping xochitl");
            s.stop()?;
            let mut writer = ctx.cache.repo.commit_writer(self.progress, None)?;
            let read = snapshot::read_tablet(
                s.tablet,
                rules,
                &snapshot::pre_read_known(&pre),
                &ctx.store,
                snapshot::Sink::plain(&mut writer),
                self.progress,
                ReadMode::Strict,
            )?;
            self.progress.step("Starting xochitl");
            let started_at = s.start()?;
            s.arm()?;
            self.progress
                .step("Matching the tablet's documents with origin's history");
            let tablet_only = snapshot::build(rules, &read, &RepoTree::default())?;
            let overrides = infer_bases(
                &ctx.store,
                rules,
                &ctx.main,
                &tablet_only,
                &ReadBlobs::new(&read, &ctx.store),
            )?;
            let mut warnings: Vec<Warning> = discarded
                .into_iter()
                .chain(mixed_model_warning(&ctx.store, &ctx.main, &info)?)
                .collect();
            let mut state = TabletState::new(
                ctx.config.repo_id.clone(),
                origin.to_string(),
                device_id,
                name,
            );
            state.base_overrides = overrides
                .iter()
                .map(|(doc, commit)| (doc.clone(), Some(commit.clone())))
                .collect();
            state.quarantine = tablet_only.quarantined.keys().cloned().collect();
            state.manifest = read_manifest(&read, started_at);
            // Attached from here on: an interrupted clone continues with
            // `sync`.
            s.write_state(&state)?;
            let prepared = Prepared {
                origin: ctx,
                state,
                read,
                writer: Some(writer),
                started_at,
                info,
                took_over,
                reconciled: None,
            };
            let (mut report, origin) = self.integrate(s, prepared)?;
            warnings.append(&mut report.warnings);
            report.warnings = warnings;
            report.inferred = overrides;
            Ok((report, Some(origin)))
        })?;
        report.warnings.extend(warnings);
        Ok(report)
    }

    /// A best-effort read while xochitl runs and before the lock (reads
    /// need none), into a commit of its own that nothing points at: the
    /// stopped read then downloads only files whose stat moved since, so
    /// the UI is down for seconds even when the tablet has gigabytes of
    /// new data. `fast-import` is checkpointed about every
    /// [`super::HubOptions::pre_read_checkpoint`] bytes and what is
    /// durable is saved at `saved`, so a cut (even mid-file, which ends
    /// the `fast-import`) loses at most that much, in this run and the
    /// next: the files an earlier pre-read saved count as known too.
    /// Every entry is checked against the tablet's stat (nanosecond
    /// mtimes only) and the cache before the stopped read trusts it.
    pub(super) fn pre_read(
        &self,
        tablet: &dyn Tablet,
        rules: &MirrorRules,
        manifest: &BTreeMap<XochitlPath, ManifestEntry>,
        repo: &Repo,
        store: &Store,
        saved: &std::path::Path,
    ) -> TabletRead {
        let known: BTreeMap<XochitlPath, Known> = snapshot::known_from_manifest(manifest)
            .into_iter()
            .chain(snapshot::saved_known(&store::load_pre_read(saved)))
            .collect();
        let mut save = |entries: &BTreeMap<XochitlPath, ManifestEntry>| {
            store::save_pre_read(saved, entries);
        };
        let Ok(mut writer) = repo.commit_writer(self.progress, None) else {
            return TabletRead::default();
        };
        let read = snapshot::read_tablet(
            tablet,
            rules,
            &known,
            store,
            snapshot::Sink {
                writer: &mut writer,
                checkpoints: Some(snapshot::Checkpoints {
                    every: self.options.pre_read_checkpoint.max(1),
                    save: &mut save,
                }),
            },
            self.progress,
            ReadMode::BestEffort,
        )
        .unwrap_or_default();
        let time = unix_now();
        let committed = writer.commit(&CommitMeta {
            parents: Vec::new(),
            author: rmu_signature(time),
            committer: rmu_signature(time),
            message: "rmu hub: pre-read (not pushed)\n".to_string(),
        });
        if committed.is_ok() {
            save(&read.manifest_entries());
        }
        read
    }

    /// Whether to pre-read: not while another run holds the tablet lock
    /// (unless it is being broken), since this run would most likely be
    /// refused.
    fn may_pre_read(&self, tablet: &dyn Tablet) -> Result<bool> {
        Ok(self.options.break_lock || tablet.lock_state()? == LockState::Free)
    }

    /// Steps 1–6 (with step 4 only if `reconcile`).
    fn prepare<'p>(&'p self, s: &mut Session, reconcile: bool) -> Result<Prepared<'p>> {
        self.progress.step("Checking origin");
        let first = s.tablet.read_state()?.ok_or(CycleError::NotAttached)?;
        let url = self.origin_url(&first.origin)?;
        let (cache, refs) = store::open_cache(&self.git, &self.cache_dir, &first.repo_id, &url)?;
        let mut origin = OriginCtx::new(cache, url, Some(&first.device_id), Some(refs))?;
        origin.check(&first)?;
        let rules = origin.rules;
        // Read-only refusals first (checked again under the lock): a
        // paired tablet is refused, and a locked one is not downloaded in
        // full for nothing.
        if s.tablet.cloud_paired()? {
            return Err(CycleError::CloudPaired.into());
        }
        let pre = if self.may_pre_read(s.tablet)? {
            self.pre_read(
                s.tablet,
                rules,
                &first.manifest,
                &origin.cache.repo,
                &origin.store,
                &store::pre_read_path(&self.cache_dir, &first.repo_id, Some(&first.device_id)),
            )
        } else {
            TabletRead::default()
        };
        self.progress.step("Locking the tablet");
        let took_over = s.acquire(&self.request, self.options.break_lock)?;
        s.arm()?;
        if s.tablet.cloud_paired()? {
            return Err(CycleError::CloudPaired.into());
        }
        let mut state = s.tablet.read_state()?.ok_or(CycleError::NotAttached)?;
        if state.repo_id != first.repo_id
            || state.device_id != first.device_id
            || state.origin != first.origin
        {
            return Err(CycleError::StateChanged.into());
        }
        // Under the lock: a run that held it before this one may have
        // pushed since step 1 (a journal must be judged against that, and
        // a newer base or device ref must not read as a rewound origin).
        origin.refresh(&state.device_id)?;
        origin.check(&state)?;
        let info = s.tablet.device_info()?;
        self.progress.step("Stopping xochitl");
        s.stop()?;
        let known: BTreeMap<XochitlPath, Known> = snapshot::known_from_manifest(&state.manifest)
            .into_iter()
            .chain(snapshot::pre_read_known(&pre))
            .collect();
        let mut writer = origin.cache.repo.commit_writer(self.progress, None)?;
        let mut read = snapshot::read_tablet(
            s.tablet,
            rules,
            &known,
            &origin.store,
            snapshot::Sink::plain(&mut writer),
            self.progress,
            ReadMode::Strict,
        )?;
        let reconciled = match state.journal.clone().filter(|_| reconcile) {
            Some(journal) => {
                self.progress.step("Reconciling an interrupted sync");
                let (reconciled, wrote) =
                    self.reconcile(s, &origin, &mut state, &journal, &read)?;
                if wrote {
                    s.arm()?;
                    read = snapshot::read_tablet(
                        s.tablet,
                        rules,
                        &read.as_known(),
                        &origin.store,
                        snapshot::Sink::plain(&mut writer),
                        self.progress,
                        ReadMode::Strict,
                    )?;
                }
                Some(reconciled)
            }
            None => None,
        };
        self.progress.step("Starting xochitl");
        let started_at = s.start()?;
        Ok(Prepared {
            origin,
            state,
            read,
            writer: Some(writer),
            started_at,
            info,
            took_over,
            reconciled,
        })
    }

    /// Step 4 (see "Restartability"): with xochitl stopped, judge every
    /// document whose journaled merge and snapshot versions differ by the
    /// files just read and by what the interrupted runs left staged
    /// (`plan::verdicts`), finish an apply that stopped inside a document,
    /// and write the resulting bases (journal cleared) before anything
    /// else. Returns whether it wrote documents.
    ///
    /// A merge origin no longer has but that staging proves landed (only
    /// a push that landed is ever staged for) means origin was rewound:
    /// stop. A pull-only journal (its snapshot is no ancestor of its
    /// merge, which is origin's `main` as fetched) keeps the documents
    /// still at their snapshot version on their previous base, which
    /// holds the same version and, unlike the snapshot, is on `main`.
    fn reconcile(
        &self,
        s: &mut Session,
        origin: &OriginCtx,
        state: &mut TabletState,
        journal: &Journal,
        read: &TabletRead,
    ) -> Result<(Reconciled, bool)> {
        let (store, rules) = (&origin.store, origin.rules);
        let landed = store.has_commit(&journal.merge)?
            && origin
                .cache
                .repo
                .is_ancestor(&journal.merge, &origin.main)?;
        let listing = s.tablet.staged_files()?;
        let staged = plan::staged_runs(
            listing.entries.iter().map(|entry| entry.path.as_str()),
            &journal.merge,
        );
        if !landed && !staged.is_empty() {
            return Err(CycleError::OriginRewound {
                base: journal.merge.clone(),
            }
            .into());
        }
        if !landed {
            // The push never landed: the tablet's bases are unchanged and
            // its edits are picked up by this run's snapshot.
            state.journal = None;
            s.write_state(state)?;
            return Ok((
                Reconciled {
                    dropped: true,
                    ..Reconciled::default()
                },
                false,
            ));
        }
        if !store.has_commit(&journal.snapshot)? {
            return Err(CycleError::MissingObject(journal.snapshot.clone()).into());
        }
        let merge_tree = store.tree_of(rules, &journal.merge)?;
        let snapshot_tree = store.tree_of(rules, &journal.snapshot)?;
        let json = Overlay::read(read, store);
        let tablet = read.doc_files();
        let forks = message::forks(&message::trailers(&store.commit(&journal.merge)?.message));
        let journaled = plan::Journaled {
            rules,
            merge: &merge_tree,
            snapshot: &snapshot_tree,
            tablet: &tablet,
            staged: &staged,
            forks: &forks,
            json: &json,
        };
        let verdicts = plan::verdicts(&journaled)?;
        // The step-14 tree checks, against the tablet as just read.
        let (finish, skipped) = plan::finish(&plan::Finish {
            journaled: &journaled,
            verdicts: &verdicts,
            local: &read.local_json(),
            current: &read.items(),
            merged: &tree_items(&merge_tree, &json)?,
        })?;
        let finished: Vec<DocId> = finish.iter().map(|planned| planned.doc.clone()).collect();
        if !finish.is_empty() {
            // Into this run's own staging directory, with a sentinel of its
            // own: the interrupted run's directory is what this judgement
            // rests on until the state is written, and a crash before that
            // must find these documents finished too.
            let sizes = self.stage(s, store, &finish, Some(&journal.merge))?;
            s.arm()?;
            s.tablet.apply(s.lock(), &apply_plan(&finish, &sizes)?)?;
        }
        let kept: Vec<DocId> = verdicts
            .iter()
            .filter(|(doc, verdict)| {
                matches!(verdict, plan::Verdict::AtSnapshot | plan::Verdict::Other)
                    || skipped.contains(*doc)
            })
            .map(|(doc, _)| doc.clone())
            .collect();
        let pull_only = !origin
            .cache
            .repo
            .is_ancestor(&journal.snapshot, &journal.merge)?;
        let previous = state.clone();
        state.base = Some(journal.merge.clone());
        // An explicit override even where the snapshot lacks the
        // document: falling back to the default base would read it as
        // deleted by the tablet.
        state.base_overrides = kept
            .iter()
            .map(|doc| {
                let base = if pull_only {
                    previous.base_for(doc).cloned()
                } else {
                    Some(journal.snapshot.clone())
                };
                (doc.clone(), base)
            })
            .collect();
        state.journal = None;
        s.write_state(state)?;
        Ok((
            Reconciled {
                dropped: false,
                finished: finished.clone(),
                kept,
            },
            !finished.is_empty(),
        ))
    }

    /// Upload the planned writes into this run's staging directory, in
    /// batches with the watchdog re-armed before each, then, for an apply
    /// of `merge`, the sentinel naming the planned documents
    /// (`plan::sentinel_files`); the size of each staged file.
    fn stage(
        &self,
        s: &Session,
        store: &Store,
        planned: &[Planned],
        merge: Option<&Oid>,
    ) -> Result<BTreeMap<XochitlPath, u64>> {
        let contents: Vec<(XochitlPath, &Content)> = planned
            .iter()
            .flat_map(|entry| match &entry.change {
                Change::Write { writes, .. } => writes
                    .iter()
                    .map(|write| Ok((xochitl_path(&entry.doc, &write.file)?, &write.content)))
                    .collect(),
                Change::Delete => Vec::new(),
            })
            .collect::<Result<_>>()?;
        let blob_sizes = store
            .repo
            .object_sizes(contents.iter().filter_map(|(_, content)| match content {
                Content::Blob(oid) => Some(oid),
                Content::Bytes(_) => None,
            }))?;
        let size_of = |content: &Content| match content {
            Content::Blob(oid) => blob_sizes.get(oid).copied().unwrap_or_default(),
            Content::Bytes(bytes) => bytes.len() as u64,
        };
        let total: u64 = contents.iter().map(|(_, content)| size_of(content)).sum();
        let batches = contents.iter().fold(
            Vec::<(Vec<&(XochitlPath, &Content)>, u64)>::new(),
            |mut batches, item| {
                let size = size_of(item.1);
                match batches.last_mut() {
                    Some((batch, bytes)) if *bytes + size <= STAGE_BATCH_BYTES => {
                        batch.push(item);
                        *bytes += size;
                    }
                    _ => batches.push((vec![item], size)),
                }
                batches
            },
        );
        let mut sizes = BTreeMap::new();
        let mut transferred = 0u64;
        batches.iter().try_for_each(|(batch, _)| -> Result<()> {
            s.arm()?;
            let files: Vec<XochitlPath> = batch.iter().map(|(path, _)| path.clone()).collect();
            let content: BTreeMap<&XochitlPath, &Content> = batch
                .iter()
                .map(|(path, content)| (path, *content))
                .collect();
            s.tablet.stage(s.lock(), &files, &mut |path, sink| {
                let mut send = |size: u64, reader: &mut dyn std::io::Read| -> Result<()> {
                    sizes.insert(path.clone(), size);
                    sink(size, reader)?;
                    transferred += size;
                    self.progress.bytes(transferred, Some(total));
                    Ok(())
                };
                match content.get(path) {
                    Some(Content::Blob(oid)) => store.stream_blob(oid, &mut send),
                    Some(Content::Bytes(bytes)) => send(bytes.len() as u64, &mut &bytes[..]),
                    None => Err(DeviceError::Stage {
                        path: path.to_string(),
                        detail: "not planned".to_string(),
                    }
                    .into()),
                }
            })
        })?;
        if let Some(merge) = merge {
            let sentinel =
                plan::sentinel_files(merge, planned.iter().map(|entry| entry.doc.clone()))
                    .into_iter()
                    .map(XochitlPath::new)
                    .collect::<std::result::Result<Vec<_>, _>>()?;
            s.arm()?;
            s.tablet.stage(s.lock(), &sentinel, &mut |_, sink| {
                sink(0, &mut std::io::empty())
            })?;
        }
        self.progress.bytes_done();
        Ok(sizes)
    }

    /// Step 7: commit the snapshot (parents: base, device ref head) and
    /// push the device ref with a lease, then record it on the tablet.
    /// A device ref already holding this very tree is reused. Returns the
    /// snapshot commit and whether it was pushed.
    fn backup_snapshot(
        &self,
        s: &mut Session,
        p: &mut Prepared,
        snapshot: &Snapshot,
    ) -> Result<(Oid, bool)> {
        let (reference, head, lease) = device_ref(&p.origin, &p.state)?;
        let author = author(&p.state, &p.info);
        let quarantined: BTreeSet<DocId> = snapshot.quarantined.keys().cloned().collect();
        let parents: Vec<Oid> = p.state.base.iter().chain(head.iter()).cloned().collect();
        let writer = p.writer.take().expect("one snapshot per run");
        // Always written: it also stores every blob the reads streamed.
        let commit = commit_tree(
            writer,
            &snapshot.tree,
            &Self::meta(&p.state, parents, message::snapshot(&author, &quarantined)),
        )?;
        let store = &p.origin.store;
        if let Some(head) = &head
            && store.commit(head)?.tree == store.commit(&commit)?.tree
        {
            if p.state.device_ref.as_ref() != Some(head) {
                p.state.device_ref = Some(head.clone());
                s.write_state(&p.state)?;
            }
            return Ok((head.clone(), false));
        }
        s.arm()?;
        let local = store::local_device_ref(&p.state.device_id);
        let repo = &p.origin.cache.repo;
        match store::push(
            repo,
            &p.origin.url,
            &commit,
            &reference,
            Some(&lease),
            &local,
        ) {
            // A forge that refuses custom refs gets the branch-shaped name.
            Err(Error::PushFailed { .. }) if head.is_none() => {
                let [_, fallback] = store::device_ref_names(&p.state.device_id);
                store::push(
                    repo,
                    &p.origin.url,
                    &commit,
                    &fallback,
                    Some(&Expected::Absent),
                    &local,
                )?;
            }
            other => other?,
        }
        p.state.device_ref = Some(commit.clone());
        s.write_state(&p.state)?;
        Ok((commit, true))
    }

    /// Steps 7–16; the origin handed back for maintenance.
    fn integrate(&self, s: &mut Session, mut p: Prepared) -> Result<(SyncReport, OriginCtx)> {
        let rules = p.origin.rules;
        let base_tree = virtual_base(&p.origin.store, rules, &p.state)?;
        let snapshot = snapshot::build(rules, &p.read, &base_tree)?;
        self.progress.step("Backing up the tablet to origin");
        let (snapshot_commit, _) = self.backup_snapshot(s, &mut p, &snapshot)?;
        let pushed = self.merge_and_push(s, &mut p, &snapshot, &base_tree, &snapshot_commit)?;
        self.apply_and_record(s, p, &snapshot, &snapshot_commit, pushed)
    }

    /// Steps 8–11: fetch, check, merge, guard, journal, push; back to 8
    /// when the push loses to another writer.
    fn merge_and_push(
        &self,
        s: &mut Session,
        p: &mut Prepared,
        snapshot: &Snapshot,
        base_tree: &RepoTree,
        snapshot_commit: &Oid,
    ) -> Result<Pushed> {
        let attempts = self.options.push_attempts.max(1);
        let outcome = (1..=attempts).try_fold((), |(), attempt| {
            match self.attempt_push(s, p, snapshot, base_tree, snapshot_commit, attempt) {
                Ok(Some(pushed)) => ControlFlow::Break(Ok(pushed)),
                Ok(None) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(Err(error)),
            }
        });
        match outcome {
            ControlFlow::Break(result) => result,
            ControlFlow::Continue(()) => Err(CycleError::PushRetriesExhausted(attempts).into()),
        }
    }

    /// One pass of steps 8–11; `None` when the push lost to another
    /// writer.
    fn attempt_push(
        &self,
        s: &mut Session,
        p: &mut Prepared,
        snapshot: &Snapshot,
        base_tree: &RepoTree,
        snapshot_commit: &Oid,
        attempt: usize,
    ) -> Result<Option<Pushed>> {
        s.arm()?;
        self.progress.step("Fetching origin");
        p.origin.refresh(&p.state.device_id)?;
        p.origin.check(&p.state)?;
        self.progress.step("Merging");
        let run = self.run_merge(&p.origin, &p.state, base_tree, snapshot, &p.read)?;
        let outcome = run.outcome;
        if outcome.guard.blocked() {
            return Err(CycleError::DeletionsNeedConsent {
                from_origin: outcome.guard.from_origin.iter().cloned().collect(),
                from_tablet: outcome.guard.from_tablet.iter().cloned().collect(),
                empty_snapshot: outcome.guard.empty_snapshot,
            }
            .into());
        }
        let store = &p.origin.store;
        let main = p.origin.main.clone();
        // The tablet already agrees with origin: nothing to push, even if
        // the merge took the tablet's bytes for documents that differ only
        // in view state or derived fields (pushing those would make two
        // tablets push each other's view state on every sync). Nothing to
        // apply either (the plan is empty): no journal.
        if run.up_to_date {
            return Ok(Some(Pushed {
                update: MainUpdate::UpToDate,
                base: main,
                merged: run.theirs,
                outcome,
                attempts: attempt,
            }));
        }
        // The tablet has no changes of its own: origin's tree is applied
        // as it is, with no merge commit (the snapshot is on the device
        // ref, for the journal). Documents that keep the snapshot as their
        // base keep their previous one instead, which holds the same
        // version and is on `main`.
        if run.pull_only {
            p.state.journal = Some(Journal {
                merge: main.clone(),
                snapshot: snapshot_commit.clone(),
            });
            s.write_state(&p.state)?;
            return Ok(Some(Pushed {
                update: MainUpdate::Pulled,
                base: main,
                merged: run.theirs,
                outcome,
                attempts: attempt,
            }));
        }
        let record = MergeRecord {
            notes: outcome.notes.iter().map(ToString::to_string).collect(),
            forks: outcome
                .forks
                .iter()
                .map(|fork| (fork.original.clone(), fork.copy.clone()))
                .collect(),
            consented: outcome.guard.consented(),
        };
        let (merged, writer) = (run.merged, run.writer);
        let listing = merged.to_listing();
        let first_parent = store.commit(snapshot_commit)?.parents.first().cloned();
        let (update, commit) = if listing == snapshot.tree.to_listing()
            && first_parent.as_ref() == Some(&main)
            && record.is_empty()
        {
            drop(writer);
            (
                MainUpdate::FastForward(snapshot_commit.clone()),
                snapshot_commit.clone(),
            )
        } else {
            let commit = commit_tree(
                writer,
                &merged,
                &Self::meta(
                    &p.state,
                    vec![main.clone(), snapshot_commit.clone()],
                    message::merge(&author(&p.state, &p.info), &record),
                ),
            )?;
            (MainUpdate::Merge(commit.clone()), commit)
        };
        // 10. Durably, before the push attempt: from here on origin may
        // know a merge the tablet has not applied.
        p.state.journal = Some(Journal {
            merge: commit.clone(),
            snapshot: snapshot_commit.clone(),
        });
        s.write_state(&p.state)?;
        self.progress.step("Pushing to origin");
        let pushed = Pushed {
            update,
            base: commit.clone(),
            merged,
            outcome,
            attempts: attempt,
        };
        match store::push(
            &p.origin.cache.repo,
            &p.origin.url,
            &commit,
            store::MAIN,
            None,
            store::LOCAL_MAIN,
        ) {
            Ok(()) => Ok(Some(pushed)),
            Err(Error::PushRejected { .. }) => Ok(None),
            Err(error) => {
                // The acknowledgement may be what was lost: origin decides.
                let landed = p.origin.refresh(&p.state.device_id).is_ok()
                    && p.origin
                        .cache
                        .repo
                        .is_ancestor(&commit, &p.origin.main)
                        .unwrap_or(false);
                if landed { Ok(Some(pushed)) } else { Err(error) }
            }
        }
    }

    /// Step 14's re-read: a fresh stat pass, then the `.metadata` and
    /// `.content` whose stat moved or that are new (any document: the
    /// tree checks need them), and the SHA-256 of other moved files of
    /// planned documents. A step-5 stat with a coarse mtime in the second
    /// of the step-6 start counts as moved (xochitl could have changed
    /// the file within that second).
    fn recheck(
        &self,
        s: &Session,
        rules: &MirrorRules,
        read: &TabletRead,
        started_at: i64,
        planned: &BTreeSet<DocId>,
    ) -> Result<Recheck> {
        let listing = s.tablet.stat_all()?;
        let now: BTreeMap<XochitlPath, (MirroredFile, StatSignature)> = listing
            .entries
            .iter()
            .filter(|entry| entry.is_regular())
            .filter_map(|entry| match rules.classify(&entry.path) {
                Ok(Classification::Mirrored(file)) => Some(
                    XochitlPath::new(entry.path.as_str())
                        .map(|path| (path, (file, entry.stat)))
                        .map_err(Error::from),
                ),
                Ok(Classification::Excluded(_)) => None,
                Err(error) => Some(Err(error.into())),
            })
            .collect::<Result<_>>()?;
        let moved_paths = plan::moved_since_read(
            &read
                .files
                .iter()
                .map(|(path, file)| (path.clone(), file.stat))
                .collect(),
            &now.iter()
                .map(|(path, (_, stat))| (path.clone(), *stat))
                .collect(),
            started_at,
        );
        let file_of = |path: &XochitlPath| {
            now.get(path)
                .map(|(file, _)| file)
                .or_else(|| read.files.get(path).map(|read| &read.file))
        };
        let present_moved: Vec<(&XochitlPath, &MirroredFile)> = moved_paths
            .iter()
            .filter_map(|path| now.get(path).map(|(file, _)| (path, file)))
            .collect();
        let json_paths: Vec<XochitlPath> = present_moved
            .iter()
            .filter(|(_, file)| plan::json_kind(&file.file).is_some())
            .map(|(path, _)| (*path).clone())
            .collect();
        let hash_paths: Vec<XochitlPath> = present_moved
            .iter()
            .filter(|(_, file)| {
                plan::json_kind(&file.file).is_none() && planned.contains(&file.doc)
            })
            .map(|(path, _)| (*path).clone())
            .collect();
        let reread = s.tablet.read_files(&json_paths)?;
        let hashes = s.tablet.sha256(&hash_paths)?;
        let parse = |bytes: &Option<Vec<u8>>| {
            bytes
                .as_ref()
                .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        };
        // A moved `.metadata`/`.content` that is gone now counts as gone.
        let json = moved_paths
            .iter()
            .filter_map(|path| {
                let file = file_of(path)?;
                let kind = plan::json_kind(&file.file)?;
                Some(((file.doc.clone(), kind), reread.get(path).and_then(parse)))
            })
            .collect();
        let moved = present_moved
            .iter()
            .filter(|(_, file)| planned.contains(&file.doc))
            .filter_map(|(path, file)| {
                let reread = match plan::json_kind(&file.file) {
                    Some(_) => Reread::Json(reread.get(*path).and_then(parse)),
                    None => Reread::Sha256(hashes.get(*path).cloned().flatten()?),
                };
                Some((file.doc.clone(), file.file.clone(), reread))
            })
            .fold(
                BTreeMap::<DocId, BTreeMap<DocFile, Reread>>::new(),
                |mut moved, (doc, file, reread)| {
                    moved.entry(doc).or_default().insert(file, reread);
                    moved
                },
            );
        let touched = moved_paths
            .iter()
            .filter_map(|path| file_of(path).map(|file| file.doc.clone()))
            .collect();
        let files_now = now.values().fold(
            BTreeMap::<DocId, BTreeSet<DocFile>>::new(),
            |mut files, (file, _)| {
                files
                    .entry(file.doc.clone())
                    .or_default()
                    .insert(file.file.clone());
                files
            },
        );
        Ok(Recheck {
            now: now
                .into_iter()
                .map(|(path, (_, stat))| (path, stat))
                .collect(),
            files_now,
            moved,
            json,
            touched,
        })
    }

    /// Steps 12–16: stage, stop, re-read, verify, tree checks, apply,
    /// record, start.
    fn apply_and_record(
        &self,
        s: &mut Session,
        mut p: Prepared,
        snapshot: &Snapshot,
        snapshot_commit: &Oid,
        pushed: Pushed,
    ) -> Result<(SyncReport, OriginCtx)> {
        let protect: BTreeSet<DocId> = snapshot.quarantined.keys().cloned().collect();
        let tablet = tablet_versions(&p.read);
        let local = p.read.local_json();
        let (applied, left_alone) = {
            let json = Overlay::read(&p.read, &p.origin.store);
            let plan::PlanOutput {
                planned,
                left_alone,
            } = plan::plan(&plan::PlanInput {
                rules: p.origin.rules,
                merged: &pushed.merged,
                tablet: &tablet,
                protect: &protect,
                local: &local,
                json: &json,
            })?;
            debug_assert!(
                pushed.update != MainUpdate::UpToDate || planned.is_empty(),
                "a run that found the tablet up to date applies nothing (it has no journal)"
            );
            let applied = if planned.is_empty() {
                Applied::default()
            } else {
                self.apply_planned(
                    s,
                    &p,
                    ApplyInput {
                        snapshot,
                        snapshot_commit,
                        pushed: &pushed,
                        planned,
                        json: &json,
                        local,
                        tablet: &tablet,
                        protect: &protect,
                    },
                )?
            };
            (applied, left_alone)
        };
        // 15. Record.
        self.progress.step("Recording the sync on the tablet");
        let previous = p.state.clone();
        p.state.base = Some(pushed.base.clone());
        // A skipped document keeps its snapshot version as its base: the
        // snapshot commit itself when `main` now contains it; otherwise
        // (nothing pushed) a commit on `main` holding the same version:
        // `main` when the tablet matched it, else the document's previous
        // base (the tablet had no changes of its own).
        let skipped_base = |doc: &DocId| match &pushed.update {
            MainUpdate::Merge(_) | MainUpdate::FastForward(_) => Some(snapshot_commit.clone()),
            MainUpdate::UpToDate => Some(pushed.base.clone()),
            MainUpdate::Pulled => previous.base_for(doc).cloned(),
        };
        p.state.base_overrides = applied
            .skipped
            .iter()
            .map(|doc| (doc.clone(), skipped_base(doc)))
            .chain(
                protect
                    .iter()
                    .chain(&left_alone)
                    .map(|doc| (doc.clone(), previous.base_for(doc).cloned())),
            )
            .collect();
        p.state.manifest = plan::manifest(&plan::ManifestInput {
            read: &p.read.stats(),
            read_reference_ns: p.started_at,
            before_apply: applied.before.as_ref(),
            written: &applied.written,
            removed: &applied.removed,
            written_reference_ns: applied.written_at,
        });
        p.state.quarantine = protect;
        p.state.journal = None;
        s.write_state(&p.state)?;
        let mut warnings = Vec::new();
        if applied.before.is_some()
            && let Err(error) = s.tablet.clear_staging(s.lock())
        {
            warnings.push(Warning::StagingNotCleared {
                error: error.to_string(),
            });
        }
        // 16.
        if s.maybe_stopped {
            self.progress.step("Starting xochitl");
            s.start()?;
        }
        let report = SyncReport {
            update: pushed.update,
            snapshot: snapshot_commit.clone(),
            base: pushed.base,
            inferred: BTreeMap::new(),
            applied: applied.applied,
            skipped: applied.skipped.into_iter().collect(),
            quarantined: quarantine_report(snapshot),
            orphans: snapshot.orphans.iter().cloned().collect(),
            read: read_summary(&p.read),
            notes: pushed.outcome.notes.clone(),
            forks: pushed.outcome.forks.clone(),
            summary: pushed.outcome.summary(),
            deletions: pushed.outcome.guard.clone(),
            reconciled: p.reconciled.take(),
            push_attempts: pushed.attempts,
            took_over: p.took_over.take(),
            warnings,
        };
        Ok((report, p.origin))
    }

    /// Steps 12–14 for a non-empty plan.
    fn apply_planned(&self, s: &mut Session, p: &Prepared, input: ApplyInput) -> Result<Applied> {
        let ApplyInput {
            snapshot,
            snapshot_commit,
            pushed,
            planned,
            json,
            mut local,
            tablet,
            protect,
        } = input;
        let (rules, store) = (p.origin.rules, &p.origin.store);
        let pairs = recorded_forks(store, &pushed.base, snapshot_commit)?;
        let partners = plan::partners(pairs.iter().cloned());
        let copies: BTreeSet<DocId> = pairs.iter().map(|(_, copy)| copy.clone()).collect();
        let merged_items = tree_items(&pushed.merged, json)?;
        // Quarantined documents keep their base version's place: a folder
        // deleted above one must count as orphaning it.
        let mut current_items: BTreeMap<DocId, TreeItem> = tree_items(&snapshot.tree, json)?
            .into_iter()
            .filter(|(doc, _)| protect.contains(doc))
            .chain(p.read.items())
            .collect();
        let planned = plan::order(planned, &merged_items, &current_items, &copies);
        // 12. Stage while xochitl runs.
        self.progress.step("Staging changes on the tablet");
        s.arm()?;
        s.tablet.clear_staging(s.lock())?;
        let mut sizes = self.stage(s, store, &planned, Some(&pushed.base))?;
        // 13.
        self.progress.step("Stopping xochitl");
        s.stop()?;
        // 14. Re-read, verify, check the live tree.
        self.progress.step("Applying changes");
        let planned_docs: BTreeSet<DocId> = planned.iter().map(|entry| entry.doc.clone()).collect();
        let recheck = self.recheck(s, rules, &p.read, p.started_at, &planned_docs)?;
        let before = p.read.doc_files();
        let sha256: HashMap<Oid, String> = recheck
            .moved
            .iter()
            .flat_map(|(doc, files)| {
                files
                    .iter()
                    .filter(|(_, reread)| matches!(reread, Reread::Sha256(_)))
                    .filter_map(|(file, _)| before.get(doc)?.get(file).cloned())
            })
            .collect::<BTreeSet<Oid>>()
            .into_iter()
            .map(|blob| Ok((blob.clone(), store.blob_sha256(&blob)?)))
            .collect::<Result<_>>()?;
        recheck.json.iter().for_each(|((doc, kind), value)| {
            match value {
                Some(value) => {
                    local
                        .entry(doc.clone())
                        .or_default()
                        .insert(*kind, value.clone());
                }
                None => {
                    if let Some(files) = local.get_mut(doc) {
                        files.remove(kind);
                    }
                }
            }
            if *kind == JsonFile::Metadata && !protect.contains(doc) {
                match value.as_ref().filter(|value| value.is_object()) {
                    Some(metadata) => {
                        current_items.insert(doc.clone(), TreeItem::from_metadata(metadata));
                    }
                    None => {
                        current_items.remove(doc);
                    }
                }
            }
        });
        let skipped = plan::skipped(&plan::Checks {
            rules,
            planned: &planned,
            before: &before,
            files_now: &recheck.files_now,
            moved: &recheck.moved,
            sha256: &sha256,
            json,
            current: &current_items,
            merged: &merged_items,
            touched: &recheck.touched,
            partners: &partners,
        })?;
        let staged = plan::contents(&planned);
        // The overlay again, with view state re-read while stopped.
        let no_json = LocalJson::new();
        let final_plan: Vec<Planned> = planned
            .into_iter()
            .filter(|entry| !skipped.contains(&entry.doc))
            .map(|entry| match entry.change {
                Change::Write { .. } => Ok(plan::plan_doc(
                    rules,
                    &entry.doc,
                    pushed.merged.docs.get(&entry.doc),
                    tablet.get(&entry.doc),
                    local.get(&entry.doc).unwrap_or(&no_json),
                    json,
                )?
                .unwrap_or(entry)),
                Change::Delete => Ok(entry),
            })
            .collect::<Result<_>>()?;
        let restage = plan::restage(&final_plan, &staged);
        if !restage.is_empty() {
            sizes.extend(self.stage(s, store, &restage, None)?);
        }
        let mut applied = Applied {
            skipped,
            before: Some(recheck.now),
            ..Applied::default()
        };
        if final_plan.is_empty() {
            return Ok(applied);
        }
        s.arm()?;
        let report = s
            .tablet
            .apply(s.lock(), &apply_plan(&final_plan, &sizes)?)?;
        let blobs: BTreeMap<String, Option<Oid>> = plan::contents(&final_plan)
            .into_iter()
            .map(|(path, content)| {
                let blob = match content {
                    Content::Blob(oid) => Some(oid),
                    Content::Bytes(_) => None,
                };
                (path, blob)
            })
            .collect();
        applied.written = report
            .written
            .iter()
            .map(|entry: &StatEntry| {
                let blob = blobs.get(&entry.path).cloned().flatten();
                Ok((XochitlPath::new(entry.path.as_str())?, (entry.stat, blob)))
            })
            .collect::<Result<_>>()?;
        applied.removed = tablet_paths(plan::removed(&final_plan, &before))?;
        applied.written_at = report.recorded_at_ns;
        applied.applied = report.applied;
        Ok(applied)
    }
}

/// The device apply plan for planned documents, with staged sizes.
fn apply_plan(planned: &[Planned], sizes: &BTreeMap<XochitlPath, u64>) -> Result<ApplyPlan> {
    let docs = planned
        .iter()
        .map(|entry| {
            Ok(match &entry.change {
                Change::Delete => DocApply::Delete {
                    doc: entry.doc.clone(),
                },
                Change::Write { writes, removes } => DocApply::Write {
                    doc: entry.doc.clone(),
                    write: writes
                        .iter()
                        .map(|write| {
                            let path = xochitl_path(&entry.doc, &write.file)?;
                            let size = sizes.get(&path).copied().ok_or_else(|| {
                                Error::from(DeviceError::InvalidPlan(format!(
                                    "{path} was not staged"
                                )))
                            })?;
                            Ok(StagedFile {
                                file: write.file.clone(),
                                size,
                                mode: write.mode,
                            })
                        })
                        .collect::<Result<_>>()?,
                    remove: removes.clone(),
                },
            })
        })
        .collect::<Result<_>>()?;
    Ok(ApplyPlan { docs })
}

/// Per-document base inference for `clone`: the newest ancestor of `main`
/// whose version of the document the tablet's is, by the merge's own
/// rules, an unchanged version of ([`merge::unchanged`]: opening a PDF,
/// view state, tag order and so on are no change). Blobs come from
/// `blobs`, which serves the tablet's JSON before the cache's.
fn infer_bases(
    store: &Store,
    rules: &MirrorRules,
    main: &Oid,
    tablet: &Snapshot,
    blobs: &dyn BlobSource,
) -> Result<BTreeMap<DocId, Oid>> {
    tablet
        .validation
        .docs
        .iter()
        .filter(|(_, outcome)| **outcome == DocOutcome::Ok)
        .filter_map(|(doc, _)| Some((doc, tablet.tree.docs.get(doc)?)))
        .map(|(doc, ours)| -> Result<Option<(DocId, Oid)>> {
            let found = store.newest_matching(rules, main, doc, &mut |candidate| {
                merge::unchanged(rules, doc, candidate, ours, blobs)
            })?;
            Ok(found.map(|commit| (doc.clone(), commit)))
        })
        .filter_map(Result::transpose)
        .collect()
}

/// The warning `clone` gives when a tablet of a new model attaches to a
/// repository that has seen exactly one model.
fn mixed_model_warning(store: &Store, main: &Oid, info: &DeviceInfo) -> Result<Option<Warning>> {
    let Some(model) = info.model.as_deref() else {
        return Ok(None);
    };
    let models: BTreeSet<String> = store
        .messages(&[main.as_str()], true)?
        .iter()
        .flat_map(|text| {
            let trailers = message::trailers(text);
            message::values(&trailers, message::MODEL)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    Ok(match models.iter().next() {
        Some(seen) if models.len() == 1 && seen != model => Some(Warning::MixedModel {
            seen: seen.clone(),
            model: model.to_string(),
        }),
        _ => None,
    })
}
