//! `clone` (with `--reattach`): attach a tablet, inferring each
//! document's base from origin's history, then sync.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::Prepared;
use super::{read_manifest, saved_then_pre_read};
use crate::error::Result;
use crate::git::Oid;
use crate::hub::cycle::message;
use crate::hub::cycle::origin::OriginCtx;
use crate::hub::cycle::snapshot::{self, ReadMode, Snapshot, TabletRead};
use crate::hub::cycle::store::{self, Store};
use crate::hub::cycle::{CycleError, Hub, SyncReport, Warning, check_url, default_name};
use crate::hub::device::{DeviceInfo, Tablet};
use crate::hub::merge::{self, BlobSource};
use crate::hub::model::{
    DeviceId, DeviceName, DocId, DocOutcome, MirrorRules, RepoId, RepoTree, TabletState,
};

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

impl Hub<'_> {
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
    ///
    /// [`super::HubOptions::origin`]: crate::hub::cycle::HubOptions::origin
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
            let destination = s.tablet.destination();
            let saved = store::pre_read_path(
                &self.cache_dir,
                &config.repo_id,
                match before.as_ref() {
                    Some(state) => store::PreReadOf::Device(&state.device_id),
                    None => store::PreReadOf::Destination(&destination),
                },
            );
            let pre = if self.may_pre_read(s.tablet)? {
                self.pre_read(
                    s.tablet,
                    rules,
                    &BTreeMap::new(),
                    &cache.repo,
                    &probe,
                    &saved,
                )?
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
                &saved_then_pre_read(&saved, &pre),
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
            // `sync`. What a pre-read saved before the tablet had a device
            // id is its own no longer.
            s.write_state(&state)?;
            if before.is_none() {
                let _ = std::fs::remove_file(&saved);
            }
            let prepared = Prepared {
                origin: ctx,
                state,
                read,
                writer: Some(writer),
                started_at: std::cell::Cell::new(Some(started_at)),
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
