//! Pure merge engine for `rmu hub`, phase 1 (see "Merge rules" in
//! `docs/hub-design.md`).
//!
//! [`merge`] takes per-document versions of the tablet snapshot
//! ("ours"), origin `main` ("theirs") and a virtual base tree (the
//! default base with override documents swapped in, assembled by the
//! caller), and decides every document:
//!
//! 1. **Per document**, by the table in the design. Versions that are
//!    byte-identical (same blob ids) are decided without reading their
//!    contents (the tree rules still read every present `.metadata`). Otherwise a side "changed" when its normalised
//!    `.metadata`, its fork-class key or its tag sets differ from base
//!    (view-state and derived fields never count; see [`Keys`]).
//! 2. **Changed on both, differently**: `.metadata` merges per field
//!    (origin wins concurrent conflicts, with a note; `lastModified` is
//!    the maximum), `tags`/`pageTags` merge as sets, and the fork class
//!    is taken whole from the one side that changed it. If both changed
//!    it, the document **forks**: origin keeps the UUID and the tablet's
//!    version becomes a new document with a deterministic UUID.
//! 3. **Tree rules**, in rounds until nothing changes: trash vs edit,
//!    orphans (resurrected from history, else moved to the root),
//!    cycles. Every loop has a proven bound; exceeding it, or ending with
//!    a broken tree, is an error.
//! 4. **Forks** are named and placed next to their original.
//! 5. The **deletion guard** counts deletions in both directions.
//!
//! Everything is pure: blob contents come through [`BlobSource`] and
//! origin's history through [`History`], which the sync cycle implements
//! over git and tests implement over in-memory maps. Iteration is over
//! sorted maps only, so the result never depends on hash order.
//!
//! Errors: this module calls caller-supplied readers whose failures are
//! [`crate::error::Error`], so it returns [`crate::Result`]; its own
//! failures are [`MergeError`], a variant of that error.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::ops::ControlFlow;
use std::rc::Rc;

use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::error::Result;
use crate::git::Oid;
use crate::hub::model::{
    DeviceName, DocFile, DocId, DocVersion, FileKind, JsonFile, ListingEntry, MirrorRules,
    RepoTree, Seg, TreeListing,
};
use crate::xochitl::{self, ItemKind, TRASH_PARENT};

/// Namespace of fork UUIDs (UUIDv5). Never change it: recomputing a
/// merge must recreate the identical fork.
const FORK_NAMESPACE: Uuid = Uuid::from_u128(0x2c5f_7b9e_41d8_4a63_b0e2_9d7c_6f18_a354);

/// `.content` fields merged as sets rather than as fork class.
const SET_FIELDS: [&str; 2] = ["tags", "pageTags"];

/// Keys a page entry of a trivially materialised page list may have.
const MATERIALISED_PAGE_KEYS: [&str; 5] = ["id", "idx", "redir", "scrollTime", "verticalScroll"];

/// Errors of the merge engine itself.
#[derive(Debug, Error)]
pub enum MergeError {
    /// A `.metadata` or `.content` blob that does not parse. Snapshots
    /// are validated before they are committed, so this means a damaged
    /// or hand-made commit in origin; the run stops rather than guess.
    #[error("invalid JSON in {file} of document {doc}: {error}")]
    InvalidJson {
        doc: DocId,
        file: &'static str,
        error: String,
    },

    /// A blob an in-memory [`BlobSource`] does not hold.
    #[error("blob {0} is missing")]
    MissingBlob(Oid),

    /// A fork date that is not `yyyy-mm-dd`.
    #[error("invalid fork date {0:?}: expected yyyy-mm-dd")]
    InvalidDate(String),

    /// A tree rule exceeded its proven step bound: a bug in rmu. Nothing
    /// is pushed.
    #[error("tree rule '{rule}' did not settle within {bound} steps")]
    NoFixpoint { rule: &'static str, bound: usize },

    /// Repairing the tree would resurrect implausibly many folders from
    /// history.
    #[error("more than {0} folders would have to be resurrected from history")]
    TooManyResurrections(usize),

    /// The tree rules finished with violations left: a bug in rmu.
    #[error("the merged tree is still invalid: {0:?}")]
    BrokenTree(Vec<TreeViolation>),
}

// ---- inputs ----------------------------------------------------------------

/// Reads blob contents. Only `.metadata` and `.content` blobs are read:
/// the `.metadata` of **every present merged document** (the tree rules
/// need every parent), plus the `.metadata`/`.content` of documents
/// changed on both sides and of documents near the trash. Each blob is
/// read at most once per merge; an implementation over git should batch
/// (`cat-file --batch`).
pub trait BlobSource {
    fn read(&self, id: &Oid) -> Result<Vec<u8>>;
}

impl BlobSource for BTreeMap<Oid, Vec<u8>> {
    fn read(&self, id: &Oid) -> Result<Vec<u8>> {
        self.get(id)
            .cloned()
            .ok_or_else(|| MergeError::MissingBlob(id.clone()).into())
    }
}

impl<S: std::hash::BuildHasher> BlobSource for HashMap<Oid, Vec<u8>, S> {
    fn read(&self, id: &Oid) -> Result<Vec<u8>> {
        self.get(id)
            .cloned()
            .ok_or_else(|| MergeError::MissingBlob(id.clone()).into())
    }
}

/// Origin's history, for resurrecting the parents of orphans.
pub trait History {
    /// The present versions of `doc` in origin's history: theirs itself
    /// and its first-parent ancestors (never device refs), newest first.
    /// Called only for a parent an item would otherwise lack, when
    /// neither theirs nor the base has that parent present outside the
    /// trash.
    fn present_versions(&self, doc: &DocId) -> Result<Vec<DocVersion>>;
}

/// No history: orphans go to the root.
pub struct NoHistory;

impl History for NoHistory {
    fn present_versions(&self, _doc: &DocId) -> Result<Vec<DocVersion>> {
        Ok(Vec::new())
    }
}

impl History for BTreeMap<DocId, Vec<DocVersion>> {
    fn present_versions(&self, doc: &DocId) -> Result<Vec<DocVersion>> {
        Ok(self.get(doc).cloned().unwrap_or_default())
    }
}

/// The three trees of a merge.
#[derive(Debug, Clone, Copy)]
pub struct MergeInput<'a> {
    /// Each document at its own base version (empty for documents with
    /// the empty base).
    pub base: &'a RepoTree,
    /// The tablet snapshot.
    pub ours: &'a RepoTree,
    /// Origin `main`.
    pub theirs: &'a RepoTree,
}

/// Deletion-guard thresholds: consent is needed when more than `count`
/// documents **and** more than `percent` % of the library would be
/// deleted, or more than `absolute` documents regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardThresholds {
    pub count: usize,
    pub percent: usize,
    pub absolute: usize,
}

impl Default for GuardThresholds {
    fn default() -> Self {
        Self {
            count: 10,
            percent: 10,
            absolute: 50,
        }
    }
}

impl GuardThresholds {
    /// Whether deleting `deleted` of `library` documents needs consent.
    pub fn exceeded(&self, deleted: usize, library: usize) -> bool {
        deleted > self.absolute || (deleted > self.count && deleted * 100 > self.percent * library)
    }
}

/// Merge parameters.
#[derive(Debug, Clone, Copy)]
pub struct MergeParams<'a> {
    /// The repository's mirror rules.
    pub rules: &'a MirrorRules,
    /// The tablet's name, used in fork names.
    pub device_name: &'a DeviceName,
    /// Today's date as `yyyy-mm-dd`, used in fork names (the merge has
    /// no clock).
    pub date: &'a str,
    /// `--allow-deletions`.
    pub allow_deletions: bool,
    pub thresholds: GuardThresholds,
    /// Documents whose deletion in origin (since the base) carries
    /// recorded consent in a commit trailer. Only these are exempt from
    /// the tablet-direction count.
    pub consented_deletions: &'a BTreeSet<DocId>,
}

// ---- outputs ---------------------------------------------------------------

/// A side of the merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The tablet snapshot.
    Ours,
    /// Origin `main`.
    Theirs,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Ours => "the tablet",
            Side::Theirs => "origin",
        })
    }
}

/// A file of a merged version: an existing blob, or new content the
/// caller must write (merged JSON, fork metadata).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergedEntry {
    Existing(ListingEntry),
    /// Regular-file content, not yet a blob.
    New(Vec<u8>),
}

/// One document's merged version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedVersion {
    pub files: BTreeMap<DocFile, MergedEntry>,
    /// Carried-forward paths, from the version the fork class came from.
    pub other: BTreeMap<String, ListingEntry>,
}

impl MergedVersion {
    /// A version taken unchanged.
    pub fn existing(version: &DocVersion) -> Self {
        Self {
            files: version
                .files
                .iter()
                .map(|(file, entry)| (file.clone(), MergedEntry::Existing(entry.clone())))
                .collect(),
            other: version.other.clone(),
        }
    }

    /// A document is present when its `metadata` exists.
    pub fn is_present(&self) -> bool {
        self.files.contains_key(&DocFile::Metadata)
    }
}

/// How a document was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Neither side changed it.
    Unchanged,
    /// Changed on one side only: that side's version.
    Changed(Side),
    /// Changed identically on both: taken once (origin's bytes).
    Identical,
    /// New on one side.
    Added(Side),
    /// New on both and identical: taken once.
    AddedBoth,
    /// Deleted on one side (unchanged on the other), or on both.
    Deleted(Option<Side>),
    /// Deleted on the other side but changed on this one: kept.
    Kept(Side),
    /// Changed on both: per-field and set merges, fork class from one
    /// side.
    Merged,
    /// Fork class changed on both: origin's version kept here, the
    /// tablet's version became `copy`.
    Forked { copy: DocId },
    /// The tablet's version of `original`, as a new document.
    ForkCopy { original: DocId },
    /// Brought back from origin's history as an orphan's ancestor.
    Resurrected,
}

/// Something the user should know about one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub doc: DocId,
    #[serde(flatten)]
    pub kind: NoteKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoteKind {
    /// Both sides changed this `.metadata` field differently; origin's
    /// value was taken.
    MetadataConflict { field: String },
    /// Deleted on one side but changed on the other, so it was kept.
    KeptOverDeletion { deleted_by: Side },
    /// Brought to the tablet although the library had deleted it (here,
    /// on another tablet, or before this tablet was cloned): another
    /// tablet changed it after the deletion (or added it back), so the
    /// merge there kept it. Noted by the sync cycle, not the merge.
    ReturnedAfterDeletion,
    /// The fork class changed on both sides; the tablet's version is now
    /// `copy`.
    Forked { copy: DocId },
    /// The fork this merge would create already exists.
    ForkAlreadyPresent { copy: DocId },
    /// The `.content` format is unknown to this rmu, so the document was
    /// compared as opaque JSON.
    Opaque,
    /// Moved out of the trash because it, or a document inside it, was
    /// edited on a side that did not trash it.
    RestoredFromTrash { parent: String },
    /// Brought back from history because a present item needed it.
    Resurrected,
    /// Its parent folder is gone and has no history: moved to the root.
    OrphanToRoot { missing_parent: String },
    /// Its parent is not a folder: moved to the root.
    ParentNotFolder { parent: String },
    /// It was on a folder cycle: moved to `parent` (`""` = root).
    CycleBroken { parent: String },
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let doc = &self.doc;
        let place = |parent: &str| match parent {
            "" => "the root".to_string(),
            other => other.to_string(),
        };
        match &self.kind {
            NoteKind::MetadataConflict { field } => write!(
                f,
                "{doc}: \"{field}\" changed on both sides; kept origin's value"
            ),
            NoteKind::KeptOverDeletion { deleted_by } => write!(
                f,
                "{doc}: deleted on {deleted_by} but changed on the other side; kept"
            ),
            NoteKind::ReturnedAfterDeletion => write!(
                f,
                "{doc}: deleted from the library earlier, but another tablet changed or restored it since; back from origin"
            ),
            NoteKind::Forked { copy } => write!(
                f,
                "{doc}: changed on both sides; the tablet's version is now {copy}"
            ),
            NoteKind::ForkAlreadyPresent { copy } => {
                write!(f, "{doc}: its conflict copy {copy} already exists")
            }
            NoteKind::Opaque => write!(f, "{doc}: unknown .content format; compared as a whole"),
            NoteKind::RestoredFromTrash { parent } => write!(
                f,
                "{doc}: moved out of the trash to {}, so an edit is not lost",
                place(parent)
            ),
            NoteKind::Resurrected => {
                write!(f, "{doc}: restored from history to hold a present item")
            }
            NoteKind::OrphanToRoot { missing_parent } => write!(
                f,
                "{doc}: parent {missing_parent} is gone; moved to the root"
            ),
            NoteKind::ParentNotFolder { parent } => write!(
                f,
                "{doc}: parent {parent} is not a folder; moved to the root"
            ),
            NoteKind::CycleBroken { parent } => write!(
                f,
                "{doc}: was on a folder cycle; moved to {}",
                place(parent)
            ),
        }
    }
}

/// A fork, for the merge commit's `Rmu-Fork` trailer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fork {
    pub original: DocId,
    pub copy: DocId,
    pub name: String,
}

/// The deletion guard's verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeletionGuard {
    /// Present in origin, absent from the merge: deleted from origin.
    pub from_origin: BTreeSet<DocId>,
    /// Present on the tablet, absent from the merge, minus exempt ones:
    /// apply would delete them from the tablet.
    pub from_tablet: BTreeSet<DocId>,
    /// Tablet deletions that origin accepted with recorded consent.
    pub exempt: BTreeSet<DocId>,
    /// The snapshot is empty and the base is not.
    pub empty_snapshot: bool,
    /// A threshold is exceeded, or the snapshot is empty.
    pub needs_consent: bool,
    /// `--allow-deletions` was given.
    pub allowed: bool,
}

impl DeletionGuard {
    /// The sync must stop before pushing.
    pub fn blocked(&self) -> bool {
        self.needs_consent && !self.allowed
    }

    /// The deletions to record as consented in the merge commit's
    /// trailer: this merge's deletions from origin, when
    /// `--allow-deletions` was given.
    pub fn consented(&self) -> BTreeSet<DocId> {
        if self.allowed {
            self.from_origin.clone()
        } else {
            BTreeSet::new()
        }
    }
}

/// Counts of [`Resolution`]s, for `--dry-run`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub unchanged: usize,
    pub changed_ours: usize,
    pub changed_theirs: usize,
    pub identical: usize,
    pub added: usize,
    pub deleted: usize,
    pub kept: usize,
    pub merged: usize,
    pub forked: usize,
    pub resurrected: usize,
}

/// The result of [`merge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Every document of the merged tree (absent documents are deleted).
    pub docs: BTreeMap<DocId, MergedVersion>,
    /// Paths outside document subtrees: origin's, unchanged (sync never
    /// changes `.rmu/*`, and snapshots carry them forward from base).
    pub extra: TreeListing,
    /// How each document of base, ours, theirs or the result was decided.
    pub resolutions: BTreeMap<DocId, Resolution>,
    pub notes: Vec<Note>,
    pub forks: Vec<Fork>,
    pub guard: DeletionGuard,
}

impl Summary {
    fn count(mut self, resolution: &Resolution) -> Self {
        match resolution {
            Resolution::Unchanged => self.unchanged += 1,
            Resolution::Changed(Side::Ours) => self.changed_ours += 1,
            Resolution::Changed(Side::Theirs) => self.changed_theirs += 1,
            Resolution::Identical => self.identical += 1,
            Resolution::Added(_) | Resolution::AddedBoth => self.added += 1,
            Resolution::Deleted(_) => self.deleted += 1,
            Resolution::Kept(_) => self.kept += 1,
            Resolution::Merged => self.merged += 1,
            Resolution::Forked { .. } | Resolution::ForkCopy { .. } => self.forked += 1,
            Resolution::Resurrected => self.resurrected += 1,
        }
        self
    }
}

impl MergeOutcome {
    pub fn summary(&self) -> Summary {
        self.resolutions
            .values()
            .fold(Summary::default(), Summary::count)
    }

    /// [`Self::summary`] as a sync reports it: a document changed on one
    /// side, or identically on both, counts as unchanged when
    /// `view_state_only` says its merged version differs from its base
    /// only in view state and derived fields. The merge decides such
    /// documents by blob ids, without reading them; the sync pushes and
    /// applies nothing for them, and its reports must agree.
    pub fn summary_with(
        &self,
        mut view_state_only: impl FnMut(&DocId) -> Result<bool>,
    ) -> Result<Summary> {
        self.resolutions
            .iter()
            .try_fold(Summary::default(), |summary, (doc, resolution)| {
                let counted = match resolution {
                    Resolution::Changed(_) | Resolution::Identical if view_state_only(doc)? => {
                        &Resolution::Unchanged
                    }
                    other => other,
                };
                Ok(summary.count(counted))
            })
    }
}

// ---- tree checks -----------------------------------------------------------

/// One item of a folder tree, for [`tree_violations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeItem {
    /// Parent UUID, `""` for the root, `"trash"` for the trash.
    pub parent: String,
    /// `None` for item types other than documents and folders.
    pub kind: Option<ItemKind>,
}

impl TreeItem {
    /// From parsed `.metadata`.
    pub fn from_metadata(metadata: &Value) -> Self {
        Self {
            parent: parent_of(metadata),
            kind: xochitl::item_kind(metadata),
        }
    }
}

/// A broken tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TreeViolation {
    /// The parent does not exist (an orphan).
    MissingParent { item: DocId, parent: String },
    /// The parent exists but is not a folder.
    ParentNotFolder { item: DocId, parent: DocId },
    /// A parent cycle, starting at its smallest UUID.
    Cycle(Vec<DocId>),
}

/// Every violation of a folder tree: orphans, parents that are not
/// folders, and parent cycles, in a stable order. Pure, so step 14 of
/// the sync cycle checks the tablet's post-apply tree with the same
/// function the merge uses.
pub fn tree_violations(items: &BTreeMap<DocId, TreeItem>) -> Vec<TreeViolation> {
    let cycles: BTreeSet<Vec<DocId>> = items
        .keys()
        .filter_map(|start| cycle_from(items, start))
        .collect();
    parent_violations(items)
        .into_iter()
        .chain(cycles.into_iter().map(TreeViolation::Cycle))
        .collect()
}

/// Orphans and parents that are not folders, in item order.
fn parent_violations(items: &BTreeMap<DocId, TreeItem>) -> Vec<TreeViolation> {
    items
        .iter()
        .filter(|(_, item)| !is_root_or_trash(&item.parent))
        .filter_map(|(id, item)| match DocId::new(item.parent.as_str()) {
            Ok(parent) => match items.get(&parent) {
                None => Some(TreeViolation::MissingParent {
                    item: id.clone(),
                    parent: item.parent.clone(),
                }),
                Some(found) if found.kind != Some(ItemKind::Folder) => {
                    Some(TreeViolation::ParentNotFolder {
                        item: id.clone(),
                        parent,
                    })
                }
                Some(_) => None,
            },
            Err(_) => Some(TreeViolation::MissingParent {
                item: id.clone(),
                parent: item.parent.clone(),
            }),
        })
        .collect()
}

fn is_root_or_trash(parent: &str) -> bool {
    parent.is_empty() || parent == TRASH_PARENT
}

/// The parent of `id` among `items`, if it is one of them.
fn tree_parent(items: &BTreeMap<DocId, TreeItem>, id: &DocId) -> Option<DocId> {
    items
        .get(id)
        .and_then(|item| DocId::new(item.parent.as_str()).ok())
        .filter(|parent| items.contains_key(parent))
}

/// `start`, then its parents among `items`, toward the root: the walk
/// the tree checks and rules share. It ends below the root, the trash or
/// a parent that is not an item; on a cycle it stops after
/// `items.len() + 1` steps, by which point it has repeated.
fn ancestors<'a>(
    items: &'a BTreeMap<DocId, TreeItem>,
    start: &DocId,
) -> impl Iterator<Item = DocId> + 'a {
    std::iter::successors(Some(start.clone()), move |id| tree_parent(items, id))
        .take(items.len() + 1)
}

/// The cycle reached by walking parents from `start`, rotated to begin
/// at its smallest member.
fn cycle_from(items: &BTreeMap<DocId, TreeItem>, start: &DocId) -> Option<Vec<DocId>> {
    let walk: Vec<DocId> = ancestors(items, start).collect();
    // The walk either ends (root, trash, missing parent) or repeats;
    // the cycle is what lies between the first repeat and its earlier
    // occurrence.
    let (begin, end) = walk.iter().enumerate().find_map(|(index, id)| {
        walk[..index]
            .iter()
            .position(|earlier| earlier == id)
            .map(|begin| (begin, index))
    })?;
    let cycle = &walk[begin..end];
    let min = cycle
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.cmp(b.1))
        .map(|(index, _)| index)?;
    Some(cycle[min..].iter().chain(&cycle[..min]).cloned().collect())
}

// ---- the merge -------------------------------------------------------------

/// Merge one tablet snapshot with origin. See the module docs.
pub fn merge(
    input: &MergeInput,
    blobs: &dyn BlobSource,
    history: &dyn History,
    params: &MergeParams,
) -> Result<MergeOutcome> {
    validate_date(params.date)?;
    let ctx = Ctx {
        rules: params.rules,
        input,
        blobs: Blobs {
            source: blobs,
            cache: RefCell::new(HashMap::new()),
        },
    };
    let ids: BTreeSet<&DocId> = input
        .base
        .docs
        .keys()
        .chain(input.ours.docs.keys())
        .chain(input.theirs.docs.keys())
        .collect();
    let state = ids.into_iter().try_fold(State::default(), |state, id| {
        let doc = ctx.merge_doc(id)?;
        state.with_doc(&ctx, id, doc)
    })?;
    let state = tree_rules(&ctx, history, state)?;
    let state = place_forks(params, state);
    let state = finish_metadata(&ctx, state)?;
    let guard = deletion_guard(input, &state.versions, params);
    Ok(MergeOutcome {
        docs: state.versions,
        extra: input.theirs.extra.clone(),
        resolutions: state.resolutions,
        notes: state.notes,
        forks: state.forks,
        guard,
    })
}

/// Whether `side` is an unchanged version of `base` by the merge's own
/// rules: what the per-document table calls "no change". View-state and
/// derived fields are ignored, `tags`/`pageTags` compare as sets, the
/// page array's order carries no meaning, a first-open materialisation of
/// `base`'s empty page list reads as that list, and an unknown `.content`
/// format compares as normalised JSON. Both must be present documents.
/// `clone` uses it to find a document's base in origin's history.
pub fn unchanged(
    rules: &MirrorRules,
    doc: &DocId,
    base: &DocVersion,
    side: &DocVersion,
    blobs: &dyn BlobSource,
) -> Result<bool> {
    if !base.is_present() || !side.is_present() {
        return Ok(false);
    }
    let tree = |version: &DocVersion| RepoTree {
        docs: [(doc.clone(), version.clone())].into(),
        extra: TreeListing::new(),
    };
    let (base_tree, side_tree) = (tree(base), tree(side));
    let input = MergeInput {
        base: &base_tree,
        ours: &side_tree,
        theirs: &side_tree,
    };
    let ctx = Ctx {
        rules,
        input: &input,
        blobs: Blobs {
            source: blobs,
            cache: RefCell::new(HashMap::new()),
        },
    };
    let (keys, base_keys) = ctx.side_and_base_keys(doc, &ctx.sides(doc), Side::Ours)?;
    Ok(keys == base_keys)
}

fn validate_date(date: &str) -> Result<()> {
    let bytes = date.as_bytes();
    let valid = bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, b)| match index {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    if valid {
        Ok(())
    } else {
        Err(MergeError::InvalidDate(date.to_string()).into())
    }
}

/// Memoised, parsed JSON blobs.
struct Blobs<'a> {
    source: &'a dyn BlobSource,
    cache: RefCell<HashMap<Oid, Rc<Value>>>,
}

impl Blobs<'_> {
    fn json(&self, doc: &DocId, file: JsonFile, entry: &ListingEntry) -> Result<Rc<Value>> {
        if let Some(value) = self.cache.borrow().get(&entry.id) {
            return Ok(Rc::clone(value));
        }
        let bytes = self.source.read(&entry.id)?;
        let value =
            serde_json::from_slice::<Value>(&bytes).map_err(|err| MergeError::InvalidJson {
                doc: doc.clone(),
                file: match file {
                    JsonFile::Metadata => ".metadata",
                    JsonFile::Content => ".content",
                },
                error: err.to_string(),
            })?;
        let value = Rc::new(value);
        self.cache
            .borrow_mut()
            .insert(entry.id.clone(), Rc::clone(&value));
        Ok(value)
    }
}

/// One document's versions.
#[derive(Clone, Copy)]
struct Sides<'a> {
    base: Option<&'a DocVersion>,
    ours: Option<&'a DocVersion>,
    theirs: Option<&'a DocVersion>,
}

impl<'a> Sides<'a> {
    fn side(&self, side: Side) -> Option<&'a DocVersion> {
        match side {
            Side::Ours => self.ours,
            Side::Theirs => self.theirs,
        }
    }
}

fn present(version: Option<&DocVersion>) -> bool {
    version.is_some_and(DocVersion::is_present)
}

/// What decides whether a side changed. View-state and derived fields
/// are never part of it.
#[derive(Debug, Clone, PartialEq)]
struct Keys {
    /// Normalised `.metadata`.
    metadata: Option<Value>,
    /// The fork class.
    core: ForkKey,
    /// `tags` and `pageTags` as sets (canonical JSON strings).
    sets: [BTreeSet<String>; 2],
}

impl Keys {
    fn empty() -> Self {
        Self {
            metadata: None,
            core: ForkKey::default(),
            sets: Default::default(),
        }
    }
}

/// Every mirrored file except `.metadata`, compared as one unit: the
/// `.content` key, plus mode and blob id of every other file (pages,
/// payloads, pagedata, highlights, templates, carried-forward paths).
#[derive(Debug, Clone, Default, PartialEq)]
struct ForkKey {
    content: Option<Value>,
    files: BTreeMap<String, (u32, String)>,
}

impl ForkKey {
    /// Deterministic text for the fork UUID. `serde_json` keeps object
    /// keys sorted (no `preserve_order`), so equal keys give equal text.
    fn canonical(&self) -> String {
        let content = self
            .content
            .as_ref()
            .map_or_else(|| "-".to_string(), Value::to_string);
        self.files.iter().fold(
            format!("content {content}\n"),
            |text, (name, (mode, id))| text + &format!("{name} {mode:o} {id}\n"),
        )
    }
}

struct Ctx<'a> {
    rules: &'a MirrorRules,
    input: &'a MergeInput<'a>,
    blobs: Blobs<'a>,
}

/// One document's per-document decision.
struct DocMerge {
    version: Option<MergedVersion>,
    resolution: Resolution,
    notes: Vec<NoteKind>,
    fork: Option<PendingFork>,
}

impl DocMerge {
    fn take(version: Option<&DocVersion>, resolution: Resolution) -> Self {
        Self {
            version: version.map(MergedVersion::existing),
            resolution,
            notes: Vec::new(),
            fork: None,
        }
    }

    fn noted(mut self, note: NoteKind) -> Self {
        self.notes.push(note);
        self
    }
}

/// A fork waiting for its name and place (after the tree rules).
struct PendingFork {
    original: DocId,
    copy: DocId,
    version: MergedVersion,
    metadata: Value,
}

impl<'a> Ctx<'a> {
    fn sides(&self, id: &DocId) -> Sides<'a> {
        Sides {
            base: self.input.base.docs.get(id),
            ours: self.input.ours.docs.get(id),
            theirs: self.input.theirs.docs.get(id),
        }
    }

    fn json(
        &self,
        id: &DocId,
        file: JsonFile,
        version: Option<&DocVersion>,
    ) -> Result<Option<Rc<Value>>> {
        let doc_file = match file {
            JsonFile::Metadata => DocFile::Metadata,
            JsonFile::Content => DocFile::Content,
        };
        version
            .and_then(|version| version.files.get(&doc_file))
            .map(|entry| self.blobs.json(id, file, entry))
            .transpose()
    }

    /// The parent a side gives `id`, when that side has it present.
    fn side_parent(&self, id: &DocId, version: Option<&DocVersion>) -> Result<Option<String>> {
        if !present(version) {
            return Ok(None);
        }
        Ok(self
            .json(id, JsonFile::Metadata, version)?
            .map(|metadata| parent_of(&metadata)))
    }

    /// A document is opaque when ours' or theirs' `.content` has a format
    /// this rmu does not know. The base does not count: after a firmware
    /// update that migrated every `.content` on both tablets, documents
    /// whose two sides agree are still an identical change.
    fn opaque(&self, id: &DocId, sides: &Sides) -> Result<bool> {
        [sides.ours, sides.theirs]
            .into_iter()
            .filter(|version| present(*version))
            .try_fold(false, |opaque, version| {
                Ok(opaque
                    || self
                        .json(id, JsonFile::Content, version)?
                        .is_some_and(|content| !content_known(&content)))
            })
    }

    /// The keys of a present version. `reference` is the `.content` a
    /// trivial first-open materialisation is judged against.
    fn keys(
        &self,
        id: &DocId,
        version: &DocVersion,
        reference: Option<&Value>,
        opaque: bool,
    ) -> Result<Keys> {
        let metadata = self
            .json(id, JsonFile::Metadata, Some(version))?
            .map(|metadata| self.rules.normalise(JsonFile::Metadata, &metadata));
        let content = self.json(id, JsonFile::Content, Some(version))?;
        let has_ink = version
            .files
            .keys()
            .any(|file| file.kind() == FileKind::Page);
        let content_key = content
            .as_deref()
            .map(|content| content_key(self.rules, content, reference, has_ink, opaque));
        let files = version
            .files
            .iter()
            .filter(|(file, _)| !matches!(file, DocFile::Metadata | DocFile::Content))
            .map(|(file, entry)| (file.repo_name(), entry))
            .chain(
                version
                    .other
                    .iter()
                    .map(|(name, entry)| (name.clone(), entry)),
            )
            .map(|(name, entry)| (name, (entry.mode.bits(), entry.id.to_string())))
            .collect();
        let sets = SET_FIELDS.map(|field| match (&content, opaque) {
            (Some(content), false) => set_values(content, field)
                .iter()
                .map(Value::to_string)
                .collect(),
            _ => BTreeSet::new(),
        });
        Ok(Keys {
            metadata,
            core: ForkKey {
                content: content_key,
                files,
            },
            sets,
        })
    }

    /// The keys of `side` and base, judged the way the per-document
    /// table judges them.
    fn side_and_base_keys(&self, id: &DocId, sides: &Sides, side: Side) -> Result<(Keys, Keys)> {
        let opaque = self.opaque(id, sides)?;
        let base_content = self.json(id, JsonFile::Content, sides.base)?;
        let base = match sides.base.filter(|base| base.is_present()) {
            Some(base) => self.keys(id, base, base_content.as_deref(), opaque)?,
            None => Keys::empty(),
        };
        let keys = match sides.side(side).filter(|version| version.is_present()) {
            Some(version) => self.keys(id, version, base_content.as_deref(), opaque)?,
            None => Keys::empty(),
        };
        Ok((keys, base))
    }

    /// Whether `side` changed the document's fork-class content (the
    /// "edit" of trash vs edit). A document new on `side` counts.
    fn fork_changed(&self, id: &DocId, side: Side) -> Result<bool> {
        let sides = self.sides(id);
        if !present(sides.side(side)) {
            return Ok(false);
        }
        let (keys, base) = self.side_and_base_keys(id, &sides, side)?;
        Ok(keys.core != base.core)
    }

    /// Whether a present `side` changed anything but view state and
    /// derived fields, judged against the other side's deletion: moving
    /// it to the trash does not count (trashed on one tablet, emptied
    /// from the trash on the other is a deletion, not a conflict).
    fn changed_against_deletion(&self, id: &DocId, sides: &Sides, side: Side) -> Result<bool> {
        let (mut keys, base) = self.side_and_base_keys(id, sides, side)?;
        let base_parent = base
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("parent"))
            .cloned();
        if let Some(Value::Object(metadata)) = keys.metadata.as_mut()
            && metadata.get("parent").and_then(Value::as_str) == Some(TRASH_PARENT)
        {
            match base_parent {
                Some(parent) => metadata.insert("parent".to_string(), parent),
                None => metadata.remove("parent"),
            };
        }
        Ok(keys != base)
    }

    /// Whether `side`'s own tree has `id` on a trash path: that side put
    /// it (or an ancestor) in the trash itself.
    fn in_own_trash(&self, id: &DocId, side: Side) -> Result<bool> {
        let tree = match side {
            Side::Ours => self.input.ours,
            Side::Theirs => self.input.theirs,
        };
        let walk = (0..=tree.docs.len()).try_fold(id.clone(), |current, _| {
            match self.side_parent(&current, tree.docs.get(&current)) {
                Err(err) => ControlFlow::Break(Err(err)),
                Ok(Some(parent)) if parent == TRASH_PARENT => ControlFlow::Break(Ok(true)),
                Ok(Some(parent)) => match DocId::new(parent.as_str()) {
                    Ok(next) if present(tree.docs.get(&next)) => ControlFlow::Continue(next),
                    _ => ControlFlow::Break(Ok(false)),
                },
                Ok(None) => ControlFlow::Break(Ok(false)),
            }
        });
        match walk {
            ControlFlow::Break(result) => result,
            // A cycle in a side's own tree never reaches the trash.
            ControlFlow::Continue(_) => Ok(false),
        }
    }

    fn merge_doc(&self, id: &DocId) -> Result<DocMerge> {
        let s = self.sides(id);
        let (pb, po, pt) = (present(s.base), present(s.ours), present(s.theirs));
        // Byte-identical versions decide without reading anything.
        if s.ours == s.theirs {
            let resolution = match (s.ours == s.base, pb, pt) {
                (true, _, _) => Resolution::Unchanged,
                (false, true, false) => Resolution::Deleted(None),
                (false, false, true) => Resolution::AddedBoth,
                _ => Resolution::Identical,
            };
            return Ok(DocMerge::take(s.theirs, resolution));
        }
        if s.ours == s.base {
            return Ok(DocMerge::take(s.theirs, one_side(pb, pt, Side::Theirs)));
        }
        if s.theirs == s.base {
            return Ok(DocMerge::take(s.ours, one_side(pb, po, Side::Ours)));
        }
        match (pb, po, pt) {
            (_, false, false) => Ok(DocMerge::take(
                s.theirs,
                if pb {
                    Resolution::Deleted(None)
                } else {
                    Resolution::Changed(Side::Theirs)
                },
            )),
            (false, true, false) => Ok(DocMerge::take(s.ours, Resolution::Added(Side::Ours))),
            (false, false, true) => Ok(DocMerge::take(s.theirs, Resolution::Added(Side::Theirs))),
            (true, false, true) => self.deleted_on_one_side(id, &s, Side::Ours),
            (true, true, false) => self.deleted_on_one_side(id, &s, Side::Theirs),
            (_, true, true) => self.both_present(id, &s),
        }
    }

    /// Deleted on `deleted_by`, present on the other side: kept if the
    /// other side changed it (edits beat deletion), else deleted.
    fn deleted_on_one_side(&self, id: &DocId, s: &Sides, deleted_by: Side) -> Result<DocMerge> {
        let other = match deleted_by {
            Side::Ours => Side::Theirs,
            Side::Theirs => Side::Ours,
        };
        Ok(if self.changed_against_deletion(id, s, other)? {
            DocMerge::take(s.side(other), Resolution::Kept(other))
                .noted(NoteKind::KeptOverDeletion { deleted_by })
        } else {
            DocMerge::take(s.side(deleted_by), Resolution::Deleted(Some(deleted_by)))
        })
    }

    /// Present on both sides (base present, or new on both).
    fn both_present(&self, id: &DocId, s: &Sides) -> Result<DocMerge> {
        let (ours, theirs) = (s.ours.expect("present"), s.theirs.expect("present"));
        let opaque = self.opaque(id, s)?;
        let base_present = s.base.filter(|base| base.is_present());
        let base_content = self.json(id, JsonFile::Content, base_present)?;
        let ours_content = self.json(id, JsonFile::Content, Some(ours))?;
        let theirs_content = self.json(id, JsonFile::Content, Some(theirs))?;
        // New on both: both sides' materialisations are judged against an
        // empty page list, so two tablets that each opened the same
        // imported PDF agree (rules for "new on both").
        let unopened = Rc::new(serde_json::json!({ "cPages": { "pages": [] } }));
        let (ours_ref, theirs_ref) = match base_present {
            Some(_) => (base_content.clone(), base_content.clone()),
            None => (Some(Rc::clone(&unopened)), Some(unopened)),
        };
        // The base against itself: a list `rmu hub add` materialised reads
        // as the empty list, as the sides' materialisations of it do.
        let kb = match base_present {
            Some(base) => self.keys(id, base, base_content.as_deref(), opaque)?,
            None => Keys::empty(),
        };
        let ko = self.keys(id, ours, ours_ref.as_deref(), opaque)?;
        let kt = self.keys(id, theirs, theirs_ref.as_deref(), opaque)?;
        if ko == kt {
            let resolution = match base_present {
                Some(_) if ko == kb => Resolution::Unchanged,
                Some(_) => Resolution::Identical,
                None => Resolution::AddedBoth,
            };
            return Ok(DocMerge::take(Some(theirs), resolution));
        }
        if base_present.is_some() && ko == kb {
            return Ok(DocMerge::take(
                Some(theirs),
                Resolution::Changed(Side::Theirs),
            ));
        }
        // Origin only materialised the page list on first open: it
        // counts as unchanged, but if the tablet materialised too, origin's
        // page ids win, so the fork class still comes from origin. Both
        // materialised = keyed cores equal while the raw page lists
        // differ.
        let both_materialised = || -> Result<bool> {
            Ok(ko.core == kt.core
                && self.keys(id, ours, None, opaque)?.core
                    != self.keys(id, theirs, None, opaque)?.core)
        };
        if base_present.is_some() && kt == kb && !both_materialised()? {
            return Ok(DocMerge::take(Some(ours), Resolution::Changed(Side::Ours)));
        }
        self.combine(
            id,
            s,
            (&kb, &ko, &kt),
            opaque,
            (
                base_content.as_deref(),
                ours_content.as_deref(),
                theirs_content.as_deref(),
            ),
        )
    }

    /// Changed on both, differently: per-field metadata merge, set
    /// merges, fork class from the one side that changed it, or a fork.
    fn combine(
        &self,
        id: &DocId,
        s: &Sides,
        (kb, ko, kt): (&Keys, &Keys, &Keys),
        opaque: bool,
        (base_content, ours_content, theirs_content): (
            Option<&Value>,
            Option<&Value>,
            Option<&Value>,
        ),
    ) -> Result<DocMerge> {
        let (ours, theirs) = (s.ours.expect("present"), s.theirs.expect("present"));
        let base = s.base.filter(|base| base.is_present());
        let md_base = self.json(id, JsonFile::Metadata, base)?;
        let md_ours = self.json(id, JsonFile::Metadata, Some(ours))?;
        let md_theirs = self.json(id, JsonFile::Metadata, Some(theirs))?;
        let empty = Value::Object(Map::new());
        let (metadata, conflicts) = merge_metadata(
            self.rules,
            md_base.as_deref(),
            md_ours.as_deref().unwrap_or(&empty),
            md_theirs.as_deref().unwrap_or(&empty),
        );
        let fork_side = if ko.core == kt.core || ko.core == kb.core {
            Some(Side::Theirs)
        } else if kt.core == kb.core {
            Some(Side::Ours)
        } else {
            None
        };
        let (winner, winner_content) = match fork_side.unwrap_or(Side::Theirs) {
            Side::Ours => (ours, ours_content),
            Side::Theirs => (theirs, theirs_content),
        };
        let content = winner_content.map(|content| match opaque {
            true => content.clone(),
            false => merge_sets(content, base_content, ours_content, theirs_content),
        });
        let mut files: BTreeMap<DocFile, MergedEntry> = winner
            .files
            .iter()
            .filter(|(file, _)| !matches!(file, DocFile::Metadata | DocFile::Content))
            .map(|(file, entry)| (file.clone(), MergedEntry::Existing(entry.clone())))
            .collect();
        files.insert(
            DocFile::Metadata,
            json_entry(
                &metadata,
                [(md_ours.as_deref(), ours), (md_theirs.as_deref(), theirs)]
                    .into_iter()
                    .filter_map(|(value, version)| {
                        Some((value?, version.files.get(&DocFile::Metadata)?))
                    }),
            ),
        );
        if let (Some(content), Some(entry)) = (&content, winner.files.get(&DocFile::Content)) {
            files.insert(
                DocFile::Content,
                json_entry(
                    content,
                    winner_content.map(|value| (value, entry)).into_iter(),
                ),
            );
        }
        let notes: Vec<NoteKind> = opaque
            .then_some(NoteKind::Opaque)
            .into_iter()
            .chain(
                conflicts
                    .into_iter()
                    .map(|field| NoteKind::MetadataConflict { field }),
            )
            .collect();
        let version = MergedVersion {
            files,
            other: winner.other.clone(),
        };
        Ok(match fork_side {
            Some(_) => DocMerge {
                version: Some(version),
                resolution: Resolution::Merged,
                notes,
                fork: None,
            },
            None => {
                let copy = fork_id(id, &ko.core);
                DocMerge {
                    version: Some(version),
                    resolution: Resolution::Forked { copy: copy.clone() },
                    notes,
                    fork: Some(PendingFork {
                        original: id.clone(),
                        copy,
                        version: MergedVersion::existing(ours),
                        metadata: md_ours.as_deref().cloned().unwrap_or(empty),
                    }),
                }
            }
        })
    }
}

/// The resolution when one side's version is taken because the other
/// side did not change it.
fn one_side(base_present: bool, side_present: bool, side: Side) -> Resolution {
    match (base_present, side_present) {
        (true, false) => Resolution::Deleted(Some(side)),
        (false, true) => Resolution::Added(side),
        _ => Resolution::Changed(side),
    }
}

/// The deterministic UUID of the tablet's version of `original` as a
/// fork: UUIDv5 over the original UUID and the canonical text of the
/// forked version's fork class (view state and derived fields
/// excluded), so recomputing a merge recreates the identical fork.
fn fork_id(original: &DocId, core: &ForkKey) -> DocId {
    let name = format!("{original}\n{}", core.canonical());
    DocId::new(Uuid::new_v5(&FORK_NAMESPACE, name.as_bytes()).to_string())
        .expect("a hyphenated UUID is a document id")
}

fn parent_of(metadata: &Value) -> String {
    metadata
        .get("parent")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn visible_name(metadata: &Value) -> Option<&str> {
    metadata
        .get("visibleName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
}

/// A `.content` format this rmu knows: a folder's `[]`, or
/// `formatVersion: 2` whose page list (if any) is entries with string
/// ids.
fn content_known(content: &Value) -> bool {
    match content {
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => {
            map.get("formatVersion").and_then(Value::as_u64) == Some(2)
                && match map.get("cPages") {
                    None => true,
                    Some(Value::Object(cpages)) => match cpages.get("pages") {
                        None => true,
                        Some(Value::Array(pages)) => pages
                            .iter()
                            .all(|page| page.get("id").is_some_and(Value::is_string)),
                        Some(_) => false,
                    },
                    Some(_) => false,
                }
        }
        _ => false,
    }
}

fn pages(content: &Value) -> Option<&Vec<Value>> {
    content
        .get("cPages")
        .and_then(|cpages| cpages.get("pages"))
        .and_then(Value::as_array)
}

fn register_value<'v>(entry: &'v Value, key: &str) -> Option<&'v Value> {
    entry.get(key).and_then(|register| register.get("value"))
}

/// Whether `side`'s page list is xochitl's first-open materialisation of
/// `reference`'s empty one: one entry per PDF page, `redir` equal to the
/// page's position in `idx` order, nothing inserted, deleted or
/// templated, and no ink. A reference that is itself such a list, of the
/// same length, counts as empty (a list `rmu hub add` wrote, materialised
/// again by a tablet with ids of its own; `reference` may be `side`
/// itself, so that list reads as empty too). (`has_ink` is
/// belt-and-braces: `.rm` files are in the fork key anyway, so hiding the
/// page list can never hide ink; it keeps this function faithful to the
/// definition in the design.)
fn trivially_materialised(reference: &Value, side: &Value, has_ink: bool) -> bool {
    let (Some(before), Some(after)) = (pages(reference), pages(side)) else {
        return false;
    };
    let unopened =
        before.is_empty() || (before.len() == after.len() && trivial_page_list(reference));
    !has_ink && unopened && !after.is_empty() && trivial_page_list(side)
}

/// Whether a `.content`'s page list has the shape of a first-open
/// materialisation: keys only `id`, `idx`, `redir` and the per-page view
/// state, `redir` equal to the position in `idx` order, and a page count
/// equal to `cPages.original` when that is set.
fn trivial_page_list(content: &Value) -> bool {
    let Some(pages) = pages(content) else {
        return false;
    };
    let only_known_keys = pages.iter().all(|page| {
        page.as_object().is_some_and(|entry| {
            entry
                .keys()
                .all(|key| MATERIALISED_PAGE_KEYS.contains(&key.as_str()))
        })
    });
    let mut ordered: Vec<(&str, &str, Option<u64>)> = pages
        .iter()
        .map(|page| {
            (
                register_value(page, "idx")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                page.get("id").and_then(Value::as_str).unwrap_or(""),
                register_value(page, "redir").and_then(Value::as_u64),
            )
        })
        .collect();
    ordered.sort();
    let positional = ordered
        .iter()
        .enumerate()
        .all(|(position, (_, _, redir))| *redir == Some(position as u64));
    let original_matches =
        register_value(content.get("cPages").unwrap_or(&Value::Null), "original")
            .and_then(Value::as_i64)
            .is_none_or(|original| original == pages.len() as i64);
    only_known_keys && positional && original_matches
}

/// The fork-class key of a `.content`: normalised, without the set
/// fields, the page list sorted by `id` (array order carries no meaning:
/// `idx` orders pages), and a trivial materialisation of `reference`'s
/// empty page list read as that empty list. Opaque content is only
/// normalised.
fn content_key(
    rules: &MirrorRules,
    content: &Value,
    reference: Option<&Value>,
    has_ink: bool,
    opaque: bool,
) -> Value {
    let mut key = rules.normalise(JsonFile::Content, content);
    if opaque {
        // The enumerated view-state and derived paths are stripped even
        // from an unknown format (reading never counts); the rest is
        // compared as it is.
        return key;
    }
    if let Some(map) = key.as_object_mut() {
        SET_FIELDS.iter().for_each(|field| {
            map.remove(*field);
        });
    }
    let materialised =
        reference.is_some_and(|reference| trivially_materialised(reference, content, has_ink));
    if let Some(pages) = key
        .get_mut("cPages")
        .and_then(|cpages| cpages.get_mut("pages"))
        .and_then(Value::as_array_mut)
    {
        if materialised {
            pages.clear();
        } else {
            pages.sort_by(|a, b| {
                let id = |page: &Value| page.get("id").and_then(Value::as_str).map(str::to_string);
                id(a).cmp(&id(b))
            });
        }
    }
    key
}

/// A set field's values (missing or not an array: empty).
fn set_values(content: &Value, field: &str) -> Vec<Value> {
    content
        .get(field)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Three-way set merge: a value is kept when both sides have it or one
/// side added it; a removal on one side wins unless the other added it.
/// Order: `theirs`' survivors, then `ours`' additions.
fn merge_set(base: &[Value], ours: &[Value], theirs: &[Value]) -> Vec<Value> {
    theirs
        .iter()
        .filter(|value| ours.contains(value) || !base.contains(value))
        .chain(
            ours.iter()
                .filter(|value| !base.contains(value) && !theirs.contains(value)),
        )
        .fold(Vec::new(), |mut merged, value| {
            if !merged.contains(value) {
                merged.push(value.clone());
            }
            merged
        })
}

/// `winner` with `tags`/`pageTags` replaced by their three-way set
/// merges. A field missing everywhere stays missing; an empty result is
/// written only where the winner already had the field (xochitl adds
/// `tags: []` on first open, rmu's skeleton has none).
fn merge_sets(
    winner: &Value,
    base: Option<&Value>,
    ours: Option<&Value>,
    theirs: Option<&Value>,
) -> Value {
    let values = |content: Option<&Value>, field| {
        content
            .map(|content| set_values(content, field))
            .unwrap_or_default()
    };
    SET_FIELDS.iter().fold(winner.clone(), |mut merged, field| {
        let result = merge_set(
            &values(base, field),
            &values(ours, field),
            &values(theirs, field),
        );
        if let Some(map) = merged.as_object_mut()
            && (!result.is_empty() || map.contains_key(*field))
        {
            map.insert((*field).to_string(), Value::Array(result));
        }
        merged
    })
}

/// Per-field three-way `.metadata` merge. Concurrent different values
/// take origin's, reported (except view-state fields, which only the
/// overlay on apply cares about); `lastModified` is the later of both
/// sides, parsed leniently; unknown fields are preserved.
fn merge_metadata(
    rules: &MirrorRules,
    base: Option<&Value>,
    ours: &Value,
    theirs: &Value,
) -> (Value, Vec<String>) {
    let (Some(o), Some(t)) = (ours.as_object(), theirs.as_object()) else {
        return (theirs.clone(), Vec::new());
    };
    let b = base.and_then(Value::as_object);
    let view_state: BTreeSet<&str> = rules
        .view_state(JsonFile::Metadata)
        .iter()
        .filter_map(|path| match path.segments() {
            [Seg::Key(key)] => Some(*key),
            _ => None,
        })
        .collect();
    let keys: BTreeSet<&String> = o
        .keys()
        .chain(t.keys())
        .chain(b.into_iter().flat_map(Map::keys))
        .collect();
    let (merged, conflicts) = keys.into_iter().fold(
        (Map::new(), Vec::new()),
        |(mut merged, mut conflicts), key| {
            let (bv, ov, tv) = (b.and_then(|b| b.get(key)), o.get(key), t.get(key));
            let value = if key == "lastModified" {
                match (ov, tv) {
                    (Some(ov), Some(tv)) => Some(
                        if xochitl::lenient_i64(Some(ov)) > xochitl::lenient_i64(Some(tv)) {
                            ov
                        } else {
                            tv
                        },
                    ),
                    (ov, tv) => ov.or(tv),
                }
            } else if ov == tv || ov == bv {
                tv
            } else if tv == bv {
                ov
            } else {
                if !view_state.contains(key.as_str()) {
                    conflicts.push(key.clone());
                }
                tv
            };
            if let Some(value) = value {
                merged.insert(key.clone(), value.clone());
            }
            (merged, conflicts)
        },
    );
    (Value::Object(merged), conflicts)
}

/// The entry for merged JSON: an existing blob whose parsed value is
/// equal (keeps origin's bytes stable), else new pretty-printed content.
fn json_entry<'v>(
    value: &Value,
    mut candidates: impl Iterator<Item = (&'v Value, &'v ListingEntry)>,
) -> MergedEntry {
    candidates
        .find(|(candidate, _)| *candidate == value)
        .map(|(_, entry)| MergedEntry::Existing(entry.clone()))
        .unwrap_or_else(|| MergedEntry::New(json_bytes(value)))
}

fn json_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec_pretty(value).expect("JSON values serialize")
}

// ---- tree rules ------------------------------------------------------------

/// Upper bound on folders resurrected from history in one merge. A real
/// library never comes near it; reaching it means a damaged history.
const MAX_RESURRECTIONS: usize = 10_000;

/// Apply `step` until it changes nothing and report whether any step
/// changed something. `bound` is the proven maximum number of changing
/// steps: exceeding it is a bug in rmu and an error, never a silently
/// unfinished tree.
fn fixpoint<S>(
    state: S,
    bound: usize,
    rule: &'static str,
    mut step: impl FnMut(S) -> Result<(S, bool)>,
) -> Result<(S, bool)> {
    let outcome = (0..=bound).try_fold((state, false), |(state, changed), _| match step(state) {
        Ok((state, true)) => ControlFlow::Continue((state, true)),
        Ok((state, false)) => ControlFlow::Break(Ok((state, changed))),
        Err(err) => ControlFlow::Break(Err(err)),
    });
    match outcome {
        ControlFlow::Break(result) => result,
        ControlFlow::Continue(_) => Err(MergeError::NoFixpoint { rule, bound }.into()),
    }
}

/// The merged tree while the tree rules run.
#[derive(Default)]
struct State {
    versions: BTreeMap<DocId, MergedVersion>,
    /// Parsed `.metadata` of every present merged document.
    metadata: BTreeMap<DocId, Value>,
    /// Parent and kind of every present merged document, kept in step
    /// with `metadata` (never rebuilt).
    items: BTreeMap<DocId, TreeItem>,
    /// Documents whose metadata the tree rules rewrote.
    dirty: BTreeSet<DocId>,
    resolutions: BTreeMap<DocId, Resolution>,
    notes: Vec<Note>,
    pending: Vec<PendingFork>,
    forks: Vec<Fork>,
    /// Items restored from the trash or resurrected: sent to the root if
    /// still on a cycle.
    moved_in: BTreeSet<DocId>,
    /// Parents looked up in history (found or not).
    tried: BTreeSet<DocId>,
    /// Memoised [`is_edit`] answers.
    edit_status: BTreeMap<(DocId, Side), bool>,
    resurrections: usize,
}

impl State {
    fn with_doc(mut self, ctx: &Ctx, id: &DocId, doc: DocMerge) -> Result<Self> {
        self.resolutions.insert(id.clone(), doc.resolution);
        self.notes.extend(doc.notes.into_iter().map(|kind| Note {
            doc: id.clone(),
            kind,
        }));
        if let Some(version) = doc.version {
            if version.is_present() {
                let metadata = merged_metadata(ctx, id, &version)?;
                self.insert_present(id, metadata);
            }
            self.versions.insert(id.clone(), version);
        }
        self.pending.extend(doc.fork);
        Ok(self)
    }

    fn insert_present(&mut self, id: &DocId, metadata: Value) {
        self.items
            .insert(id.clone(), TreeItem::from_metadata(&metadata));
        self.metadata.insert(id.clone(), metadata);
    }

    fn set_parent(&mut self, id: &DocId, parent: &str) {
        if let Some(Value::Object(map)) = self.metadata.get_mut(id) {
            map.insert("parent".to_string(), Value::String(parent.to_string()));
            self.dirty.insert(id.clone());
        }
        if let Some(item) = self.items.get_mut(id) {
            item.parent = parent.to_string();
        }
    }

    fn note(&mut self, doc: &DocId, kind: NoteKind) {
        self.notes.push(Note {
            doc: doc.clone(),
            kind,
        });
    }
}

/// `start`'s path up to and including the nearest item (itself
/// included) whose parent is the trash, if there is one.
fn trash_path(items: &BTreeMap<DocId, TreeItem>, start: &DocId) -> Option<Vec<DocId>> {
    let walk: Vec<DocId> = ancestors(items, start).collect();
    let end = walk.iter().position(|id| {
        items
            .get(id)
            .is_some_and(|item| item.parent == TRASH_PARENT)
    })?;
    Some(walk[..=end].to_vec())
}

/// Whether `start`'s path reaches the trash (no allocation).
fn on_trash_path(items: &BTreeMap<DocId, TreeItem>, start: &DocId) -> bool {
    ancestors(items, start).any(|id| {
        items
            .get(&id)
            .is_some_and(|item| item.parent == TRASH_PARENT)
    })
}

/// Whether `item` may be moved under `parent` without creating an
/// orphan or a cycle: the root, or a present folder not below `item`.
fn safe_parent(items: &BTreeMap<DocId, TreeItem>, item: &DocId, parent: &str) -> bool {
    parent.is_empty()
        || DocId::new(parent).is_ok_and(|parent| {
            items
                .get(&parent)
                .is_some_and(|found| found.kind == Some(ItemKind::Folder))
                && !ancestors(items, &parent).any(|ancestor| ancestor == *item)
        })
}

/// A [`safe_parent`] that is also off every trash path.
fn trash_free_parent(items: &BTreeMap<DocId, TreeItem>, item: &DocId, parent: &str) -> bool {
    safe_parent(items, item, parent)
        && (parent.is_empty()
            || DocId::new(parent).is_ok_and(|parent| !on_trash_path(items, &parent)))
}

/// The parsed metadata of a merged version.
fn merged_metadata(ctx: &Ctx, id: &DocId, version: &MergedVersion) -> Result<Value> {
    match version.files.get(&DocFile::Metadata) {
        Some(MergedEntry::Existing(entry)) => {
            Ok((*ctx.blobs.json(id, JsonFile::Metadata, entry)?).clone())
        }
        Some(MergedEntry::New(bytes)) => {
            Ok(serde_json::from_slice(bytes).expect("the merge writes valid JSON"))
        }
        None => Ok(Value::Null),
    }
}

/// Whether `side` changed `doc`'s fork-class content outside its own
/// trash, so `doc` must not end up on a trash path (edits beat
/// deletion). Decided only for documents found on a trash path, and
/// memoised: the tree rules move items, they never change what a side
/// did, so an ordinary sync parses nothing for it.
fn is_edit(
    ctx: &Ctx,
    memo: &mut BTreeMap<(DocId, Side), bool>,
    doc: &DocId,
    side: Side,
) -> Result<bool> {
    if let Some(known) = memo.get(&(doc.clone(), side)) {
        return Ok(*known);
    }
    let sides = ctx.sides(doc);
    let version = sides.side(side);
    // New on that side: changed without reading anything.
    let edit = present(version)
        && version != sides.base
        && (!present(sides.base) || ctx.fork_changed(doc, side)?)
        && !ctx.in_own_trash(doc, side)?;
    memo.insert((doc.clone(), side), edit);
    Ok(edit)
}

/// Tree rules in rounds of trash vs edit, orphans, cycles, until a round
/// changes nothing.
///
/// Two changing rounds suffice: trash vs edit only moves items to safe
/// parents (see [`safe_parent`]), so it creates neither orphans nor
/// cycles. After round one has resolved orphans and cycles, round two
/// can change only through trash vs edit (taking out of the trash what
/// a resurrection or cycle break put there), and round three changes
/// nothing. Anything else is an error, as is a tree left with
/// violations.
fn tree_rules(ctx: &Ctx, history: &dyn History, state: State) -> Result<State> {
    let (state, _) = fixpoint(state, 2, "tree rules", |state| {
        let bound = 2 * state.items.len() + 1;
        let (state, restored) = fixpoint(state, bound, "trash vs edit", |state| {
            restore_step(ctx, state)
        })?;
        let bound = state.items.len() + 2 * MAX_RESURRECTIONS;
        let (state, resurrected) = fixpoint(state, bound, "orphans", |state| {
            orphan_step(ctx, history, state)
        })?;
        let bound = 2 * state.items.len() + 1;
        let (state, broken) = fixpoint(state, bound, "cycles", |state| cycle_step(ctx, state))?;
        Ok((state, restored || resurrected || broken))
    })?;
    let violations = tree_violations(&state.items);
    if violations.is_empty() {
        Ok(state)
    } else {
        Err(MergeError::BrokenTree(violations).into())
    }
}

/// Tree rule 1, one move: edits beat deletion. For the first edit whose
/// document sits on a trash path, the nearest trashed item goes back to
/// its pre-trash parent: the one the editing side gives it, or base's if
/// that side lacks it (the walk then continues upward, in case that
/// parent is in the trash too). If it has none (the editing side trashed
/// it too, or no such parent is safe), the item just below it on the path
/// moves out instead, to the editing side's parent, base's, or the root,
/// whichever is first off every trash path.
///
/// Bound: no move ever gives an item the trash as parent, so each
/// restore of a trashed item lowers the number of trash-parented items
/// (at most one per item). Each move of the item below puts it on a
/// trash-free path for good: items off trash paths never move and their
/// paths never change, since every move is of an item on a trash path
/// (at most one per item). So there are at most two moves per item.
fn restore_step(ctx: &Ctx, mut state: State) -> Result<(State, bool)> {
    // Documents in item order, sides ours then theirs (deterministic),
    // stopping at the first edit.
    let found = {
        let State {
            items, edit_status, ..
        } = &mut state;
        items
            .keys()
            .filter(|doc| on_trash_path(items, doc))
            .flat_map(|doc| [(doc, Side::Ours), (doc, Side::Theirs)])
            .try_fold((), |(), (doc, side)| {
                match is_edit(ctx, edit_status, doc, side) {
                    Ok(true) => ControlFlow::Break(Ok(Some((doc.clone(), side)))),
                    Ok(false) => ControlFlow::Continue(()),
                    Err(err) => ControlFlow::Break(Err(err)),
                }
            })
    };
    let Some((doc, side)) = (match found {
        ControlFlow::Break(result) => result?,
        ControlFlow::Continue(()) => None,
    }) else {
        return Ok((state, false));
    };
    let path = trash_path(&state.items, &doc).expect("a candidate is on a trash path");
    let (item, parent) = restore_move(ctx, &state.items, side, &path)?;
    state.set_parent(&item, &parent);
    state.note(&item, NoteKind::RestoredFromTrash { parent });
    state.moved_in.insert(item);
    Ok((state, true))
}

/// Which item moves, and where (see [`restore_step`]).
fn restore_move(
    ctx: &Ctx,
    items: &BTreeMap<DocId, TreeItem>,
    side: Side,
    path: &[DocId],
) -> Result<(DocId, String)> {
    let trashed = path.last().expect("a trash path is never empty");
    let sides = ctx.sides(trashed);
    let pre_trash = match ctx.side_parent(trashed, sides.side(side))? {
        // The editing side trashed it too: it stays.
        Some(parent) => Some(parent).filter(|parent| parent != TRASH_PARENT),
        None => ctx
            .side_parent(trashed, sides.base)?
            .filter(|parent| parent != TRASH_PARENT),
    };
    if let Some(parent) = pre_trash.filter(|parent| safe_parent(items, trashed, parent)) {
        return Ok((trashed.clone(), parent));
    }
    let below = path
        .len()
        .checked_sub(2)
        .map_or(trashed, |index| &path[index]);
    let sides = ctx.sides(below);
    let parent = [
        ctx.side_parent(below, sides.side(side))?,
        ctx.side_parent(below, sides.base)?,
    ]
    .into_iter()
    .flatten()
    .find(|parent| parent != TRASH_PARENT && trash_free_parent(items, below, parent))
    .unwrap_or_default();
    Ok((below.clone(), parent))
}

/// Tree rule 2, one step: a present item whose parent is missing gets
/// that parent back: theirs' or the base's own version, else origin's
/// history (a resurrected folder that was in the trash returns to its
/// last location outside it); without any, or for a parent that is not
/// a folder, the item moves to the root.
///
/// Bound: each step resurrects a parent never looked up before (at most
/// [`MAX_RESURRECTIONS`]) or moves an item to the root, after which it
/// cannot be an orphan; so steps ≤ items + 2 × the resurrection cap.
fn orphan_step(ctx: &Ctx, history: &dyn History, mut state: State) -> Result<(State, bool)> {
    let Some(violation) = parent_violations(&state.items).into_iter().next() else {
        return Ok((state, false));
    };
    match violation {
        TreeViolation::ParentNotFolder { item, parent } => {
            state.set_parent(&item, "");
            state.note(
                &item,
                NoteKind::ParentNotFolder {
                    parent: parent.to_string(),
                },
            );
        }
        TreeViolation::MissingParent { item, parent } => {
            let candidate = DocId::new(parent.as_str())
                .ok()
                .filter(|id| !state.tried.contains(id));
            let versions = match &candidate {
                Some(id) => {
                    if state.resurrections >= MAX_RESURRECTIONS {
                        return Err(MergeError::TooManyResurrections(MAX_RESURRECTIONS).into());
                    }
                    state.tried.insert(id.clone());
                    parent_versions(ctx, history, id)?
                }
                None => Vec::new(),
            };
            match (candidate, versions.first()) {
                (Some(id), Some(newest)) => {
                    let metadata = resurrected_metadata(ctx, &id, &versions)?;
                    let stored = ctx.json(&id, JsonFile::Metadata, Some(newest))?;
                    if stored.as_deref() != Some(&metadata) {
                        state.dirty.insert(id.clone());
                    }
                    state
                        .versions
                        .insert(id.clone(), MergedVersion::existing(newest));
                    state.insert_present(&id, metadata);
                    state
                        .resolutions
                        .insert(id.clone(), Resolution::Resurrected);
                    state.note(&id, NoteKind::Resurrected);
                    state.moved_in.insert(id);
                    state.resurrections += 1;
                }
                _ => {
                    state.set_parent(&item, "");
                    state.note(
                        &item,
                        NoteKind::OrphanToRoot {
                            missing_parent: parent,
                        },
                    );
                }
            }
        }
        TreeViolation::Cycle(_) => unreachable!("parent_violations reports no cycles"),
    }
    Ok((state, true))
}

/// The versions a missing parent can come back from, newest first:
/// theirs' or the base's own version first (origin may still have the
/// folder the tablet deleted), then origin's history, which is needed
/// only when neither has it present outside the trash.
fn parent_versions(ctx: &Ctx, history: &dyn History, id: &DocId) -> Result<Vec<DocVersion>> {
    let sides = ctx.sides(id);
    let local = [sides.theirs, sides.base]
        .into_iter()
        .flatten()
        .find(|version| version.is_present());
    match local {
        Some(version) if ctx.side_parent(id, Some(version))?.as_deref() != Some(TRASH_PARENT) => {
            Ok(vec![version.clone()])
        }
        Some(version) => Ok(std::iter::once(version.clone())
            .chain(history.present_versions(id)?)
            .collect()),
        None => history.present_versions(id),
    }
}

/// The newest version's metadata, with its parent replaced by the last
/// non-trash parent in history when it was in the trash (root if none).
fn resurrected_metadata(ctx: &Ctx, id: &DocId, versions: &[DocVersion]) -> Result<Value> {
    let parents: Vec<Value> = versions
        .iter()
        .map(|version| {
            ctx.json(id, JsonFile::Metadata, Some(version))
                .map(|metadata| metadata.map(|m| (*m).clone()).unwrap_or(Value::Null))
        })
        .collect::<Result<_>>()?;
    let newest = parents.first().cloned().unwrap_or(Value::Null);
    if parent_of(&newest) != TRASH_PARENT {
        return Ok(newest);
    }
    let outside = parents
        .iter()
        .map(parent_of)
        .find(|parent| parent != TRASH_PARENT)
        .unwrap_or_default();
    Ok(match newest {
        Value::Object(mut map) => {
            map.insert("parent".to_string(), Value::String(outside));
            Value::Object(map)
        }
        other => other,
    })
}

/// Tree rule 3, one step: while the folder tree has a cycle, move one
/// folder on it.
///
/// In order of preference: a folder restored from the trash or
/// resurrected goes to the root; else a folder whose parent differs from
/// origin's (one the tablet moved) takes origin's parent, if that is the
/// root or a present folder; else a folder origin does not have goes to
/// the root; else (never reached while origin's tree is acyclic) the
/// smallest goes to the root.
///
/// Bound: every step either sends a folder to the root, after which it
/// can never be on a cycle again (the root ends every walk), or gives a
/// folder origin's parent, after which the second preference never picks
/// it again. So there are at most two steps per folder. A cycle with no
/// candidate would consist of origin's own edges, which is impossible
/// since origin's tree is acyclic; the last preference makes termination
/// independent of that.
fn cycle_step(ctx: &Ctx, mut state: State) -> Result<(State, bool)> {
    let items = &state.items;
    let Some(cycle) = items.keys().find_map(|start| cycle_from(items, start)) else {
        return Ok((state, false));
    };
    let theirs_parents: Vec<(DocId, Option<String>)> = cycle
        .iter()
        .map(|id| {
            let sides = ctx.sides(id);
            ctx.side_parent(id, sides.theirs)
                .map(|parent| (id.clone(), parent))
        })
        .collect::<Result<_>>()?;
    let usable = |parent: &str| {
        parent.is_empty()
            || DocId::new(parent).is_ok_and(|id| {
                items
                    .get(&id)
                    .is_some_and(|item| item.kind == Some(ItemKind::Folder))
            })
    };
    let (folder, parent) = cycle
        .iter()
        .find(|id| state.moved_in.contains(*id))
        .map(|id| (id.clone(), String::new()))
        .or_else(|| {
            theirs_parents.iter().find_map(|(id, parent)| {
                parent
                    .as_ref()
                    .filter(|parent| {
                        usable(parent) && items.get(id).is_some_and(|item| item.parent != **parent)
                    })
                    .map(|parent| (id.clone(), parent.clone()))
            })
        })
        .or_else(|| {
            theirs_parents
                .iter()
                .find(|(_, parent)| parent.is_none())
                .map(|(id, _)| (id.clone(), String::new()))
        })
        .unwrap_or_else(|| (cycle[0].clone(), String::new()));
    state.set_parent(&folder, &parent);
    state.note(&folder, NoteKind::CycleBroken { parent });
    Ok((state, true))
}

/// Name and place forks next to their originals (after the tree rules,
/// so a copy lands where its original ended up). Forks are handled in
/// UUID order, and a name already used in that folder gets a short id
/// suffix, so naming is deterministic.
fn place_forks(params: &MergeParams, mut state: State) -> State {
    let mut pending = std::mem::take(&mut state.pending);
    pending.sort_by(|a, b| a.copy.cmp(&b.copy));
    pending.into_iter().fold(state, |mut state, fork| {
        if state.versions.contains_key(&fork.copy) {
            state.note(
                &fork.original,
                NoteKind::ForkAlreadyPresent {
                    copy: fork.copy.clone(),
                },
            );
            return state;
        }
        let original = state
            .metadata
            .get(&fork.original)
            .cloned()
            .unwrap_or(Value::Null);
        let parent = parent_of(&original);
        let base_name = visible_name(&original)
            .map(str::to_string)
            .unwrap_or_else(|| fork.original.to_string());
        let taken = |name: &str| {
            state.metadata.values().any(|metadata| {
                parent_of(metadata) == parent && visible_name(metadata) == Some(name)
            })
        };
        let plain = format!(
            "{base_name} (conflict {} {})",
            params.device_name, params.date
        );
        let name = if taken(&plain) {
            format!(
                "{base_name} (conflict {} {} {})",
                params.device_name,
                params.date,
                &fork.copy.as_str()[..8]
            )
        } else {
            plain
        };
        let metadata = match fork.metadata {
            Value::Object(mut map) => {
                map.insert("visibleName".to_string(), Value::String(name.clone()));
                map.insert("parent".to_string(), Value::String(parent));
                Value::Object(map)
            }
            other => other,
        };
        let mut version = fork.version;
        version
            .files
            .insert(DocFile::Metadata, MergedEntry::New(json_bytes(&metadata)));
        state.versions.insert(fork.copy.clone(), version);
        state.insert_present(&fork.copy, metadata);
        state.resolutions.insert(
            fork.copy.clone(),
            Resolution::ForkCopy {
                original: fork.original.clone(),
            },
        );
        state.note(
            &fork.original,
            NoteKind::Forked {
                copy: fork.copy.clone(),
            },
        );
        state.forks.push(Fork {
            original: fork.original,
            copy: fork.copy,
            name,
        });
        state
    })
}

/// Write the metadata the tree rules changed, reusing a side's blob when
/// the result equals it.
fn finish_metadata(ctx: &Ctx, state: State) -> Result<State> {
    let dirty: Vec<DocId> = state.dirty.iter().cloned().collect();
    dirty.into_iter().try_fold(state, |mut state, id| {
        let Some(metadata) = state.metadata.get(&id).cloned() else {
            return Ok(state);
        };
        let sides = ctx.sides(&id);
        let candidates: Vec<(Rc<Value>, ListingEntry)> = [sides.theirs, sides.ours, sides.base]
            .into_iter()
            .flatten()
            .filter_map(|version| version.files.get(&DocFile::Metadata))
            .map(|entry| {
                ctx.blobs
                    .json(&id, JsonFile::Metadata, entry)
                    .map(|value| (value, entry.clone()))
            })
            .collect::<Result<_>>()?;
        let entry = json_entry(
            &metadata,
            candidates
                .iter()
                .map(|(value, entry)| (value.as_ref(), entry)),
        );
        if let Some(version) = state.versions.get_mut(&id) {
            version.files.insert(DocFile::Metadata, entry);
        }
        Ok(state)
    })
}

/// The deletion guard over the final merged tree.
fn deletion_guard(
    input: &MergeInput,
    merged: &BTreeMap<DocId, MergedVersion>,
    params: &MergeParams,
) -> DeletionGuard {
    let present_in = |tree: &RepoTree| -> BTreeSet<DocId> {
        tree.docs
            .iter()
            .filter(|(_, version)| version.is_present())
            .map(|(id, _)| id.clone())
            .collect()
    };
    let merged_present: BTreeSet<DocId> = merged
        .iter()
        .filter(|(_, version)| version.is_present())
        .map(|(id, _)| id.clone())
        .collect();
    let (base, ours, theirs) = (
        present_in(input.base),
        present_in(input.ours),
        present_in(input.theirs),
    );
    let from_origin: BTreeSet<DocId> = theirs.difference(&merged_present).cloned().collect();
    let (exempt, from_tablet): (BTreeSet<DocId>, BTreeSet<DocId>) = ours
        .difference(&merged_present)
        .cloned()
        .partition(|id| !theirs.contains(id) && params.consented_deletions.contains(id));
    let empty_snapshot = ours.is_empty() && !base.is_empty();
    let needs_consent = empty_snapshot
        || params.thresholds.exceeded(from_origin.len(), theirs.len())
        || params.thresholds.exceeded(from_tablet.len(), ours.len());
    DeletionGuard {
        from_origin,
        from_tablet,
        exempt,
        empty_snapshot,
        needs_consent,
        allowed: params.allow_deletions,
    }
}

#[cfg(test)]
mod tests;
