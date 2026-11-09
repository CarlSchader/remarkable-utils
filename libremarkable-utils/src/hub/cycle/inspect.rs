//! `status` and `sync --dry-run`: read-only. They never stop xochitl,
//! take the lock or push; their snapshot is taken while xochitl runs, so
//! it is best effort (files that change while read are left out), and
//! the commit holding its blobs is local garbage no ref points to.

use super::*;
use crate::hub::model::DocOutcome;
use snapshot::ReadMode;

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
        .map(|nested| nested.into_iter().flatten().collect())
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

    fn inspect(&self, tablet: &dyn Tablet) -> Result<(StatusReport, DryPlan)> {
        self.progress.step("Checking origin");
        let state = tablet.read_state()?.ok_or(CycleError::NotAttached)?;
        let url = self.origin_url(&state.origin)?;
        let cache = store::open_cache(&self.git, &self.cache_dir, &state.repo_id, &url)?;
        let origin = OriginCtx::new(cache, url, Some(&state.device_id))?;
        origin.check(&state)?;
        let lock = tablet.lock_state()?;
        let (store, rules) = (&origin.store, origin.rules);
        let mut writer = origin.cache.repo.commit_writer(self.progress, None)?;
        let read = snapshot::read_tablet(
            tablet,
            rules,
            &snapshot::known_from_manifest(&state.manifest),
            store,
            &mut writer,
            self.progress,
            ReadMode::BestEffort,
        )?;
        let base_tree = virtual_base(store, rules, &state)?;
        let snapshot = snapshot::build(rules, &read, &base_tree)?;
        // Stores the blobs read; nothing points at it.
        commit_tree(
            writer,
            &snapshot.tree,
            &Self::meta(
                &state,
                Vec::new(),
                "rmu hub: status snapshot (not pushed)\n".to_string(),
            ),
        )?;
        let main = origin.main.clone();
        let theirs = store.tree_of(rules, &main)?;
        let consented = consented_deletions(store, &main, &state, &snapshot.tree, &theirs)?;
        let date = self.date();
        let outcome = merge::merge(
            &MergeInput {
                base: &base_tree,
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
                device_name: &state.device_name,
                date: &date,
                allow_deletions: self.options.allow_deletions,
                thresholds: self.options.thresholds,
                consented_deletions: &consented,
            },
        )?;
        let present =
            |tree: &RepoTree, doc: &DocId| tree.docs.get(doc).is_some_and(DocVersion::is_present);
        let changes = base_tree
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
        let open_conflicts = open_conflicts(store, rules, &main, &theirs)?;
        // What the sync would write: the merge realised into a commit no
        // ref points to, so the plan can read its new blobs.
        let mut writer = origin.cache.repo.commit_writer(self.progress, None)?;
        let merged = realise(&mut writer, &outcome)?;
        commit_tree(
            writer,
            &merged,
            &Self::meta(
                &state,
                Vec::new(),
                "rmu hub: dry-run merge (not pushed)\n".to_string(),
            ),
        )?;
        let up_to_date = trees_equivalent(rules, &snapshot.tree, &theirs, store)?
            && trees_equivalent(rules, &merged, &theirs, store)?;
        let protect: BTreeSet<DocId> = snapshot.quarantined.keys().cloned().collect();
        let tablet_files = read
            .doc_files()
            .iter()
            .map(|(doc, files)| (doc.clone(), tablet_version(files)))
            .collect();
        let planned = plan::plan(&plan::PlanInput {
            rules,
            merged: &merged,
            tablet: &tablet_files,
            protect: &protect,
            local: &read.local_json(),
            json: store,
        })?;
        let status = StatusReport {
            repo_id: state.repo_id.clone(),
            device_id: state.device_id.clone(),
            device_name: state.device_name.clone(),
            origin: origin.url.clone(),
            lock,
            journal_pending: state.journal.is_some(),
            quarantined: snapshot
                .quarantined
                .iter()
                .map(|(doc, reason)| (doc.clone(), reason.to_string()))
                .collect(),
            orphans: snapshot.orphans.iter().cloned().collect(),
            read: read_summary(&read),
            changes,
            behind,
            open_conflicts,
            guard: outcome.guard.clone(),
            summary: outcome.summary(),
            notes: outcome.notes.clone(),
            forks: outcome.forks.clone(),
        };
        Ok((
            status,
            DryPlan {
                planned,
                would_push: !up_to_date,
            },
        ))
    }
}

/// What a sync would do to the tablet and origin.
struct DryPlan {
    planned: Vec<Planned>,
    would_push: bool,
}
