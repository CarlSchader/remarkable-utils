//! Pure data model for `rmu hub` (see `docs/hub-design.md`).
//!
//! Everything here is I/O-free, so it is unit-testable without a
//! device or a git repository:
//!
//! - **Layout mapping**: xochitl-relative paths on the tablet
//!   (`<uuid>.metadata`, `<uuid>/<page>.rm`, ...) to repository paths
//!   (`docs/<uuid>/metadata`, `docs/<uuid>/pages/<page>.rm`, ...) and
//!   back, plus classification of everything else on the tablet as
//!   excluded, with a reason.
//! - **Mirror rules** ([`MirrorRules`]), selected by the repository's
//!   mirror-rules version: the allowlist of file kinds and the
//!   view-state, derived and model-specific JSON paths.
//! - **Formats**: `.rmu/hub.json` ([`RepoConfig`]) and the tablet state
//!   ([`TabletState`]), with format versions and refusal of newer ones.
//! - **Normalisation** of `.metadata`/`.content` for "changed?"
//!   comparisons, and the view-state **overlay** used on apply.
//! - **Document grouping** of flat tree listings ([`RepoTree`]).
//! - **Snapshot validation** ([`MirrorRules::validate_snapshot`]) and
//!   **carry-forward** ([`carry_forward`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::git::{self, FileMode, ObjectKind, Oid};
use crate::relative_path::split_safe_relative;
use crate::xochitl::{self, ItemKind};

/// Repository path of the origin's identity and rules file.
pub const REPO_CONFIG_PATH: &str = ".rmu/hub.json";

/// Path of the hub state file on the tablet (outside the xochitl data
/// dir; `/home` survives firmware updates).
pub const TABLET_STATE_PATH: &str = "/home/root/.local/share/rmu/hub.json";

/// Newest mirror-rules version this rmu supports.
pub const SUPPORTED_MIRROR_RULES: u32 = 1;

/// File-name prefix of rmu's temporary files in the data dir (e.g.
/// `write_remote_file`'s `.rmu-upload-XXXXXX`). Never mirrored.
pub const RMU_TEMP_PREFIX: &str = ".rmu-";

/// Longest accepted page id. Real page ids are 36-character UUIDs.
const MAX_PAGE_ID_LEN: usize = 128;

/// Longest accepted device name, in characters.
const MAX_DEVICE_NAME_CHARS: usize = 64;

/// Errors from the hub data model.
#[derive(Debug, Error)]
pub enum ModelError {
    /// A path that is absolute or has empty, `.` or `..` components.
    #[error("unsafe path '{0}': must be relative, without empty, '.' or '..' components")]
    UnsafePath(String),

    /// A document UUID that is not in canonical lowercase form.
    #[error("invalid document UUID '{0}'")]
    InvalidDocId(String),

    /// A page id with characters outside `[A-Za-z0-9_-]`, or empty.
    #[error("invalid page id '{0}'")]
    InvalidPageId(String),

    /// A repo or device id that is not a canonical lowercase UUID.
    #[error("invalid id '{0}': expected a lowercase UUID")]
    InvalidId(String),

    /// A device name that is empty, too long, or has control characters
    /// or angle brackets (git identities cannot contain `<` or `>`).
    #[error(
        "invalid device name {0:?}: must be 1 to 64 characters without control characters, '<' or '>'"
    )]
    InvalidDeviceName(String),

    /// A repository path that is not UTF-8. Never converted lossily: that
    /// would rename a carried-forward path, in effect deleting it.
    #[error("repository path '{}' is not UTF-8", .0.escape_ascii())]
    NonUtf8Path(Vec<u8>),

    /// A tree entry whose mode git does not write for non-tree entries.
    #[error("repository path '{path}' has unsupported mode {mode:06o}")]
    UnsupportedEntryMode { path: String, mode: u32 },

    /// A tree listing that names one path twice (a malformed tree):
    /// keeping either entry would silently drop the other.
    #[error("repository path '{0}' appears twice in one tree")]
    DuplicatePath(String),

    /// A repository path outside `docs/<uuid>/` and `.rmu/`.
    #[error("repository path '{0}' is outside docs/<uuid>/ and .rmu/")]
    OutsideLayout(String),

    /// A repository path that never maps to a tablet file.
    #[error("repository path '{0}' is not mirrored to the tablet")]
    NotMirrored(String),

    /// A tablet path that is not mirrored to the repository.
    #[error("tablet path '{path}' is not mirrored ({reason})")]
    Excluded { path: String, reason: Exclusion },

    /// A state file whose format version this rmu does not support.
    #[error("{what} has format version {found}; this rmu supports version {supported}")]
    UnsupportedFormat {
        what: &'static str,
        found: u64,
        supported: u32,
    },

    /// A repository whose mirror rules are newer than this rmu.
    #[error(
        "repository uses mirror-rules version {found}; this rmu supports 1 to {supported} (upgrade rmu)"
    )]
    UnsupportedRules { found: u32, supported: u32 },

    /// A mirror-rules version that never existed (0).
    #[error("invalid mirror-rules version {0}")]
    InvalidRules(u32),

    /// The snapshot's files for a validated document differ from the
    /// files validation saw: the snapshot is not reliable and must not
    /// be committed.
    #[error(
        "snapshot of document {doc} does not match its validation (missing: {missing:?}, unexpected: {unexpected:?})"
    )]
    SnapshotMismatch {
        doc: DocId,
        missing: Vec<String>,
        unexpected: Vec<String>,
    },

    /// A state file that is not valid JSON of the expected shape.
    #[error("invalid {what}: {source}")]
    Json {
        what: &'static str,
        source: serde_json::Error,
    },
}

type Result<T> = std::result::Result<T, ModelError>;

// ---- ids -------------------------------------------------------------------

fn is_canonical_uuid(s: &str) -> bool {
    uuid::Uuid::try_parse(s).is_ok_and(|u| u.hyphenated().to_string() == s)
}

fn is_page_id(s: &str) -> bool {
    (1..=MAX_PAGE_ID_LEN).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_device_name(s: &str) -> bool {
    (1..=MAX_DEVICE_NAME_CHARS).contains(&s.chars().count())
        && !s.chars().any(|c| c.is_control() || c == '<' || c == '>')
}

fn is_safe_relative_path(s: &str) -> bool {
    checked_components(s).is_ok()
}

/// A string newtype validated on construction and on deserialisation.
macro_rules! validated_id {
    ($(#[$meta:meta])* $name:ident, $valid:path, $err:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap an id.
            pub fn new(id: impl Into<String>) -> Result<Self> {
                let id = id.into();
                if $valid(&id) {
                    Ok(Self(id))
                } else {
                    Err(ModelError::$err(id))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ModelError;

            fn try_from(id: String) -> Result<Self> {
                Self::new(id)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

validated_id!(
    /// A document or folder UUID, in canonical lowercase hyphenated form
    /// (the form xochitl writes). Anything else on the tablet is an
    /// unknown file, never mirrored.
    DocId,
    is_canonical_uuid,
    InvalidDocId
);

validated_id!(
    /// A page id inside a document: `[A-Za-z0-9_-]`, 1 to 128 bytes, so
    /// it can never carry a separator or a `.`/`..` component. Real page
    /// ids are UUIDs.
    PageId,
    is_page_id,
    InvalidPageId
);

validated_id!(
    /// The origin's identity, created by `init` (a random UUID).
    RepoId,
    is_canonical_uuid,
    InvalidId
);

validated_id!(
    /// A tablet's identity, created at clone (a random UUID; never the
    /// serial or hostname). Used in the device ref name.
    DeviceId,
    is_canonical_uuid,
    InvalidId
);

validated_id!(
    /// A tablet's user-chosen name: 1 to 64 characters, no control
    /// characters, no `<` or `>`. It ends up in git identities (which
    /// cannot contain angle brackets), fork names and commit trailers, so
    /// a newline must never get through.
    DeviceName,
    is_device_name,
    InvalidDeviceName
);

validated_id!(
    /// A safe path relative to the xochitl data dir (no absolute path,
    /// no empty, `.` or `..` components): manifest keys can never point
    /// the device layer outside the data dir.
    XochitlPath,
    is_safe_relative_path,
    UnsafePath
);

impl RepoId {
    /// A fresh random repo id.
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl DeviceId {
    /// A fresh random device id.
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

// ---- layout ----------------------------------------------------------------

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
fn checked_components(path: &str) -> Result<Vec<&str>> {
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

// ---- mirror rules ----------------------------------------------------------

/// One segment of a [`JsonPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seg {
    /// An object key.
    Key(&'static str),
    /// Every element of an array. When overlaying, elements are matched
    /// by their `id` field (page entries).
    Each,
}

/// A JSON path such as `cPages.pages[*].scrollTime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonPath(&'static [Seg]);

impl JsonPath {
    pub fn segments(&self) -> &'static [Seg] {
        self.0
    }
}

impl fmt::Display for JsonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .enumerate()
            .try_for_each(|(index, seg)| match seg {
                Seg::Key(key) if index == 0 => f.write_str(key),
                Seg::Key(key) => write!(f, ".{key}"),
                Seg::Each => f.write_str("[*]"),
            })
    }
}

macro_rules! json_path {
    (@seg *) => {
        Seg::Each
    };
    (@seg $key:literal) => {
        Seg::Key($key)
    };
    ($($seg:tt),+ $(,)?) => {
        JsonPath(&[$(json_path!(@seg $seg)),+])
    };
}

/// The two JSON files whose fields the rules distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonFile {
    Metadata,
    Content,
}

/// JSON path lists for `.metadata` and `.content`.
#[derive(Debug, Clone, Copy)]
struct PerFile {
    metadata: &'static [JsonPath],
    content: &'static [JsonPath],
}

impl PerFile {
    fn get(&self, file: JsonFile) -> &'static [JsonPath] {
        match file {
            JsonFile::Metadata => self.metadata,
            JsonFile::Content => self.content,
        }
    }
}

/// What a repository mirrors and how documents are compared, selected
/// by the repository's mirror-rules version (never rmu's newest: see
/// "Rules version" in `docs/hub-design.md`).
#[derive(Debug, Clone, Copy)]
pub struct MirrorRules {
    version: u32,
    kinds: &'static [FileKind],
    view_state: PerFile,
    derived: PerFile,
    model_specific: PerFile,
}

static RULES_V1: MirrorRules = MirrorRules {
    version: 1,
    kinds: &[
        FileKind::Metadata,
        FileKind::Content,
        FileKind::Pagedata,
        FileKind::Template,
        FileKind::PayloadPdf,
        FileKind::PayloadEpub,
        FileKind::Page,
        FileKind::PageMetadata,
        FileKind::Highlights,
    ],
    view_state: PerFile {
        metadata: &[
            json_path!("lastOpened"),
            json_path!("lastOpenedPage"),
            json_path!("new"),
            json_path!("metadatamodified"),
            json_path!("modified"),
            json_path!("synced"),
            json_path!("version"),
        ],
        content: &[
            json_path!("cPages", "lastOpened"),
            json_path!("cPages", "pages", *, "scrollTime"),
            json_path!("cPages", "pages", *, "verticalScroll"),
            json_path!("customZoomCenterX"),
            json_path!("customZoomCenterY"),
            json_path!("customZoomOrientation"),
            json_path!("customZoomPageHeight"),
            json_path!("customZoomPageWidth"),
            json_path!("customZoomScale"),
            json_path!("zoomMode"),
            json_path!("extraMetadata"),
            json_path!("lastOpenedPage"),
            json_path!("transform"),
            json_path!("keyboardMetadata"),
        ],
    },
    derived: PerFile {
        metadata: &[json_path!("lastModified")],
        content: &[
            json_path!("pageCount"),
            json_path!("sizeInBytes"),
            json_path!("cPages", "original"),
            json_path!("documentMetadata"),
            json_path!("cPages", "uuids"),
        ],
    },
    model_specific: PerFile {
        metadata: &[],
        content: &[
            json_path!("customZoomCenterX"),
            json_path!("customZoomCenterY"),
            json_path!("customZoomOrientation"),
            json_path!("customZoomPageHeight"),
            json_path!("customZoomPageWidth"),
            json_path!("customZoomScale"),
        ],
    },
};

impl MirrorRules {
    /// The rules for a repository's mirror-rules version; an error for
    /// versions newer than this rmu supports (or invalid ones).
    pub fn for_version(version: u32) -> Result<&'static MirrorRules> {
        match version {
            0 => Err(ModelError::InvalidRules(0)),
            1 => Ok(&RULES_V1),
            found => Err(ModelError::UnsupportedRules {
                found,
                supported: SUPPORTED_MIRROR_RULES,
            }),
        }
    }

    /// The newest rules, for `init`.
    pub fn latest() -> &'static MirrorRules {
        &RULES_V1
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// Whether files of this kind are mirrored.
    pub fn allows(&self, kind: FileKind) -> bool {
        self.kinds.contains(&kind)
    }

    /// View-state paths: device-local, never a change, overlaid on apply.
    pub fn view_state(&self, file: JsonFile) -> &'static [JsonPath] {
        self.view_state.get(file)
    }

    /// Derived paths: ignored when deciding whether a document changed.
    pub fn derived(&self, file: JsonFile) -> &'static [JsonPath] {
        self.derived.get(file)
    }

    /// Model-specific view state, dropped for documents new to a tablet.
    pub fn model_specific(&self, file: JsonFile) -> &'static [JsonPath] {
        self.model_specific.get(file)
    }

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

    /// The value used for "changed?" comparisons and verification:
    /// `value` without its view-state and derived paths. Everything
    /// else, including unknown fields, is kept.
    pub fn normalise(&self, file: JsonFile, value: &Value) -> Value {
        self.view_state(file).iter().chain(self.derived(file)).fold(
            value.clone(),
            |mut out, path| {
                remove_path(&mut out, path.segments());
                out
            },
        )
    }

    /// The JSON to write on apply. For a document the tablet already
    /// has (`local` = the file re-read while xochitl is stopped), the
    /// tablet keeps its own view state: each view-state path takes
    /// `local`'s value, or is removed when `local` has none; page entries
    /// are matched by `id`, and pages the tablet does not have keep
    /// `target`'s values. For a document new to the tablet (`local` =
    /// `None`), model-specific fields are dropped instead.
    pub fn view_state_for_apply(
        &self,
        file: JsonFile,
        target: &Value,
        local: Option<&Value>,
    ) -> Value {
        match local {
            Some(local) => self
                .view_state(file)
                .iter()
                .fold(target.clone(), |mut out, path| {
                    overlay_path(&mut out, Some(local), path.segments());
                    out
                }),
            None => self
                .model_specific(file)
                .iter()
                .fold(target.clone(), |mut out, path| {
                    remove_path(&mut out, path.segments());
                    out
                }),
        }
    }
}

/// Remove the value at `path` (every array element for [`Seg::Each`]).
fn remove_path(value: &mut Value, path: &[Seg]) {
    match (path, value) {
        ([Seg::Key(key)], Value::Object(map)) => {
            map.remove(*key);
        }
        ([Seg::Key(key), rest @ ..], Value::Object(map)) => {
            if let Some(child) = map.get_mut(*key) {
                remove_path(child, rest);
            }
        }
        ([Seg::Each, rest @ ..], Value::Array(items)) => {
            items.iter_mut().for_each(|item| remove_path(item, rest));
        }
        _ => {}
    }
}

/// Make `target`'s value at `path` equal to `local`'s: copied when
/// `local` has one, removed when it does not. Array elements are
/// matched by `id`; unmatched target elements are left alone.
fn overlay_path(target: &mut Value, local: Option<&Value>, path: &[Seg]) {
    match (path, target) {
        ([Seg::Key(key)], Value::Object(map)) => match local.and_then(|l| l.get(*key)) {
            Some(value) => {
                map.insert((*key).to_string(), value.clone());
            }
            None => {
                map.remove(*key);
            }
        },
        ([Seg::Key(key), rest @ ..], Value::Object(map)) => {
            if let Some(child) = map.get_mut(*key) {
                overlay_path(child, local.and_then(|l| l.get(*key)), rest);
            }
        }
        ([Seg::Each, rest @ ..], Value::Array(items)) => {
            let local_items = local.and_then(Value::as_array);
            items.iter_mut().for_each(|item| {
                let counterpart = item.get("id").cloned().and_then(|id| {
                    local_items?
                        .iter()
                        .find(|candidate| candidate.get("id") == Some(&id))
                });
                if let Some(counterpart) = counterpart {
                    overlay_path(item, Some(counterpart), rest);
                }
            });
        }
        _ => {}
    }
}

// ---- formats ---------------------------------------------------------------

/// Parse a versioned JSON file, refusing any format version but
/// `supported` before looking at the rest.
fn parse_versioned<T: DeserializeOwned>(
    what: &'static str,
    bytes: &[u8],
    supported: u32,
) -> Result<T> {
    #[derive(Deserialize)]
    struct Probe {
        format: u64,
    }
    let json = |source| ModelError::Json { what, source };
    let probe: Probe = serde_json::from_slice(bytes).map_err(json)?;
    if probe.format != u64::from(supported) {
        return Err(ModelError::UnsupportedFormat {
            what,
            found: probe.format,
            supported,
        });
    }
    serde_json::from_slice(bytes).map_err(json)
}

fn to_pretty_json(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).expect("hub state serializes")
}

/// `.rmu/hub.json`: the origin's identity and mirror-rules version.
/// Written only by `init` and an explicit upgrade, never by sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub format: u32,
    pub repo_id: RepoId,
    pub mirror_rules: u32,
    /// Fields this rmu does not know, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl RepoConfig {
    pub const FORMAT: u32 = 1;

    /// A config for a new origin, with the newest rules.
    pub fn new(repo_id: RepoId) -> Self {
        Self {
            format: Self::FORMAT,
            repo_id,
            mirror_rules: MirrorRules::latest().version(),
            extra: BTreeMap::new(),
        }
    }

    /// Parse, refusing a newer format or newer mirror rules.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let config: Self = parse_versioned(REPO_CONFIG_PATH, bytes, Self::FORMAT)?;
        config.rules()?;
        Ok(config)
    }

    pub fn to_json(&self) -> String {
        to_pretty_json(self)
    }

    /// The repository's mirror rules.
    pub fn rules(&self) -> Result<&'static MirrorRules> {
        MirrorRules::for_version(self.mirror_rules)
    }
}

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// A file's stat signature. Times are nanoseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatSignature {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: u64,
}

impl StatSignature {
    /// Whether this stat may be recorded in the manifest (git's remedy
    /// for racy entries). `reference_ns` is the device time of the
    /// xochitl start that follows the stat (for step-5 stats), or the
    /// record time (for stats taken after apply wrote the file).
    ///
    /// A coarse mtime (zero sub-second part) in the same second as the
    /// reference, or later, could hide a change xochitl makes within
    /// that second, so such an entry is left out; a missing entry means
    /// "dirty, read again next run". Nanosecond mtimes, as on the Paper
    /// Pro's ext4, are always kept, so applied files are not downloaded
    /// again on every sync.
    pub fn keep_in_manifest(&self, reference_ns: i64) -> bool {
        let coarse = self.mtime_ns.rem_euclid(NANOS_PER_SEC) == 0;
        !coarse || self.mtime_ns.div_euclid(NANOS_PER_SEC) < reference_ns.div_euclid(NANOS_PER_SEC)
    }
}

/// A manifest entry: the stat signature a file had when its blob was
/// recorded. Serialised compactly as `[size, mtime, ctime, inode, blob]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ManifestTuple", into = "ManifestTuple")]
pub struct ManifestEntry {
    pub stat: StatSignature,
    pub blob: Oid,
}

type ManifestTuple = (u64, i64, i64, u64, Oid);

impl From<ManifestTuple> for ManifestEntry {
    fn from((size, mtime_ns, ctime_ns, inode, blob): ManifestTuple) -> Self {
        Self {
            stat: StatSignature {
                size,
                mtime_ns,
                ctime_ns,
                inode,
            },
            blob,
        }
    }
}

impl From<ManifestEntry> for ManifestTuple {
    fn from(entry: ManifestEntry) -> Self {
        let StatSignature {
            size,
            mtime_ns,
            ctime_ns,
            inode,
        } = entry.stat;
        (size, mtime_ns, ctime_ns, inode, entry.blob)
    }
}

/// Written before every push attempt, cleared once the apply is
/// reconciled (see "Restartability").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub merge: Oid,
    pub snapshot: Oid,
}

/// The hub state stored on the tablet ([`TABLET_STATE_PATH`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabletState {
    pub format: u32,
    /// Must match origin's [`RepoConfig::repo_id`].
    pub repo_id: RepoId,
    /// Default origin URL; overridable per computer.
    pub origin: String,
    pub device_id: DeviceId,
    pub device_name: DeviceName,
    /// Default base commit for every document. `None` is the empty base
    /// (a clone where no document matched a version in origin). The key
    /// is required: a silently missing base would bring back every
    /// deletion.
    #[serde(deserialize_with = "Option::deserialize")]
    pub base: Option<Oid>,
    /// Documents whose base differs from `base`; `None` is the empty
    /// base.
    #[serde(default)]
    pub base_overrides: BTreeMap<DocId, Option<Oid>>,
    /// Device-ref head, written right after each device-ref push.
    #[serde(deserialize_with = "Option::deserialize")]
    pub device_ref: Option<Oid>,
    /// Xochitl-relative path → stat signature and blob at last record.
    /// Racy entries are left out (see [`StatSignature::keep_in_manifest`]).
    #[serde(default)]
    pub manifest: BTreeMap<XochitlPath, ManifestEntry>,
    /// Malformed documents, reported every run.
    #[serde(default)]
    pub quarantine: BTreeSet<DocId>,
    /// Required key: a silently missing journal would skip
    /// reconciliation.
    #[serde(deserialize_with = "Option::deserialize")]
    pub journal: Option<Journal>,
    /// Fields this rmu does not know, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl TabletState {
    pub const FORMAT: u32 = 1;

    /// State for a freshly attached tablet (empty base, no history).
    pub fn new(
        repo_id: RepoId,
        origin: String,
        device_id: DeviceId,
        device_name: DeviceName,
    ) -> Self {
        Self {
            format: Self::FORMAT,
            repo_id,
            origin,
            device_id,
            device_name,
            base: None,
            base_overrides: BTreeMap::new(),
            device_ref: None,
            manifest: BTreeMap::new(),
            quarantine: BTreeSet::new(),
            journal: None,
            extra: BTreeMap::new(),
        }
    }

    /// Parse, refusing a newer format.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        parse_versioned("tablet hub state", bytes, Self::FORMAT)
    }

    pub fn to_json(&self) -> String {
        to_pretty_json(self)
    }

    /// The base commit of one document (`None` = empty base).
    pub fn base_for(&self, doc: &DocId) -> Option<&Oid> {
        self.base_overrides
            .get(doc)
            .map_or(self.base.as_ref(), Option::as_ref)
    }
}

// ---- trees and documents ---------------------------------------------------

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

/// Build a [`TreeListing`] from a raw recursive listing
/// ([`git::Repo::ls_tree_recursive`]). This is the one place where git's
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

// ---- snapshot validation ---------------------------------------------------

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
    use serde_json::json;

    // Fabricated ids, in the shapes xochitl writes.
    const DOC: &str = "8c1f0b9e-0000-4000-8000-000000000001";
    const DOC2: &str = "8c1f0b9e-0000-4000-8000-000000000002";
    const FOLDER: &str = "8c1f0b9e-0000-4000-8000-00000000000f";
    const PAGE: &str = "8c1f0b9e-0000-4000-8000-0000000000a1";
    const PAGE2: &str = "8c1f0b9e-0000-4000-8000-0000000000a2";
    const REPO: &str = "0f0e0d0c-0000-4000-8000-00000000b001";
    const DEVICE: &str = "0f0e0d0c-0000-4000-8000-00000000c001";

    fn rules() -> &'static MirrorRules {
        MirrorRules::for_version(1).unwrap()
    }

    fn doc(uuid: &str) -> DocId {
        DocId::new(uuid).unwrap()
    }

    fn page(id: &str) -> PageId {
        PageId::new(id).unwrap()
    }

    fn oid(n: u32) -> Oid {
        format!("{n:040x}").parse().unwrap()
    }

    fn mirrored(uuid: &str, file: DocFile) -> MirroredFile {
        MirroredFile {
            doc: doc(uuid),
            file,
        }
    }

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

    /// The v1 rules without PDF payloads, standing in for an older,
    /// narrower rule set.
    fn rules_without_pdf() -> MirrorRules {
        static KINDS: [FileKind; 8] = [
            FileKind::Metadata,
            FileKind::Content,
            FileKind::Pagedata,
            FileKind::Template,
            FileKind::PayloadEpub,
            FileKind::Page,
            FileKind::PageMetadata,
            FileKind::Highlights,
        ];
        MirrorRules {
            kinds: &KINDS,
            ..*rules()
        }
    }

    // ---- ids ---------------------------------------------------------------

    #[test]
    fn doc_ids_must_be_canonical_lowercase_uuids() {
        assert!(DocId::new(DOC).is_ok());
        [
            "",
            "not-a-uuid",
            "8C1F0B9E-0000-4000-8000-000000000001",
            "8c1f0b9e000040008000000000000001",
            "{8c1f0b9e-0000-4000-8000-000000000001}",
            "urn:uuid:8c1f0b9e-0000-4000-8000-000000000001",
            "8c1f0b9e-0000-4000-8000-000000000001/..",
            "../8c1f0b9e-0000-4000-8000-000000000001",
        ]
        .iter()
        .for_each(|bad| {
            assert!(
                matches!(DocId::new(*bad), Err(ModelError::InvalidDocId(_))),
                "{bad}"
            )
        });
    }

    #[test]
    fn page_ids_reject_separators_and_dots() {
        assert!(PageId::new(PAGE).is_ok());
        assert!(PageId::new("page_1-x").is_ok());
        [
            "",
            ".",
            "..",
            "a/b",
            "a.b",
            "a\\b",
            "a b",
            "a\0b",
            &"x".repeat(129),
        ]
        .iter()
        .for_each(|bad| {
            assert!(
                matches!(PageId::new(*bad), Err(ModelError::InvalidPageId(_))),
                "{bad}"
            )
        });
    }

    #[test]
    fn object_ids_are_lowercase_sha1_or_sha256_hex() {
        assert!("a".repeat(40).parse::<Oid>().is_ok());
        assert!("0123456789abcdef".repeat(4).parse::<Oid>().is_ok());
        [
            "",
            &"a".repeat(39),
            &"a".repeat(41),
            &"A".repeat(40),
            &"g".repeat(40),
        ]
        .iter()
        .for_each(|bad| {
            assert!(
                matches!(
                    bad.parse::<Oid>(),
                    Err(crate::error::Error::InvalidObjectId(_))
                ),
                "{bad}"
            )
        });
    }

    #[test]
    fn repo_and_device_ids() {
        assert!(RepoId::new(REPO).is_ok());
        assert!(DeviceId::new(DEVICE).is_ok());
        assert!(matches!(
            DeviceId::new("my-tablet"),
            Err(ModelError::InvalidId(_))
        ));
        let random = DeviceId::random();
        assert!(DeviceId::new(random.as_str()).is_ok());
        assert_ne!(RepoId::random(), RepoId::random());
    }

    // ---- layout ------------------------------------------------------------

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

    // ---- rules -------------------------------------------------------------

    #[test]
    fn rules_versions() {
        assert_eq!(rules().version(), 1);
        assert_eq!(MirrorRules::latest().version(), SUPPORTED_MIRROR_RULES);
        assert!(matches!(
            MirrorRules::for_version(0),
            Err(ModelError::InvalidRules(0))
        ));
        [2, u32::MAX].iter().for_each(|version| {
            assert!(matches!(
                MirrorRules::for_version(*version),
                Err(ModelError::UnsupportedRules { found, supported: 1 }) if found == *version
            ));
        });
        let v1 = rules();
        [
            FileKind::Metadata,
            FileKind::Content,
            FileKind::Pagedata,
            FileKind::Template,
            FileKind::PayloadPdf,
            FileKind::PayloadEpub,
            FileKind::Page,
            FileKind::PageMetadata,
            FileKind::Highlights,
        ]
        .iter()
        .for_each(|kind| assert!(v1.allows(*kind)));
    }

    #[test]
    fn json_path_lists_match_the_design() {
        let names =
            |paths: &[JsonPath]| -> Vec<String> { paths.iter().map(ToString::to_string).collect() };
        let r = rules();
        assert_eq!(
            names(r.view_state(JsonFile::Metadata)),
            [
                "lastOpened",
                "lastOpenedPage",
                "new",
                "metadatamodified",
                "modified",
                "synced",
                "version"
            ]
        );
        assert_eq!(names(r.derived(JsonFile::Metadata)), ["lastModified"]);
        assert_eq!(
            names(r.view_state(JsonFile::Content)),
            [
                "cPages.lastOpened",
                "cPages.pages[*].scrollTime",
                "cPages.pages[*].verticalScroll",
                "customZoomCenterX",
                "customZoomCenterY",
                "customZoomOrientation",
                "customZoomPageHeight",
                "customZoomPageWidth",
                "customZoomScale",
                "zoomMode",
                "extraMetadata",
                "lastOpenedPage",
                "transform",
                "keyboardMetadata"
            ]
        );
        assert_eq!(
            names(r.derived(JsonFile::Content)),
            [
                "pageCount",
                "sizeInBytes",
                "cPages.original",
                "documentMetadata",
                "cPages.uuids"
            ]
        );
        // Model-specific fields are all view state, and all customZoom*.
        let view = names(r.view_state(JsonFile::Content));
        names(r.model_specific(JsonFile::Content))
            .iter()
            .for_each(|path| {
                assert!(path.starts_with("customZoom"));
                assert!(view.contains(path));
            });
        assert_eq!(r.model_specific(JsonFile::Content).len(), 6);
        assert!(r.model_specific(JsonFile::Metadata).is_empty());
    }

    // ---- formats -----------------------------------------------------------

    #[test]
    fn repo_config_round_trip() {
        let config = RepoConfig::new(RepoId::new(REPO).unwrap());
        assert_eq!(config.format, 1);
        assert_eq!(config.mirror_rules, 1);
        let json = config.to_json();
        assert_eq!(RepoConfig::parse(json.as_bytes()).unwrap(), config);
        assert_eq!(config.rules().unwrap().version(), 1);
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value,
            json!({ "format": 1, "repo_id": REPO, "mirror_rules": 1 })
        );
    }

    #[test]
    fn repo_config_refuses_newer_versions() {
        let newer_format = json!({ "format": 2, "repo_id": REPO, "mirror_rules": 1 });
        assert!(matches!(
            RepoConfig::parse(newer_format.to_string().as_bytes()),
            Err(ModelError::UnsupportedFormat {
                found: 2,
                supported: 1,
                ..
            })
        ));
        // A newer format is refused before its contents are looked at.
        let newer_format_new_shape = json!({ "format": 7, "library": {} });
        assert!(matches!(
            RepoConfig::parse(newer_format_new_shape.to_string().as_bytes()),
            Err(ModelError::UnsupportedFormat { found: 7, .. })
        ));
        let newer_rules = json!({ "format": 1, "repo_id": REPO, "mirror_rules": 2 });
        assert!(matches!(
            RepoConfig::parse(newer_rules.to_string().as_bytes()),
            Err(ModelError::UnsupportedRules {
                found: 2,
                supported: 1
            })
        ));
        let zero = json!({ "format": 0, "repo_id": REPO, "mirror_rules": 1 });
        assert!(matches!(
            RepoConfig::parse(zero.to_string().as_bytes()),
            Err(ModelError::UnsupportedFormat { found: 0, .. })
        ));
    }

    #[test]
    fn repo_config_rejects_malformed_input() {
        [
            json!({ "repo_id": REPO, "mirror_rules": 1 }).to_string(),
            json!({ "format": 1, "repo_id": "nope", "mirror_rules": 1 }).to_string(),
            json!({ "format": 1, "mirror_rules": 1 }).to_string(),
            json!({ "format": 1, "repo_id": REPO }).to_string(),
            json!({ "format": "1", "repo_id": REPO, "mirror_rules": 1 }).to_string(),
            "not json".to_string(),
        ]
        .iter()
        .for_each(|input| {
            assert!(
                matches!(
                    RepoConfig::parse(input.as_bytes()),
                    Err(ModelError::Json { .. })
                ),
                "{input}"
            );
        });
    }

    #[test]
    fn repo_config_preserves_unknown_fields() {
        let input = json!({
            "format": 1, "repo_id": REPO, "mirror_rules": 1,
            "future": { "x": [1, 2] }
        });
        let config = RepoConfig::parse(input.to_string().as_bytes()).unwrap();
        assert_eq!(config.extra["future"], json!({ "x": [1, 2] }));
        let back: Value = serde_json::from_str(&config.to_json()).unwrap();
        assert_eq!(back, input);
    }

    fn full_state_json() -> Value {
        json!({
            "format": 1,
            "repo_id": REPO,
            "origin": "ssh://library.example/srv/library.git",
            "device_id": DEVICE,
            "device_name": "Paper Pro c001",
            "base": oid(1).as_str(),
            "base_overrides": { DOC: oid(2).as_str(), DOC2: null },
            "device_ref": oid(3).as_str(),
            "manifest": {
                format!("{DOC}.metadata"): [312, 1767225600123456789_i64, 1767225600987654321_i64, 131074, oid(4).as_str()],
                format!("{DOC}/{PAGE}.rm"): [0, -1, 0, 18446744073709551615_u64, oid(5).as_str()],
            },
            "quarantine": [DOC2],
            "journal": { "merge": oid(6).as_str(), "snapshot": oid(7).as_str() },
        })
    }

    #[test]
    fn tablet_state_round_trip() {
        let input = full_state_json();
        let state = TabletState::parse(input.to_string().as_bytes()).unwrap();
        assert_eq!(state.device_name.as_str(), "Paper Pro c001");
        assert_eq!(
            state.manifest[&XochitlPath::new(format!("{DOC}.metadata")).unwrap()],
            ManifestEntry {
                stat: StatSignature {
                    size: 312,
                    mtime_ns: 1767225600123456789,
                    ctime_ns: 1767225600987654321,
                    inode: 131074,
                },
                blob: oid(4),
            }
        );
        assert_eq!(
            state.manifest[&XochitlPath::new(format!("{DOC}/{PAGE}.rm")).unwrap()]
                .stat
                .inode,
            u64::MAX
        );
        assert_eq!(
            state.journal,
            Some(Journal {
                merge: oid(6),
                snapshot: oid(7)
            })
        );
        assert!(state.quarantine.contains(&doc(DOC2)));
        let back: Value = serde_json::from_str(&state.to_json()).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn tablet_state_new_is_minimal() {
        let state = TabletState::new(
            RepoId::new(REPO).unwrap(),
            "ssh://library.example/srv/library.git".to_string(),
            DeviceId::new(DEVICE).unwrap(),
            DeviceName::new("tablet").unwrap(),
        );
        let json = state.to_json();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["base"], Value::Null);
        assert_eq!(value["journal"], Value::Null);
        assert_eq!(value["manifest"], json!({}));
        assert_eq!(TabletState::parse(json.as_bytes()).unwrap(), state);
        // Empty collections may be omitted.
        let sparse = json!({
            "format": 1, "repo_id": REPO, "origin": "o", "device_id": DEVICE,
            "device_name": "tablet", "base": null, "device_ref": null, "journal": null,
        });
        let parsed = TabletState::parse(sparse.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.base, None);
        assert!(parsed.manifest.is_empty() && parsed.quarantine.is_empty());
        assert!(parsed.base_overrides.is_empty());
    }

    #[test]
    fn tablet_state_requires_nullable_keys() {
        // A missing base would read as "empty base" and bring back every
        // deletion; a missing journal would skip reconciliation.
        ["base", "device_ref", "journal"].iter().for_each(|key| {
            let mut input = full_state_json();
            input.as_object_mut().unwrap().remove(*key);
            assert!(
                matches!(
                    TabletState::parse(input.to_string().as_bytes()),
                    Err(ModelError::Json { .. })
                ),
                "{key}"
            );
            input[*key] = Value::Null;
            assert!(TabletState::parse(input.to_string().as_bytes()).is_ok());
        });
    }

    #[test]
    fn base_for_honours_overrides_including_empty_base() {
        let state = TabletState::parse(full_state_json().to_string().as_bytes()).unwrap();
        assert_eq!(state.base_for(&doc(DOC)), Some(&oid(2)));
        assert_eq!(state.base_for(&doc(DOC2)), None);
        assert_eq!(state.base_for(&doc(FOLDER)), Some(&oid(1)));
        let fresh = TabletState {
            base: None,
            ..state
        };
        assert_eq!(fresh.base_for(&doc(FOLDER)), None);
    }

    #[test]
    fn tablet_state_refuses_newer_format_and_bad_values() {
        let mut newer = full_state_json();
        newer["format"] = json!(2);
        assert!(matches!(
            TabletState::parse(newer.to_string().as_bytes()),
            Err(ModelError::UnsupportedFormat { found: 2, .. })
        ));

        let bad_values = [
            ("base", json!("HEAD")),
            ("device_ref", json!("ABCDEF")),
            ("device_id", json!("serial-123")),
            ("base_overrides", json!({ "not-a-uuid": null })),
            ("quarantine", json!(["../x"])),
            ("journal", json!({ "merge": oid(1).as_str() })),
            ("manifest", json!({ "x": [1, 2, 3, oid(1).as_str()] })),
            ("manifest", json!({ "x": [-1, 2, 3, 4, oid(1).as_str()] })),
            (
                "manifest",
                json!({ "../../etc/shadow": [1, 2, 3, 4, oid(1).as_str()] }),
            ),
            ("manifest", json!({ "/abs": [1, 2, 3, 4, oid(1).as_str()] })),
            ("device_name", json!("evil\nRmu-Consent: yes")),
            ("device_name", json!("")),
            ("device_name", json!("x".repeat(65))),
        ];
        bad_values.iter().for_each(|(field, value)| {
            let mut input = full_state_json();
            input[*field] = value.clone();
            assert!(
                matches!(
                    TabletState::parse(input.to_string().as_bytes()),
                    Err(ModelError::Json { .. })
                ),
                "{field}: {value}"
            );
        });
    }

    #[test]
    fn tablet_state_preserves_unknown_fields() {
        let mut input = full_state_json();
        input["future_note"] = json!({ "kept": [1, 2] });
        let state = TabletState::parse(input.to_string().as_bytes()).unwrap();
        let back: Value = serde_json::from_str(&state.to_json()).unwrap();
        assert_eq!(back, input);
    }

    // ---- normalisation and overlay -----------------------------------------

    fn sample_metadata() -> Value {
        json!({
            "createdTime": "1767225600000",
            "lastModified": "1767312000000",
            "lastOpened": "1767398400000",
            "lastOpenedPage": 4,
            "new": false,
            "parent": "",
            "pinned": false,
            "source": "",
            "type": "DocumentType",
            "visibleName": "Example notebook",
            "futureField": { "kept": true }
        })
    }

    fn sample_content() -> Value {
        json!({
            "cPages": {
                "lastOpened": { "timestamp": "1:12", "value": PAGE2 },
                "original": { "timestamp": "1:1", "value": 2 },
                "pages": [
                    { "id": PAGE,
                      "idx": { "timestamp": "1:2", "value": "ba" },
                      "redir": { "timestamp": "1:2", "value": 0 } },
                    { "id": "8c1f0b9e-0000-4000-8000-0000000000a3",
                      "idx": { "timestamp": "1:5", "value": "bam" },
                      "modifed": "1767312000000",
                      "template": { "timestamp": "1:6", "value": "Blank" } },
                    { "id": PAGE2,
                      "idx": { "timestamp": "1:2", "value": "bb" },
                      "redir": { "timestamp": "1:2", "value": 1 },
                      "scrollTime": { "timestamp": "1:11", "value": "2026-01-02T00:00:00Z" },
                      "verticalScroll": { "timestamp": "1:11", "value": 0 } }
                ],
                "uuids": [ { "first": "0f0e0d0c-0000-4000-8000-00000000a001", "second": 1 } ],
                "futureRegister": { "timestamp": "1:3", "value": 1 }
            },
            "coverPageNumber": 0,
            "customZoomCenterX": 0, "customZoomCenterY": 936,
            "customZoomOrientation": "portrait",
            "customZoomPageHeight": 1872, "customZoomPageWidth": 1404,
            "customZoomScale": 1,
            "documentMetadata": { "title": "Example", "authors": ["A. Author"] },
            "extraMetadata": { "LastPen": "Ballpointv2", "LastActiveTool": "primary" },
            "fileType": "pdf",
            "fontName": "", "lineHeight": -1, "margins": 100,
            "formatVersion": 2,
            "orientation": "portrait",
            "pageCount": 3,
            "pageTags": [], "tags": [],
            "sizeInBytes": "1048576",
            "textAlignment": "left", "textScale": 1,
            "zoomMode": "bestFit"
        })
    }

    #[test]
    fn normalise_metadata_drops_view_state_and_derived_fields() {
        let normal = rules().normalise(JsonFile::Metadata, &sample_metadata());
        assert_eq!(
            normal,
            json!({
                "createdTime": "1767225600000",
                "parent": "",
                "pinned": false,
                "source": "",
                "type": "DocumentType",
                "visibleName": "Example notebook",
                "futureField": { "kept": true }
            })
        );
        // Older-firmware/rmu folder bookkeeping is view state too.
        let folder = json!({
            "createdTime": 1, "lastModified": 2, "metadatamodified": false,
            "modified": false, "parent": "", "pinned": false, "synced": false,
            "type": "CollectionType", "version": 0, "visibleName": "Books"
        });
        assert_eq!(
            rules().normalise(JsonFile::Metadata, &folder),
            json!({ "createdTime": 1, "parent": "", "pinned": false,
                    "type": "CollectionType", "visibleName": "Books" })
        );
    }

    #[test]
    fn normalise_content_drops_view_state_and_derived_fields() {
        let normal = rules().normalise(JsonFile::Content, &sample_content());
        assert_eq!(
            normal,
            json!({
                "cPages": {
                    "pages": [
                        { "id": PAGE,
                          "idx": { "timestamp": "1:2", "value": "ba" },
                          "redir": { "timestamp": "1:2", "value": 0 } },
                        { "id": "8c1f0b9e-0000-4000-8000-0000000000a3",
                          "idx": { "timestamp": "1:5", "value": "bam" },
                          "modifed": "1767312000000",
                          "template": { "timestamp": "1:6", "value": "Blank" } },
                        { "id": PAGE2,
                          "idx": { "timestamp": "1:2", "value": "bb" },
                          "redir": { "timestamp": "1:2", "value": 1 } }
                    ],
                    "futureRegister": { "timestamp": "1:3", "value": 1 }
                },
                "coverPageNumber": 0,
                "fileType": "pdf",
                "fontName": "", "lineHeight": -1, "margins": 100,
                "formatVersion": 2,
                "orientation": "portrait",
                "pageTags": [], "tags": [],
                "textAlignment": "left", "textScale": 1
            })
        );
        // Idempotent.
        assert_eq!(rules().normalise(JsonFile::Content, &normal), normal);
    }

    #[test]
    fn reading_is_not_a_change_but_ink_is() {
        let r = rules();
        let base = sample_content();
        let mut read = base.clone();
        read["cPages"]["lastOpened"] = json!({ "timestamp": "1:40", "value": PAGE });
        read["cPages"]["pages"][2]["scrollTime"]["value"] = json!("2026-02-01T00:00:00Z");
        read["cPages"]["pages"][0]["verticalScroll"] = json!({ "timestamp": "1:41", "value": 12 });
        read["extraMetadata"]["LastPen"] = json!("Fineliner");
        read["sizeInBytes"] = json!("2000000");
        read["lastOpenedPage"] = json!(2);
        assert_eq!(
            r.normalise(JsonFile::Content, &read),
            r.normalise(JsonFile::Content, &base)
        );

        let mut inked = base.clone();
        inked["cPages"]["pages"][1]["modifed"] = json!("1767400000000");
        assert_ne!(
            r.normalise(JsonFile::Content, &inked),
            r.normalise(JsonFile::Content, &base)
        );
        let mut tagged = base.clone();
        tagged["tags"] = json!(["work"]);
        assert_ne!(
            r.normalise(JsonFile::Content, &tagged),
            r.normalise(JsonFile::Content, &base)
        );

        let mut opened = sample_metadata();
        opened["lastOpened"] = json!("1767500000000");
        opened["lastModified"] = json!(1767500000000_i64);
        opened["new"] = json!(true);
        assert_eq!(
            r.normalise(JsonFile::Metadata, &opened),
            r.normalise(JsonFile::Metadata, &sample_metadata())
        );
        let mut renamed = sample_metadata();
        renamed["visibleName"] = json!("Renamed");
        assert_ne!(
            r.normalise(JsonFile::Metadata, &renamed),
            r.normalise(JsonFile::Metadata, &sample_metadata())
        );
    }

    #[test]
    fn normalise_tolerates_other_shapes() {
        let r = rules();
        // Folder .content and the old flat page list are left alone.
        assert_eq!(r.normalise(JsonFile::Content, &json!([])), json!([]));
        let old = json!({ "fileType": "", "pages": [PAGE, PAGE2], "pageCount": 2 });
        assert_eq!(
            r.normalise(JsonFile::Content, &old),
            json!({ "fileType": "", "pages": [PAGE, PAGE2] })
        );
        // Unexpected types under known keys do not panic.
        let odd = json!({ "cPages": { "pages": "nope", "lastOpened": 3 }, "extraMetadata": 1 });
        assert_eq!(
            r.normalise(JsonFile::Content, &odd),
            json!({ "cPages": { "pages": "nope" } })
        );
        assert_eq!(r.normalise(JsonFile::Metadata, &json!(null)), json!(null));
    }

    #[test]
    fn overlay_keeps_the_tablets_view_state() {
        let r = rules();
        // Origin's merged version: another tablet read it elsewhere, and
        // a page was inserted.
        let mut target = sample_content();
        target["cPages"]["lastOpened"] = json!({ "timestamp": "1:90", "value": PAGE });
        target["cPages"]["pages"][0]["scrollTime"] =
            json!({ "timestamp": "1:91", "value": "2026-03-01T00:00:00Z" });
        target["cPages"]["pages"][2]["verticalScroll"] =
            json!({ "timestamp": "1:92", "value": 99 });
        target["cPages"]["pages"].as_array_mut().unwrap().push(
            json!({ "id": "8c1f0b9e-0000-4000-8000-0000000000a4",
                          "idx": { "timestamp": "1:93", "value": "bc" },
                          "scrollTime": { "timestamp": "1:94", "value": "2026-03-02T00:00:00Z" } }),
        );
        target["zoomMode"] = json!("fitWidth");
        target["extraMetadata"] = json!({ "LastPen": "Fineliner" });
        target["tags"] = json!(["merged"]);
        // The tablet's own version: no lastOpenedPage, no zoomMode.
        let mut local = sample_content();
        local.as_object_mut().unwrap().remove("zoomMode");
        local["cPages"]["pages"][0]["verticalScroll"] = json!({ "timestamp": "1:20", "value": 5 });

        let applied = r.view_state_for_apply(JsonFile::Content, &target, Some(&local));
        let pages = applied["cPages"]["pages"].as_array().unwrap();
        // Tablet's values win, matched by page id...
        assert_eq!(
            applied["cPages"]["lastOpened"],
            local["cPages"]["lastOpened"]
        );
        assert_eq!(pages[0].get("scrollTime"), None);
        assert_eq!(pages[0]["verticalScroll"]["value"], json!(5));
        assert_eq!(pages[2]["verticalScroll"]["value"], json!(0));
        assert_eq!(
            pages[2]["scrollTime"],
            local["cPages"]["pages"][2]["scrollTime"]
        );
        // ...a page the tablet does not have keeps origin's values...
        assert_eq!(
            pages[3]["scrollTime"]["value"],
            json!("2026-03-02T00:00:00Z")
        );
        // ...view state the tablet lacks is removed...
        assert_eq!(applied.get("zoomMode"), None);
        assert_eq!(applied["extraMetadata"], local["extraMetadata"]);
        assert_eq!(applied["customZoomCenterY"], json!(936));
        // ...and everything else is origin's.
        assert_eq!(applied["tags"], json!(["merged"]));
        assert_eq!(pages.len(), 4);
        assert_eq!(
            r.normalise(JsonFile::Content, &applied),
            r.normalise(JsonFile::Content, &target)
        );
    }

    #[test]
    fn overlay_metadata() {
        let r = rules();
        let mut target = sample_metadata();
        target["visibleName"] = json!("Renamed elsewhere");
        target["lastOpened"] = json!("1767999999999");
        let mut local = sample_metadata();
        local["new"] = json!(true);
        local["lastOpened"] = json!("0");
        local["synced"] = json!(true);
        let applied = r.view_state_for_apply(JsonFile::Metadata, &target, Some(&local));
        assert_eq!(applied["visibleName"], json!("Renamed elsewhere"));
        assert_eq!(applied["new"], json!(true));
        assert_eq!(applied["lastOpened"], json!("0"));
        assert_eq!(applied["synced"], json!(true));
        assert_eq!(applied["lastModified"], target["lastModified"]);
        assert_eq!(applied["futureField"], json!({ "kept": true }));
    }

    #[test]
    fn new_documents_drop_model_specific_fields_only() {
        let r = rules();
        let target = sample_content();
        let applied = r.view_state_for_apply(JsonFile::Content, &target, None);
        let mut expected = target.clone();
        [
            "customZoomCenterX",
            "customZoomCenterY",
            "customZoomOrientation",
            "customZoomPageHeight",
            "customZoomPageWidth",
            "customZoomScale",
        ]
        .iter()
        .for_each(|key| {
            expected.as_object_mut().unwrap().remove(*key);
        });
        assert_eq!(applied, expected);
        assert_eq!(applied["zoomMode"], json!("bestFit"));
        assert_eq!(
            r.view_state_for_apply(JsonFile::Metadata, &sample_metadata(), None),
            sample_metadata()
        );
    }

    // ---- grouping ----------------------------------------------------------

    fn listing(entries: &[(&str, u32)]) -> TreeListing {
        entries
            .iter()
            .map(|(path, n)| {
                (
                    path.replace("{d}", DOC)
                        .replace("{d2}", DOC2)
                        .replace("{f}", FOLDER)
                        .replace("{p}", PAGE),
                    ListingEntry::regular(oid(*n)),
                )
            })
            .collect()
    }

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

    // ---- snapshot validation -----------------------------------------------

    fn stat(size: u64) -> StatSignature {
        StatSignature {
            size,
            mtime_ns: 1_767_225_600_000_000_000,
            ctime_ns: 1_767_225_600_000_000_000,
            inode: 1,
        }
    }

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

    // ---- review follow-ups ------------------------------------------------

    #[test]
    fn device_names() {
        [
            "Paper Pro c001",
            "Ünïcödé tablet",
            &"x".repeat(64),
            &"é".repeat(64),
        ]
        .iter()
        .for_each(|name| assert!(DeviceName::new(*name).is_ok(), "{name}"));
        [
            "",
            "a\nb",
            "a\rb",
            "tab\there",
            "nul\0",
            "esc\u{1b}[0m",
            &"x".repeat(65),
            // Git identities cannot contain angle brackets.
            "Evil <evil@example.invalid>",
            "a<b",
            "a>b",
        ]
        .iter()
        .for_each(|name| {
            assert!(
                matches!(
                    DeviceName::new(*name),
                    Err(ModelError::InvalidDeviceName(_))
                ),
                "{name:?}"
            )
        });
    }

    #[test]
    fn xochitl_paths_must_be_safe() {
        [
            format!("{DOC}.metadata"),
            format!("{DOC}/{PAGE}.rm"),
            ".tree".to_string(),
        ]
        .iter()
        .for_each(|path| assert!(XochitlPath::new(path.as_str()).is_ok(), "{path}"));
        ["", "/abs", "../x", "a/../b", "a//b", "a/", "./a"]
            .iter()
            .for_each(|path| {
                assert!(
                    matches!(XochitlPath::new(*path), Err(ModelError::UnsafePath(_))),
                    "{path}"
                )
            });
    }

    fn mtime(secs: i64, nanos: i64) -> StatSignature {
        StatSignature {
            mtime_ns: secs * 1_000_000_000 + nanos,
            ..stat(10)
        }
    }

    #[test]
    fn racy_coarse_mtimes_are_left_out_of_the_manifest() {
        let reference = 1_767_225_600 * 1_000_000_000 + 250_000_000;
        // Coarse mtime in an earlier second: kept.
        assert!(mtime(1_767_225_599, 0).keep_in_manifest(reference));
        // Coarse mtime in the reference's second, or later: left out.
        assert!(!mtime(1_767_225_600, 0).keep_in_manifest(reference));
        assert!(!mtime(1_767_225_601, 0).keep_in_manifest(reference));
        // Reference exactly on a second boundary.
        assert!(!mtime(1_767_225_600, 0).keep_in_manifest(1_767_225_600 * 1_000_000_000));
        // Fine-grained mtimes are always kept, even after the reference.
        assert!(mtime(1_767_225_600, 1).keep_in_manifest(reference));
        assert!(mtime(1_767_225_700, 999_999_999).keep_in_manifest(reference));
        // Pre-epoch times use floor division.
        assert!(!mtime(-1, 0).keep_in_manifest(-1));
        assert!(!mtime(-1, 0).keep_in_manifest(-999_999_999));
        assert!(mtime(-2, 0).keep_in_manifest(-1));
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

    #[test]
    fn messages() {
        assert_eq!(
            Exclusion::NotInRules(FileKind::PayloadPdf).to_string(),
            ".pdf payload files are not mirrored by these rules"
        );
        assert_eq!(
            ModelError::InvalidRules(0).to_string(),
            "invalid mirror-rules version 0"
        );
        assert!(
            ModelError::UnsupportedRules {
                found: 2,
                supported: 1
            }
            .to_string()
            .contains("upgrade rmu")
        );
        assert_eq!(
            QuarantineReason::InvalidMetadata("not a JSON object".to_string()).to_string(),
            "invalid .metadata: not a JSON object"
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
