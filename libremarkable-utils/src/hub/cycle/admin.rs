//! `remote` and `detach`: the tablet's attachment, without a sync. Both
//! leave xochitl running and never touch documents.

use super::steps::device_ref;
use super::store;
use super::{
    CycleError, DetachOptions, DetachReport, Hub, OriginCtx, RemoteReport, SetUrlReport, Warning,
    check_url,
};
use crate::error::Result;
use crate::hub::device::Tablet;
use crate::hub::model::TabletState;

/// While a journal is pending, the origin must have its merge: only
/// there can `sync` finish it.
fn journal_merge_in(origin: &OriginCtx, state: &TabletState) -> Result<()> {
    match &state.journal {
        Some(journal) if !origin.store.is_ancestor(&journal.merge, &origin.main)? => {
            Err(CycleError::JournalMergeMissing {
                merge: journal.merge.clone(),
            }
            .into())
        }
        _ => Ok(()),
    }
}

fn remote_report(state: &TabletState, effective: String) -> RemoteReport {
    RemoteReport {
        repo_id: state.repo_id.clone(),
        device_id: state.device_id.clone(),
        device_name: state.device_name.clone(),
        origin: state.origin.clone(),
        effective,
    }
}

impl Hub<'_> {
    /// `rmu hub remote`: the tablet's attachment and the origin URL it
    /// stores, and the one this invocation would use
    /// ([`super::HubOptions::origin`] if set). Read-only: no lock.
    pub fn remote(&self, tablet: &dyn Tablet) -> Result<RemoteReport> {
        let state = tablet.read_state()?.ok_or(CycleError::NotAttached)?;
        let effective = self.origin_url(&state.origin)?;
        Ok(remote_report(&state, effective))
    }

    /// `rmu hub remote set-url <url>`: store `url` as the tablet's origin,
    /// under the lock. `url` must hold the tablet's repository (its repo
    /// id, a `main` with every base of the tablet in its history, and the
    /// device ref the tablet recorded, continuing it), checked before the
    /// lock and again under it (a sync from another computer in between
    /// may have moved the bases): a wrong URL is refused here, not at the
    /// next sync. [`super::HubOptions::origin`] does not apply.
    pub fn set_remote_url(&self, tablet: &dyn Tablet, url: &str) -> Result<SetUrlReport> {
        check_url(url)?;
        let (mut report, warnings) = self.session(tablet, |s| {
            self.progress.step("Checking origin");
            let first = s.tablet.read_state()?.ok_or(CycleError::NotAttached)?;
            let (cache, refs) = store::open_cache(&self.git, &self.cache_dir, &first.repo_id, url)?;
            let mut origin =
                OriginCtx::new(cache, url.to_string(), Some(&first.device_id), Some(refs))?;
            origin.check(&first)?;
            device_ref(&origin, &first)?;
            journal_merge_in(&origin, &first)?;
            self.progress.step("Locking the tablet");
            let took_over = s.acquire(&self.request, self.options.break_lock)?;
            s.arm()?;
            let mut state = s.tablet.read_state()?.ok_or(CycleError::NotAttached)?;
            if state.repo_id != first.repo_id || state.device_id != first.device_id {
                return Err(CycleError::StateChanged.into());
            }
            origin.refresh(&state.device_id)?;
            origin.check(&state)?;
            device_ref(&origin, &state)?;
            journal_merge_in(&origin, &state)?;
            let previous = std::mem::replace(&mut state.origin, url.to_string());
            s.write_state(&state)?;
            let report = SetUrlReport {
                previous,
                remote: remote_report(&state, url.to_string()),
                took_over,
                warnings: Vec::new(),
            };
            Ok((report, None))
        })?;
        report.warnings.extend(warnings);
        Ok(report)
    }

    /// `rmu hub detach`: forget the hub state on the tablet, under the
    /// lock. Documents are untouched; the staging directories go too.
    /// While an interrupted sync's journal is pending it refuses
    /// (`DetachJournalPending`: documents its apply was writing may be
    /// half-updated, and `sync` would finish them) unless `force`, which
    /// discards the journal with a warning. With
    /// `prune_ref`, the tablet's device ref is then deleted from origin
    /// (the URL as for a sync), leased on the value just advertised; a
    /// failure there is a warning, and the ref can be deleted by hand.
    pub fn detach(&self, tablet: &dyn Tablet, options: DetachOptions) -> Result<DetachReport> {
        let DetachOptions {
            prune_device_ref: prune_ref,
            force,
        } = options;
        let (mut report, warnings) = self.session(tablet, |s| {
            self.progress.step("Locking the tablet");
            let took_over = s.acquire(&self.request, self.options.break_lock)?;
            s.arm()?;
            let state = s.tablet.read_state()?.ok_or(CycleError::NotAttached)?;
            let mut warnings = Vec::new();
            if state.journal.is_some() {
                if !force {
                    return Err(CycleError::DetachJournalPending.into());
                }
                warnings.push(Warning::JournalDiscarded);
            }
            if let Err(error) = s.tablet.clear_staging(s.lock()) {
                warnings.push(Warning::StagingNotCleared {
                    error: error.to_string(),
                });
            }
            // The state first: a crash before the prune leaves a device
            // ref nothing points at (prunable by hand), never a state
            // whose device ref is gone (the next sync would stop on it).
            self.progress.step("Forgetting the hub state");
            s.tablet.remove_state(s.lock())?;
            let _ = std::fs::remove_file(store::pre_read_path(
                &self.cache_dir,
                &state.repo_id,
                store::PreReadOf::Device(&state.device_id),
            ));
            let pruned = if prune_ref {
                self.progress.step("Deleting the device ref from origin");
                match self.prune_device_ref(&state) {
                    Ok(pruned) => pruned,
                    Err(error) => {
                        warnings.push(Warning::DeviceRefNotPruned {
                            error: error.to_string(),
                        });
                        None
                    }
                }
            } else {
                None
            };
            let report = DetachReport {
                repo_id: state.repo_id,
                device_id: state.device_id,
                pruned,
                took_over,
                warnings,
            };
            Ok((report, None))
        })?;
        report.warnings.extend(warnings);
        Ok(report)
    }

    /// Delete the tablet's device ref (whichever name origin has) at the
    /// value origin advertises; the name deleted, if any.
    fn prune_device_ref(&self, state: &TabletState) -> Result<Option<String>> {
        let url = self.origin_url(&state.origin)?;
        let (cache, refs) = store::open_cache(&self.git, &self.cache_dir, &state.repo_id, &url)?;
        store::device_ref_names(&state.device_id)
            .into_iter()
            .filter_map(|name| {
                let advertised = refs.iter().find(|reference| reference.name == name)?;
                Some((name, advertised.oid.clone()))
            })
            .try_fold(None, |pruned, (name, oid)| {
                cache.repo.delete_remote_ref(&url, &name, &oid)?;
                Ok(pruned.or(Some(name)))
            })
    }
}
