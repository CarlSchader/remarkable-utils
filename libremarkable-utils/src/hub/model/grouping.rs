//! Trees and documents: flat tree listings, built from git's, grouped
//! by document ([`RepoTree`]) and back.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{DocFile, DocId, FileKind, MirrorRules, ModelError, RepoPath, Result};
use crate::git::{self, FileMode, ObjectKind, Oid};
use crate::xochitl::{self, ItemKind};

/// One non-tree entry of a [`TreeListing`]. Unlike [`git::TreeEntry`]
/// (a raw `ls-tree` record with a byte path), its path is the listing's
/// key, as UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListingEntry {
    pub mode: FileMode,
    pub id: Oid,
}

impl ListingEntry {
    pub fn regular(id: Oid) -> Self {
        Self {
            mode: FileMode::Regular,
            id,
        }
    }
}

/// A flat, recursive tree listing: repository path → entry.
pub type TreeListing = BTreeMap<String, ListingEntry>;

/// Build a [`TreeListing`] from a raw recursive listing (the store's walk
/// over raw tree objects, or [`git::Repo::ls_tree_recursive`]: both give
/// git's canonical modes). This is the one place where git's
/// byte paths become the model's string paths: a non-UTF-8 path is
/// refused ([`ModelError::NonUtf8Path`]), never converted lossily, since
/// a renamed carried-forward path would in effect be deleted. Tree
/// entries are dropped (their contents imply them); any other mode git
/// does not write is refused ([`ModelError::UnsupportedEntryMode`]), and
/// so is a path listed twice ([`ModelError::DuplicatePath`]).
pub fn tree_listing(entries: impl IntoIterator<Item = git::TreeEntry>) -> Result<TreeListing> {
    entries
        .into_iter()
        .filter(|entry| entry.kind != ObjectKind::Tree)
        .try_fold(TreeListing::new(), |mut listing, entry| {
            let path = String::from_utf8(entry.path)
                .map_err(|err| ModelError::NonUtf8Path(err.into_bytes()))?;
            let mode = FileMode::from_bits(entry.mode).ok_or_else(|| {
                ModelError::UnsupportedEntryMode {
                    path: path.clone(),
                    mode: entry.mode,
                }
            })?;
            let entry = ListingEntry {
                mode,
                id: entry.oid,
            };
            match listing.entry(path) {
                std::collections::btree_map::Entry::Occupied(taken) => {
                    Err(ModelError::DuplicatePath(taken.key().clone()))
                }
                std::collections::btree_map::Entry::Vacant(free) => {
                    free.insert(entry);
                    Ok(listing)
                }
            }
        })
}

/// One document's version in a tree: its `docs/<uuid>/` subtree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocVersion {
    /// Files the rules mirror.
    pub files: BTreeMap<DocFile, ListingEntry>,
    /// Other paths in the subtree (relative to it), carried forward.
    pub other: BTreeMap<String, ListingEntry>,
}

impl DocVersion {
    /// A document is present when its `metadata` exists.
    pub fn is_present(&self) -> bool {
        self.files.contains_key(&DocFile::Metadata)
    }

    /// Item kind from the parsed `metadata` when available; otherwise a
    /// version with payloads, pages or highlights is a document.
    pub fn item_kind(&self, metadata: Option<&Value>) -> Option<ItemKind> {
        match metadata {
            Some(metadata) => xochitl::item_kind(metadata),
            None => self
                .files
                .keys()
                .any(|file| {
                    matches!(
                        file.kind(),
                        FileKind::PayloadPdf
                            | FileKind::PayloadEpub
                            | FileKind::Page
                            | FileKind::PageMetadata
                            | FileKind::Highlights
                    )
                })
                .then_some(ItemKind::Document),
        }
    }
}

/// A tree listing grouped by document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoTree {
    pub docs: BTreeMap<DocId, DocVersion>,
    /// Every path outside a `docs/<uuid>/` subtree: `.rmu/*`, and
    /// anything else (hand-committed files, malformed UUIDs), carried
    /// forward verbatim and never applied to a tablet.
    pub extra: TreeListing,
}

impl MirrorRules {
    /// Group a flat tree listing into per-document versions. Listings
    /// from git come through [`tree_listing`], which refuses non-UTF-8
    /// paths.
    ///
    /// Only regular and executable entries count as mirrored files;
    /// anything else at a mirrored name (a symlink, a gitlink) goes to
    /// [`DocVersion::other`]: carried forward, never applied. Also fails
    /// on unsafe paths, which git trees cannot normally contain.
    pub fn group_tree(
        &self,
        listing: impl IntoIterator<Item = (String, ListingEntry)>,
    ) -> Result<RepoTree> {
        listing
            .into_iter()
            .try_fold(RepoTree::default(), |mut tree, (path, entry)| {
                match self.parse_repo_path(&path) {
                    Ok(RepoPath::Mirrored(file)) if entry.mode.is_file() => {
                        tree.docs
                            .entry(file.doc)
                            .or_default()
                            .files
                            .insert(file.file, entry);
                    }
                    Ok(RepoPath::Mirrored(file)) => {
                        tree.docs
                            .entry(file.doc)
                            .or_default()
                            .other
                            .insert(file.file.repo_name(), entry);
                    }
                    Ok(RepoPath::DocOther { doc, name }) => {
                        tree.docs.entry(doc).or_default().other.insert(name, entry);
                    }
                    Ok(RepoPath::Rmu(_))
                    | Err(ModelError::OutsideLayout(_) | ModelError::InvalidDocId(_)) => {
                        tree.extra.insert(path, entry);
                    }
                    Err(err) => return Err(err),
                }
                Ok(tree)
            })
    }
}

impl RepoTree {
    /// The inverse of [`MirrorRules::group_tree`].
    pub fn to_listing(&self) -> TreeListing {
        self.docs
            .iter()
            .flat_map(|(doc, version)| {
                version
                    .files
                    .iter()
                    .map(|(file, entry)| (file.repo_name(), entry))
                    .chain(
                        version
                            .other
                            .iter()
                            .map(|(name, entry)| (name.clone(), entry)),
                    )
                    .map(move |(name, entry)| (format!("docs/{doc}/{name}"), entry.clone()))
            })
            .chain(self.extra.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::model::fixtures::*;
    use serde_json::json;

    #[test]
    fn group_tree_and_back() {
        let mut flat = listing(&[
            (".rmu/hub.json", 1),
            ("README.md", 2),
            ("docs/not-a-uuid/metadata", 3),
            ("docs/{d}/metadata", 4),
            ("docs/{d}/content", 5),
            ("docs/{d}/payload.pdf", 6),
            ("docs/{d}/pages/{p}.rm", 7),
            ("docs/{d}/highlights/{p}.json", 8),
            ("docs/{d}/future/thing.bin", 9),
            ("docs/{f}/metadata", 10),
            ("docs/{f}/content", 11),
        ]);
        flat.insert(
            format!("docs/{DOC}/pages/run.sh"),
            ListingEntry {
                mode: FileMode::Executable,
                id: oid(12),
            },
        );
        let tree = rules().group_tree(flat.clone()).unwrap();
        assert_eq!(tree.docs.len(), 2);
        let version = &tree.docs[&doc(DOC)];
        assert!(version.is_present());
        assert_eq!(
            version.files.keys().cloned().collect::<Vec<_>>(),
            [
                DocFile::Metadata,
                DocFile::Content,
                DocFile::PayloadPdf,
                DocFile::Page(page(PAGE)),
                DocFile::Highlights(page(PAGE)),
            ]
        );
        assert_eq!(version.files[&DocFile::Page(page(PAGE))].id, oid(7));
        assert_eq!(
            version.other.keys().collect::<Vec<_>>(),
            ["future/thing.bin", "pages/run.sh"]
        );
        assert_eq!(
            tree.extra.keys().collect::<Vec<_>>(),
            [".rmu/hub.json", "README.md", "docs/not-a-uuid/metadata"]
        );
        assert_eq!(tree.to_listing(), flat);
    }

    #[test]
    fn group_tree_depends_on_rules() {
        let flat = listing(&[("docs/{d}/metadata", 1), ("docs/{d}/payload.pdf", 2)]);
        let tree = rules_without_pdf().group_tree(flat.clone()).unwrap();
        let version = &tree.docs[&doc(DOC)];
        assert!(!version.files.contains_key(&DocFile::PayloadPdf));
        assert_eq!(version.other["payload.pdf"].id, oid(2));
        assert_eq!(tree.to_listing(), flat);
    }

    #[test]
    fn group_tree_rejects_unsafe_paths() {
        let flat = listing(&[("docs/{d}/../{d2}/metadata", 1)]);
        assert!(matches!(
            rules().group_tree(flat.clone()),
            Err(ModelError::UnsafePath(_))
        ));
    }

    #[test]
    fn empty_trees() {
        let tree = rules().group_tree(TreeListing::new()).unwrap();
        assert_eq!(tree, RepoTree::default());
        assert!(tree.to_listing().is_empty());
    }

    #[test]
    fn entry_modes() {
        [
            FileMode::Regular,
            FileMode::Executable,
            FileMode::Symlink,
            FileMode::Gitlink,
        ]
        .iter()
        .for_each(|mode| assert_eq!(FileMode::from_bits(mode.bits()), Some(*mode)));
        assert_eq!(FileMode::from_bits(0o040000), None);
    }

    #[test]
    fn item_kinds() {
        let tree = rules()
            .group_tree(listing(&[
                ("docs/{d}/metadata", 1),
                ("docs/{d}/pages/{p}.rm", 2),
                ("docs/{f}/metadata", 3),
                ("docs/{d2}/pagedata", 4),
            ]))
            .unwrap();
        let notebook = &tree.docs[&doc(DOC)];
        let folder = &tree.docs[&doc(FOLDER)];
        let pagedata_only = &tree.docs[&doc(DOC2)];
        let folder_md = json!({ "type": "CollectionType" });
        assert_eq!(folder.item_kind(Some(&folder_md)), Some(ItemKind::Folder));
        assert_eq!(
            notebook.item_kind(Some(&sample_metadata())),
            Some(ItemKind::Document)
        );
        assert_eq!(
            notebook.item_kind(Some(&json!({ "type": "TemplateType" }))),
            None
        );
        // Without metadata: pages make it a document; otherwise unknown.
        assert_eq!(notebook.item_kind(None), Some(ItemKind::Document));
        assert_eq!(folder.item_kind(None), None);
        assert_eq!(pagedata_only.item_kind(None), None);
        assert!(!pagedata_only.is_present());
    }

    #[test]
    fn only_file_entries_count_as_mirrored_files() {
        let mut flat = listing(&[("docs/{d}/content", 1)]);
        flat.insert(
            format!("docs/{DOC}/metadata"),
            ListingEntry {
                mode: FileMode::Gitlink,
                id: oid(2),
            },
        );
        flat.insert(
            format!("docs/{DOC}/pages/{PAGE}.rm"),
            ListingEntry {
                mode: FileMode::Symlink,
                id: oid(3),
            },
        );
        flat.insert(
            format!("docs/{DOC}/payload.pdf"),
            ListingEntry {
                mode: FileMode::Executable,
                id: oid(4),
            },
        );
        let tree = rules().group_tree(flat.clone()).unwrap();
        let version = &tree.docs[&doc(DOC)];
        assert!(!version.is_present());
        assert_eq!(
            version.files.keys().cloned().collect::<Vec<_>>(),
            [DocFile::Content, DocFile::PayloadPdf]
        );
        assert_eq!(
            version.other.keys().cloned().collect::<Vec<_>>(),
            ["metadata".to_string(), format!("pages/{PAGE}.rm")]
        );
        assert_eq!(tree.to_listing(), flat);
        assert!(FileMode::Regular.is_file() && FileMode::Executable.is_file());
        assert!(!FileMode::Symlink.is_file() && !FileMode::Gitlink.is_file());
    }

    fn git_entry(path: &[u8], mode: u32, kind: ObjectKind, n: u32) -> git::TreeEntry {
        git::TreeEntry {
            mode,
            kind,
            oid: oid(n),
            size: (kind == ObjectKind::Blob).then_some(10),
            path: path.to_vec(),
        }
    }

    #[test]
    fn tree_listing_converts_git_entries() {
        let doc_path = |name: &str| format!("docs/{DOC}/{name}").into_bytes();
        let entries = vec![
            git_entry(b"docs", 0o040000, ObjectKind::Tree, 1),
            git_entry(&doc_path("metadata"), 0o100644, ObjectKind::Blob, 2),
            git_entry(&doc_path("pages"), 0o040000, ObjectKind::Tree, 3),
            git_entry(&doc_path("tool.sh"), 0o100755, ObjectKind::Blob, 4),
            git_entry(&doc_path("link"), 0o120000, ObjectKind::Blob, 5),
            git_entry(&doc_path("sub"), 0o160000, ObjectKind::Commit, 6),
            git_entry(b"README.md", 0o100644, ObjectKind::Blob, 7),
        ];
        let listing = tree_listing(entries).unwrap();
        let entry = |mode, n| ListingEntry { mode, id: oid(n) };
        assert_eq!(
            listing,
            BTreeMap::from([
                (format!("docs/{DOC}/metadata"), entry(FileMode::Regular, 2)),
                (
                    format!("docs/{DOC}/tool.sh"),
                    entry(FileMode::Executable, 4)
                ),
                (format!("docs/{DOC}/link"), entry(FileMode::Symlink, 5)),
                (format!("docs/{DOC}/sub"), entry(FileMode::Gitlink, 6)),
                ("README.md".to_string(), entry(FileMode::Regular, 7)),
            ])
        );
        let tree = rules().group_tree(listing.clone()).unwrap();
        assert!(tree.docs[&doc(DOC)].is_present());
        assert_eq!(tree.to_listing(), listing);
        assert!(tree_listing(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn tree_listing_refuses_non_utf8_paths_and_unknown_modes() {
        let err = tree_listing(vec![git_entry(
            b"docs/caf\xe9",
            0o100644,
            ObjectKind::Blob,
            1,
        )])
        .unwrap_err();
        assert!(matches!(&err, ModelError::NonUtf8Path(path) if path == b"docs/caf\xe9"));
        assert_eq!(
            err.to_string(),
            "repository path 'docs/caf\\xe9' is not UTF-8"
        );
        // Tree entries are dropped before the check, but a non-UTF-8
        // directory's contents carry its name and are refused.
        assert!(matches!(
            tree_listing(vec![
                git_entry(b"\xff", 0o040000, ObjectKind::Tree, 1),
                git_entry(b"\xff/metadata", 0o100644, ObjectKind::Blob, 2),
            ]),
            Err(ModelError::NonUtf8Path(_))
        ));
        let err = tree_listing(vec![git_entry(b"old", 0o100664, ObjectKind::Blob, 1)]).unwrap_err();
        assert!(matches!(
            &err,
            ModelError::UnsupportedEntryMode { path, mode: 0o100664 } if path == "old"
        ));
        assert_eq!(
            err.to_string(),
            "repository path 'old' has unsupported mode 100664"
        );
    }

    #[test]
    fn tree_listing_refuses_a_path_listed_twice() {
        let path = format!("docs/{DOC}/metadata").into_bytes();
        let err = tree_listing(vec![
            git_entry(&path, 0o100644, ObjectKind::Blob, 1),
            git_entry(b"README.md", 0o100644, ObjectKind::Blob, 3),
            git_entry(&path, 0o100644, ObjectKind::Blob, 2),
        ])
        .unwrap_err();
        assert!(matches!(&err, ModelError::DuplicatePath(p) if p.as_bytes() == path));
    }
}
