//! Steps 12–16: stage, stop, re-read, verify, check the live tree,
//! apply, record, start.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use super::{Prepared, Pushed};
use crate::error::{Error, Result};
use crate::git::Oid;
use crate::hub::cycle::message;
use crate::hub::cycle::origin::OriginCtx;
use crate::hub::cycle::plan::{self, Change, Content, JsonBlobs, LocalJson, Planned, Reread};
use crate::hub::cycle::session::Session;
use crate::hub::cycle::snapshot::{Snapshot, TabletRead};
use crate::hub::cycle::store::Store;
use crate::hub::cycle::{
    Hub, MainUpdate, Overlay, SyncReport, Warning, quarantine_report, read_summary,
    tablet_versions, tree_items, xochitl_path,
};
use crate::hub::device::{ApplyPlan, DeviceError, DocApply, StagedFile, StatEntry};
use crate::hub::merge::TreeItem;
use crate::hub::model::{
    Classification, DocFile, DocId, DocVersion, JsonFile, MirrorRules, MirroredFile, StatSignature,
    XochitlPath,
};

/// Staging uploads go in batches of about this many bytes, with the
/// watchdog re-armed before each: a timeout then bounds one batch, not a
/// whole (first-clone sized) upload, which would otherwise fire it
/// mid-transfer on every attempt.
const STAGE_BATCH_BYTES: u64 = 64 * 1024 * 1024;

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
    /// Upload the planned writes into this run's staging directory, in
    /// batches with the watchdog re-armed before each, then, for an apply
    /// of `merge`, the sentinel naming the planned documents
    /// (`plan::sentinel_files`); the size of each staged file.
    pub(super) fn stage(
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
    pub(super) fn apply_and_record(
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
            names: pushed.names,
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
pub(super) fn apply_plan(
    planned: &[Planned],
    sizes: &BTreeMap<XochitlPath, u64>,
) -> Result<ApplyPlan> {
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
