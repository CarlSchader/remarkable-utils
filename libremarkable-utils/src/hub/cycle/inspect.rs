//! `status` and `sync --dry-run`: read-only. They never stop xochitl,
//! take the lock or push. Their snapshot is taken while xochitl runs: a
//! best-effort pre-read, then a full read of what moved since (retried
//! once if a file changes while read), so it is complete but may be a
//! moment out of date. The commits holding its blobs and the would-be
//! merge are local garbage no ref points to.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::merge_run::names_of;
use super::message;
use super::origin::{OriginCtx, virtual_base};
use super::plan::{self, Change, JsonBlobs, Planned};
use super::snapshot::{self, ReadMode, TabletRead};
use super::store::{self, Store};
use super::{
    CycleError, DocChange, DryRunReport, Hub, OpenConflict, StatusReport, commit_tree,
    quarantine_report, read_summary, tablet_versions,
};
use crate::error::{Error, Result};
use crate::git::Oid;
use crate::hub::device::Tablet;
use crate::hub::model::{
    DocFile, DocId, DocOutcome, DocVersion, MirrorRules, RepoTree, TabletState,
};

/// The visible name of a document version, if it has one.
fn visible_name(store: &Store, version: Option<&DocVersion>) -> Result<Option<String>> {
    let Some(entry) = version.and_then(|version| version.files.get(&DocFile::Metadata)) else {
        return Ok(None);
    };
    Ok(store.json(&entry.id)?.and_then(|metadata| {
        metadata
            .get("visibleName")
            .and_then(Value::as_str)
            .map(str::to_string)
    }))
}

/// Conflict copies on origin `main` that still have the name their fork
/// gave them (deleting or renaming a copy closes it).
fn open_conflicts(
    store: &Store,
    rules: &MirrorRules,
    main: &Oid,
    theirs: &RepoTree,
) -> Result<Vec<OpenConflict>> {
    store
        .first_parents(main)?
        .iter()
        .map(|commit| {
            let forks = message::forks(&message::trailers(&store.commit(commit)?.message));
            forks
                .into_iter()
                .map(|(original, copy)| {
                    let named =
                        visible_name(store, store.version_at(rules, commit, &copy)?.as_ref())?;
                    let now = visible_name(store, theirs.docs.get(&copy))?;
                    Ok(match (named, now) {
                        (Some(named), Some(now)) if named == now => Some(OpenConflict {
                            original,
                            copy,
                            name: now,
                        }),
                        _ => None,
                    })
                })
                .filter_map(Result::transpose)
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()
        .map(|nested| {
            // A copy recorded by more than one commit (a fork trailer is
            // carried by every commit that re-records it) is listed once.
            let mut seen = BTreeSet::new();
            nested
                .into_iter()
                .flatten()
                .filter(|conflict| seen.insert(conflict.copy.clone()))
                .collect()
        })
}

/// What a sync would do to the tablet and origin.
struct DryPlan {
    planned: Vec<Planned>,
    would_push: bool,
}

impl Hub<'_> {
    /// `rmu hub status`: the tablet's changes since its base, how far
    /// behind origin it is, quarantined documents, open conflict copies,
    /// and what a sync would decide about deletions.
    pub fn status(&self, tablet: &dyn Tablet) -> Result<StatusReport> {
        let result = self.inspect(tablet).map(|(status, _)| status);
        self.progress.finished();
        result
    }

    /// `rmu hub sync --dry-run`: the status plus the documents a sync
    /// would write to and delete from the tablet.
    pub fn dry_run(&self, tablet: &dyn Tablet) -> Result<DryRunReport> {
        let result = self.inspect(tablet).map(|(status, plan)| {
            let (deletes, writes): (Vec<Planned>, Vec<Planned>) = plan
                .planned
                .into_iter()
                .partition(|entry| entry.change == Change::Delete);
            DryRunReport {
                status,
                writes: writes.into_iter().map(|entry| entry.doc).collect(),
                deletes: deletes.into_iter().map(|entry| entry.doc).collect(),
                would_push: plan.would_push,
            }
        });
        self.progress.finished();
        result
    }

    /// The tablet read while xochitl runs, its blobs committed.
    fn read_running(
        &self,
        tablet: &dyn Tablet,
        origin: &OriginCtx,
        state: &TabletState,
    ) -> Result<TabletRead> {
        let pre = self.pre_read(
            tablet,
            origin.rules,
            &state.manifest,
            &origin.cache.repo,
            &origin.store,
            &store::pre_read_path(
                &self.cache_dir,
                &state.repo_id,
                store::PreReadOf::Device(&state.device_id),
            ),
        )?;
        let known = snapshot::known_from_manifest(&state.manifest)
            .into_iter()
            .chain(snapshot::pre_read_known(&pre))
            .collect();
        let attempt = || -> Result<TabletRead> {
            let mut writer = origin.cache.repo.commit_writer(self.progress, None)?;
            let read = snapshot::read_tablet(
                tablet,
                origin.rules,
                &known,
                &origin.store,
                snapshot::Sink::plain(&mut writer),
                self.progress,
                ReadMode::Strict,
            )?;
            commit_tree(
                writer,
                &RepoTree::default(),
                &Self::meta(
                    state,
                    Vec::new(),
                    "rmu hub: status read (not pushed)\n".to_string(),
                ),
            )?;
            Ok(read)
        };
        // A file xochitl changed while it was read: once more. Not after
        // the tablet stopped answering: that try would only time out too.
        attempt().or_else(|error| match error {
            Error::Unreachable(_) => Err(error),
            _ => attempt(),
        })
    }

    fn inspect(&self, tablet: &dyn Tablet) -> Result<(StatusReport, DryPlan)> {
        self.progress.step("Checking origin");
        let state = tablet.read_state()?.ok_or(CycleError::NotAttached)?;
        let url = self.origin_url(&state.origin)?;
        let (cache, refs) = store::open_cache(&self.git, &self.cache_dir, &state.repo_id, &url)?;
        let origin = OriginCtx::new(cache, url, Some(&state.device_id), Some(refs))?;
        origin.check(&state)?;
        let lock = tablet.lock_state()?;
        let (store, rules) = (&origin.store, origin.rules);
        let read = self.read_running(tablet, &origin, &state)?;
        let base_tree = virtual_base(store, rules, &state)?;
        let snapshot = snapshot::build(rules, &read, &base_tree)?;
        let main = origin.main.clone();
        let run = self.run_merge(&origin, &state, &base_tree, &snapshot, &read)?;
        let present =
            |tree: &RepoTree, doc: &DocId| tree.docs.get(doc).is_some_and(DocVersion::is_present);
        let changes: BTreeMap<DocId, DocChange> = base_tree
            .docs
            .keys()
            .chain(snapshot.tree.docs.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|doc| snapshot.validation.docs.get(*doc) != Some(&DocOutcome::Orphan))
            .map(|doc| -> Result<Option<(DocId, DocChange)>> {
                let change = match (present(&base_tree, doc), present(&snapshot.tree, doc)) {
                    (false, true) => Some(DocChange::Added),
                    (true, false) => Some(DocChange::Deleted),
                    (true, true)
                        if !plan::versions_equivalent(
                            rules,
                            base_tree.docs.get(doc),
                            snapshot.tree.docs.get(doc),
                            store,
                        )? =>
                    {
                        Some(DocChange::Modified)
                    }
                    _ => None,
                };
                Ok(change.map(|change| (doc.clone(), change)))
            })
            .filter_map(Result::transpose)
            .collect::<Result<_>>()?;
        let behind = match &state.base {
            Some(base) => store
                .repo
                .rev_list(&[main.as_str(), &format!("^{base}")], true)?
                .len(),
            None => store.first_parents(&main)?.len(),
        };
        let open_conflicts = open_conflicts(store, rules, &main, &run.theirs)?;
        // What the sync would write: the merge realised into a commit no
        // ref points to, so the plan can read its new blobs.
        commit_tree(
            run.writer,
            &run.merged,
            &Self::meta(
                &state,
                Vec::new(),
                "rmu hub: dry-run merge (not pushed)\n".to_string(),
            ),
        )?;
        let protect: BTreeSet<DocId> = snapshot.quarantined.keys().cloned().collect();
        let planned = plan::plan(&plan::PlanInput {
            rules,
            merged: &run.merged,
            tablet: &tablet_versions(&read),
            protect: &protect,
            local: &read.local_json(),
            json: store,
        })?
        .planned;
        // Names for what the tablet no longer has (or would lose), from
        // origin's or the tablet's history.
        let deleted = changes
            .iter()
            .filter(|(_, change)| **change == DocChange::Deleted)
            .map(|(doc, _)| doc)
            .chain(
                planned
                    .iter()
                    .filter(|entry| entry.change == Change::Delete)
                    .map(|entry| &entry.doc),
            );
        let mut names = names_of(deleted, &[&run.theirs, &snapshot.tree, &base_tree], store)?;
        names.extend(run.names);
        let outcome = run.outcome;
        let status = StatusReport {
            repo_id: state.repo_id.clone(),
            device_id: state.device_id.clone(),
            device_name: state.device_name.clone(),
            origin: origin.url.clone(),
            lock,
            journal_pending: state.journal.is_some(),
            quarantined: quarantine_report(&snapshot),
            orphans: snapshot.orphans.iter().cloned().collect(),
            read: read_summary(&read),
            changes,
            behind,
            open_conflicts,
            summary: run.summary,
            guard: outcome.guard,
            names,
            notes: outcome.notes,
            forks: outcome.forks,
        };
        Ok((
            status,
            DryPlan {
                planned,
                would_push: !(run.up_to_date || run.pull_only),
            },
        ))
    }
}
