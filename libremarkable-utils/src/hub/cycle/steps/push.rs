//! Steps 7–11: back the snapshot up on the device ref, then fetch,
//! merge, journal and push `main`.

use std::collections::BTreeSet;
use std::ops::ControlFlow;

use super::{Prepared, Pushed};
use crate::error::{Error, Result};
use crate::git::{Expected, Oid};
use crate::hub::cycle::message::{self, Author, MergeRecord};
use crate::hub::cycle::origin::OriginCtx;
use crate::hub::cycle::session::Session;
use crate::hub::cycle::snapshot::Snapshot;
use crate::hub::cycle::store;
use crate::hub::cycle::{CycleError, Hub, MainUpdate, commit_tree};
use crate::hub::device::DeviceInfo;
use crate::hub::model::{DocId, Journal, RepoTree, TabletState};

/// The device ref to push to, its current head (the snapshot's second
/// parent) and the lease. A head that does not continue the recorded
/// one (rewritten or replaced), or none where one was recorded (origin
/// moved without `refs/rmu/*`), stops the run (see "Device ref
/// bookkeeping"); `clone --reattach` is the way out.
pub(in crate::hub::cycle) fn device_ref(
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

/// This tablet as the author of its commits.
fn author<'a>(state: &'a TabletState, info: &'a DeviceInfo) -> Author<'a> {
    Author {
        name: &state.device_name,
        id: &state.device_id,
        model: info.model.as_deref(),
        firmware: info.firmware.as_deref(),
    }
}

impl Hub<'_> {
    /// Step 7: commit the snapshot (parents: base, device ref head) and
    /// push the device ref with a lease, then record it on the tablet.
    /// A device ref already holding this very tree is reused. Returns the
    /// snapshot commit and whether it was pushed.
    pub(super) fn backup_snapshot(
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

    /// Steps 8–11: fetch, check, merge, guard, journal, push; back to 8
    /// when the push loses to another writer.
    pub(super) fn merge_and_push(
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
        let (outcome, names, summary) = (run.outcome, run.names, run.summary);
        if outcome.guard.blocked() {
            return Err(CycleError::DeletionsNeedConsent {
                from_origin: outcome.guard.from_origin.iter().cloned().collect(),
                from_tablet: outcome.guard.from_tablet.iter().cloned().collect(),
                empty_snapshot: outcome.guard.empty_snapshot,
                names,
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
                summary,
                attempts: attempt,
                names,
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
                summary,
                attempts: attempt,
                names,
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
            summary,
            attempts: attempt,
            names,
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
}
