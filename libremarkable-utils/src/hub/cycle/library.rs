//! What `log`, `restore` and `add` share (see "History, restore and add"
//! in `docs/hub-design.md`): origin without a sync (through `--origin`, or
//! the URL a tablet stores), the library's items at a commit, documents
//! resolved by UUID or logical path, and one new document pushed onto
//! `main` with the sync's discipline (fast-forward only, retried after a
//! rejection, a lost acknowledgement resolved by fetching).

use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;

use serde_json::Value;

use super::history::{DocStep, HistoryIndex};
use super::origin::OriginCtx;
use super::plan::JsonBlobs;
use super::store::{self, Store};
use super::{CycleError, Hub, Place, TabletBase, check_url, rmu_signature, unix_now};
use crate::error::{Error, Result};
use crate::git::{CommitMeta, FileMode, Oid, Signature};
use crate::hub::device::{DeviceInfo, Tablet};
use crate::hub::model::{
    DocFile, DocId, DocVersion, ListingEntry, MirrorRules, RepoTree, TabletState,
};
use crate::xochitl::{self, Item, TRASH_PARENT};

/// Origin, opened for `log`, `restore` or `add`, and the tablet that
/// supplied its URL or was named with it.
pub(super) struct Library {
    pub origin: OriginCtx,
    pub tablet: Option<Attached>,
}

/// A tablet's hub state and what it says about itself.
pub(super) struct Attached {
    pub state: TabletState,
    pub info: DeviceInfo,
}

impl Library {
    /// The tablet's standing for a report: `base` is its default base, or
    /// one document's.
    pub fn tablet_base(&self, doc: Option<&DocId>) -> Option<TabletBase> {
        self.tablet.as_ref().map(|attached| {
            let state = &attached.state;
            TabletBase {
                device_id: state.device_id.clone(),
                device_name: state.device_name.clone(),
                base: match doc {
                    Some(doc) => state.base_for(doc).cloned(),
                    None => state.base.clone(),
                },
                overrides: state.base_overrides.len(),
            }
        })
    }

    /// The commit author: the tablet's device name (as in its syncs), or
    /// rmu.
    pub fn author(&self, time: i64) -> Signature {
        match &self.tablet {
            Some(attached) => Signature {
                name: attached.state.device_name.to_string(),
                email: format!("{}@rmu.invalid", attached.state.device_id),
                time,
                offset_minutes: 0,
            },
            None => rmu_signature(time),
        }
    }
}

/// One new document for [`Hub::push_new_doc`]: its files except
/// `.metadata`, already in the cache.
pub(super) struct NewDoc {
    pub doc: DocId,
    pub files: BTreeMap<DocFile, ListingEntry>,
    pub other: BTreeMap<String, ListingEntry>,
}

/// What one attempt decided against the `main` it builds on.
pub(super) struct Decided<T> {
    pub metadata: Vec<u8>,
    pub message: String,
    pub outcome: T,
}

impl Hub<'_> {
    /// Origin for `log`, `restore` and `add`. With a tablet: its hub
    /// state, its stored URL (unless [`super::HubOptions::origin`]), its
    /// repository's cache, and origin must have its repo id; `info` also
    /// reads its model and firmware. Without: the URL given, through the
    /// cache this computer keeps for it (created as `clone` creates it).
    pub(super) fn open_library(&self, tablet: Option<&dyn Tablet>, info: bool) -> Result<Library> {
        self.progress.step("Fetching origin");
        let Some(tablet) = tablet else {
            let url = self.options.origin.clone().ok_or(CycleError::NoOrigin)?;
            check_url(&url)?;
            let (cache, main) = store::bootstrap_cache(&self.git, &self.cache_dir, &url)?;
            return Ok(Library {
                origin: OriginCtx::fetched(cache, url, main)?,
                tablet: None,
            });
        };
        let state = tablet.read_state()?.ok_or(CycleError::NotAttached)?;
        let url = self.origin_url(&state.origin)?;
        let (cache, refs) = store::open_cache(&self.git, &self.cache_dir, &state.repo_id, &url)?;
        let origin = OriginCtx::new(cache, url, None, Some(refs))?;
        if origin.config.repo_id != state.repo_id {
            return Err(CycleError::RepoMismatch {
                tablet: state.repo_id.clone(),
                origin: origin.config.repo_id.clone(),
            }
            .into());
        }
        let info = match info {
            true => tablet.device_info()?,
            false => DeviceInfo::default(),
        };
        Ok(Library {
            origin,
            tablet: Some(Attached { state, info }),
        })
    }

    /// Commit `new` onto origin's `main` and push it, fast-forward only.
    /// `decide` settles its `.metadata` and the commit message against
    /// the `main` of each attempt (the folder may be gone, a name taken);
    /// a rejected push fetches and decides again, at most
    /// [`super::HubOptions::push_attempts`] times. Returns the commit,
    /// the attempts it took and the last decision's outcome.
    pub(super) fn push_new_doc<T>(
        &self,
        library: &mut Library,
        new: &NewDoc,
        decide: &mut dyn FnMut(&OriginCtx, &RepoTree) -> Result<Decided<T>>,
    ) -> Result<(Oid, usize, T)> {
        let attempts = self.options.push_attempts.max(1);
        let outcome = (1..=attempts).try_fold((), |(), attempt| {
            match self.attempt_new_doc(library, new, decide) {
                Ok(Some((commit, outcome))) => ControlFlow::Break(Ok((commit, attempt, outcome))),
                Ok(None) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(Err(error)),
            }
        });
        match outcome {
            ControlFlow::Break(result) => result,
            ControlFlow::Continue(()) => Err(CycleError::PushRetriesExhausted(attempts).into()),
        }
    }

    /// One attempt of [`Self::push_new_doc`]; `None` when the push lost
    /// to another writer (origin fetched again).
    fn attempt_new_doc<T>(
        &self,
        library: &mut Library,
        new: &NewDoc,
        decide: &mut dyn FnMut(&OriginCtx, &RepoTree) -> Result<Decided<T>>,
    ) -> Result<Option<(Oid, T)>> {
        let origin = &library.origin;
        let theirs = origin.store.tree_of(origin.rules, &origin.main)?;
        if theirs.docs.contains_key(&new.doc) {
            return Err(Error::InvalidGitInput(format!(
                "document {} already exists on origin",
                new.doc
            )));
        }
        let decided = decide(origin, &theirs)?;
        let mut writer = origin.cache.repo.commit_writer(self.progress, None)?;
        theirs.to_listing().iter().try_for_each(|(path, entry)| {
            writer.add_existing(path.as_str(), entry.mode, &entry.id)
        })?;
        let prefix = format!("docs/{}", new.doc);
        new.files
            .iter()
            .filter(|(file, _)| **file != DocFile::Metadata)
            .map(|(file, entry)| (file.repo_name(), entry))
            .chain(new.other.iter().map(|(name, entry)| (name.clone(), entry)))
            .try_for_each(|(name, entry)| {
                writer.add_existing(format!("{prefix}/{name}"), entry.mode, &entry.id)
            })?;
        writer.add_bytes(
            format!("{prefix}/metadata"),
            FileMode::Regular,
            &decided.metadata,
        )?;
        let time = unix_now();
        let commit = writer.commit(&CommitMeta {
            parents: vec![origin.main.clone()],
            author: library.author(time),
            committer: rmu_signature(time),
            message: decided.message,
        })?;
        let origin = &mut library.origin;
        self.progress.step("Pushing to origin");
        match store::push(
            &origin.cache.repo,
            &origin.url,
            &commit,
            store::MAIN,
            None,
            store::LOCAL_MAIN,
        ) {
            Ok(()) => Ok(Some((commit, decided.outcome))),
            Err(Error::PushRejected { .. }) => {
                origin.refresh_main()?;
                Ok(None)
            }
            Err(error) => {
                // The acknowledgement may be what was lost: origin decides.
                let landed = origin.refresh_main().is_ok()
                    && origin
                        .store
                        .is_ancestor(&commit, &origin.main)
                        .unwrap_or(false);
                match landed {
                    true => Ok(Some((commit, decided.outcome))),
                    false => Err(error),
                }
            }
        }
    }
}

/// The folders and documents of a tree (present documents whose
/// `.metadata` is an object of a known type).
pub(super) fn items_of(tree: &RepoTree, json: &dyn JsonBlobs) -> Result<Vec<Item>> {
    tree.docs
        .iter()
        .filter_map(|(doc, version)| Some((doc, version.files.get(&DocFile::Metadata)?)))
        .map(|(doc, entry)| {
            Ok(json.json(&entry.id)?.and_then(|metadata| {
                xochitl::item_from_metadata(doc.as_str(), &metadata, None, None)
            }))
        })
        .filter_map(Result::transpose)
        .collect()
}

/// Where an item with parent `parent` is, by the items of one tree (see
/// [`Place`]).
pub(super) fn place(items: &[Item], parent: &str) -> Place {
    let by_uuid: HashMap<&str, &Item> = items
        .iter()
        .map(|item| (item.uuid.as_str(), item))
        .collect();
    place_with(parent, items.len(), &mut |uuid| {
        Ok(by_uuid
            .get(uuid)
            .map(|item| (item.visible_name.clone(), item.parent.clone())))
    })
    .expect("an infallible lookup")
}

/// A folder's name and parent, by its UUID (`None`: not a present
/// folder or document).
pub(super) type FolderLookup<'a> = dyn FnMut(&str) -> Result<Option<(String, String)>> + 'a;

/// [`place`] by a lookup of each folder's name and parent; a chain
/// longer than `bound` folders is broken (a cycle).
pub(super) fn place_with(parent: &str, bound: usize, lookup: &mut FolderLookup) -> Result<Place> {
    let mut names = Vec::new();
    let mut current = parent.to_string();
    let root = loop {
        match current.as_str() {
            "" => break "",
            TRASH_PARENT => break "trash/",
            uuid => match lookup(uuid)? {
                Some((name, next)) if names.len() <= bound => {
                    names.push(name);
                    current = next;
                }
                _ => break "?/",
            },
        }
    };
    names.reverse();
    let path = match (root, names.is_empty()) {
        ("", true) => "/".to_string(),
        ("", false) => format!("/{}", names.join("/")),
        ("trash/", true) => TRASH_PARENT.to_string(),
        (root, _) => format!("{root}{}", names.join("/")),
    };
    Ok(Place {
        parent: parent.to_string(),
        path,
    })
}

/// A folder a new document can go into: the root, or a present folder
/// whose parent chain reaches the root (not the trash, not a broken
/// chain).
pub(super) fn usable_folder(items: &[Item], parent: &str) -> bool {
    parent.is_empty()
        || items
            .iter()
            .any(|item| item.uuid == parent && item.is_folder())
            && place(items, parent).path.starts_with('/')
}

/// A document's visible name from its `.metadata`, if it has one.
pub(super) fn name_of(metadata: &Value) -> Option<String> {
    metadata
        .get("visibleName")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The metadata of a version, if present and an object.
pub(super) fn metadata_of(
    json: &dyn JsonBlobs,
    version: &DocVersion,
) -> Result<Option<std::rc::Rc<Value>>> {
    match version.files.get(&DocFile::Metadata) {
        Some(entry) => Ok(json
            .json(&entry.id)?
            .filter(|metadata| metadata.is_object())),
        None => Ok(None),
    }
}

/// The document `reference` names, at commit `from` of the indexed
/// history: a UUID (any case) that was in the history, or a logical path
/// resolved at `from`, else at each older first-parent commit in turn
/// (the newest commit where it matches anything decides). An ambiguous
/// path is refused, never guessed.
pub(super) fn resolve_doc(
    store: &Store,
    rules: &MirrorRules,
    index: &HistoryIndex,
    from: &Oid,
    reference: &str,
) -> Result<DocId> {
    let lowered = reference.trim().to_ascii_lowercase();
    if let Ok(doc) = DocId::new(lowered) {
        return match index
            .versions(&doc)
            .iter()
            .any(|(_, version)| version.is_some())
        {
            true => Ok(doc),
            false => Err(CycleError::NotInHistory(reference.to_string()).into()),
        };
    }
    let chain = index
        .chain(from)
        .ok_or_else(|| CycleError::NotOnMain(from.clone()))?;
    // Each document's `.metadata` blob at the commit being looked at.
    let metadata: BTreeMap<DocId, Oid> = store
        .tree_of(rules, from)?
        .docs
        .into_iter()
        .filter_map(|(doc, version)| Some((doc, version.files.get(&DocFile::Metadata)?.id.clone())))
        .collect();
    let resolve = |metadata: &BTreeMap<DocId, Oid>| -> Result<Option<DocId>> {
        let items = metadata
            .iter()
            .map(|(doc, id)| {
                Ok(store.json(id)?.and_then(|metadata| {
                    xochitl::item_from_metadata(doc.as_str(), &metadata, None, None)
                }))
            })
            .filter_map(Result::transpose)
            .collect::<Result<Vec<Item>>>()?;
        match xochitl::resolve_item_ref(&items, reference) {
            Ok(item) => Ok(Some(DocId::new(item.uuid.as_str())?)),
            Err(Error::PathNotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    };
    if let Some(found) = resolve(&metadata)? {
        return Ok(found);
    }
    // Walk back: each step turns the state at a commit into the state at
    // its first parent; resolve again whenever a `.metadata` moved.
    let step_back = |mut metadata: BTreeMap<DocId, Oid>, changes: &[DocStep<'_>]| {
        changes
            .iter()
            .try_fold(false, |moved, (doc, before, _)| -> Result<bool> {
                let id = match before {
                    Some(subtree) => store
                        .doc_version(rules, doc, subtree)?
                        .files
                        .get(&DocFile::Metadata)
                        .map(|entry| entry.id.clone()),
                    None => None,
                };
                let previous = match &id {
                    Some(id) => metadata.insert((*doc).clone(), id.clone()),
                    None => metadata.remove(*doc),
                };
                Ok(moved || previous != id)
            })
            .map(|moved| (metadata, moved))
    };
    let found = chain.iter().try_fold(metadata, |metadata, step| {
        let found = step_back(metadata, &step.changes).and_then(|(metadata, moved)| {
            let found = match moved {
                true => resolve(&metadata)?,
                false => None,
            };
            Ok((metadata, found))
        });
        match found {
            Ok((_, Some(doc))) => ControlFlow::Break(Ok(doc)),
            Ok((metadata, None)) => ControlFlow::Continue(metadata),
            Err(error) => ControlFlow::Break(Err(error)),
        }
    });
    match found {
        ControlFlow::Break(result) => result,
        ControlFlow::Continue(_) => Err(Error::PathNotFound(reference.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xochitl::ItemKind;

    fn item(uuid: &str, name: &str, parent: &str, kind: ItemKind) -> Item {
        Item {
            uuid: uuid.to_string(),
            visible_name: name.to_string(),
            parent: parent.to_string(),
            kind,
            file_type: None,
            created_time: 0,
            last_modified: 0,
            size_bytes: None,
        }
    }

    fn library() -> Vec<Item> {
        vec![
            item("books", "Books", "", ItemKind::Folder),
            item("math", "Math", "books", ItemKind::Folder),
            item("old", "Old", "trash", ItemKind::Folder),
            item("inner", "Inner", "old", ItemKind::Folder),
            item("doc", "Doc", "math", ItemKind::Document),
            item("loop-a", "A", "loop-b", ItemKind::Folder),
            item("loop-b", "B", "loop-a", ItemKind::Folder),
        ]
    }

    #[test]
    fn places_name_the_folder_path() {
        let items = library();
        let path = |parent: &str| place(&items, parent).path;
        assert_eq!(path(""), "/");
        assert_eq!(path("books"), "/Books");
        assert_eq!(path("math"), "/Books/Math");
        assert_eq!(path("trash"), "trash");
        assert_eq!(path("inner"), "trash/Old/Inner");
        assert_eq!(path("gone"), "?/");
        assert!(path("loop-a").starts_with("?/"));
        assert_eq!(place(&items, "math").parent, "math");
    }

    #[test]
    fn usable_folders_are_present_folders_outside_the_trash() {
        let items = library();
        assert!(usable_folder(&items, ""));
        assert!(usable_folder(&items, "math"));
        assert!(!usable_folder(&items, "doc"), "a document");
        assert!(!usable_folder(&items, "inner"), "in the trash");
        assert!(!usable_folder(&items, "trash"));
        assert!(!usable_folder(&items, "gone"));
        assert!(!usable_folder(&items, "loop-a"), "a broken chain");
    }
}
