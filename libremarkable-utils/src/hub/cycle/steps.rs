//! The cycle's steps (see the parent module for the step list): `sync`
//! and `backup`, and steps 1–6 that every run shares (`prepare`,
//! `pre_read`). The submodules hold the rest: `clone`, `reconcile`
//! (step 4), `push` (steps 7–11) and `apply` (steps 12–16).

mod apply;
mod clone;
mod push;
mod reconcile;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::origin::{OriginCtx, virtual_base};
use super::plan;
use super::session::Session;
use super::snapshot::{self, Known, ReadMode, TabletRead};
use super::store::{self, Store};
use super::{
    BackupReport, CycleError, Hub, MainUpdate, Reconciled, SyncReport, TakeOver, quarantine_report,
    read_summary, rmu_signature, unix_now,
};
use crate::error::{Error, Result};
use crate::git::{CommitMeta, CommitWriter, Oid, Repo};
use crate::hub::device::{DeviceInfo, LockState, Tablet};
use crate::hub::merge::{MergeOutcome, Summary};
use crate::hub::model::{DocId, ManifestEntry, MirrorRules, RepoTree, TabletState, XochitlPath};

pub(super) use self::push::device_ref;

/// What steps 1–6 produced.
struct Prepared<'p> {
    origin: OriginCtx,
    state: TabletState,
    read: TabletRead,
    /// Holds every blob the reads streamed; it becomes the snapshot.
    writer: Option<CommitWriter<'p>>,
    /// How long the fetch under the lock (step 2) took: a slow one means
    /// slow network steps, so no single stop.
    fetch_took: Duration,
    info: DeviceInfo,
    took_over: Option<TakeOver>,
    reconciled: Option<Reconciled>,
}

/// What steps 8–11 decided.
struct Pushed {
    update: MainUpdate,
    base: Oid,
    merged: RepoTree,
    outcome: MergeOutcome,
    /// The merge's summary as reported (as `MergeRun::summary`).
    summary: Summary,
    attempts: usize,
    names: BTreeMap<DocId, String>,
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

/// What the stopped read may take as known besides the manifest: what
/// earlier pre-reads saved at `saved`, then this run's pre-read (newer).
/// Both are checked against the tablet's stat and the cache.
fn saved_then_pre_read(saved: &std::path::Path, pre: &TabletRead) -> BTreeMap<XochitlPath, Known> {
    snapshot::saved_known(&store::load_pre_read(saved))
        .into_iter()
        .chain(snapshot::pre_read_known(pre))
        .collect()
}

impl Hub<'_> {
    /// `rmu hub sync`: the full cycle.
    pub fn sync(&self, tablet: &dyn Tablet) -> Result<SyncReport> {
        let (mut report, warnings) = self.session(tablet, |s| {
            let prepared = self.prepare(s, true)?;
            self.start_or_defer(s, &prepared)?;
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
            self.start_xochitl(s)?;
            let base_tree = virtual_base(&p.origin.store, p.origin.rules, &p.state)?;
            let snapshot = snapshot::build(p.origin.rules, &p.read, &base_tree)?;
            self.progress.step("Backing up the tablet to origin");
            let (commit, pushed) = self.backup_snapshot(s, &mut p, &snapshot)?;
            p.state.manifest = read_manifest(&p.read, s.started_at().expect("started"));
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
    ) -> Result<TabletRead> {
        let known: BTreeMap<XochitlPath, Known> = snapshot::known_from_manifest(manifest)
            .into_iter()
            .chain(snapshot::saved_known(&store::load_pre_read(saved)))
            .collect();
        let mut save = |entries: &BTreeMap<XochitlPath, ManifestEntry>| {
            store::save_pre_read(saved, entries);
        };
        let Ok(mut writer) = repo.commit_writer(self.progress, None) else {
            return Ok(TabletRead::default());
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
        );
        // A read that failed outright (its stat pass, say) read nothing:
        // the saved progress stays as its last checkpoint left it. A
        // tablet that stopped answering ends the run.
        let read = match read {
            Ok(read) => read,
            Err(error @ Error::Unreachable(_)) => return Err(error),
            Err(_) => return Ok(TabletRead::default()),
        };
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
        Ok(read)
    }

    /// Whether to pre-read: not while another run holds the tablet lock,
    /// since this run would be refused, unless the lock is stale (its
    /// watchdog fired, as after a crash) or being broken. (A lock left by
    /// a run the tablet rebooted under also goes stale, but telling needs
    /// the boot id; such a run reads without a pre-read, from the saved
    /// one.)
    fn may_pre_read(&self, tablet: &dyn Tablet) -> Result<bool> {
        Ok(self.options.break_lock
            || tablet.lock_state()? == LockState::Free
            || tablet.watchdog_fired()?)
    }

    /// Step 6: start xochitl (the session keeps the device time, the
    /// read's racy-entry reference).
    fn start_xochitl(&self, s: &mut Session) -> Result<()> {
        self.progress.step("Starting xochitl");
        s.start()?;
        Ok(())
    }

    /// Step 6 of a sync: start xochitl, or keep it stopped through step
    /// 16 when the sync looks small ([`super::SingleStop`]), so the user
    /// sees one outage and xochitl gets one start. Only `sync` comes here:
    /// `clone` and `backup` always start at step 6.
    fn start_or_defer(&self, s: &mut Session, p: &Prepared) -> Result<()> {
        let Some(limits) = self.options.single_stop else {
            return self.start_xochitl(s);
        };
        match self.single_stop_verdict(s, p, &limits)? {
            None => {
                self.progress.step("Keeping xochitl stopped: a small sync");
                s.defer_start(limits.watchdog)
            }
            Some(plan::StartNow::NothingToApply) => self.start_xochitl(s),
            Some(why) => {
                self.progress.step(&format!("Starting xochitl: {why}"));
                s.start().map(drop)
            }
        }
    }

    /// Why a sync starts xochitl at step 6, if it does
    /// ([`plan::single_stop_at_start`]). The trees the estimate compares
    /// are only read when origin moved since the bases.
    fn single_stop_verdict(
        &self,
        s: &Session,
        p: &Prepared,
        limits: &super::SingleStop,
    ) -> Result<Option<plan::StartNow>> {
        let unchanged =
            p.state.base.as_ref() == Some(&p.origin.main) && p.state.base_overrides.is_empty();
        if unchanged {
            return Ok(Some(plan::StartNow::NothingToApply));
        }
        let store = &p.origin.store;
        let base_tree = virtual_base(store, p.origin.rules, &p.state)?;
        let theirs = store.tree_of(p.origin.rules, &p.origin.main)?;
        let estimate = plan::network_estimate(
            &base_tree.doc_blobs(),
            &p.read.doc_blobs_sized(),
            &theirs.doc_blobs(),
        );
        let apply_bytes: u64 = store
            .repo
            .object_sizes(&estimate.apply_blobs)?
            .values()
            .sum();
        Ok(plan::single_stop_at_start(
            limits,
            &estimate,
            apply_bytes,
            s.stopped_for().unwrap_or_default(),
            p.fetch_took,
        ))
    }

    /// While xochitl is kept stopped: start it now, if `why` (for the
    /// progress line), the run going on as the two-window cycle.
    fn end_single_stop(&self, s: &mut Session, why: Option<plan::StartNow>) -> Result<()> {
        match why {
            Some(why) if s.deferring() => {
                self.progress.step(&format!("Starting xochitl: {why}"));
                // The push attempt and staging arm next, at the run's own
                // timeout again; after an empty plan only the record is
                // left, under the watchdog already pending.
                s.start().map(drop)
            }
            _ => Ok(()),
        }
    }

    /// While xochitl is kept stopped: start it once the budget is spent.
    /// Checked before every push attempt (fetch, merge, journal, push
    /// follow) and, with the plan, before staging
    /// ([`plan::single_stop_at_plan`]); nothing after staging, where only
    /// the apply is left (a start there would be stopped again during its
    /// start-up).
    fn check_single_stop(&self, s: &mut Session) -> Result<()> {
        let why = self
            .options
            .single_stop
            .zip(s.stopped_for())
            .and_then(|(limits, stopped_for)| plan::single_stop_overdue(&limits, stopped_for));
        self.end_single_stop(s, why)
    }

    /// Steps 1–5 (with step 4 only if `reconcile`); xochitl is left
    /// stopped, for the caller's step 6.
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
        let saved = store::pre_read_path(
            &self.cache_dir,
            &first.repo_id,
            store::PreReadOf::Device(&first.device_id),
        );
        let pre = if self.may_pre_read(s.tablet)? {
            self.pre_read(
                s.tablet,
                rules,
                &first.manifest,
                &origin.cache.repo,
                &origin.store,
                &saved,
            )?
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
        let fetch = Instant::now();
        origin.refresh(&state.device_id)?;
        let fetch_took = fetch.elapsed();
        origin.check(&state)?;
        let info = s.tablet.device_info()?;
        self.progress.step("Stopping xochitl");
        s.stop()?;
        let known: BTreeMap<XochitlPath, Known> = snapshot::known_from_manifest(&state.manifest)
            .into_iter()
            .chain(saved_then_pre_read(&saved, &pre))
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
        Ok(Prepared {
            origin,
            state,
            read,
            writer: Some(writer),
            fetch_took,
            info,
            took_over,
            reconciled,
        })
    }

    /// Steps 7–16, after step 6 (`clone` comes here too, its xochitl
    /// started); the origin handed back for maintenance.
    fn integrate(&self, s: &mut Session, mut p: Prepared) -> Result<(SyncReport, OriginCtx)> {
        let rules = p.origin.rules;
        let base_tree = virtual_base(&p.origin.store, rules, &p.state)?;
        let snapshot = snapshot::build(rules, &p.read, &base_tree)?;
        self.progress.step("Backing up the tablet to origin");
        let (snapshot_commit, _) = self.backup_snapshot(s, &mut p, &snapshot)?;
        let pushed = self.merge_and_push(s, &mut p, &snapshot, &base_tree, &snapshot_commit)?;
        self.apply_and_record(s, p, &snapshot, &snapshot_commit, pushed)
    }
}
