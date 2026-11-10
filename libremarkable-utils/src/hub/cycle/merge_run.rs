//! The merge a sync runs against origin's `main` (and `--dry-run`
//! previews): consent, the merge, its new blobs, and what it means for
//! pushing.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use super::message;
use super::origin::OriginCtx;
use super::plan::{self, JsonBlobs};
use super::snapshot::{Snapshot, TabletRead};
use super::store::{Store, StoreHistory};
use super::{Hub, Overlay, tablet_versions};
use crate::error::Result;
use crate::git::{CommitWriter, Oid};
use crate::hub::merge::{
    self, MergeInput, MergeOutcome, MergeParams, MergedEntry, Note, NoteKind, Summary,
};
use crate::hub::model::{
    DocFile, DocId, DocVersion, ListingEntry, MirrorRules, RepoTree, TabletState,
};

/// A merge against origin `main` as a sync runs it (and `--dry-run`
/// previews it): consent, the merge itself, its new blobs streamed into a
/// commit writer not committed yet, and what it means for pushing.
pub(super) struct MergeRun<'p> {
    pub(super) theirs: RepoTree,
    pub(super) outcome: MergeOutcome,
    pub(super) merged: RepoTree,
    /// Holds the merge's new blobs, until it commits.
    pub(super) writer: CommitWriter<'p>,
    /// The tablet already agrees with origin: nothing to push or apply.
    /// Quarantined documents carry their base version, and documents the
    /// apply leaves alone keep their previous base: neither counts.
    pub(super) up_to_date: bool,
    /// The tablet has no changes of its own (every document equals its
    /// base version) and the merge is origin's tree: nothing to push.
    pub(super) pull_only: bool,
    /// The visible names of the documents the deletion guard lists.
    pub(super) names: BTreeMap<DocId, String>,
    /// The merge's summary as reported: documents whose merged version
    /// is equivalent to their base count as unchanged.
    pub(super) summary: Summary,
}

impl<'a> Hub<'a> {
    /// The merge a sync runs against origin's current `main`.
    pub(super) fn run_merge<'p>(
        &'p self,
        origin: &OriginCtx,
        state: &TabletState,
        base_tree: &RepoTree,
        snapshot: &Snapshot,
        read: &TabletRead,
    ) -> Result<MergeRun<'p>> {
        let (store, rules, main) = (&origin.store, origin.rules, &origin.main);
        let theirs = store.tree_of(rules, main)?;
        let consented = consented_deletions(store, main, state, &snapshot.tree, &theirs)?;
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
                tip: main,
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
        let mut outcome = outcome;
        let returned = returned_after_deletion(store, main, state, base_tree, snapshot, &outcome)?;
        outcome.notes.extend(returned);
        let mut writer = origin.cache.repo.commit_writer(self.progress, None)?;
        let (merged, new_blobs) = realise(&mut writer, &outcome)?;
        let merged_json = Overlay::new_blobs(&new_blobs, store);
        let json = Overlay::read(read, &merged_json);
        let guard = &outcome.guard;
        let names = names_of(
            guard
                .from_origin
                .iter()
                .chain(&guard.from_tablet)
                .chain(&guard.exempt),
            &[&theirs, &snapshot.tree, base_tree],
            &json,
        )?;
        let left_alone = plan::left_alone(rules, &merged, &tablet_versions(read), &json)?;
        let except: BTreeSet<DocId> = snapshot
            .quarantined
            .keys()
            .cloned()
            .chain(left_alone)
            .collect();
        let none = BTreeSet::new();
        let merge_is_theirs = trees_equivalent(rules, &merged, &theirs, &json, &none)?;
        let up_to_date =
            merge_is_theirs && trees_equivalent(rules, &snapshot.tree, &theirs, &json, &except)?;
        let pull_only =
            merge_is_theirs && trees_equivalent(rules, &snapshot.tree, base_tree, &json, &none)?;
        let summary = outcome.summary_with(|doc| {
            plan::versions_equivalent(rules, base_tree.docs.get(doc), merged.docs.get(doc), &json)
        })?;
        drop(json);
        Ok(MergeRun {
            theirs,
            outcome,
            merged,
            writer,
            up_to_date,
            pull_only,
            names,
            summary,
        })
    }
}

/// The tablet's documents that origin deleted with recorded consent:
/// present in `ours`, absent from `theirs`, and named by an
/// `Rmu-Deletion-Consent` trailer of a commit since that document's own
/// base. A document with the empty base never counts (the merge never
/// deletes it from the tablet: it reads as new).
fn consented_deletions(
    store: &Store,
    main: &Oid,
    state: &TabletState,
    ours: &RepoTree,
    theirs: &RepoTree,
) -> Result<BTreeSet<DocId>> {
    let present =
        |tree: &RepoTree, doc: &DocId| tree.docs.get(doc).is_some_and(DocVersion::is_present);
    let by_base = ours
        .docs
        .keys()
        .filter(|doc| present(ours, doc) && !present(theirs, doc))
        .filter_map(|doc| Some((state.base_for(doc)?.clone(), doc.clone())))
        .fold(
            BTreeMap::<Oid, Vec<DocId>>::new(),
            |mut groups, (base, doc)| {
                groups.entry(base).or_default().push(doc);
                groups
            },
        );
    by_base
        .into_iter()
        .try_fold(BTreeSet::new(), |mut consented, (base, docs)| {
            let since = format!("^{base}");
            let recorded: BTreeSet<DocId> = store
                .messages(&[main.as_str(), &since], false)?
                .iter()
                .flat_map(|text| {
                    let trailers = message::trailers(text);
                    message::doc_ids(message::values(&trailers, message::CONSENT))
                })
                .collect();
            consented.extend(docs.into_iter().filter(|doc| recorded.contains(doc)));
            Ok(consented)
        })
}

/// The `visibleName` of each of `docs`, from the first of `trees` whose
/// version of it has one (documents with none are left out).
pub(super) fn names_of<'a>(
    docs: impl IntoIterator<Item = &'a DocId>,
    trees: &[&RepoTree],
    json: &dyn JsonBlobs,
) -> Result<BTreeMap<DocId, String>> {
    let name_in = |tree: &RepoTree, doc: &DocId| -> Result<Option<String>> {
        let Some(entry) = tree
            .docs
            .get(doc)
            .and_then(|version| version.files.get(&DocFile::Metadata))
        else {
            return Ok(None);
        };
        Ok(json.json(&entry.id)?.and_then(|metadata| {
            metadata
                .get("visibleName")
                .and_then(Value::as_str)
                .map(str::to_string)
        }))
    };
    docs.into_iter()
        .map(|doc| -> Result<Option<(DocId, String)>> {
            let name = trees
                .iter()
                .map(|tree| name_in(tree, doc))
                .find_map(Result::transpose)
                .transpose()?;
            Ok(name.map(|name| (doc.clone(), name)))
        })
        .filter_map(Result::transpose)
        .collect()
}

/// Notes for documents the merge brings to the tablet that its own
/// history had deleted: absent from the snapshot and from the document's
/// base, but present in an earlier first-parent ancestor of that base.
/// Another tablet kept (or re-added) such a document; without a note it
/// would come back unexplained.
fn returned_after_deletion(
    store: &Store,
    main: &Oid,
    state: &TabletState,
    base_tree: &RepoTree,
    snapshot: &Snapshot,
    outcome: &MergeOutcome,
) -> Result<Vec<Note>> {
    let present =
        |tree: &RepoTree, doc: &DocId| tree.docs.get(doc).is_some_and(DocVersion::is_present);
    let candidates: Vec<(&DocId, &Oid)> = outcome
        .docs
        .iter()
        .filter(|(doc, version)| {
            version.files.contains_key(&DocFile::Metadata)
                && !present(&snapshot.tree, doc)
                && !present(base_tree, doc)
        })
        .filter_map(|(doc, _)| Some((doc, state.base_for(doc)?)))
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let index = store.history(main)?;
    Ok(candidates
        .into_iter()
        .filter(|(doc, base)| index.deleted_at(base, doc) == Some(true))
        .map(|(doc, _)| Note {
            doc: doc.clone(),
            kind: NoteKind::ReturnedAfterDeletion,
        })
        .collect())
}

/// Every document of either tree has equivalent versions in both.
fn trees_equivalent(
    rules: &MirrorRules,
    a: &RepoTree,
    b: &RepoTree,
    json: &dyn JsonBlobs,
    except: &BTreeSet<DocId>,
) -> Result<bool> {
    a.docs
        .keys()
        .chain(b.docs.keys())
        .filter(|doc| !except.contains(*doc))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .try_fold(true, |same, doc| {
            Ok(same && plan::versions_equivalent(rules, a.docs.get(doc), b.docs.get(doc), json)?)
        })
}

/// Write every new blob of a merge outcome; its tree, and the new
/// blobs' contents (the cache has them only once the commit is written).
fn realise(
    writer: &mut CommitWriter,
    outcome: &MergeOutcome,
) -> Result<(RepoTree, HashMap<Oid, Vec<u8>>)> {
    let mut written = HashMap::new();
    let docs = outcome
        .docs
        .iter()
        .map(|(doc, version)| {
            let files = version
                .files
                .iter()
                .map(|(file, entry)| {
                    let entry = match entry {
                        MergedEntry::Existing(entry) => entry.clone(),
                        MergedEntry::New(bytes) => {
                            let oid = writer.write_blob(
                                &format!("docs/{doc}/{}", file.repo_name()),
                                bytes.len() as u64,
                                &mut &bytes[..],
                            )?;
                            written.insert(oid.clone(), bytes.clone());
                            ListingEntry::regular(oid)
                        }
                    };
                    Ok((file.clone(), entry))
                })
                .collect::<Result<_>>()?;
            Ok((
                doc.clone(),
                DocVersion {
                    files,
                    other: version.other.clone(),
                },
            ))
        })
        .collect::<Result<_>>()?;
    Ok((
        RepoTree {
            docs,
            extra: outcome.extra.clone(),
        },
        written,
    ))
}
