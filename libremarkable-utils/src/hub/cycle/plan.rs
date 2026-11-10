//! Pure decisions of the sync cycle (see `docs/hub-design.md`): when two
//! versions of a document are equivalent, what to write to the tablet
//! and in which order, which planned documents a verification or a
//! live-tablet tree check skips, how an interrupted run's documents are
//! reconciled, and which manifest entries to record. Blob contents come
//! through [`JsonBlobs`]; nothing here does I/O of its own.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::rc::Rc;

use serde_json::Value;

use crate::error::Result;
use crate::git::{FileMode, Oid};
use crate::hub::merge::{TreeItem, TreeViolation, tree_violations};
use crate::hub::model::{
    DocFile, DocId, DocVersion, JsonFile, ListingEntry, ManifestEntry, MirrorRules, RepoTree,
    StatSignature, XochitlPath,
};
use crate::xochitl::TRASH_PARENT;

/// Parsed `.metadata`/`.content` blobs.
pub(super) trait JsonBlobs {
    /// The blob as JSON; `None` if it is not valid JSON.
    fn json(&self, id: &Oid) -> Result<Option<Rc<Value>>>;
}

impl JsonBlobs for HashMap<Oid, Value> {
    fn json(&self, id: &Oid) -> Result<Option<Rc<Value>>> {
        Ok(self.get(id).cloned().map(Rc::new))
    }
}

/// The JSON kind of a mirrored file, if it is one.
pub(super) fn json_kind(file: &DocFile) -> Option<JsonFile> {
    match file {
        DocFile::Metadata => Some(JsonFile::Metadata),
        DocFile::Content => Some(JsonFile::Content),
        _ => None,
    }
}

/// A version's files if it is a present document (has `metadata`), else
/// none: a subtree without `metadata` is never applied to a tablet.
fn present_files(version: Option<&DocVersion>) -> Option<&BTreeMap<DocFile, ListingEntry>> {
    version
        .filter(|version| version.is_present())
        .map(|version| &version.files)
}

/// Whether two JSON values of one kind are equal once view-state and
/// derived fields are removed.
pub(super) fn json_equivalent(rules: &MirrorRules, kind: JsonFile, a: &Value, b: &Value) -> bool {
    a == b || rules.normalise(kind, a) == rules.normalise(kind, b)
}

/// Whether two blobs of `file` are equivalent: the same blob, or for
/// `.metadata`/`.content` equal JSON once normalised. Unparsable JSON is
/// equivalent only to the same blob.
pub(super) fn blobs_equivalent(
    rules: &MirrorRules,
    file: &DocFile,
    a: &Oid,
    b: &Oid,
    json: &dyn JsonBlobs,
) -> Result<bool> {
    if a == b {
        return Ok(true);
    }
    let Some(kind) = json_kind(file) else {
        return Ok(false);
    };
    Ok(match (json.json(a)?, json.json(b)?) {
        (Some(a), Some(b)) => json_equivalent(rules, kind, &a, &b),
        _ => false,
    })
}

fn entries_equivalent(
    rules: &MirrorRules,
    file: &DocFile,
    a: &ListingEntry,
    b: &ListingEntry,
    json: &dyn JsonBlobs,
) -> Result<bool> {
    Ok(a.mode == b.mode && blobs_equivalent(rules, file, &a.id, &b.id, json)?)
}

/// Whether two versions of a document are equivalent: both absent, or
/// both present with the same mirrored files, each equivalent
/// ([`blobs_equivalent`]). Carried-forward paths are not compared: they
/// never reach a tablet.
pub(super) fn versions_equivalent(
    rules: &MirrorRules,
    a: Option<&DocVersion>,
    b: Option<&DocVersion>,
    json: &dyn JsonBlobs,
) -> Result<bool> {
    match (present_files(a), present_files(b)) {
        (None, None) => Ok(true),
        (Some(a), Some(b)) if a.keys().eq(b.keys()) => {
            a.iter().try_fold(true, |same, (file, entry)| {
                Ok(same && entries_equivalent(rules, file, entry, &b[file], json)?)
            })
        }
        _ => Ok(false),
    }
}

// ---- the apply plan -----------------------------------------------------------

/// What a planned file's bytes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Content {
    /// A blob of the local cache, written as it is.
    Blob(Oid),
    /// Merged JSON with the tablet's view state overlaid: bytes that are
    /// not a blob rmu has.
    Bytes(Vec<u8>),
}

/// One file to move into place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Write {
    pub file: DocFile,
    pub mode: FileMode,
    pub content: Content,
}

/// One document's change on the tablet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Change {
    Write {
        writes: Vec<Write>,
        removes: Vec<DocFile>,
    },
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Planned {
    pub doc: DocId,
    pub change: Change,
}

/// The JSON a document has on the tablet now, by kind: what view state
/// is overlaid from.
pub(super) type LocalJson = BTreeMap<JsonFile, Value>;

/// Plan one document: bring the tablet from `current` (the files it has,
/// as read) to `target`. A `.metadata`/`.content` is written only when it
/// differs once normalised, with the tablet's view state (`local`)
/// overlaid, or for a document new to the tablet without model-specific
/// fields; other files when their blob or mode differs. Mirrored files
/// the tablet has and `target` lacks are removed (also leftovers without
/// a `.metadata`, once the document exists); a present document `target`
/// lacks is deleted. Leftovers of a document `target` lacks are left
/// alone. `None` when nothing needs to change.
pub(super) fn plan_doc(
    rules: &MirrorRules,
    doc: &DocId,
    target: Option<&DocVersion>,
    current: Option<&DocVersion>,
    local: &LocalJson,
    json: &dyn JsonBlobs,
) -> Result<Option<Planned>> {
    let present = current.is_some_and(DocVersion::is_present);
    let current = current.map(|current| &current.files);
    let change = match (present_files(target), current) {
        (None, _) if present => Some(Change::Delete),
        (None, _) => None,
        (Some(target), current) => {
            let writes = target
                .iter()
                .map(|(file, entry)| -> Result<Option<Write>> {
                    let unchanged = match current.and_then(|current| current.get(file)) {
                        Some(now) => entries_equivalent(rules, file, entry, now, json)?,
                        None => false,
                    };
                    if unchanged {
                        return Ok(None);
                    }
                    let content = match json_kind(file) {
                        Some(kind) => overlaid(
                            rules,
                            kind,
                            &entry.id,
                            present.then(|| local.get(&kind)).flatten(),
                            json,
                        )?,
                        None => Content::Blob(entry.id.clone()),
                    };
                    Ok(Some(Write {
                        file: file.clone(),
                        mode: entry.mode,
                        content,
                    }))
                })
                .filter_map(Result::transpose)
                .collect::<Result<Vec<_>>>()?;
            let removes: Vec<DocFile> = current
                .into_iter()
                .flat_map(BTreeMap::keys)
                .filter(|file| !target.contains_key(*file))
                .cloned()
                .collect();
            (!writes.is_empty() || !removes.is_empty()).then_some(Change::Write { writes, removes })
        }
    };
    Ok(change.map(|change| Planned {
        doc: doc.clone(),
        change,
    }))
}

/// The bytes to write for a merged JSON blob: the blob itself when the
/// overlay changes nothing, else the overlaid value.
fn overlaid(
    rules: &MirrorRules,
    kind: JsonFile,
    id: &Oid,
    local: Option<&Value>,
    json: &dyn JsonBlobs,
) -> Result<Content> {
    let Some(target) = json.json(id)? else {
        return Ok(Content::Blob(id.clone()));
    };
    let applied = rules.view_state_for_apply(kind, &target, local);
    Ok(if applied == *target {
        Content::Blob(id.clone())
    } else {
        Content::Bytes(serde_json::to_vec_pretty(&applied).expect("JSON values serialize"))
    })
}

/// What [`plan`] needs.
pub(super) struct PlanInput<'a> {
    pub rules: &'a MirrorRules,
    pub merged: &'a RepoTree,
    /// The tablet's mirrored files as read, per document.
    pub tablet: &'a BTreeMap<DocId, DocVersion>,
    /// Never written or deleted (quarantined documents).
    pub protect: &'a BTreeSet<DocId>,
    /// The tablet's current JSON, per document.
    pub local: &'a BTreeMap<DocId, LocalJson>,
    pub json: &'a dyn JsonBlobs,
}

/// Whether the tablet's files of a document without a `.metadata` may be
/// overwritten: only when every one is already the target's (what rmu's
/// own interrupted write leaves). Anything else may be the user's
/// document that lost its `.metadata`; it is left alone (and reported as
/// an orphan by validation) rather than overwritten.
fn leftovers_are_ours(
    rules: &MirrorRules,
    target: Option<&DocVersion>,
    current: &DocVersion,
    json: &dyn JsonBlobs,
) -> Result<bool> {
    let Some(target) = present_files(target) else {
        return Ok(true);
    };
    current.files.iter().try_fold(true, |ours, (file, entry)| {
        Ok(ours
            && match target.get(file) {
                Some(wanted) => entries_equivalent(rules, file, wanted, entry, json)?,
                None => false,
            })
    })
}

/// Documents the apply leaves alone: on the tablet without a
/// `.metadata`, present in the merge, and not rmu's own leftovers of it
/// ([`leftovers_are_ours`]).
pub(super) fn left_alone(
    rules: &MirrorRules,
    merged: &RepoTree,
    tablet: &BTreeMap<DocId, DocVersion>,
    json: &dyn JsonBlobs,
) -> Result<BTreeSet<DocId>> {
    tablet
        .iter()
        .filter(|(doc, current)| {
            !current.is_present() && merged.docs.get(*doc).is_some_and(DocVersion::is_present)
        })
        .map(|(doc, current)| {
            Ok(
                (!leftovers_are_ours(rules, merged.docs.get(doc), current, json)?)
                    .then(|| doc.clone()),
            )
        })
        .filter_map(Result::transpose)
        .collect()
}

/// What [`plan`] decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PlanOutput {
    /// Every document whose tablet files must change, in document order.
    pub planned: Vec<Planned>,
    /// Documents the merge would change but whose files without a
    /// `.metadata` are not rmu's leftovers: left alone, they keep their
    /// previous base (as quarantined ones do), so a later repair never
    /// reads the merge as the tablet's own change.
    pub left_alone: BTreeSet<DocId>,
}

/// Plan the apply. Protected documents, and files without a `.metadata`
/// that are not rmu's own leftovers, are never touched.
pub(super) fn plan(input: &PlanInput) -> Result<PlanOutput> {
    let empty = LocalJson::new();
    let ids: BTreeSet<&DocId> = input
        .merged
        .docs
        .keys()
        .chain(input.tablet.keys())
        .filter(|doc| !input.protect.contains(*doc))
        .collect();
    ids.into_iter()
        .try_fold(PlanOutput::default(), |mut output, doc| {
            let current = input.tablet.get(doc);
            let Some(planned) = plan_doc(
                input.rules,
                doc,
                input.merged.docs.get(doc),
                current,
                input.local.get(doc).unwrap_or(&empty),
                input.json,
            )?
            else {
                return Ok(output);
            };
            let ours = match current {
                Some(current) if !current.is_present() => leftovers_are_ours(
                    input.rules,
                    input.merged.docs.get(doc),
                    current,
                    input.json,
                )?,
                _ => true,
            };
            if ours {
                output.planned.push(planned);
            } else {
                output.left_alone.insert(doc.clone());
            }
            Ok(output)
        })
}

/// Number of ancestors of `id` in `items` (bounded, so a cycle ends).
fn depth(items: &BTreeMap<DocId, TreeItem>, id: &DocId) -> usize {
    chain(items, id).len() - 1
}

/// Apply order: writes first, parents before children (by depth in the
/// merged tree), conflict copies before other documents at the same
/// depth, so an interrupted apply leaves the tablet's own version
/// somewhere; then deletions, children before parents (by depth in the
/// tablet's tree), so an interruption never orphans a remaining item.
pub(super) fn order(
    mut planned: Vec<Planned>,
    merged: &BTreeMap<DocId, TreeItem>,
    current: &BTreeMap<DocId, TreeItem>,
    copies: &BTreeSet<DocId>,
) -> Vec<Planned> {
    planned.sort_by_cached_key(|entry| match entry.change {
        Change::Write { .. } => (
            0,
            depth(merged, &entry.doc) as isize,
            !copies.contains(&entry.doc),
            entry.doc.clone(),
        ),
        Change::Delete => (
            1,
            -(depth(current, &entry.doc) as isize),
            false,
            entry.doc.clone(),
        ),
    });
    planned
}

// ---- checks before the apply ----------------------------------------------------

/// A file of a planned document whose stat moved since the snapshot, as
/// re-read at step 14.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Reread {
    /// `.metadata`/`.content`: the parsed file (`None` if unparsable).
    Json(Option<Value>),
    /// Any other file: its SHA-256.
    Sha256(String),
}

/// Whether a planned document's tablet files still match what the
/// snapshot read (`before`, by blob): the same mirrored files, and each
/// file whose stat moved since equivalent to what was read
/// (`.metadata`/`.content` as normalised JSON, so a document merely being
/// read is not skipped forever; other files by SHA-256, `sha256` giving
/// the read blobs').
pub(super) fn verified(
    rules: &MirrorRules,
    before: &BTreeMap<DocFile, Oid>,
    files_now: &BTreeSet<DocFile>,
    moved: &BTreeMap<DocFile, Reread>,
    json: &dyn JsonBlobs,
    sha256: &HashMap<Oid, String>,
) -> Result<bool> {
    if !before.keys().eq(files_now.iter()) {
        return Ok(false);
    }
    moved.iter().try_fold(true, |ok, (file, now)| {
        if !ok {
            return Ok(false);
        }
        let Some(blob) = before.get(file) else {
            return Ok(false);
        };
        Ok(match (now, json_kind(file)) {
            (Reread::Json(Some(value)), Some(kind)) => json
                .json(blob)?
                .is_some_and(|read| json_equivalent(rules, kind, &read, value)),
            (Reread::Sha256(hash), None) => sha256.get(blob) == Some(hash),
            _ => false,
        })
    })
}

/// The documents tied to each other as fork groups (original and copy):
/// skipped together or not at all.
pub(super) type Partners = BTreeMap<DocId, BTreeSet<DocId>>;

/// Fork groups from `(original, copy)` pairs.
pub(super) fn partners(forks: impl IntoIterator<Item = (DocId, DocId)>) -> Partners {
    forks
        .into_iter()
        .fold(Partners::new(), |mut partners, (original, copy)| {
            partners
                .entry(original.clone())
                .or_default()
                .insert(copy.clone());
            partners.entry(copy).or_default().insert(original);
            partners
        })
}

/// `docs` and, transitively, their fork partners.
pub(super) fn with_partners(docs: BTreeSet<DocId>, partners: &Partners) -> BTreeSet<DocId> {
    // Each step adds at least one document or ends the walk.
    std::iter::successors(Some(docs), |set| {
        let grown: BTreeSet<DocId> = set
            .iter()
            .flat_map(|doc| partners.get(doc).into_iter().flatten())
            .chain(set)
            .cloned()
            .collect();
        (grown.len() > set.len()).then_some(grown)
    })
    .last()
    .unwrap_or_default()
}

fn involved(violation: &TreeViolation) -> Vec<DocId> {
    match violation {
        TreeViolation::MissingParent { item, parent } => std::iter::once(item.clone())
            .chain(DocId::new(parent.as_str()).ok())
            .collect(),
        TreeViolation::ParentNotFolder { item, parent } => vec![item.clone(), parent.clone()],
        TreeViolation::Cycle(members) => members.clone(),
    }
}

/// The tablet's tree after applying the planned documents not skipped:
/// `planned` maps each planned document to its item after the apply
/// (`None`: deleted).
fn project(
    current: &BTreeMap<DocId, TreeItem>,
    planned: &BTreeMap<DocId, Option<TreeItem>>,
    skipped: &BTreeSet<DocId>,
) -> BTreeMap<DocId, TreeItem> {
    planned
        .iter()
        .filter(|(doc, _)| !skipped.contains(*doc))
        .fold(current.clone(), |mut items, (doc, item)| {
            match item {
                Some(item) => items.insert(doc.clone(), item.clone()),
                None => items.remove(doc),
            };
            items
        })
}

/// `id` and its ancestors in `items`, nearest first (bounded, so a cycle
/// ends).
fn chain(items: &BTreeMap<DocId, TreeItem>, id: &DocId) -> Vec<DocId> {
    std::iter::successors(Some(id.clone()), |current| {
        items
            .get(current)
            .and_then(|item| DocId::new(item.parent.as_str()).ok())
            .filter(|parent| items.contains_key(parent))
    })
    .take(items.len() + 1)
    .collect()
}

fn on_trash(items: &BTreeMap<DocId, TreeItem>, id: &DocId) -> bool {
    chain(items, id).iter().any(|member| {
        items
            .get(member)
            .is_some_and(|item| item.parent == TRASH_PARENT)
    })
}

/// Skip planned documents until `blame` finds nothing more to skip:
/// `blame` gets the skipped set and returns the planned documents to add
/// (none: done). Every round skips at least one more planned document or
/// ends, and once all are skipped nothing differs from the tablet, so the
/// bound is never reached; reaching it skips everything.
fn skip_until_settled(
    planned: &BTreeMap<DocId, Option<TreeItem>>,
    partners: &Partners,
    blame: impl Fn(&BTreeSet<DocId>) -> Option<BTreeSet<DocId>>,
) -> BTreeSet<DocId> {
    let everything = || planned.keys().cloned().collect::<BTreeSet<_>>();
    let outcome = (0..=planned.len()).try_fold(BTreeSet::new(), |skipped, _| {
        match blame(&skipped) {
            None => ControlFlow::Break(skipped),
            Some(blamed) => {
                let blamed: BTreeSet<DocId> = blamed
                    .into_iter()
                    .filter(|doc| planned.contains_key(doc) && !skipped.contains(doc))
                    .collect();
                if blamed.is_empty() {
                    // Cannot happen (only planned documents differ from
                    // the tablet); skipping everything is the safe answer.
                    return ControlFlow::Break(everything());
                }
                ControlFlow::Continue(
                    skipped
                        .union(&with_partners(blamed, partners))
                        .cloned()
                        .collect(),
                )
            }
        }
    });
    match outcome {
        ControlFlow::Break(skipped) => skipped,
        ControlFlow::Continue(_) => everything(),
    }
}

/// The step-14 tree checks: planned documents whose change would give
/// the live tablet a violation it does not have now (an orphan, e.g. a
/// folder deleted that gained children; a parent that is not a folder;
/// a cycle) are skipped, with their fork partners, until the projected
/// tree has no new violations. `planned` maps each planned document to
/// its item after the apply (`None`: deleted).
pub(super) fn tree_skips(
    current: &BTreeMap<DocId, TreeItem>,
    planned: &BTreeMap<DocId, Option<TreeItem>>,
    partners: &Partners,
) -> BTreeSet<DocId> {
    let baseline: BTreeSet<TreeViolation> = tree_violations(current).into_iter().collect();
    skip_until_settled(planned, partners, |skipped| {
        let fresh: Vec<TreeViolation> = tree_violations(&project(current, planned, skipped))
            .into_iter()
            .filter(|violation| !baseline.contains(violation))
            .collect();
        (!fresh.is_empty()).then(|| fresh.iter().flat_map(involved).collect())
    })
}

/// Touched documents (changed by the user since the snapshot) that the
/// projected tree puts in the trash although they are not there now, and
/// everything above them: e.g. a folder origin trashed while the user
/// wrote in a notebook inside it. That notebook would land in the trash
/// on a side whose own tree then has it there, and the edit could no
/// longer pull it out; the changes doing it are skipped instead.
fn trash_blame<'a>(
    current: &'a BTreeMap<DocId, TreeItem>,
    projected: &'a BTreeMap<DocId, TreeItem>,
    touched: &'a BTreeSet<DocId>,
) -> impl Iterator<Item = DocId> + 'a {
    touched
        .iter()
        .filter(|doc| {
            projected.contains_key(*doc) && on_trash(projected, doc) && !on_trash(current, doc)
        })
        .flat_map(|doc| chain(projected, doc))
}

// ---- reconciliation ---------------------------------------------------------------

/// Where an interrupted run left one document whose merge and snapshot
/// versions differ (see "Restartability").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Every file is at the merge version: base = merge.
    AtMerge,
    /// Every file is still at the snapshot version: base override =
    /// snapshot, and the next merge brings the change again.
    AtSnapshot,
    /// Every file is at one of the two, some at each (the apply stopped
    /// inside this document): finish applying it, then base = merge.
    Mixed,
    /// The apply stopped inside this document and the user changed it
    /// since, but only in files the merge leaves alone or the apply had
    /// already written: finish it to this version (the files still
    /// pending from the merge, the user's elsewhere: [`partial_target`]),
    /// then base = merge.
    Partial(DocVersion),
    /// Anything else (the user changed it): base override = snapshot.
    Other,
}

/// Judge one document: `tablet` is its mirrored files now, by blob.
pub(super) fn verdict(
    rules: &MirrorRules,
    merge: Option<&DocVersion>,
    snapshot: Option<&DocVersion>,
    tablet: &BTreeMap<DocFile, Oid>,
    json: &dyn JsonBlobs,
) -> Result<Verdict> {
    let empty = BTreeMap::new();
    let (merge, snapshot) = (
        present_files(merge).unwrap_or(&empty),
        present_files(snapshot).unwrap_or(&empty),
    );
    let files: BTreeSet<&DocFile> = merge
        .keys()
        .chain(snapshot.keys())
        .chain(tablet.keys())
        .collect();
    let at = |version: &BTreeMap<DocFile, ListingEntry>, file: &DocFile| -> Result<bool> {
        match (version.get(file), tablet.get(file)) {
            (None, None) => Ok(true),
            (Some(entry), Some(now)) => blobs_equivalent(rules, file, &entry.id, now, json),
            _ => Ok(false),
        }
    };
    let (all_merge, all_snapshot, each_either) = files.into_iter().try_fold(
        (true, true, true),
        |(all_merge, all_snapshot, each_either), file| -> Result<_> {
            let (m, s) = (at(merge, file)?, at(snapshot, file)?);
            Ok((all_merge && m, all_snapshot && s, each_either && (m || s)))
        },
    )?;
    Ok(match (all_merge, all_snapshot, each_either) {
        (true, _, _) => Verdict::AtMerge,
        (_, true, _) => Verdict::AtSnapshot,
        (_, _, true) => Verdict::Mixed,
        _ => Verdict::Other,
    })
}

/// Whether an interrupted apply finished a document that the user has
/// changed since ([`Verdict::Other`]): every file the apply was to write
/// (merge files not equivalent to the snapshot's) has left the staging
/// directory (`pending`: the files still staged by the run that staged
/// this merge completely), and every file it was to remove is gone. The
/// user's changes are then on top of the merge (base = merge) rather
/// than on top of the snapshot, which would read the merge as their
/// own change and fork it.
pub(super) fn apply_finished(
    rules: &MirrorRules,
    doc: &DocId,
    merge: Option<&DocVersion>,
    snapshot: Option<&DocVersion>,
    tablet: &BTreeMap<DocFile, Oid>,
    pending: &BTreeSet<String>,
    json: &dyn JsonBlobs,
) -> Result<bool> {
    let (Some(merge), snapshot) = (present_files(merge), present_files(snapshot)) else {
        return Ok(false);
    };
    let written = merge
        .iter()
        .try_fold(true, |done, (file, entry)| -> Result<bool> {
            if !done {
                return Ok(false);
            }
            let unchanged = match snapshot.and_then(|snapshot| snapshot.get(file)) {
                Some(before) => entries_equivalent(rules, file, entry, before, json)?,
                None => false,
            };
            Ok(unchanged || !pending.contains(&file.xochitl_path(doc)))
        })?;
    let removed = snapshot
        .into_iter()
        .flat_map(BTreeMap::keys)
        .filter(|file| !merge.contains_key(*file))
        .all(|file| !tablet.contains_key(file));
    Ok(written && removed)
}

/// For a document an interrupted apply stopped inside and the user changed
/// since ([`Verdict::Other`] by its files): the version to finish it to,
/// if every file is one of
/// - at the merge version;
/// - one the merge leaves as the snapshot had it, whatever the user did
///   to it (their change);
/// - at the snapshot version with the apply's write still pending
///   (`pending`: the files the run that staged this merge completely
///   has not moved) or, for a file the merge removes, still there;
/// - one the merge changes that the apply had moved into place, changed
///   by the user since (their change on top of the merge);
///
/// and some file of the merge's change was written and some is pending.
/// The version is the merge's files where they are pending and the
/// tablet's elsewhere, so finishing writes only the pending files, never
/// one the user changed, and the document then takes base = merge with
/// the user's changes on top. `None` otherwise (nothing applied yet: the
/// snapshot base merges the user's change; or a user change the merge
/// conflicts with: forked by the next merge, see "Known limits").
pub(super) fn partial_target(
    rules: &MirrorRules,
    doc: &DocId,
    merge: Option<&DocVersion>,
    snapshot: Option<&DocVersion>,
    tablet: &BTreeMap<DocFile, Oid>,
    pending: &BTreeSet<String>,
    json: &dyn JsonBlobs,
) -> Result<Option<DocVersion>> {
    let (Some(merge), Some(snapshot)) = (present_files(merge), present_files(snapshot)) else {
        return Ok(None);
    };
    let same = |file: &DocFile, a: Option<&Oid>, b: Option<&Oid>| -> Result<bool> {
        match (a, b) {
            (None, None) => Ok(true),
            (Some(a), Some(b)) => blobs_equivalent(rules, file, a, b, json),
            _ => Ok(false),
        }
    };
    let files: BTreeSet<&DocFile> = merge
        .keys()
        .chain(snapshot.keys())
        .chain(tablet.keys())
        .collect();
    // Per file: take the merge's entry (pending) or keep the tablet's,
    // and whether the merge's change to it was written.
    let judged = files.into_iter().try_fold(
        Some((BTreeMap::new(), false, false)),
        |state, file| -> Result<_> {
            let Some((mut target, written, waiting)) = state else {
                return Ok(None);
            };
            let m = merge.get(file);
            let (m_id, s_id, t_id) = (
                m.map(|e| &e.id),
                snapshot.get(file).map(|e| &e.id),
                tablet.get(file),
            );
            let at_merge = same(file, m_id, t_id)?;
            let merge_keeps = same(file, m_id, s_id)?;
            let staged = pending.contains(&file.xochitl_path(doc));
            let keep = |target: &mut BTreeMap<DocFile, ListingEntry>| {
                if let Some(blob) = t_id {
                    let mode = m
                        .or_else(|| snapshot.get(file))
                        .map_or(crate::git::FileMode::Regular, |entry| entry.mode);
                    target.insert(
                        file.clone(),
                        ListingEntry {
                            mode,
                            id: blob.clone(),
                        },
                    );
                }
            };
            let (written, waiting) = if at_merge {
                keep(&mut target);
                (written || !merge_keeps, waiting)
            } else if merge_keeps {
                keep(&mut target);
                (written, waiting)
            } else if same(file, s_id, t_id)? && (m.is_none() || staged) {
                if let Some(entry) = m {
                    target.insert(file.clone(), entry.clone());
                }
                (written, true)
            } else if m.is_some() && !staged {
                keep(&mut target);
                (true, waiting)
            } else {
                return Ok(None);
            };
            Ok(Some((target, written, waiting)))
        },
    )?;
    Ok(judged
        .filter(|(target, written, waiting)| {
            *written && *waiting && target.contains_key(&DocFile::Metadata)
        })
        .map(|(files, ..)| DocVersion {
            files,
            other: BTreeMap::new(),
        }))
}

/// The first [`partial_target`] of `doc` among the runs that planned it.
fn partial_of(
    input: &Journaled,
    doc: &DocId,
    files: &BTreeMap<DocFile, Oid>,
) -> Result<Option<DocVersion>> {
    input
        .staged
        .iter()
        .filter(|run| run.planned.contains(doc))
        .map(|run| {
            partial_target(
                input.rules,
                doc,
                input.merge.docs.get(doc),
                input.snapshot.docs.get(doc),
                files,
                &run.pending,
                input.json,
            )
        })
        .find_map(Result::transpose)
        .transpose()
}

/// Staged after everything an apply of a merge needs, in the same run's
/// staging directory: `.rmu-staged-<merge>/<doc>` for every document the
/// plan changes, then `.rmu-staged-<merge>/done`, in one transfer (in
/// order, so `done` means the whole list arrived). Reconciliation reads
/// them back through the staging listing: which documents a run planned,
/// and which of their files it had not moved yet.
pub(super) const STAGED_SENTINEL: &str = ".rmu-staged-";

/// The sentinel files for an apply of `merge` changing `docs`, `done`
/// last.
pub(super) fn sentinel_files(merge: &Oid, docs: impl IntoIterator<Item = DocId>) -> Vec<String> {
    docs.into_iter()
        .map(|doc| format!("{STAGED_SENTINEL}{merge}/{doc}"))
        .chain(std::iter::once(format!("{STAGED_SENTINEL}{merge}/done")))
        .collect()
}

/// One run that completely staged an apply of the journaled merge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct StagedRun {
    /// The documents its plan changed.
    pub planned: BTreeSet<DocId>,
    /// Its files still staged (xochitl-relative): not moved into place.
    pub pending: BTreeSet<String>,
}

/// The runs whose staging directory holds a complete sentinel for
/// `merge`, from the staging listing (`<run>/<path>` per staged file).
pub(super) fn staged_runs<'a>(
    listing: impl IntoIterator<Item = &'a str>,
    merge: &Oid,
) -> Vec<StagedRun> {
    let prefix = format!("{STAGED_SENTINEL}{merge}/");
    let runs = listing
        .into_iter()
        .filter_map(|path| path.split_once('/'))
        .fold(
            BTreeMap::<&str, (bool, StagedRun)>::new(),
            |mut runs, (run, path)| {
                let (done, staged) = runs.entry(run).or_default();
                match path.strip_prefix(&prefix) {
                    Some("done") => *done = true,
                    Some(doc) => {
                        if let Ok(doc) = DocId::new(doc) {
                            staged.planned.insert(doc);
                        }
                    }
                    // Another merge's sentinel says nothing about this one.
                    None if path.starts_with(STAGED_SENTINEL) => {}
                    None => {
                        staged.pending.insert(path.to_string());
                    }
                }
                runs
            },
        );
    runs.into_values()
        .filter_map(|(done, staged)| done.then_some(staged))
        .collect()
}

/// What reconciliation judges.
pub(super) struct Journaled<'a> {
    pub rules: &'a MirrorRules,
    pub merge: &'a RepoTree,
    pub snapshot: &'a RepoTree,
    /// The tablet's mirrored files now, per document, by blob.
    pub tablet: &'a BTreeMap<DocId, BTreeMap<DocFile, Oid>>,
    /// Runs that completely staged an apply of the merge.
    pub staged: &'a [StagedRun],
    /// `(original, copy)` of the merge's forks.
    pub forks: &'a [(DocId, DocId)],
    pub json: &'a dyn JsonBlobs,
}

/// The verdict on every document whose merge and snapshot versions
/// differ (see "Restartability"). [`verdict`] judges the files; then
/// only a document some run planned can be a [`Verdict::Mixed`] that
/// gets finished (anything else that looks half-applied, e.g. a
/// quarantined document that lost its `.metadata`, was never touched by
/// an apply and is [`Verdict::Other`]); a [`Verdict::Other`] that a run
/// planned and finished ([`apply_finished`]) is at the merge with the
/// user's changes on top; and a conflict copy is finished only together
/// with its original.
pub(super) fn verdicts(input: &Journaled) -> Result<BTreeMap<DocId, Verdict>> {
    let no_files = BTreeMap::new();
    let planned: BTreeSet<&DocId> = input.staged.iter().flat_map(|run| &run.planned).collect();
    let ids: BTreeSet<&DocId> = input
        .merge
        .docs
        .keys()
        .chain(input.snapshot.docs.keys())
        .collect();
    let verdicts = ids
        .into_iter()
        .map(|doc| -> Result<Option<(DocId, Verdict)>> {
            let (merge, snapshot) = (input.merge.docs.get(doc), input.snapshot.docs.get(doc));
            if versions_equivalent(input.rules, merge, snapshot, input.json)? {
                return Ok(None);
            }
            let files = input.tablet.get(doc).unwrap_or(&no_files);
            let judged = match verdict(input.rules, merge, snapshot, files, input.json)? {
                Verdict::Mixed if !planned.contains(doc) => Verdict::Other,
                Verdict::Other => {
                    let finished = input.staged.iter().try_fold(false, |found, run| {
                        Ok::<_, crate::Error>(
                            found
                                || (run.planned.contains(doc)
                                    && apply_finished(
                                        input.rules,
                                        doc,
                                        merge,
                                        snapshot,
                                        files,
                                        &run.pending,
                                        input.json,
                                    )?),
                        )
                    })?;
                    if finished {
                        Verdict::AtMerge
                    } else {
                        partial_of(input, doc, files)?.map_or(Verdict::Other, Verdict::Partial)
                    }
                }
                other => other,
            };
            Ok(Some((doc.clone(), judged)))
        })
        .filter_map(Result::transpose)
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(input
        .forks
        .iter()
        .fold(verdicts, |mut verdicts, (original, copy)| {
            let original_done = matches!(
                verdicts.get(original),
                None | Some(Verdict::AtMerge | Verdict::Mixed | Verdict::Partial(_))
            );
            if !original_done
                && matches!(
                    verdicts.get(copy),
                    Some(Verdict::Mixed | Verdict::Partial(_))
                )
            {
                verdicts.insert(copy.clone(), Verdict::Other);
            }
            verdicts
        }))
}

/// What reconciliation finishes.
pub(super) struct Finish<'a> {
    /// What the verdicts were judged on.
    pub journaled: &'a Journaled<'a>,
    pub verdicts: &'a BTreeMap<DocId, Verdict>,
    /// The tablet's current JSON, per document (view state to overlay).
    pub local: &'a BTreeMap<DocId, LocalJson>,
    /// The tablet's tree as read, and every merged document's item.
    pub current: &'a BTreeMap<DocId, TreeItem>,
    pub merged: &'a BTreeMap<DocId, TreeItem>,
}

/// Reconciliation's finish (see "Restartability"): a plan for every
/// [`Verdict::Mixed`] document (an interrupted deletion finished as a
/// deletion, leftovers included), minus those the step-14 tree checks
/// refuse on the tablet as read, in apply order; and the refused ones,
/// which keep the snapshot as their base instead.
pub(super) fn finish(input: &Finish) -> Result<(Vec<Planned>, BTreeSet<DocId>)> {
    let journaled = input.journaled;
    let empty = LocalJson::new();
    let no_files = BTreeMap::new();
    // Each document to finish, its plan, and whether the user's own
    // `.metadata` stays (its tree item is then the tablet's).
    let planned: Vec<(Planned, bool)> = input
        .verdicts
        .iter()
        .filter(|(_, verdict)| matches!(verdict, Verdict::Mixed | Verdict::Partial(_)))
        .map(|(doc, verdict)| -> Result<Option<(Planned, bool)>> {
            let files = journaled.tablet.get(doc).unwrap_or(&no_files);
            let (target, own_metadata) = match verdict {
                // The user's own `.metadata` stays when the target keeps it.
                Verdict::Partial(target) => (
                    target,
                    target.files.get(&DocFile::Metadata).map(|entry| &entry.id)
                        == files.get(&DocFile::Metadata),
                ),
                _ => match journaled.merge.docs.get(doc).filter(|v| v.is_present()) {
                    Some(target) => (target, false),
                    // An interrupted deletion (`.metadata` goes first):
                    // finish it, leftovers included.
                    None => {
                        return Ok(Some((
                            Planned {
                                doc: doc.clone(),
                                change: Change::Delete,
                            },
                            false,
                        )));
                    }
                },
            };
            let current = journaled.tablet.get(doc).map(|files| DocVersion {
                files: files
                    .iter()
                    .map(|(file, blob)| (file.clone(), ListingEntry::regular(blob.clone())))
                    .collect(),
                other: BTreeMap::new(),
            });
            Ok(plan_doc(
                journaled.rules,
                doc,
                Some(target),
                current.as_ref(),
                input.local.get(doc).unwrap_or(&empty),
                journaled.json,
            )?
            .map(|planned| (planned, own_metadata)))
        })
        .filter_map(Result::transpose)
        .collect::<Result<_>>()?;
    let items: BTreeMap<DocId, Option<TreeItem>> = planned
        .iter()
        .map(|(entry, own_metadata)| {
            let item = match (&entry.change, own_metadata) {
                (Change::Delete, _) => None,
                (Change::Write { .. }, true) => input.current.get(&entry.doc).cloned(),
                (Change::Write { .. }, false) => input.merged.get(&entry.doc).cloned(),
            };
            (entry.doc.clone(), item)
        })
        .collect();
    let skipped = tree_skips(
        input.current,
        &items,
        &partners(journaled.forks.iter().cloned()),
    );
    let copies: BTreeSet<DocId> = journaled
        .forks
        .iter()
        .map(|(_, copy)| copy.clone())
        .collect();
    let kept = planned
        .into_iter()
        .map(|(entry, _)| entry)
        .filter(|entry| !skipped.contains(&entry.doc))
        .collect();
    Ok((order(kept, input.merged, input.current, &copies), skipped))
}

/// Step 14: the mirrored files whose stat moved since the read: changed
/// or gone, new, and those whose read stat is racy (a coarse mtime in the
/// second of the step-6 start or later, `started_at`: xochitl may have
/// changed them within that second without moving their stat).
pub(super) fn moved_since_read(
    read: &BTreeMap<XochitlPath, StatSignature>,
    now: &BTreeMap<XochitlPath, StatSignature>,
    started_at: i64,
) -> BTreeSet<XochitlPath> {
    read.iter()
        .filter(|(path, stat)| now.get(*path) != Some(stat) || !stat.keep_in_manifest(started_at))
        .map(|(path, _)| path.clone())
        .chain(now.keys().filter(|path| !read.contains_key(*path)).cloned())
        .collect()
}

// ---- step 14 ---------------------------------------------------------------------------

/// What the step-14 checks see.
pub(super) struct Checks<'a> {
    pub rules: &'a MirrorRules,
    pub planned: &'a [Planned],
    /// The step-5 read, per document, by blob.
    pub before: &'a BTreeMap<DocId, BTreeMap<DocFile, Oid>>,
    /// Mirrored files per document now.
    pub files_now: &'a BTreeMap<DocId, BTreeSet<DocFile>>,
    /// Re-read files of planned documents whose stat moved.
    pub moved: &'a BTreeMap<DocId, BTreeMap<DocFile, Reread>>,
    /// SHA-256 of the read blobs of moved non-JSON files.
    pub sha256: &'a HashMap<Oid, String>,
    pub json: &'a dyn JsonBlobs,
    /// The live tablet's tree, after the re-read.
    pub current: &'a BTreeMap<DocId, TreeItem>,
    /// Every merged document's item.
    pub merged: &'a BTreeMap<DocId, TreeItem>,
    /// Documents with any file moved, new or gone since the read.
    pub touched: &'a BTreeSet<DocId>,
    pub partners: &'a Partners,
}

/// The documents not to apply: those whose files changed since the read
/// ([`verified`]), then, to a fixpoint, changes that would carry a
/// touched document into the trash ([`trash_blame`]) or break the live
/// tree (as [`tree_skips`] judges), each with its fork partners.
pub(super) fn skipped(checks: &Checks) -> Result<BTreeSet<DocId>> {
    let (no_files, no_set, no_moves) = (BTreeMap::new(), BTreeSet::new(), BTreeMap::new());
    let changed = checks
        .planned
        .iter()
        .map(|entry| {
            verified(
                checks.rules,
                checks.before.get(&entry.doc).unwrap_or(&no_files),
                checks.files_now.get(&entry.doc).unwrap_or(&no_set),
                checks.moved.get(&entry.doc).unwrap_or(&no_moves),
                checks.json,
                checks.sha256,
            )
            .map(|ok| (!ok).then(|| entry.doc.clone()))
        })
        .filter_map(Result::transpose)
        .collect::<Result<BTreeSet<_>>>()?;
    let items = |skipped: &BTreeSet<DocId>| -> BTreeMap<DocId, Option<TreeItem>> {
        checks
            .planned
            .iter()
            .filter(|entry| !skipped.contains(&entry.doc))
            .map(|entry| {
                let item = match entry.change {
                    Change::Delete => None,
                    Change::Write { .. } => checks.merged.get(&entry.doc).cloned(),
                };
                (entry.doc.clone(), item)
            })
            .collect()
    };
    let changed = with_partners(changed, checks.partners);
    // One fixpoint for both checks: a change skipped by one can make
    // another fail the other.
    let planned: BTreeMap<DocId, Option<TreeItem>> = items(&changed);
    let baseline: BTreeSet<TreeViolation> = tree_violations(checks.current).into_iter().collect();
    let settled = skip_until_settled(&planned, checks.partners, |skipped| {
        let projected = project(checks.current, &planned, skipped);
        let blamed: BTreeSet<DocId> = tree_violations(&projected)
            .iter()
            .filter(|violation| !baseline.contains(*violation))
            .flat_map(involved)
            .chain(trash_blame(checks.current, &projected, checks.touched))
            .filter(|doc| planned.contains_key(doc) && !skipped.contains(doc))
            .collect();
        (!blamed.is_empty()).then_some(blamed)
    });
    Ok(with_partners(
        changed.union(&settled).cloned().collect(),
        checks.partners,
    ))
}

/// The writes of `plan` whose content differs from what was staged
/// (`staged`, by xochitl path): overlays re-done with view state re-read
/// at step 14.
pub(super) fn restage(plan: &[Planned], staged: &BTreeMap<String, Content>) -> Vec<Planned> {
    plan.iter()
        .filter_map(|entry| match &entry.change {
            Change::Write { writes, .. } => {
                let changed: Vec<Write> = writes
                    .iter()
                    .filter(|write| {
                        staged.get(&write.file.xochitl_path(&entry.doc)) != Some(&write.content)
                    })
                    .cloned()
                    .collect();
                (!changed.is_empty()).then(|| Planned {
                    doc: entry.doc.clone(),
                    change: Change::Write {
                        writes: changed,
                        removes: Vec::new(),
                    },
                })
            }
            Change::Delete => None,
        })
        .collect()
}

/// Every write of `plan` by xochitl path, with its content.
pub(super) fn contents(plan: &[Planned]) -> BTreeMap<String, Content> {
    plan.iter()
        .flat_map(|entry| match &entry.change {
            Change::Write { writes, .. } => writes
                .iter()
                .map(|write| (write.file.xochitl_path(&entry.doc), write.content.clone()))
                .collect(),
            Change::Delete => Vec::new(),
        })
        .collect()
}

/// The files `plan` removes (xochitl paths): removed files, and every
/// file read of a deleted document.
pub(super) fn removed(
    plan: &[Planned],
    read: &BTreeMap<DocId, BTreeMap<DocFile, Oid>>,
) -> BTreeSet<String> {
    plan.iter()
        .flat_map(|entry| match &entry.change {
            Change::Write { removes, .. } => removes
                .iter()
                .map(|file| file.xochitl_path(&entry.doc))
                .collect::<Vec<_>>(),
            Change::Delete => read
                .get(&entry.doc)
                .into_iter()
                .flat_map(BTreeMap::keys)
                .map(|file| file.xochitl_path(&entry.doc))
                .collect(),
        })
        .collect()
}

// ---- single stop ---------------------------------------------------------------------

/// What a sync would send over the network, estimated after the step-5
/// read and before the merge, to decide whether xochitl may stay stopped
/// through steps 7–12 ([`super::SingleStop`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct NetworkEstimate {
    /// Documents the apply would write or delete: those origin changed
    /// since the base and the tablet does not hold as origin does.
    pub apply_docs: usize,
    /// Origin's blobs those documents need that the tablet lacks (their
    /// sizes are in the local cache).
    pub apply_blobs: BTreeSet<Oid>,
    /// Bytes of the tablet's own: files of documents it changed whose
    /// blobs the base lacks (the device-ref push), plus the whole tablet
    /// version of documents both sides changed (a conflict copy is
    /// staged).
    pub tablet_bytes: u64,
}

/// The [`NetworkEstimate`] from the base, the tablet as read (each file's
/// blob and size) and origin's `main`, per document, by blob ids. An
/// estimate, not the plan: view state, the tree rules and the merge's own
/// decisions can only make the apply smaller or add a conflict copy,
/// which is counted. Pure.
pub(super) fn network_estimate(
    base: &BTreeMap<DocId, BTreeMap<DocFile, Oid>>,
    tablet: &BTreeMap<DocId, BTreeMap<DocFile, (Oid, u64)>>,
    origin: &BTreeMap<DocId, BTreeMap<DocFile, Oid>>,
) -> NetworkEstimate {
    let blobs = |files: Option<&BTreeMap<DocFile, (Oid, u64)>>| -> BTreeMap<DocFile, Oid> {
        files
            .into_iter()
            .flatten()
            .map(|(file, (blob, _))| (file.clone(), blob.clone()))
            .collect()
    };
    let none = BTreeMap::new();
    let changed_by_tablet = |doc: &DocId| blobs(tablet.get(doc)) != *base.get(doc).unwrap_or(&none);
    let to_apply: Vec<&DocId> = base
        .keys()
        .chain(origin.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|doc| {
            let theirs = origin.get(*doc).unwrap_or(&none);
            theirs != base.get(*doc).unwrap_or(&none) && *theirs != blobs(tablet.get(*doc))
        })
        .collect();
    let apply_blobs = to_apply
        .iter()
        .filter_map(|doc| origin.get(*doc).map(|files| (doc, files)))
        .flat_map(|(doc, files)| {
            let held: BTreeSet<Oid> = blobs(tablet.get(*doc)).into_values().collect();
            files
                .values()
                .filter(move |blob| !held.contains(*blob))
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect();
    let size = |files: &BTreeMap<DocFile, (Oid, u64)>, keep: &dyn Fn(&Oid) -> bool| -> u64 {
        files
            .values()
            .filter(|(blob, _)| keep(blob))
            .map(|(_, size)| size)
            .sum()
    };
    let pushed: u64 = tablet
        .iter()
        .filter(|(doc, _)| changed_by_tablet(doc))
        .map(|(doc, files)| {
            let based: BTreeSet<&Oid> = base
                .get(doc)
                .into_iter()
                .flat_map(BTreeMap::values)
                .collect();
            size(files, &|blob| !based.contains(blob))
        })
        .sum();
    let copies: u64 = to_apply
        .iter()
        .filter(|doc| changed_by_tablet(doc))
        .filter_map(|doc| tablet.get(*doc))
        .map(|files| size(files, &|_| true))
        .sum();
    NetworkEstimate {
        apply_docs: to_apply.len(),
        apply_blobs,
        tablet_bytes: pushed + copies,
    }
}

/// Why a sync starts xochitl rather than keep it stopped through step 16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartNow {
    /// Nothing to apply: the network work goes better with the UI up.
    NothingToApply,
    /// More to send or apply than a small sync may.
    TooLarge,
    /// The budget is spent.
    Overdue,
    /// The fetch under the lock took long (a slow origin, or credentials
    /// prompted for): the later network steps would too.
    SlowOrigin,
}

impl std::fmt::Display for StartNow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NothingToApply => "nothing to apply",
            Self::TooLarge => "more than a small sync",
            Self::Overdue => "the sync is taking long",
            Self::SlowOrigin => "origin is slow to answer",
        })
    }
}

/// Step 6: whether a sync keeps xochitl stopped, given the estimate, the
/// size of its origin blobs (`apply_bytes`), how long xochitl has been
/// stopped and how long the fetch under the lock took. `None`: keep it
/// stopped. A fetch that took over a quarter of the budget counts as
/// slow. Pure.
pub(super) fn single_stop_at_start(
    limits: &super::SingleStop,
    estimate: &NetworkEstimate,
    apply_bytes: u64,
    stopped_for: std::time::Duration,
    fetch_took: std::time::Duration,
) -> Option<StartNow> {
    if estimate.apply_docs == 0 {
        Some(StartNow::NothingToApply)
    } else if estimate.apply_docs > limits.max_docs
        || estimate.tablet_bytes.saturating_add(apply_bytes) > limits.max_bytes
    {
        Some(StartNow::TooLarge)
    } else if fetch_took > limits.budget / 4 {
        Some(StartNow::SlowOrigin)
    } else {
        single_stop_overdue(limits, stopped_for)
    }
}

/// Before staging: whether a sync that kept xochitl stopped goes on, given
/// its plan's documents and bytes to stage. Pure.
pub(super) fn single_stop_at_plan(
    limits: &super::SingleStop,
    docs: usize,
    bytes: u64,
    stopped_for: std::time::Duration,
) -> Option<StartNow> {
    if docs == 0 {
        Some(StartNow::NothingToApply)
    } else if docs > limits.max_docs || bytes > limits.max_bytes {
        Some(StartNow::TooLarge)
    } else {
        single_stop_overdue(limits, stopped_for)
    }
}

/// Between network steps: whether the budget is spent. Pure.
pub(super) fn single_stop_overdue(
    limits: &super::SingleStop,
    stopped_for: std::time::Duration,
) -> Option<StartNow> {
    (stopped_for >= limits.budget).then_some(StartNow::Overdue)
}

// ---- the manifest --------------------------------------------------------------------

/// What the manifest is recorded from.
pub(super) struct ManifestInput<'a> {
    /// Step-5 stat and blob of every mirrored file read.
    pub read: &'a BTreeMap<XochitlPath, (StatSignature, Oid)>,
    /// Device time of the xochitl start after the read (the racy-entry
    /// reference for its stats).
    pub read_reference_ns: i64,
    /// The stat pass before the apply, if there was one: files whose
    /// stat moved since the read are left out (read again next run).
    pub before_apply: Option<&'a BTreeMap<XochitlPath, StatSignature>>,
    /// Files the apply wrote: stat after the write, and blob if it is one
    /// rmu has (overlaid JSON is not, and is read again next run).
    pub written: &'a BTreeMap<XochitlPath, (StatSignature, Option<Oid>)>,
    /// Files the apply removed (with deleted documents).
    pub removed: &'a BTreeSet<XochitlPath>,
    /// Device time after the apply's writes.
    pub written_reference_ns: i64,
}

/// The manifest to record, racy entries left out
/// ([`StatSignature::keep_in_manifest`]).
pub(super) fn manifest(input: &ManifestInput) -> BTreeMap<XochitlPath, ManifestEntry> {
    let kept = input
        .read
        .iter()
        .filter(|(path, (stat, _))| {
            !input.written.contains_key(*path)
                && !input.removed.contains(*path)
                && input
                    .before_apply
                    .is_none_or(|now| now.get(*path) == Some(stat))
                && stat.keep_in_manifest(input.read_reference_ns)
        })
        .map(|(path, (stat, blob))| (path.clone(), *stat, blob.clone()));
    let written = input
        .written
        .iter()
        .filter(|(_, (stat, _))| stat.keep_in_manifest(input.written_reference_ns))
        .filter_map(|(path, (stat, blob))| Some((path.clone(), *stat, blob.clone()?)));
    kept.chain(written)
        .map(|(path, stat, blob)| (path, ManifestEntry { stat, blob }))
        .collect()
}

#[cfg(test)]
mod tests;
