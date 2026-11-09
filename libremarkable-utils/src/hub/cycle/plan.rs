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

/// Every document whose tablet files must change to match the merge,
/// in document order.
pub(super) fn plan(input: &PlanInput) -> Result<Vec<Planned>> {
    let empty = LocalJson::new();
    let ids: BTreeSet<&DocId> = input
        .merged
        .docs
        .keys()
        .chain(input.tablet.keys())
        .filter(|doc| !input.protect.contains(*doc))
        .collect();
    ids.into_iter()
        .map(|doc| {
            plan_doc(
                input.rules,
                doc,
                input.merged.docs.get(doc),
                input.tablet.get(doc),
                input.local.get(doc).unwrap_or(&empty),
                input.json,
            )
        })
        .filter_map(Result::transpose)
        .collect()
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

/// Planned changes that would carry a document the user touched since
/// the snapshot (`touched`) into the trash, e.g. a folder origin trashed
/// while the user wrote in a notebook inside it: that notebook would
/// land in the trash on a side whose own tree then has it there, and
/// the edit could no longer pull it out. Those changes are skipped (with
/// fork partners) until no touched document newly lands in the trash.
pub(super) fn trash_skips(
    current: &BTreeMap<DocId, TreeItem>,
    planned: &BTreeMap<DocId, Option<TreeItem>>,
    touched: &BTreeSet<DocId>,
    partners: &Partners,
) -> BTreeSet<DocId> {
    skip_until_settled(planned, partners, |skipped| {
        let projected = project(current, planned, skipped);
        let blamed: BTreeSet<DocId> = touched
            .iter()
            .filter(|doc| {
                projected.contains_key(*doc) && on_trash(&projected, doc) && !on_trash(current, doc)
            })
            .flat_map(|doc| chain(&projected, doc))
            .filter(|doc| planned.contains_key(doc) && !skipped.contains(doc))
            .collect();
        (!blamed.is_empty()).then_some(blamed)
    })
}

// ---- reconciliation ---------------------------------------------------------------

/// Where an interrupted run left one document whose merge and snapshot
/// versions differ (see "Restartability").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Every file is at the merge version: base = merge.
    AtMerge,
    /// Every file is still at the snapshot version: base override =
    /// snapshot, and the next merge brings the change again.
    AtSnapshot,
    /// Every file is at one of the two, some at each (the apply stopped
    /// inside this document): finish applying it, then base = merge.
    Mixed,
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
