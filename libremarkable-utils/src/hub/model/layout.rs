//! Layout mapping: xochitl-relative paths on the tablet to repository
//! paths and back, and classification of everything else on the tablet
//! as excluded, with a reason.

use std::fmt;

use super::{DocId, MirrorRules, ModelError, PageId, Result};
use crate::relative_path::split_safe_relative;

/// File-name prefix of rmu's temporary files in the data dir (e.g.
/// `write_remote_file`'s `.rmu-upload-XXXXXX`). Never mirrored.
pub const RMU_TEMP_PREFIX: &str = ".rmu-";

/// The kinds of file the mirror can cover; the rules decide which ones
/// a repository actually mirrors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileKind {
    Metadata,
    Content,
    Pagedata,
    Template,
    PayloadPdf,
    PayloadEpub,
    Page,
    PageMetadata,
    Highlights,
}

impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FileKind::Metadata => ".metadata",
            FileKind::Content => ".content",
            FileKind::Pagedata => ".pagedata",
            FileKind::Template => ".template",
            FileKind::PayloadPdf => ".pdf payload",
            FileKind::PayloadEpub => ".epub payload",
            FileKind::Page => ".rm page",
            FileKind::PageMetadata => "page -metadata.json",
            FileKind::Highlights => "highlights",
        })
    }
}

/// One mirrored file of a document.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DocFile {
    /// `<uuid>.metadata` ↔ `metadata`
    Metadata,
    /// `<uuid>.content` ↔ `content`
    Content,
    /// `<uuid>.pagedata` ↔ `pagedata`
    Pagedata,
    /// `<uuid>.template` ↔ `template`
    Template,
    /// `<uuid>.pdf` ↔ `payload.pdf` (PDFs, and EPUB renditions)
    PayloadPdf,
    /// `<uuid>.epub` ↔ `payload.epub`
    PayloadEpub,
    /// `<uuid>/<id>.rm` ↔ `pages/<id>.rm`
    Page(PageId),
    /// `<uuid>/<id>-metadata.json` ↔ `pages/<id>-metadata.json`
    PageMetadata(PageId),
    /// `<uuid>.highlights/<id>.json` ↔ `highlights/<id>.json`
    Highlights(PageId),
}

impl DocFile {
    pub fn kind(&self) -> FileKind {
        match self {
            DocFile::Metadata => FileKind::Metadata,
            DocFile::Content => FileKind::Content,
            DocFile::Pagedata => FileKind::Pagedata,
            DocFile::Template => FileKind::Template,
            DocFile::PayloadPdf => FileKind::PayloadPdf,
            DocFile::PayloadEpub => FileKind::PayloadEpub,
            DocFile::Page(_) => FileKind::Page,
            DocFile::PageMetadata(_) => FileKind::PageMetadata,
            DocFile::Highlights(_) => FileKind::Highlights,
        }
    }

    /// Path relative to the document's `docs/<uuid>/` subtree.
    pub fn repo_name(&self) -> String {
        match self {
            DocFile::Metadata => "metadata".to_string(),
            DocFile::Content => "content".to_string(),
            DocFile::Pagedata => "pagedata".to_string(),
            DocFile::Template => "template".to_string(),
            DocFile::PayloadPdf => "payload.pdf".to_string(),
            DocFile::PayloadEpub => "payload.epub".to_string(),
            DocFile::Page(id) => format!("pages/{id}.rm"),
            DocFile::PageMetadata(id) => format!("pages/{id}-metadata.json"),
            DocFile::Highlights(id) => format!("highlights/{id}.json"),
        }
    }

    /// Path relative to the xochitl data dir.
    pub fn xochitl_path(&self, doc: &DocId) -> String {
        match self {
            DocFile::Metadata => format!("{doc}.metadata"),
            DocFile::Content => format!("{doc}.content"),
            DocFile::Pagedata => format!("{doc}.pagedata"),
            DocFile::Template => format!("{doc}.template"),
            DocFile::PayloadPdf => format!("{doc}.pdf"),
            DocFile::PayloadEpub => format!("{doc}.epub"),
            DocFile::Page(id) => format!("{doc}/{id}.rm"),
            DocFile::PageMetadata(id) => format!("{doc}/{id}-metadata.json"),
            DocFile::Highlights(id) => format!("{doc}.highlights/{id}.json"),
        }
    }

    /// Inverse of [`DocFile::repo_name`] (ignoring the rules).
    fn from_repo_name(name: &str) -> Option<DocFile> {
        match name.split_once('/') {
            None => match name {
                "metadata" => Some(DocFile::Metadata),
                "content" => Some(DocFile::Content),
                "pagedata" => Some(DocFile::Pagedata),
                "template" => Some(DocFile::Template),
                "payload.pdf" => Some(DocFile::PayloadPdf),
                "payload.epub" => Some(DocFile::PayloadEpub),
                _ => None,
            },
            Some(("pages", file)) => Self::from_page_dir_name(file),
            Some(("highlights", file)) => Self::from_highlights_name(file),
            Some(_) => None,
        }
    }

    /// A top-level `<uuid>.<ext>` file on the tablet.
    fn from_extension(ext: &str) -> Option<DocFile> {
        match ext {
            "metadata" => Some(DocFile::Metadata),
            "content" => Some(DocFile::Content),
            "pagedata" => Some(DocFile::Pagedata),
            "template" => Some(DocFile::Template),
            "pdf" => Some(DocFile::PayloadPdf),
            "epub" => Some(DocFile::PayloadEpub),
            _ => None,
        }
    }

    /// A file in the tablet's `<uuid>/` directory (same names as the
    /// repository's `pages/`).
    fn from_page_dir_name(name: &str) -> Option<DocFile> {
        let page_id = |id: &str| PageId::new(id).ok();
        name.strip_suffix("-metadata.json")
            .and_then(page_id)
            .map(DocFile::PageMetadata)
            .or_else(|| {
                name.strip_suffix(".rm")
                    .and_then(page_id)
                    .map(DocFile::Page)
            })
    }

    /// A file in the tablet's `<uuid>.highlights/` directory (same names
    /// as the repository's `highlights/`).
    fn from_highlights_name(name: &str) -> Option<DocFile> {
        name.strip_suffix(".json")
            .and_then(|id| PageId::new(id).ok())
            .map(DocFile::Highlights)
    }
}

/// A mirrored file: which document, which file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MirroredFile {
    pub doc: DocId,
    pub file: DocFile,
}

impl MirroredFile {
    /// Path relative to the xochitl data dir.
    pub fn xochitl_path(&self) -> String {
        self.file.xochitl_path(&self.doc)
    }

    /// Path in the repository.
    pub fn repo_path(&self) -> String {
        format!("docs/{}/{}", self.doc, self.file.repo_name())
    }
}

/// Why a tablet file is not mirrored. Excluded files stay on the
/// tablet, are reported, and are never copied or deleted by a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// `<uuid>.thumbnails/...`, regenerated by xochitl.
    Thumbnail,
    /// `<uuid>.local`, device-local.
    DeviceLocal,
    /// `.tree`, the cloud-sync cache (holds account identifiers).
    CloudCache,
    /// rmu's own temporary or staging files (`.rmu-*`).
    RmuTemp,
    /// A known file kind that the repository's rules do not mirror.
    NotInRules(FileKind),
    /// Anything unrecognised.
    Unknown,
}

impl fmt::Display for Exclusion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exclusion::Thumbnail => f.write_str("thumbnail"),
            Exclusion::DeviceLocal => f.write_str("device-local file"),
            Exclusion::CloudCache => f.write_str("cloud-sync cache"),
            Exclusion::RmuTemp => f.write_str("rmu temporary file"),
            Exclusion::NotInRules(kind) => {
                write!(f, "{kind} files are not mirrored by these rules")
            }
            Exclusion::Unknown => f.write_str("unrecognised file"),
        }
    }
}

/// What a tablet file is, under a set of mirror rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Mirrored(MirroredFile),
    Excluded(Exclusion),
}

/// What a repository path is, under a set of mirror rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoPath {
    /// A file the rules mirror to the tablet.
    Mirrored(MirroredFile),
    /// A path inside a document's subtree that these rules do not cover
    /// (e.g. written under newer rules): carried forward, never applied.
    DocOther { doc: DocId, name: String },
    /// A path under `.rmu/` (relative to it). Never changed by sync.
    Rmu(String),
}

/// Split a relative path into components, rejecting absolute paths,
/// empty, `.` and `..` components, and NUL bytes.
pub(super) fn checked_components(path: &str) -> Result<Vec<&str>> {
    split_safe_relative(path).ok_or_else(|| ModelError::UnsafePath(path.to_string()))
}

/// `<uuid>` or `<uuid>.<ext>` (split at the first dot).
fn split_doc_name(name: &str) -> Option<(DocId, Option<&str>)> {
    let (uuid, ext) = match name.split_once('.') {
        Some((uuid, ext)) => (uuid, Some(ext)),
        None => (name, None),
    };
    DocId::new(uuid).ok().map(|doc| (doc, ext))
}

/// Recognise a tablet path, ignoring which kinds the rules allow.
fn recognise_xochitl(components: &[&str]) -> Classification {
    use Classification::{Excluded, Mirrored};
    let mirrored = |doc: DocId| move |file: DocFile| Mirrored(MirroredFile { doc, file });
    let unknown = Excluded(Exclusion::Unknown);
    match components {
        [".tree"] => Excluded(Exclusion::CloudCache),
        [name] => match split_doc_name(name) {
            Some((_, Some("local"))) => Excluded(Exclusion::DeviceLocal),
            Some((doc, Some(ext))) => DocFile::from_extension(ext).map_or(unknown, mirrored(doc)),
            _ => unknown,
        },
        [dir, rest @ ..] => match (split_doc_name(dir), rest) {
            (Some((_, Some("thumbnails"))), _) => Excluded(Exclusion::Thumbnail),
            (Some((doc, None)), [file]) => {
                DocFile::from_page_dir_name(file).map_or(unknown, mirrored(doc))
            }
            (Some((doc, Some("highlights"))), [file]) => {
                DocFile::from_highlights_name(file).map_or(unknown, mirrored(doc))
            }
            _ => unknown,
        },
        [] => unknown,
    }
}

impl MirrorRules {
    /// Classify a path relative to the xochitl data dir. Errors only for
    /// unsafe paths; anything else unexpected is [`Exclusion::Unknown`].
    pub fn classify(&self, path: &str) -> Result<Classification> {
        let components = checked_components(path)?;
        if components
            .last()
            .is_some_and(|name| name.starts_with(RMU_TEMP_PREFIX))
        {
            return Ok(Classification::Excluded(Exclusion::RmuTemp));
        }
        Ok(match recognise_xochitl(&components) {
            Classification::Mirrored(file) if !self.allows(file.file.kind()) => {
                Classification::Excluded(Exclusion::NotInRules(file.file.kind()))
            }
            other => other,
        })
    }

    /// Parse a repository path. Errors for unsafe paths, malformed
    /// document UUIDs under `docs/`, and paths outside `docs/<uuid>/`
    /// and `.rmu/`.
    pub fn parse_repo_path(&self, path: &str) -> Result<RepoPath> {
        checked_components(path)?;
        if let Some(rest) = path.strip_prefix(".rmu/") {
            return Ok(RepoPath::Rmu(rest.to_string()));
        }
        let (uuid, name) = path
            .strip_prefix("docs/")
            .and_then(|rest| rest.split_once('/'))
            .ok_or_else(|| ModelError::OutsideLayout(path.to_string()))?;
        let doc = DocId::new(uuid)?;
        Ok(
            match DocFile::from_repo_name(name).filter(|file| self.allows(file.kind())) {
                Some(file) => RepoPath::Mirrored(MirroredFile { doc, file }),
                None => RepoPath::DocOther {
                    doc,
                    name: name.to_string(),
                },
            },
        )
    }

    /// Map a tablet path to its repository path.
    pub fn xochitl_to_repo(&self, path: &str) -> Result<String> {
        match self.classify(path)? {
            Classification::Mirrored(file) => Ok(file.repo_path()),
            Classification::Excluded(reason) => Err(ModelError::Excluded {
                path: path.to_string(),
                reason,
            }),
        }
    }

    /// Map a repository path to its tablet path.
    pub fn repo_to_xochitl(&self, path: &str) -> Result<String> {
        match self.parse_repo_path(path)? {
            RepoPath::Mirrored(file) => Ok(file.xochitl_path()),
            RepoPath::DocOther { .. } | RepoPath::Rmu(_) => {
                Err(ModelError::NotMirrored(path.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::model::fixtures::*;

    fn every_doc_file() -> Vec<DocFile> {
        vec![
            DocFile::Metadata,
            DocFile::Content,
            DocFile::Pagedata,
            DocFile::Template,
            DocFile::PayloadPdf,
            DocFile::PayloadEpub,
            DocFile::Page(page(PAGE)),
            DocFile::PageMetadata(page(PAGE)),
            DocFile::Highlights(page(PAGE)),
        ]
    }

    #[test]
    fn layout_table() {
        let table = [
            ("{d}.metadata", "docs/{d}/metadata"),
            ("{d}.content", "docs/{d}/content"),
            ("{d}.pagedata", "docs/{d}/pagedata"),
            ("{d}.template", "docs/{d}/template"),
            ("{d}.pdf", "docs/{d}/payload.pdf"),
            ("{d}.epub", "docs/{d}/payload.epub"),
            ("{d}/{p}.rm", "docs/{d}/pages/{p}.rm"),
            ("{d}/{p}-metadata.json", "docs/{d}/pages/{p}-metadata.json"),
            ("{d}.highlights/{p}.json", "docs/{d}/highlights/{p}.json"),
        ];
        let fill = |s: &str| s.replace("{d}", DOC).replace("{p}", PAGE);
        table.iter().for_each(|(tablet, repo)| {
            assert_eq!(rules().xochitl_to_repo(&fill(tablet)).unwrap(), fill(repo));
            assert_eq!(rules().repo_to_xochitl(&fill(repo)).unwrap(), fill(tablet));
        });
    }

    #[test]
    fn layout_round_trips_every_kind() {
        every_doc_file().into_iter().for_each(|file| {
            let file = mirrored(DOC, file);
            let tablet = file.xochitl_path();
            let repo = file.repo_path();
            assert_eq!(
                rules().classify(&tablet).unwrap(),
                Classification::Mirrored(file.clone())
            );
            assert_eq!(
                rules().parse_repo_path(&repo).unwrap(),
                RepoPath::Mirrored(file.clone())
            );
            assert_eq!(rules().xochitl_to_repo(&tablet).unwrap(), repo);
            assert_eq!(rules().repo_to_xochitl(&repo).unwrap(), tablet);
        });
    }

    #[test]
    fn page_ids_containing_the_metadata_suffix_round_trip() {
        let id = page("x-metadata");
        [DocFile::Page(id.clone()), DocFile::PageMetadata(id)]
            .into_iter()
            .for_each(|file| {
                let file = mirrored(DOC, file);
                assert_eq!(
                    rules().classify(&file.xochitl_path()).unwrap(),
                    Classification::Mirrored(file.clone())
                );
                assert_eq!(
                    rules().parse_repo_path(&file.repo_path()).unwrap(),
                    RepoPath::Mirrored(file)
                );
            });
    }

    #[test]
    fn exclusions() {
        let cases = [
            ("{d}.thumbnails/{p}.png", Exclusion::Thumbnail),
            ("{d}.thumbnails/sub/x.png", Exclusion::Thumbnail),
            ("{d}.local", Exclusion::DeviceLocal),
            (".tree", Exclusion::CloudCache),
            (".rmu-upload-AbC123", Exclusion::RmuTemp),
            ("{d}/.rmu-upload-AbC123", Exclusion::RmuTemp),
            ("{d}.pdf.bak", Exclusion::Unknown),
            ("{d}.notebook", Exclusion::Unknown),
            ("{d}", Exclusion::Unknown),
            ("notes.txt", Exclusion::Unknown),
            (
                "8C1F0B9E-0000-4000-8000-000000000001.metadata",
                Exclusion::Unknown,
            ),
            ("{d}/{p}.png", Exclusion::Unknown),
            ("{d}/a.b.rm", Exclusion::Unknown),
            ("{d}/sub/{p}.rm", Exclusion::Unknown),
            ("{d}.highlights/{p}.txt", Exclusion::Unknown),
            ("{d}.highlights/sub/{p}.json", Exclusion::Unknown),
            ("{d}.other/{p}.rm", Exclusion::Unknown),
            ("not-a-uuid/{p}.rm", Exclusion::Unknown),
        ];
        cases.iter().for_each(|(path, reason)| {
            let path = path.replace("{d}", DOC).replace("{p}", PAGE);
            assert_eq!(
                rules().classify(&path).unwrap(),
                Classification::Excluded(*reason),
                "{path}"
            );
            assert!(
                matches!(
                    rules().xochitl_to_repo(&path),
                    Err(ModelError::Excluded { reason: r, .. }) if r == *reason
                ),
                "{path}"
            );
        });
    }

    #[test]
    fn unsafe_tablet_paths_are_rejected() {
        let abs = format!("/{DOC}.metadata");
        let up = format!("../{DOC}.metadata");
        let inner = format!("{DOC}/../{DOC}.metadata");
        let dot = format!("./{DOC}.metadata");
        let double = format!("{DOC}//{PAGE}.rm");
        let trailing = format!("{DOC}/");
        let nul = format!("{DOC}.meta\0data");
        ["", &abs, &up, &inner, &dot, &double, &trailing, &nul]
            .iter()
            .for_each(|path| {
                assert!(
                    matches!(rules().classify(path), Err(ModelError::UnsafePath(_))),
                    "{path:?}"
                );
            });
    }

    #[test]
    fn repo_paths() {
        let r = rules();
        assert_eq!(
            r.parse_repo_path(".rmu/hub.json").unwrap(),
            RepoPath::Rmu("hub.json".to_string())
        );
        // Inside a document but not covered: carried forward, not applied.
        [
            "pages/a/b.rm",
            "pages/a.b.rm",
            "thumbnails/x.png",
            "payload.mobi",
            "pages",
        ]
        .iter()
        .for_each(|name| {
            let path = format!("docs/{DOC}/{name}");
            assert_eq!(
                r.parse_repo_path(&path).unwrap(),
                RepoPath::DocOther {
                    doc: doc(DOC),
                    name: name.to_string()
                }
            );
            assert!(matches!(
                r.repo_to_xochitl(&path),
                Err(ModelError::NotMirrored(_))
            ));
        });
        assert!(matches!(
            r.repo_to_xochitl(".rmu/hub.json"),
            Err(ModelError::NotMirrored(_))
        ));
        // Outside the layout.
        [
            "README.md",
            ".rmu",
            "docs",
            &format!("docs/{DOC}"),
            "other/x",
        ]
        .iter()
        .for_each(|path| {
            assert!(
                matches!(r.parse_repo_path(path), Err(ModelError::OutsideLayout(_))),
                "{path}"
            );
        });
        // Malformed UUIDs and unsafe paths.
        assert!(matches!(
            r.parse_repo_path("docs/not-a-uuid/metadata"),
            Err(ModelError::InvalidDocId(_))
        ));
        [
            format!("docs/{DOC}/../{DOC2}/metadata"),
            format!("/docs/{DOC}/metadata"),
            format!("docs/{DOC}//metadata"),
            format!("docs/{DOC}/pages/./x.rm"),
            "../docs".to_string(),
        ]
        .iter()
        .for_each(|path| {
            assert!(
                matches!(r.parse_repo_path(path), Err(ModelError::UnsafePath(_))),
                "{path}"
            );
            assert!(r.repo_to_xochitl(path).is_err());
        });
    }

    #[test]
    fn narrower_rules_exclude_kinds_they_do_not_mirror() {
        let narrow = rules_without_pdf();
        assert_eq!(
            narrow.classify(&format!("{DOC}.pdf")).unwrap(),
            Classification::Excluded(Exclusion::NotInRules(FileKind::PayloadPdf))
        );
        assert_eq!(
            narrow
                .parse_repo_path(&format!("docs/{DOC}/payload.pdf"))
                .unwrap(),
            RepoPath::DocOther {
                doc: doc(DOC),
                name: "payload.pdf".to_string()
            }
        );
        assert!(narrow.allows(FileKind::PayloadEpub));
        assert!(!narrow.allows(FileKind::PayloadPdf));
    }
}
