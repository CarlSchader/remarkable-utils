//! Snapshot validation ([`MirrorRules::validate_snapshot`]) and
//! carry-forward ([`carry_forward`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

use super::{
    Classification, DocFile, DocId, DocVersion, Exclusion, FileKind, ListingEntry, MirrorRules,
    MirroredFile, ModelError, PageId, RepoTree, Result, StatSignature,
};
use crate::git::Oid;
use crate::xochitl::{self, ItemKind};

/// Result of parsing one `.metadata`/`.content` file: the JSON, or the
/// parse error.
pub type ParseResult = std::result::Result<Value, String>;

/// Why a document is quarantined: neither deleted nor changed, its
/// base version carried forward, reported every run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineReason {
    UnparsableMetadata(String),
    /// `.metadata` parsed but is not an object, or has no string `type`
    /// (every firmware writes one).
    InvalidMetadata(String),
    UnparsableContent(String),
    /// A zero-length `.rm` page.
    EmptyPage(PageId),
    /// Files that contradict each other.
    Inconsistent(String),
    /// Files without a `.metadata` for a document that base has: must
    /// never read as a deletion.
    MissingMetadata,
}

impl fmt::Display for QuarantineReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuarantineReason::UnparsableMetadata(err) => write!(f, "unparsable .metadata: {err}"),
            QuarantineReason::InvalidMetadata(why) => write!(f, "invalid .metadata: {why}"),
            QuarantineReason::UnparsableContent(err) => write!(f, "unparsable .content: {err}"),
            QuarantineReason::EmptyPage(page) => write!(f, "empty page file {page}.rm"),
            QuarantineReason::Inconsistent(why) => write!(f, "inconsistent files: {why}"),
            QuarantineReason::MissingMetadata => f.write_str(".metadata is missing"),
        }
    }
}

/// Validation outcome for one document seen in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocOutcome {
    /// Snapshot it as it is.
    Ok,
    /// Keep its base version.
    Quarantined(QuarantineReason),
    /// Files without a `.metadata` for a document base does not have
    /// (typically leftovers of an interrupted upload): ignored, reported.
    Orphan,
}

/// Per-document outcomes of a snapshot, plus the excluded files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotValidation {
    /// Every document with at least one mirrored file on the tablet.
    pub docs: BTreeMap<DocId, DocOutcome>,
    /// The mirrored files validation saw, per document (same keys as
    /// `docs`). [`carry_forward`] requires the snapshot's files for every
    /// [`DocOutcome::Ok`] document to be exactly these.
    pub files: BTreeMap<DocId, BTreeSet<DocFile>>,
    /// Tablet files that are not mirrored, for reporting.
    pub excluded: BTreeMap<String, Exclusion>,
}

impl MirrorRules {
    /// Validate a snapshot listing.
    ///
    /// `files` is every file found under the data dir (xochitl-relative)
    /// with its stat signature; `json` holds the parse result of every
    /// `.metadata` and `.content` among them (a missing entry counts as
    /// unparsable); `in_base` is the set of documents present in their
    /// base version. Only mirrored files count: leftover thumbnails or
    /// `.local` files alone do not make a document. I/O failures are
    /// not modelled here; the device layer aborts on those. Errors only
    /// for unsafe paths.
    pub fn validate_snapshot(
        &self,
        files: &BTreeMap<String, StatSignature>,
        json: &BTreeMap<String, ParseResult>,
        in_base: &BTreeSet<DocId>,
    ) -> Result<SnapshotValidation> {
        type DocFiles<'a> = BTreeMap<DocFile, (&'a str, &'a StatSignature)>;
        let (mirrored, excluded) = files.iter().try_fold(
            (BTreeMap::<DocId, DocFiles>::new(), BTreeMap::new()),
            |(mut mirrored, mut excluded), (path, stat)| {
                match self.classify(path)? {
                    Classification::Mirrored(file) => {
                        mirrored
                            .entry(file.doc)
                            .or_default()
                            .insert(file.file, (path.as_str(), stat));
                    }
                    Classification::Excluded(reason) => {
                        excluded.insert(path.clone(), reason);
                    }
                }
                Ok::<_, ModelError>((mirrored, excluded))
            },
        )?;
        let (docs, files) = mirrored
            .into_iter()
            .map(|(doc, doc_files)| {
                let parsed = |file: &DocFile| {
                    doc_files.get(file).map(|(path, _)| match json.get(*path) {
                        Some(Ok(value)) => Ok(value),
                        Some(Err(err)) => Err(err.clone()),
                        None => Err("not read".to_string()),
                    })
                };
                let outcome = match parsed(&DocFile::Metadata) {
                    None if in_base.contains(&doc) => {
                        DocOutcome::Quarantined(QuarantineReason::MissingMetadata)
                    }
                    None => DocOutcome::Orphan,
                    Some(metadata) => check_document(
                        self,
                        metadata,
                        parsed(&DocFile::Content),
                        doc_files.iter().map(|(file, (_, stat))| (file, *stat)),
                    )
                    .map_or_else(DocOutcome::Quarantined, |()| DocOutcome::Ok),
                };
                let seen = doc_files.keys().cloned().collect();
                ((doc.clone(), outcome), (doc, seen))
            })
            .unzip();
        Ok(SnapshotValidation {
            docs,
            files,
            excluded,
        })
    }
}

/// Quarantine checks for a document that has a `.metadata`.
fn check_document<'a>(
    rules: &MirrorRules,
    metadata: std::result::Result<&Value, String>,
    content: Option<std::result::Result<&Value, String>>,
    files: impl Iterator<Item = (&'a DocFile, &'a StatSignature)> + Clone,
) -> std::result::Result<(), QuarantineReason> {
    let metadata = metadata.map_err(QuarantineReason::UnparsableMetadata)?;
    if !metadata.is_object() {
        return Err(QuarantineReason::InvalidMetadata(
            "not a JSON object".to_string(),
        ));
    }
    // Committing a typeless `.metadata` would let the per-field metadata
    // merge strip `visibleName` and `parent` from origin. Unknown string
    // types stay valid.
    if !metadata.get("type").is_some_and(Value::is_string) {
        return Err(QuarantineReason::InvalidMetadata(
            "\"type\" is missing or not a string".to_string(),
        ));
    }
    let content = content
        .transpose()
        .map_err(QuarantineReason::UnparsableContent)?;
    if let Some(page) = files.clone().find_map(|(file, stat)| match file {
        DocFile::Page(page) if stat.size == 0 => Some(page.clone()),
        _ => None,
    }) {
        return Err(QuarantineReason::EmptyPage(page));
    }
    let inconsistent = |why: &str| Err(QuarantineReason::Inconsistent(why.to_string()));
    let has = |kind: FileKind| files.clone().any(|(file, _)| file.kind() == kind);
    // A payload these rules do not mirror cannot be checked for.
    let lacks = |kind: FileKind| rules.allows(kind) && !has(kind);
    match xochitl::item_kind(metadata) {
        Some(ItemKind::Folder) => {
            let document_file = [
                FileKind::PayloadPdf,
                FileKind::PayloadEpub,
                FileKind::Page,
                FileKind::PageMetadata,
                FileKind::Highlights,
            ]
            .into_iter()
            .any(has);
            if document_file {
                inconsistent("folder has document files")
            } else {
                Ok(())
            }
        }
        Some(ItemKind::Document) => match content.map(Value::as_object) {
            None => inconsistent("document has no .content"),
            Some(None) => inconsistent("document .content is not a JSON object"),
            Some(Some(content)) => match content.get("fileType").and_then(Value::as_str) {
                Some("pdf") if lacks(FileKind::PayloadPdf) => {
                    inconsistent("pdf document has no .pdf payload")
                }
                Some("epub") if lacks(FileKind::PayloadEpub) => {
                    inconsistent("epub document has no .epub payload")
                }
                _ => Ok(()),
            },
        },
        // Other item types (e.g. templates) are mirrored as they are.
        None => Ok(()),
    }
}

// ---- carry-forward ---------------------------------------------------------

/// A snapshot's mirrored files and their blob ids.
pub type SnapshotFiles = BTreeMap<MirroredFile, Oid>;

/// Build the snapshot tree from the base tree (each document at its own
/// base version; group both with the repository's rules).
///
/// For a document validated [`DocOutcome::Ok`], exactly the mirrored
/// paths are replaced by the snapshot's files; its other paths are
/// carried forward, except ones that collide with a mirrored path (equal
/// to it, or one a directory prefix of the other), which would make an
/// impossible tree. Quarantined and orphan documents keep their base
/// version. A document with no mirrored files on the tablet is deleted:
/// its whole subtree goes, carried-forward paths included. A base
/// subtree that is not a document (no `metadata`) is carried forward,
/// as are paths outside document subtrees.
///
/// The snapshot's files for every `Ok` document must be exactly the
/// files validation saw, and there must be none for documents validation
/// never saw; otherwise the inputs diverged and the snapshot is refused
/// ([`ModelError::SnapshotMismatch`]) rather than reading a missing file
/// as a deletion.
pub fn carry_forward(
    base: &RepoTree,
    snapshot: &SnapshotFiles,
    validation: &SnapshotValidation,
) -> Result<RepoTree> {
    let snapshot_docs = snapshot.iter().fold(
        BTreeMap::<&DocId, BTreeMap<DocFile, ListingEntry>>::new(),
        |mut docs, (file, blob)| {
            docs.entry(&file.doc)
                .or_default()
                .insert(file.file.clone(), ListingEntry::regular(blob.clone()));
            docs
        },
    );
    validation
        .docs
        .iter()
        .filter(|(_, outcome)| **outcome == DocOutcome::Ok)
        .map(|(doc, _)| doc)
        .chain(
            snapshot_docs
                .keys()
                .copied()
                .filter(|doc| !validation.docs.contains_key(*doc)),
        )
        .try_for_each(|doc| {
            check_snapshot_files(doc, validation.files.get(doc), snapshot_docs.get(doc))
        })?;
    let doc_ids: BTreeSet<&DocId> = base
        .docs
        .keys()
        .chain(snapshot_docs.keys().copied())
        .chain(validation.docs.keys())
        .collect();
    let docs = doc_ids
        .into_iter()
        .filter_map(|doc| {
            let base_version = base.docs.get(doc);
            let version = match (validation.docs.get(doc), snapshot_docs.get(doc)) {
                (Some(DocOutcome::Ok), Some(files)) if files.contains_key(&DocFile::Metadata) => {
                    let names: Vec<String> = files.keys().map(DocFile::repo_name).collect();
                    Some(DocVersion {
                        files: files.clone(),
                        other: base_version
                            .into_iter()
                            .flat_map(|version| &version.other)
                            .filter(|(name, _)| !names.iter().any(|m| paths_collide(name, m)))
                            .map(|(name, entry)| (name.clone(), entry.clone()))
                            .collect(),
                    })
                }
                // No mirrored file on the tablet: a document is deleted;
                // a subtree that is not one is carried forward.
                (None, None) => base_version
                    .filter(|version| !version.is_present())
                    .cloned(),
                // Quarantined or orphan: never guess, keep base.
                _ => base_version.cloned(),
            };
            version.map(|version| (doc.clone(), version))
        })
        .collect();
    Ok(RepoTree {
        docs,
        extra: base.extra.clone(),
    })
}

/// The snapshot's files for one document must be exactly the validated
/// ones (none, for a document validation did not see).
fn check_snapshot_files(
    doc: &DocId,
    validated: Option<&BTreeSet<DocFile>>,
    snapshot: Option<&BTreeMap<DocFile, ListingEntry>>,
) -> Result<()> {
    let validated: BTreeSet<&DocFile> = validated.into_iter().flatten().collect();
    let snapshot: BTreeSet<&DocFile> = snapshot.into_iter().flat_map(BTreeMap::keys).collect();
    if validated == snapshot {
        return Ok(());
    }
    let names = |files: BTreeSet<&&DocFile>| files.into_iter().map(|f| f.repo_name()).collect();
    Err(ModelError::SnapshotMismatch {
        doc: doc.clone(),
        missing: names(validated.difference(&snapshot).collect()),
        unexpected: names(snapshot.difference(&validated).collect()),
    })
}

/// Whether two paths in one subtree cannot coexist: equal, or one is a
/// directory prefix of the other.
fn paths_collide(a: &str, b: &str) -> bool {
    let inside = |inner: &str, outer: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    a == b || inside(a, b) || inside(b, a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::FileMode;
    use crate::hub::model::fixtures::*;
    use serde_json::json;

    struct Snapshot {
        files: BTreeMap<String, StatSignature>,
        json: BTreeMap<String, ParseResult>,
    }

    impl Snapshot {
        fn new() -> Self {
            Self {
                files: BTreeMap::new(),
                json: BTreeMap::new(),
            }
        }

        fn file(mut self, path: String, size: u64) -> Self {
            self.files.insert(path, stat(size));
            self
        }

        fn json(self, path: String, value: Value) -> Self {
            let mut this = self.file(path.clone(), 100);
            this.json.insert(path, Ok(value));
            this
        }

        fn broken(self, path: String) -> Self {
            let mut this = self.file(path.clone(), 3);
            this.json
                .insert(path, Err("expected value at line 1".to_string()));
            this
        }

        fn pdf(self, uuid: &str) -> Self {
            self.json(format!("{uuid}.metadata"), sample_metadata())
                .json(format!("{uuid}.content"), sample_content())
                .file(format!("{uuid}.pdf"), 1000)
                .file(format!("{uuid}/{PAGE}.rm"), 500)
                .file(format!("{uuid}.local"), 30)
                .file(format!("{uuid}.thumbnails/{PAGE}.png"), 30)
        }

        fn folder(self, uuid: &str) -> Self {
            self.json(
                format!("{uuid}.metadata"),
                json!({ "type": "CollectionType", "visibleName": "Books", "parent": "" }),
            )
            .json(format!("{uuid}.content"), json!([]))
            .file(format!("{uuid}.pagedata"), 1)
        }

        fn validate(&self, in_base: &[&str]) -> SnapshotValidation {
            let in_base = in_base.iter().map(|uuid| doc(uuid)).collect();
            rules()
                .validate_snapshot(&self.files, &self.json, &in_base)
                .unwrap()
        }

        fn outcome(&self, uuid: &str, in_base: &[&str]) -> DocOutcome {
            self.validate(in_base).docs[&doc(uuid)].clone()
        }
    }

    fn quarantined(reason: QuarantineReason) -> DocOutcome {
        DocOutcome::Quarantined(reason)
    }

    #[test]
    fn valid_documents_and_folders() {
        let snapshot = Snapshot::new()
            .pdf(DOC)
            .folder(FOLDER)
            .file(".tree".to_string(), 10)
            .file(format!("{DOC2}.thumbnails/{PAGE}.png"), 10)
            .file("stray.txt".to_string(), 10);
        let validation = snapshot.validate(&[]);
        assert_eq!(
            validation.docs,
            BTreeMap::from([(doc(DOC), DocOutcome::Ok), (doc(FOLDER), DocOutcome::Ok)])
        );
        // Excluded files are reported; a thumbnail alone makes no document.
        assert_eq!(
            validation.excluded,
            BTreeMap::from([
                (".tree".to_string(), Exclusion::CloudCache),
                (format!("{DOC}.local"), Exclusion::DeviceLocal),
                (format!("{DOC}.thumbnails/{PAGE}.png"), Exclusion::Thumbnail),
                (
                    format!("{DOC2}.thumbnails/{PAGE}.png"),
                    Exclusion::Thumbnail
                ),
                ("stray.txt".to_string(), Exclusion::Unknown),
            ])
        );
    }

    #[test]
    fn native_notebook_and_template_items_are_valid() {
        let notebook = Snapshot::new()
            .json(format!("{DOC}.metadata"), sample_metadata())
            .json(format!("{DOC}.content"), json!({ "fileType": "notebook" }))
            .file(format!("{DOC}/{PAGE}.rm"), 400);
        assert_eq!(notebook.outcome(DOC, &[]), DocOutcome::Ok);
        // Older firmware: empty fileType, no payload.
        let older = Snapshot::new()
            .json(format!("{DOC}.metadata"), sample_metadata())
            .json(format!("{DOC}.content"), json!({ "fileType": "" }));
        assert_eq!(older.outcome(DOC, &[]), DocOutcome::Ok);
        let template = Snapshot::new()
            .json(format!("{DOC}.metadata"), json!({ "type": "TemplateType" }))
            .file(format!("{DOC}.template"), 400);
        assert_eq!(template.outcome(DOC, &[]), DocOutcome::Ok);
    }

    #[test]
    fn files_without_metadata_are_orphans_or_quarantined() {
        // Leftovers of an interrupted upload: payload, content, pagedata.
        let snapshot = Snapshot::new()
            .json(format!("{DOC}.content"), sample_content())
            .file(format!("{DOC}.pdf"), 1000)
            .file(format!("{DOC}.pagedata"), 0);
        assert_eq!(snapshot.outcome(DOC, &[]), DocOutcome::Orphan);
        assert_eq!(
            snapshot.outcome(DOC, &[DOC]),
            quarantined(QuarantineReason::MissingMetadata)
        );
        // Only excluded files left: no document at all (reads as deleted).
        let leftovers = Snapshot::new()
            .file(format!("{DOC}.local"), 30)
            .file(format!("{DOC}.thumbnails/{PAGE}.png"), 30);
        assert!(leftovers.validate(&[DOC]).docs.is_empty());
    }

    #[test]
    fn unparsable_json_is_quarantined() {
        let broken_metadata = Snapshot::new()
            .broken(format!("{DOC}.metadata"))
            .json(format!("{DOC}.content"), sample_content())
            .file(format!("{DOC}.pdf"), 1000);
        assert!(matches!(
            broken_metadata.outcome(DOC, &[]),
            DocOutcome::Quarantined(QuarantineReason::UnparsableMetadata(_))
        ));
        let array_metadata = Snapshot::new()
            .json(format!("{DOC}.metadata"), json!([]))
            .json(format!("{DOC}.content"), json!([]));
        assert_eq!(
            array_metadata.outcome(DOC, &[]),
            quarantined(QuarantineReason::InvalidMetadata(
                "not a JSON object".to_string()
            ))
        );
        // Every firmware writes a string `type`; unknown strings are fine.
        [json!({}), json!({ "type": 1, "visibleName": "X" })]
            .into_iter()
            .for_each(|metadata| {
                let typeless = Snapshot::new()
                    .json(format!("{DOC}.metadata"), metadata)
                    .json(format!("{DOC}.content"), json!([]));
                assert_eq!(
                    typeless.outcome(DOC, &[]),
                    quarantined(QuarantineReason::InvalidMetadata(
                        "\"type\" is missing or not a string".to_string()
                    ))
                );
            });
        let broken_content = Snapshot::new()
            .json(format!("{DOC}.metadata"), sample_metadata())
            .broken(format!("{DOC}.content"))
            .file(format!("{DOC}.pdf"), 1000);
        assert_eq!(
            broken_content.outcome(DOC, &[DOC]),
            quarantined(QuarantineReason::UnparsableContent(
                "expected value at line 1".to_string()
            ))
        );
        // A metadata file without a parse result is never trusted.
        let unread = Snapshot::new()
            .file(format!("{DOC}.metadata"), 100)
            .json(format!("{DOC}.content"), sample_content())
            .file(format!("{DOC}.pdf"), 1000);
        assert_eq!(
            unread.outcome(DOC, &[]),
            quarantined(QuarantineReason::UnparsableMetadata("not read".to_string()))
        );
    }

    #[test]
    fn empty_pages_are_quarantined() {
        let snapshot = Snapshot::new()
            .pdf(DOC)
            .file(format!("{DOC}/{PAGE2}.rm"), 0)
            // Zero-length files of other kinds are fine.
            .file(format!("{DOC}.pagedata"), 0);
        assert_eq!(
            snapshot.outcome(DOC, &[DOC]),
            quarantined(QuarantineReason::EmptyPage(page(PAGE2)))
        );
    }

    #[test]
    fn contradicting_files_are_quarantined() {
        let inconsistent = |snapshot: Snapshot| match snapshot.outcome(DOC, &[]) {
            DocOutcome::Quarantined(QuarantineReason::Inconsistent(why)) => why,
            other => panic!("expected inconsistent, got {other:?}"),
        };
        let folder_with_pages = Snapshot::new()
            .folder(DOC)
            .file(format!("{DOC}/{PAGE}.rm"), 10);
        assert_eq!(inconsistent(folder_with_pages), "folder has document files");
        let folder_with_page_metadata = Snapshot::new()
            .folder(DOC)
            .file(format!("{DOC}/{PAGE}-metadata.json"), 10);
        assert_eq!(
            inconsistent(folder_with_page_metadata),
            "folder has document files"
        );
        let no_content = Snapshot::new()
            .json(format!("{DOC}.metadata"), sample_metadata())
            .file(format!("{DOC}.pdf"), 10);
        assert_eq!(inconsistent(no_content), "document has no .content");
        let array_content = Snapshot::new()
            .json(format!("{DOC}.metadata"), sample_metadata())
            .json(format!("{DOC}.content"), json!([]));
        assert_eq!(
            inconsistent(array_content),
            "document .content is not a JSON object"
        );
        let pdf_without_payload = Snapshot::new()
            .json(format!("{DOC}.metadata"), sample_metadata())
            .json(format!("{DOC}.content"), sample_content());
        assert_eq!(
            inconsistent(pdf_without_payload),
            "pdf document has no .pdf payload"
        );
        let epub_without_payload = Snapshot::new()
            .json(format!("{DOC}.metadata"), sample_metadata())
            .json(format!("{DOC}.content"), json!({ "fileType": "epub" }))
            .file(format!("{DOC}.pdf"), 10);
        assert_eq!(
            inconsistent(epub_without_payload),
            "epub document has no .epub payload"
        );
        // An EPUB with its rendition is fine; a folder without .content too.
        let epub = Snapshot::new()
            .json(format!("{DOC}.metadata"), sample_metadata())
            .json(format!("{DOC}.content"), json!({ "fileType": "epub" }))
            .file(format!("{DOC}.epub"), 10)
            .file(format!("{DOC}.pdf"), 10);
        assert_eq!(epub.outcome(DOC, &[]), DocOutcome::Ok);
        let bare_folder = Snapshot::new().json(
            format!("{DOC}.metadata"),
            json!({ "type": "CollectionType", "visibleName": "Books", "parent": "" }),
        );
        assert_eq!(bare_folder.outcome(DOC, &[]), DocOutcome::Ok);
    }

    #[test]
    fn payload_checks_respect_the_rules() {
        // Rules that do not mirror PDFs cannot tell a missing payload from
        // an unmirrored one, so they must not quarantine every PDF.
        let snapshot = Snapshot::new().pdf(DOC);
        let narrow = rules_without_pdf();
        let validation = narrow
            .validate_snapshot(&snapshot.files, &snapshot.json, &BTreeSet::new())
            .unwrap();
        assert_eq!(validation.docs[&doc(DOC)], DocOutcome::Ok);
        assert_eq!(
            validation.excluded[&format!("{DOC}.pdf")],
            Exclusion::NotInRules(FileKind::PayloadPdf)
        );
        // Such a folder is not inconsistent either: these rules do not see
        // the PDF.
        let folder = Snapshot::new()
            .folder(FOLDER)
            .file(format!("{FOLDER}.pdf"), 10);
        let validation = narrow
            .validate_snapshot(&folder.files, &folder.json, &BTreeSet::new())
            .unwrap();
        assert_eq!(validation.docs[&doc(FOLDER)], DocOutcome::Ok);
    }

    #[test]
    fn validation_rejects_unsafe_listing_paths() {
        let snapshot = Snapshot::new().file(format!("../{DOC}.metadata"), 10);
        assert!(matches!(
            rules().validate_snapshot(&snapshot.files, &snapshot.json, &BTreeSet::new()),
            Err(ModelError::UnsafePath(_))
        ));
    }

    #[test]
    fn quarantine_reasons_display() {
        assert_eq!(
            QuarantineReason::EmptyPage(page(PAGE)).to_string(),
            format!("empty page file {PAGE}.rm")
        );
        assert_eq!(
            QuarantineReason::MissingMetadata.to_string(),
            ".metadata is missing"
        );
    }

    #[test]
    fn validation_records_each_documents_file_set() {
        let snapshot = Snapshot::new()
            .pdf(DOC)
            .json(format!("{DOC2}.content"), json!({ "fileType": "notebook" }));
        let validation = snapshot.validate(&[]);
        assert_eq!(
            validation.files,
            BTreeMap::from([
                (
                    doc(DOC),
                    BTreeSet::from([
                        DocFile::Metadata,
                        DocFile::Content,
                        DocFile::PayloadPdf,
                        DocFile::Page(page(PAGE)),
                    ])
                ),
                (doc(DOC2), BTreeSet::from([DocFile::Content])),
            ])
        );
        assert_eq!(
            validation.files.keys().collect::<Vec<_>>(),
            validation.docs.keys().collect::<Vec<_>>()
        );
    }

    // ---- carry-forward -----------------------------------------------------

    fn snapshot_files(entries: &[(&str, DocFile, u32)]) -> SnapshotFiles {
        entries
            .iter()
            .map(|(uuid, file, n)| (mirrored(uuid, file.clone()), oid(*n)))
            .collect()
    }

    /// A validation whose file sets are exactly the snapshot's files.
    fn validated(snapshot: &SnapshotFiles, entries: &[(&str, DocOutcome)]) -> SnapshotValidation {
        let docs: BTreeMap<DocId, DocOutcome> = entries
            .iter()
            .map(|(uuid, outcome)| (doc(uuid), outcome.clone()))
            .collect();
        let files = docs
            .keys()
            .map(|id| {
                let seen = snapshot
                    .keys()
                    .filter(|file| &file.doc == id)
                    .map(|file| file.file.clone())
                    .collect();
                (id.clone(), seen)
            })
            .collect();
        SnapshotValidation {
            docs,
            files,
            excluded: BTreeMap::new(),
        }
    }

    fn base_tree() -> RepoTree {
        rules()
            .group_tree(listing(&[
                (".rmu/hub.json", 1),
                ("README.md", 2),
                ("docs/{d}/metadata", 10),
                ("docs/{d}/content", 11),
                ("docs/{d}/payload.pdf", 12),
                ("docs/{d}/pages/{p}.rm", 13),
                ("docs/{d}/future/thing.bin", 14),
                ("docs/{d2}/metadata", 20),
                ("docs/{d2}/content", 21),
                ("docs/{d2}/future/thing.bin", 22),
            ]))
            .unwrap()
    }

    #[test]
    fn carry_forward_replaces_exactly_the_mirrored_paths() {
        // DOC: page removed and content changed on the tablet.
        // DOC2: unchanged. FOLDER: new on the tablet.
        let snapshot = snapshot_files(&[
            (DOC, DocFile::Metadata, 10),
            (DOC, DocFile::Content, 31),
            (DOC, DocFile::PayloadPdf, 12),
            (DOC2, DocFile::Metadata, 20),
            (DOC2, DocFile::Content, 21),
            (FOLDER, DocFile::Metadata, 40),
            (FOLDER, DocFile::Content, 41),
        ]);
        let validation = validated(
            &snapshot,
            &[
                (DOC, DocOutcome::Ok),
                (DOC2, DocOutcome::Ok),
                (FOLDER, DocOutcome::Ok),
            ],
        );
        let result = carry_forward(&base_tree(), &snapshot, &validation)
            .unwrap()
            .to_listing();
        assert_eq!(
            result,
            listing(&[
                (".rmu/hub.json", 1),
                ("README.md", 2),
                ("docs/{d}/metadata", 10),
                ("docs/{d}/content", 31),
                ("docs/{d}/payload.pdf", 12),
                ("docs/{d}/future/thing.bin", 14),
                ("docs/{d2}/metadata", 20),
                ("docs/{d2}/content", 21),
                ("docs/{d2}/future/thing.bin", 22),
                ("docs/{f}/metadata", 40),
                ("docs/{f}/content", 41),
            ])
        );
    }

    #[test]
    fn carry_forward_deletes_whole_subtrees() {
        // DOC2 has no mirrored file on the tablet any more.
        let snapshot = snapshot_files(&[
            (DOC, DocFile::Metadata, 10),
            (DOC, DocFile::Content, 11),
            (DOC, DocFile::PayloadPdf, 12),
            (DOC, DocFile::Page(page(PAGE)), 13),
        ]);
        let validation = validated(&snapshot, &[(DOC, DocOutcome::Ok)]);
        let result = carry_forward(&base_tree(), &snapshot, &validation).unwrap();
        assert!(!result.docs.contains_key(&doc(DOC2)));
        assert!(!result.to_listing().keys().any(|path| path.contains(DOC2)));
        assert_eq!(result.docs[&doc(DOC)], base_tree().docs[&doc(DOC)]);
        // An empty snapshot deletes every document, keeps the rest.
        let empty = carry_forward(
            &base_tree(),
            &SnapshotFiles::new(),
            &SnapshotValidation::default(),
        )
        .unwrap();
        assert!(empty.docs.is_empty());
        assert_eq!(empty.extra, base_tree().extra);
    }

    #[test]
    fn carry_forward_keeps_base_subtrees_that_are_not_documents() {
        // Hand-committed or written by another tool: no metadata, so not
        // a document, and nothing for that UUID on the tablet.
        let base = rules()
            .group_tree(listing(&[
                ("docs/{f}/content", 1),
                ("docs/{f}/notes.txt", 2),
                ("docs/{d}/metadata", 3),
            ]))
            .unwrap();
        let result =
            carry_forward(&base, &SnapshotFiles::new(), &SnapshotValidation::default()).unwrap();
        assert_eq!(
            result.to_listing(),
            listing(&[("docs/{f}/content", 1), ("docs/{f}/notes.txt", 2)])
        );
    }

    #[test]
    fn carry_forward_keeps_base_for_quarantined_and_orphan_documents() {
        let snapshot = snapshot_files(&[
            // DOC: unparsable content on the tablet.
            (DOC, DocFile::Metadata, 10),
            (DOC, DocFile::Content, 99),
            // DOC2: metadata missing, base has it.
            (DOC2, DocFile::Content, 98),
            // FOLDER: an orphan, not in base.
            (FOLDER, DocFile::Content, 97),
        ]);
        let mut validation = validated(
            &snapshot,
            &[
                (
                    DOC,
                    quarantined(QuarantineReason::UnparsableContent("x".to_string())),
                ),
                (DOC2, quarantined(QuarantineReason::MissingMetadata)),
                (FOLDER, DocOutcome::Orphan),
            ],
        );
        // File sets of documents that keep their base are not checked.
        validation
            .files
            .get_mut(&doc(DOC))
            .unwrap()
            .insert(DocFile::Pagedata);
        let result = carry_forward(&base_tree(), &snapshot, &validation).unwrap();
        assert_eq!(result, base_tree());
    }

    #[test]
    fn carry_forward_keeps_base_for_ok_documents_without_metadata() {
        // An Ok outcome whose validated and snapshot file sets agree but
        // lack metadata (never produced by validate_snapshot): no
        // mismatch, and the base version is kept, not replaced.
        let snapshot = snapshot_files(&[(DOC, DocFile::Content, 77)]);
        let validation = validated(&snapshot, &[(DOC, DocOutcome::Ok)]);
        assert_eq!(
            validation.files[&doc(DOC)],
            BTreeSet::from([DocFile::Content])
        );
        let result = carry_forward(&base_tree(), &snapshot, &validation).unwrap();
        assert_eq!(result.docs[&doc(DOC)], base_tree().docs[&doc(DOC)]);
    }

    #[test]
    fn carry_forward_never_adds_quarantined_new_documents() {
        let snapshot = snapshot_files(&[(FOLDER, DocFile::Metadata, 50)]);
        let validation = validated(
            &snapshot,
            &[(
                FOLDER,
                quarantined(QuarantineReason::UnparsableMetadata("x".to_string())),
            )],
        );
        let base = RepoTree::default();
        assert_eq!(carry_forward(&base, &snapshot, &validation).unwrap(), base);
    }

    fn mismatch(result: Result<RepoTree>) -> (DocId, Vec<String>, Vec<String>) {
        match result {
            Err(ModelError::SnapshotMismatch {
                doc,
                missing,
                unexpected,
            }) => (doc, missing, unexpected),
            other => panic!("expected a snapshot mismatch, got {other:?}"),
        }
    }

    #[test]
    fn carry_forward_refuses_a_snapshot_missing_a_validated_page() {
        // Validation saw the page; the snapshot's blob list lost it. That
        // must not read as the page being deleted on the tablet.
        let tablet = Snapshot::new().pdf(DOC);
        let validation = tablet.validate(&[DOC]);
        assert_eq!(validation.docs[&doc(DOC)], DocOutcome::Ok);
        let snapshot = snapshot_files(&[
            (DOC, DocFile::Metadata, 30),
            (DOC, DocFile::Content, 31),
            (DOC, DocFile::PayloadPdf, 32),
        ]);
        assert_eq!(
            mismatch(carry_forward(&base_tree(), &snapshot, &validation)),
            (doc(DOC), vec![format!("pages/{PAGE}.rm")], vec![])
        );
    }

    #[test]
    fn carry_forward_refuses_diverged_inputs() {
        let base = base_tree();
        // A file validation never saw.
        let snapshot = snapshot_files(&[(DOC, DocFile::Metadata, 30), (DOC, DocFile::Content, 31)]);
        let mut validation = validated(&snapshot, &[(DOC, DocOutcome::Ok)]);
        validation
            .files
            .get_mut(&doc(DOC))
            .unwrap()
            .remove(&DocFile::Content);
        assert_eq!(
            mismatch(carry_forward(&base, &snapshot, &validation)),
            (doc(DOC), vec![], vec!["content".to_string()])
        );
        // An Ok document with no snapshot files at all.
        let validation = validated(&snapshot, &[(DOC, DocOutcome::Ok)]);
        assert_eq!(
            mismatch(carry_forward(&base, &SnapshotFiles::new(), &validation)),
            (
                doc(DOC),
                vec!["metadata".to_string(), "content".to_string()],
                vec![]
            )
        );
        // Snapshot files for a document validation did not see.
        let stray = snapshot_files(&[(DOC2, DocFile::Content, 78)]);
        assert_eq!(
            mismatch(carry_forward(&base, &stray, &SnapshotValidation::default())),
            (doc(DOC2), vec![], vec!["content".to_string()])
        );
    }

    #[test]
    fn carry_forward_drops_other_paths_that_collide_with_mirrored_ones() {
        let mut flat = listing(&[
            ("docs/{d}/pages", 1),
            ("docs/{d}/content/old", 3),
            ("docs/{d}/pagesx", 4),
            ("docs/{d}/notes.txt", 5),
        ]);
        flat.insert(
            format!("docs/{DOC}/metadata"),
            ListingEntry {
                mode: FileMode::Symlink,
                id: oid(2),
            },
        );
        let base = rules().group_tree(flat).unwrap();
        assert_eq!(base.docs[&doc(DOC)].other.len(), 5);
        let snapshot = snapshot_files(&[
            (DOC, DocFile::Metadata, 30),
            (DOC, DocFile::Content, 31),
            (DOC, DocFile::Page(page(PAGE)), 32),
        ]);
        let validation = validated(&snapshot, &[(DOC, DocOutcome::Ok)]);
        let result = carry_forward(&base, &snapshot, &validation).unwrap();
        assert_eq!(
            result.to_listing(),
            listing(&[
                ("docs/{d}/metadata", 30),
                ("docs/{d}/content", 31),
                ("docs/{d}/pages/{p}.rm", 32),
                ("docs/{d}/pagesx", 4),
                ("docs/{d}/notes.txt", 5),
            ])
        );
    }

    #[test]
    fn path_collisions() {
        assert!(paths_collide("pages", "pages"));
        assert!(paths_collide("pages", "pages/x.rm"));
        assert!(paths_collide("content/old", "content"));
        assert!(!paths_collide("pagesx", "pages/x.rm"));
        assert!(!paths_collide("pages/x.rm", "pages/y.rm"));
        assert!(!paths_collide("content", "contents"));
    }

    #[test]
    fn narrower_rules_carry_forward_what_they_do_not_mirror() {
        let narrow = rules_without_pdf();
        let base = narrow
            .group_tree(listing(&[
                ("docs/{d}/metadata", 10),
                ("docs/{d}/content", 11),
                ("docs/{d}/payload.pdf", 12),
            ]))
            .unwrap();
        // The tablet still has its PDF, but these rules do not see it.
        let snapshot = snapshot_files(&[(DOC, DocFile::Metadata, 30), (DOC, DocFile::Content, 11)]);
        let validation = validated(&snapshot, &[(DOC, DocOutcome::Ok)]);
        let result = carry_forward(&base, &snapshot, &validation)
            .unwrap()
            .to_listing();
        assert_eq!(
            result,
            listing(&[
                ("docs/{d}/metadata", 30),
                ("docs/{d}/content", 11),
                ("docs/{d}/payload.pdf", 12),
            ])
        );
    }

    #[test]
    fn validation_feeds_carry_forward() {
        // End to end over the pure pieces: classify, validate, carry.
        let tablet = Snapshot::new()
            .pdf(DOC)
            .broken(format!("{DOC2}.metadata"))
            .json(format!("{DOC2}.content"), json!({ "fileType": "notebook" }));
        let base = base_tree();
        let in_base: BTreeSet<DocId> = base
            .docs
            .iter()
            .filter(|(_, version)| version.is_present())
            .map(|(id, _)| id.clone())
            .collect();
        let validation = rules()
            .validate_snapshot(&tablet.files, &tablet.json, &in_base)
            .unwrap();
        let snapshot: SnapshotFiles = tablet
            .files
            .keys()
            .enumerate()
            .filter_map(|(n, path)| match rules().classify(path).unwrap() {
                Classification::Mirrored(file) => Some((file, oid(100 + n as u32))),
                Classification::Excluded(_) => None,
            })
            .collect();
        let result = carry_forward(&base, &snapshot, &validation).unwrap();
        assert_eq!(result.docs[&doc(DOC2)], base.docs[&doc(DOC2)]);
        let version = &result.docs[&doc(DOC)];
        assert_eq!(
            version.files.keys().cloned().collect::<Vec<_>>(),
            [
                DocFile::Metadata,
                DocFile::Content,
                DocFile::PayloadPdf,
                DocFile::Page(page(PAGE))
            ]
        );
        assert_eq!(version.other, base.docs[&doc(DOC)].other);
    }
}
