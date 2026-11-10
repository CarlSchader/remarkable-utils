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
//!
//! The submodules hold one concern each: `ids` (validated ids and
//! names), `layout` (mapping and classification), `rules` (mirror rules,
//! normalisation, overlay), `state` (the formats), `grouping` (trees and
//! documents) and `snapshot` (validation, carry-forward). Their public
//! items are re-exported here; use them through `hub::model`.

mod grouping;
mod ids;
mod layout;
mod rules;
mod snapshot;
mod state;

#[cfg(test)]
mod fixtures;

use thiserror::Error;

pub use self::grouping::{DocVersion, ListingEntry, RepoTree, TreeListing, tree_listing};
pub use self::ids::{DeviceId, DeviceName, DocId, PageId, RepoId, XochitlPath};
pub use self::layout::{
    Classification, DocFile, Exclusion, FileKind, MirroredFile, RMU_TEMP_PREFIX, RepoPath,
};
pub use self::rules::{JsonFile, JsonPath, MirrorRules, SUPPORTED_MIRROR_RULES, Seg};
pub use self::snapshot::{
    DocOutcome, ParseResult, QuarantineReason, SnapshotFiles, SnapshotValidation, carry_forward,
};
pub use self::state::{
    Journal, ManifestEntry, REPO_CONFIG_PATH, RepoConfig, StatSignature, TABLET_STATE_PATH,
    TabletState,
};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
