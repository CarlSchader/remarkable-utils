//! Step 4: reconcile the journal an interrupted run left.

use super::apply::apply_plan;
use crate::error::Result;
use crate::hub::cycle::message;
use crate::hub::cycle::plan;
use crate::hub::cycle::snapshot::TabletRead;
use crate::hub::cycle::{CycleError, Hub, OriginCtx, Overlay, Reconciled, Session, tree_items};
use crate::hub::model::{DocId, Journal, TabletState};

impl Hub<'_> {
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
    pub(super) fn reconcile(
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
}
