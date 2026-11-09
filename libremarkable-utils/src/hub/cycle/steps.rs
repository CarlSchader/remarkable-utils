//! The cycle's steps: `sync`, `clone` and `backup` (see the parent
//! module for the step list).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;

use serde_json::Value;

use super::message::{self, Author, MergeRecord};
use super::plan::{self, Change, Content, JsonBlobs, LocalJson, Planned, Reread};
use super::snapshot::{self, Known, ReadMode, Snapshot, TabletRead};
use super::store::{self, Store, StoreHistory};
use super::{
    BackupReport, CycleError, Hub, MainUpdate, OriginCtx, Overlay, Prepared, Pushed, Reconciled,
    Session, SyncReport, check_url, commit_tree, consented_deletions, default_name, read_summary,
    realise, rmu_signature, tablet_version, tree_items, trees_equivalent, unix_now, virtual_base,
    xochitl_path,
};
use crate::error::{Error, Result};
use crate::git::{CommitMeta, Expected, Oid, Repo};
use crate::hub::device::{
    ApplyPlan, DeviceError, DeviceInfo, DocApply, StagedFile, StatEntry, Tablet,
};
use crate::hub::merge::{self, MergeInput, MergeParams, TreeItem};
use crate::hub::model::{
    Classification, DeviceId, DeviceName, DocFile, DocId, DocOutcome, DocVersion, Journal,
    JsonFile, ManifestEntry, MirrorRules, MirroredFile, RepoTree, StatSignature, TabletState,
    XochitlPath,
};

/// Staging uploads go in batches of about this many bytes, with the
/// watchdog re-armed before each: a timeout then bounds one batch, not a
/// whole (first-clone sized) upload, which would otherwise fire it
/// mid-transfer on every attempt.
const STAGE_BATCH_BYTES: u64 = 64 * 1024 * 1024;

/// The device ref to push to, its current head (the snapshot's second
/// parent) and the lease. A head that does not continue the recorded
/// one means another tablet uses this device id (see "Device ref
/// bookkeeping"); `clone --reattach` is the way out.
fn device_ref(origin: &OriginCtx, state: &TabletState) -> Result<(String, Option<Oid>, Expected)> {
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
        (Some(_), None) => Err(conflict(&primary, "absent".to_string())),
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

fn quarantine_report(snapshot: &Snapshot) -> BTreeMap<DocId, String> {
    snapshot
        .quarantined
        .iter()
        .map(|(doc, reason)| (doc.clone(), reason.to_string()))
        .collect()
}

/// The tablet's files as read, per document, as versions.
fn tablet_versions(read: &TabletRead) -> BTreeMap<DocId, DocVersion> {
    read.doc_files()
        .iter()
        .map(|(doc, files)| (doc.clone(), tablet_version(files)))
        .collect()
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
            self.integrate(s, prepared)
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
            Ok(BackupReport {
                snapshot: commit,
                pushed,
                quarantined: quarantine_report(&snapshot),
                orphans: snapshot.orphans.iter().cloned().collect(),
                read: read_summary(&p.read),
                took_over: p.took_over,
                warnings: self.maintain(&p.origin),
            })
        })?;
        report.warnings.extend(warnings);
        Ok(report)
    }

    /// `rmu hub clone`: attach the tablet to `origin` and sync. Documents
    /// whose version matches one in origin's history (first-parent
    /// ancestors of `main`, newest first, never device refs) take that
    /// commit as their base and fast-forward; the others have the empty
    /// base ("new on both"). `reattach` does the same for a tablet that
    /// already has hub state (lost, or for a replaced origin), keeping
    /// its device id and name.
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
            let pre = self.pre_read(s.tablet, rules, &BTreeMap::new(), &cache.repo, &probe);
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
            let ctx = OriginCtx::new(cache, origin.to_string(), Some(&device_id))?;
            // Pinned from the first fetch: the pre-read was classified
            // under these rules.
            if ctx.config != config {
                return Err(CycleError::RepoConfigChanged.into());
            }
            self.progress.step("Stopping xochitl");
            s.stop()?;
            let mut writer = ctx.cache.repo.commit_writer(self.progress, None)?;
            let read = snapshot::read_tablet(
                s.tablet,
                rules,
                &snapshot::pre_read_known(&pre),
                &ctx.store,
                &mut writer,
                self.progress,
                ReadMode::Strict,
            )?;
            self.progress.step("Starting xochitl");
            let started_at = s.start()?;
            s.arm()?;
            self.progress
                .step("Matching the tablet's documents with origin's history");
            let json = Overlay::read(&read, &ctx.store);
            let tablet_only = snapshot::build(rules, &read, &RepoTree::default())?;
            let overrides = infer_bases(&ctx.store, rules, &ctx.main, &tablet_only, &json)?;
            let mut warnings: Vec<String> = mixed_model_warning(&ctx.store, &ctx.main, &info)?
                .into_iter()
                .collect();
            let mut state = TabletState::new(
                ctx.config.repo_id.clone(),
                origin.to_string(),
                device_id,
                name,
            );
            state.base_overrides = overrides;
            state.quarantine = tablet_only.quarantined.keys().cloned().collect();
            state.manifest = read_manifest(&read, started_at);
            drop(json);
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
            let mut report = self.integrate(s, prepared)?;
            warnings.append(&mut report.warnings);
            report.warnings = warnings;
            Ok(report)
        })?;
        report.warnings.extend(warnings);
        Ok(report)
    }

    /// `git maintenance run --auto` on the cache, once the tablet is back
    /// up (the user's thresholds decide whether anything happens); a
    /// failure is only a warning.
    fn maintain(&self, origin: &OriginCtx) -> Vec<String> {
        self.progress.step("Maintaining the local cache");
        origin
            .cache
            .repo
            .maintenance_auto()
            .err()
            .map(|error| format!("cache maintenance failed: {error}"))
            .into_iter()
            .collect()
    }

    /// A best-effort read while xochitl runs and before the lock (reads
    /// need none), into a commit of its own that nothing points at: the
    /// stopped read then downloads only files whose stat moved since, so
    /// the UI is down for seconds even when the tablet has gigabytes of
    /// new data. Any failure (a download cut, even mid-file, which ends
    /// the `fast-import` it streamed into) only makes it smaller, down to
    /// nothing.
    pub(super) fn pre_read(
        &self,
        tablet: &dyn Tablet,
        rules: &MirrorRules,
        known: &BTreeMap<XochitlPath, Known>,
        repo: &Repo,
        store: &Store,
    ) -> TabletRead {
        let attempt = || -> Result<TabletRead> {
            let mut writer = repo.commit_writer(self.progress, None)?;
            let read = snapshot::read_tablet(
                tablet,
                rules,
                known,
                store,
                &mut writer,
                self.progress,
                ReadMode::BestEffort,
            )?;
            let time = unix_now();
            writer.commit(&CommitMeta {
                parents: Vec::new(),
                author: rmu_signature(time),
                committer: rmu_signature(time),
                message: "rmu hub: pre-read (not pushed)\n".to_string(),
            })?;
            Ok(read)
        };
        attempt().unwrap_or_default()
    }

    /// Steps 1–6 (with step 4 only if `reconcile`).
    fn prepare<'p>(&'p self, s: &mut Session, reconcile: bool) -> Result<Prepared<'p>> {
        self.progress.step("Checking origin");
        let first = s.tablet.read_state()?.ok_or(CycleError::NotAttached)?;
        let url = self.origin_url(&first.origin)?;
        let cache = store::open_cache(&self.git, &self.cache_dir, &first.repo_id, &url)?;
        let mut origin = OriginCtx::new(cache, url, Some(&first.device_id))?;
        origin.check(&first)?;
        let rules = origin.rules;
        let pre = self.pre_read(
            s.tablet,
            rules,
            &snapshot::known_from_manifest(&first.manifest),
            &origin.cache.repo,
            &origin.store,
        );
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
            &mut writer,
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
                        &mut writer,
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
        let listing = s.tablet.staged_files()?;
        let staged = plan::staged_runs(
            listing.entries.iter().map(|entry| entry.path.as_str()),
            &journal.merge,
        );
        let forks = message::forks(&message::trailers(&store.commit(&journal.merge)?.message));
        let verdicts = plan::verdicts(&plan::Journaled {
            rules,
            merge: &merge_tree,
            snapshot: &snapshot_tree,
            tablet: &tablet,
            staged: &staged,
            forks: &forks,
            json: &json,
        })?;
        let local = read.local_json();
        let no_json = LocalJson::new();
        let finish: Vec<Planned> = verdicts
            .iter()
            .filter(|(_, verdict)| **verdict == plan::Verdict::Mixed)
            .map(|(doc, _)| {
                let Some(target) = merge_tree.docs.get(doc).filter(|v| v.is_present()) else {
                    // An interrupted deletion (`.metadata` goes first):
                    // finish it, leftovers included.
                    return Ok(Some(Planned {
                        doc: doc.clone(),
                        change: Change::Delete,
                    }));
                };
                let current = tablet.get(doc).map(tablet_version);
                plan::plan_doc(
                    rules,
                    doc,
                    Some(target),
                    current.as_ref(),
                    local.get(doc).unwrap_or(&no_json),
                    &json,
                )
            })
            .filter_map(Result::transpose)
            .collect::<Result<_>>()?;
        // The step-14 tree checks, against the tablet as just read.
        let merged_items = tree_items(&merge_tree, &json)?;
        let current_items = read.items();
        let partners = plan::partners(forks.iter().cloned());
        let finish_items = finish
            .iter()
            .map(|planned| {
                let item = match planned.change {
                    Change::Delete => None,
                    Change::Write { .. } => merged_items.get(&planned.doc).cloned(),
                };
                (planned.doc.clone(), item)
            })
            .collect();
        let skipped = plan::tree_skips(&current_items, &finish_items, &partners);
        let copies: BTreeSet<DocId> = forks.iter().map(|(_, copy)| copy.clone()).collect();
        let finish = plan::order(
            finish
                .into_iter()
                .filter(|planned| !skipped.contains(&planned.doc))
                .collect(),
            &merged_items,
            &current_items,
            &copies,
        );
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
        state.base = Some(journal.merge.clone());
        // An explicit override even where the snapshot lacks the
        // document: falling back to the default base would read it as
        // deleted by the tablet.
        state.base_overrides = kept
            .iter()
            .map(|doc| (doc.clone(), Some(journal.snapshot.clone())))
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
        let author = Author {
            name: &p.state.device_name,
            id: &p.state.device_id,
            model: p.info.model.as_deref(),
            firmware: p.info.firmware.as_deref(),
        };
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

    /// Steps 7–16.
    fn integrate(&self, s: &mut Session, mut p: Prepared) -> Result<SyncReport> {
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
        let rules = p.origin.rules;
        s.arm()?;
        self.progress.step("Fetching origin");
        p.origin.refresh(&p.state.device_id)?;
        p.origin.check(&p.state)?;
        let store = &p.origin.store;
        let main = p.origin.main.clone();
        let theirs = store.tree_of(rules, &main)?;
        let consented = consented_deletions(store, &main, &p.state, &snapshot.tree, &theirs)?;
        self.progress.step("Merging");
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
                tip: &main,
            },
            &MergeParams {
                rules,
                device_name: &p.state.device_name,
                date: &date,
                allow_deletions: self.options.allow_deletions,
                thresholds: self.options.thresholds,
                consented_deletions: &consented,
            },
        )?;
        if outcome.guard.blocked() {
            return Err(CycleError::DeletionsNeedConsent {
                from_origin: outcome.guard.from_origin.iter().cloned().collect(),
                from_tablet: outcome.guard.from_tablet.iter().cloned().collect(),
                empty_snapshot: outcome.guard.empty_snapshot,
            }
            .into());
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
        let mut writer = p.origin.cache.repo.commit_writer(self.progress, None)?;
        let (merged, new_blobs) = realise(&mut writer, &outcome)?;
        let merged_json = Overlay::new_blobs(&new_blobs, store);
        // The tablet already agrees with origin: nothing to push, even if
        // the merge took the tablet's bytes for documents that differ only
        // in view state or derived fields (pushing those would make two
        // tablets push each other's view state on every sync).
        // Quarantined documents carry their base version, which the merge
        // never takes over origin's: nothing to push for them.
        let quarantined: BTreeSet<DocId> = snapshot.quarantined.keys().cloned().collect();
        if trees_equivalent(rules, &snapshot.tree, &theirs, store, &quarantined)?
            && trees_equivalent(rules, &merged, &theirs, &merged_json, &BTreeSet::new())?
        {
            return Ok(Some(Pushed {
                update: MainUpdate::UpToDate,
                base: main,
                merged: theirs,
                outcome,
                attempts: attempt,
            }));
        }
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
            let author = Author {
                name: &p.state.device_name,
                id: &p.state.device_id,
                model: p.info.model.as_deref(),
                firmware: p.info.firmware.as_deref(),
            };
            let commit = commit_tree(
                writer,
                &merged,
                &Self::meta(
                    &p.state,
                    vec![main.clone(), snapshot_commit.clone()],
                    message::merge(&author, &record),
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
        let moved_paths: BTreeSet<&XochitlPath> = read
            .files
            .iter()
            .filter(|(path, file)| {
                now.get(*path).map(|(_, stat)| stat) != Some(&file.stat)
                    || !file.stat.keep_in_manifest(started_at)
            })
            .map(|(path, _)| path)
            .chain(now.keys().filter(|path| !read.files.contains_key(*path)))
            .collect();
        let file_of = |path: &XochitlPath| {
            now.get(path)
                .map(|(file, _)| file)
                .or_else(|| read.files.get(path).map(|read| &read.file))
        };
        let present_moved: Vec<(&XochitlPath, &MirroredFile)> = moved_paths
            .iter()
            .filter_map(|path| now.get(*path).map(|(file, _)| (*path, file)))
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
                Some(((file.doc.clone(), kind), reread.get(*path).and_then(parse)))
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
    ) -> Result<SyncReport> {
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
        p.state.base_overrides = applied
            .skipped
            .iter()
            .map(|doc| (doc.clone(), Some(snapshot_commit.clone())))
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
            warnings.push(format!("could not clear the staging directory: {error}"));
        }
        // 16.
        if s.maybe_stopped {
            self.progress.step("Starting xochitl");
            s.start()?;
        }
        warnings.extend(self.maintain(&p.origin));
        Ok(SyncReport {
            update: pushed.update,
            snapshot: snapshot_commit.clone(),
            base: pushed.base,
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
        })
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

/// Per-document base inference for `clone`: the newest first-parent
/// ancestor of `main` whose version of the document is equivalent to the
/// tablet's.
fn infer_bases(
    store: &Store,
    rules: &MirrorRules,
    main: &Oid,
    tablet: &Snapshot,
    json: &dyn JsonBlobs,
) -> Result<BTreeMap<DocId, Option<Oid>>> {
    let history = StoreHistory {
        store,
        rules,
        tip: main,
    };
    tablet
        .validation
        .docs
        .iter()
        .filter(|(_, outcome)| **outcome == DocOutcome::Ok)
        .map(|(doc, _)| -> Result<Option<(DocId, Option<Oid>)>> {
            let ours = tablet.tree.docs.get(doc);
            let found = history
                .versions(doc)?
                .into_iter()
                .map(|(commit, candidate)| {
                    plan::versions_equivalent(rules, Some(&candidate), ours, json)
                        .map(|same| same.then_some(commit))
                })
                .find_map(Result::transpose)
                .transpose()?;
            Ok(found.map(|commit| (doc.clone(), Some(commit))))
        })
        .filter_map(Result::transpose)
        .collect()
}

/// The warning `clone` gives when a tablet of a new model attaches to a
/// repository that has seen exactly one model.
fn mixed_model_warning(store: &Store, main: &Oid, info: &DeviceInfo) -> Result<Option<String>> {
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
        Some(seen) if models.len() == 1 && seen != model => Some(format!(
            "this repository has only seen {seen} tablets so far; {model} is untested with it (documents are merged per format, but nothing has been validated across models)"
        )),
        _ => None,
    })
}
