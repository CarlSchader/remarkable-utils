//! Typed errors for reMarkable operations.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Errors produced by SSH transport and logical-tree operations.
#[derive(Debug, Error)]
pub enum Error {
    /// The remote command ran but exited non-zero.
    #[error("remote command exited with status {status}: {stderr}")]
    Remote { status: i32, stderr: String },

    /// Local I/O failure (spawning ssh, reading/writing local files).
    #[error(transparent)]
    Io(#[from] io::Error),

    /// A JSON document on the device (or one we produced) failed to
    /// parse or serialize.
    #[error("invalid JSON in {path}: {source}")]
    Json {
        path: String,
        source: serde_json::Error,
    },

    /// Metadata file exists but is not a JSON object.
    #[error("invalid metadata for {0}: not a JSON object")]
    InvalidMetadata(String),

    /// A logical path did not resolve to an item.
    #[error("path not found: {0}")]
    PathNotFound(String),

    /// A path segment matched more than one item; refusing to guess.
    #[error("ambiguous path segment '{segment}' in '{path}'")]
    AmbiguousPath { segment: String, path: String },

    /// Creating/moving/renaming would collide with an existing sibling.
    #[error("an item named '{name}' already exists in '{parent}'")]
    NameConflict { name: String, parent: String },

    /// A folder was required but the reference resolved to a document.
    #[error("not a folder: {0}")]
    NotAFolder(String),

    /// A document was required but the reference resolved to a folder.
    #[error("not a document: {0}")]
    NotADocument(String),

    /// Refusing to delete a non-empty folder without `recursive`.
    #[error("folder is not empty (use recursive delete)")]
    FolderNotEmpty,

    /// Upload of a file type the device cannot render.
    #[error("unsupported file type '{0}': only .pdf and .epub are supported")]
    UnsupportedFileType(String),

    /// The root pseudo-folder is not a valid target here.
    #[error("root is not a valid target for this operation")]
    RootTarget,

    /// An empty folder path was supplied.
    #[error("folder path is empty")]
    EmptyPath,

    /// An invalid visible name was supplied.
    #[error("invalid name: {0}")]
    InvalidName(String),

    /// A local file to upload does not exist.
    #[error("file not found: {}", .0.display())]
    FileNotFound(PathBuf),

    /// Attempted to move an item into itself.
    #[error("cannot move an item into itself")]
    MoveIntoSelf,

    /// Attempted to move a folder into one of its own descendants.
    #[error("cannot move a folder into its own descendant")]
    MoveIntoDescendant,

    /// xochitl did not come back after a restart attempt.
    #[error("failed to restart xochitl: {0}")]
    XochitlRestart(String),
}

pub type Result<T> = std::result::Result<T, Error>;
