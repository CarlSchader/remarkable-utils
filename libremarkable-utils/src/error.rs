//! Typed errors for reMarkable operations.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Errors produced by SSH transport, git plumbing, and logical-tree
/// operations.
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
    #[error("folder is not empty: {0} (use recursive delete)")]
    FolderNotEmpty(String),

    /// Upload of a file type the device cannot render.
    #[error("unsupported file type '{0}': only .pdf, .epub, and .rmdoc are supported")]
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

    /// Multiple downloads need a directory to land in.
    #[error("multiple matches: output must be an existing directory, got {}", .0.display())]
    OutputNotADirectory(PathBuf),

    /// Attempted to move an item into itself.
    #[error("cannot move an item into itself")]
    MoveIntoSelf,

    /// Attempted to move a folder into one of its own descendants.
    #[error("cannot move a folder into its own descendant")]
    MoveIntoDescendant,

    /// Failed to build an `.rmdoc` bundle.
    #[error("bundle: {0}")]
    Bundle(String),

    /// xochitl did not come back after a restart attempt.
    #[error("failed to restart xochitl: {0}")]
    XochitlRestart(String),

    /// The `git` binary could not be started.
    #[error("failed to run git {command}: {source} (is git installed and on PATH?)")]
    GitSpawn { command: String, source: io::Error },

    /// A git command ran but exited non-zero.
    #[error("git {command} exited with status {status}: {stderr}")]
    Git {
        command: String,
        status: i32,
        stderr: String,
    },

    /// A git command produced output rmu could not interpret.
    #[error("unexpected output from git {command}: {detail}")]
    GitOutput { command: String, detail: String },

    /// An argument that git cannot represent (object id, tree path,
    /// identity) or that rmu refuses to pass on.
    #[error("invalid git input: {0}")]
    InvalidGitInput(String),

    /// A path expected to hold a bare git repository does not.
    #[error("not a bare git repository: {}", .0.display())]
    NotABareRepository(PathBuf),

    /// The push lost to a concurrent writer: not a fast-forward, a stale
    /// lease, or the remote's own compare-and-swap failing because the
    /// ref moved mid-push. Fetch, recompute, and retry. It does not carry
    /// the remote's current value; fetch to learn it.
    #[error("push to {reference} rejected: {reason}")]
    PushRejected { reference: String, reason: String },

    /// The remote refused the push for a reason a retry will not fix
    /// (hook, size limit, ...); `stderr` carries the remote's message.
    #[error("push to {reference} failed: {reason}: {stderr}")]
    PushFailed {
        reference: String,
        reason: String,
        stderr: String,
    },

    /// A blob reader yielded a different number of bytes than declared
    /// (e.g. a file changed between stat and read). `actual` is `None`
    /// when it yielded more: the rest is never read.
    #[error("blob for {path}: expected {expected} bytes, got {}", describe_actual(*.expected, *.actual))]
    BlobSizeMismatch {
        path: String,
        expected: u64,
        actual: Option<u64>,
    },

    /// `rmu hub` data-model error (layout, formats, mirror rules).
    #[error(transparent)]
    HubModel(#[from] crate::hub::model::ModelError),

    /// `rmu hub` merge-engine error.
    #[error(transparent)]
    HubMerge(#[from] crate::hub::merge::MergeError),

    /// A string that is not a git object id ([`crate::git::Oid`]).
    #[error("invalid object id {0:?}: expected 40 or 64 lowercase hex digits")]
    InvalidObjectId(String),

    /// `rmu hub` device-layer error (lock, watchdog, transfers, apply).
    #[error(transparent)]
    HubDevice(#[from] crate::hub::device::DeviceError),

    /// An `rmu hub` run holds the tablet's lock: other commands must not
    /// write meanwhile (`None`: the owner record is missing or unreadable).
    #[error("{}", hub_locked(.0.as_ref()))]
    HubLocked(Option<crate::hub::device::LockOwner>),

    /// `rmu hub` sync-cycle error (guards, identity and ancestry checks,
    /// the deletion guard, the local cache).
    #[error(transparent)]
    HubCycle(#[from] crate::hub::cycle::CycleError),
}

fn hub_locked(owner: Option<&crate::hub::device::LockOwner>) -> String {
    let owner = owner.map_or_else(
        || "its owner record is missing, as after a power cut while the lock was taken".to_string(),
        ToString::to_string,
    );
    format!(
        "the tablet is busy: an rmu hub run holds its lock ({owner}); nothing more was written. \
         Try again once that run has finished; if it is gone (its computer crashed or was \
         closed), `rmu hub sync --break-lock` frees the tablet"
    )
}

fn describe_actual(expected: u64, actual: Option<u64>) -> String {
    actual.map_or_else(|| format!("more than {expected}"), |n| n.to_string())
}

pub type Result<T> = std::result::Result<T, Error>;
