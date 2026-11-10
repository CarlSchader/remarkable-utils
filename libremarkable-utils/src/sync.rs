//! One-way folder sync between a local directory and the tablet's
//! logical document tree. Design: `docs/sync-design.md`.
//!
//! Split per repo conventions: snapshot builders and the planner are
//! **pure** (unit-tested without a device); [`execute`] is the thin
//! I/O layer that applies a [`Plan`] through [`Client`] and the local
//! filesystem, updating the sync-state file incrementally so an
//! interrupted sync resumes cleanly.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::bundle;
use crate::client::Client;
use crate::error::{Error, Result};
use crate::progress::Progress;
use crate::ssh::{SshSession, shell_quote};
use crate::xdg;
use crate::xochitl::{self, Item};

#[cfg(all(test, unix))]
mod symlink_tests;

/// Name of the **legacy** in-tree state file (pre-v2 sync). No longer
/// read or written; used only to warn users that it can be deleted.
pub const LEGACY_STATE_FILE_NAME: &str = ".rmu-sync.json";

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// A parsed sync endpoint argument (scp conventions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Local(String),
    /// `[user@]host:path` — `destination` goes to ssh verbatim so ssh
    /// config resolution applies; `path` is a logical device path.
    Remote {
        destination: String,
        path: String,
    },
}

/// Parse an endpoint argument. scp rule: remote iff it contains a `:`
/// **before the first `/`** (escape colon-containing local paths with
/// a `./` prefix).
pub fn parse_endpoint(arg: &str) -> Endpoint {
    match arg.find(':') {
        Some(colon) if colon > 0 && !arg[..colon].contains('/') && !arg[..colon].contains('\\') => {
            Endpoint::Remote {
                destination: arg[..colon].to_string(),
                path: arg[colon + 1..].to_string(),
            }
        }
        _ => Endpoint::Local(arg.to_string()),
    }
}

/// Probe whether an ssh host is a reMarkable tablet: the xochitl data
/// dir and binary are the strong signal (present on rM1/rM2/Paper Pro).
/// Distinguishes "reachable but not a tablet" (`Ok(false)`) from
/// connection failure (`Err`, ssh exits 255).
pub fn probe_remarkable(session: &SshSession, xochitl_dir: &str) -> Result<bool> {
    let command = format!(
        "test -d {} && test -e /usr/bin/xochitl",
        shell_quote(xochitl_dir)
    );
    let output = session.run(&command)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => Err(Error::Remote {
            status: code.unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// Syncable file kinds, by local extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocKind {
    Pdf,
    Epub,
    Rmdoc,
}

impl DocKind {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "pdf" => Some(Self::Pdf),
            "epub" => Some(Self::Epub),
            "rmdoc" => Some(Self::Rmdoc),
            _ => None,
        }
    }

    /// The payload `fileType` this kind maps to on the device.
    pub fn device_file_type(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Epub => "epub",
            Self::Rmdoc => "notebook",
        }
    }
}

/// A syncable local file (directories are implicit in the paths).
#[derive(Debug, Clone)]
pub struct LocalEntry {
    /// Relative path with `/` separators, e.g. `Books/notes.pdf`.
    pub rel_path: String,
    pub kind: DocKind,
    pub size: u64,
    pub mtime_ms: i64,
    /// SHA-256 of the content; filled by [`attach_hashes`] (`None`
    /// when the endpoint cannot hash).
    pub hash: Option<String>,
}

/// Walk a local sync root, collecting syncable files. Dotfiles and
/// dot-directories (including the state file) are skipped; unsupported
/// files are counted but otherwise left alone.
pub fn local_snapshot(root: &Path) -> Result<(Vec<LocalEntry>, usize)> {
    fn walk(
        dir: &Path,
        prefix: &str,
        entries: &mut Vec<LocalEntry>,
        ignored: &mut usize,
    ) -> Result<()> {
        let mut children: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        children.sort_by_key(|entry| entry.file_name());
        children.iter().try_for_each(|child| -> Result<()> {
            let name = child.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return Ok(());
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let file_type = child.file_type()?;
            if file_type.is_dir() {
                return walk(&child.path(), &rel, entries, ignored);
            }
            if !file_type.is_file() {
                return Ok(());
            }
            let kind = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .and_then(|ext| DocKind::from_extension(&ext));
            match kind {
                Some(kind) => {
                    let metadata = child.metadata()?;
                    entries.push(LocalEntry {
                        rel_path: rel,
                        kind,
                        size: metadata.len(),
                        mtime_ms: mtime_ms(&metadata),
                        hash: None,
                    });
                }
                None => *ignored += 1,
            }
            Ok(())
        })
    }

    let mut entries = Vec::new();
    let mut ignored = 0;
    walk(root, "", &mut entries, &mut ignored)?;
    Ok((entries, ignored))
}

/// SHA-256 of a byte slice, lowercase hex.
pub fn hash_bytes(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(data))
}

/// SHA-256 of a file, streamed (large payloads never fully in memory).
pub fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// The cached hash for a local entry, when its stat stamp says the
/// content cannot have changed since last sync (Unison fastcheck).
pub fn cached_hash(entry: &LocalEntry, state: &SyncState) -> Option<String> {
    state.entries.get(&entry.rel_path).and_then(|st| {
        (st.local_size == entry.size && st.local_mtime_ms == entry.mtime_ms)
            .then(|| st.local_hash.clone())
            .flatten()
    })
}

/// Fill in content hashes for a snapshot: stamp-matched entries reuse
/// the archive's hash (no I/O); everything else is hashed by the
/// endpoint. Thin I/O shell over [`cached_hash`].
pub fn attach_hashes(
    fs_side: &dyn FsEndpoint,
    entries: &mut [LocalEntry],
    state: &SyncState,
) -> Result<()> {
    entries.iter_mut().try_for_each(|entry| {
        entry.hash = match cached_hash(entry, state) {
            Some(hash) => Some(hash),
            None => fs_side.content_hash(&entry.rel_path)?,
        };
        Ok(())
    })
}

fn mtime_ms(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Document types on the device side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteType {
    Pdf,
    Epub,
    /// Native notebook (or missing `.content`); pulls as `.rmdoc`.
    Notebook,
}

impl RemoteType {
    /// Local file extension a pull produces.
    fn local_extension(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Epub => "epub",
            Self::Notebook => "rmdoc",
        }
    }

    fn pulled_kind(self) -> DocKind {
        match self {
            Self::Pdf => DocKind::Pdf,
            Self::Epub => DocKind::Epub,
            Self::Notebook => DocKind::Rmdoc,
        }
    }
}

/// A document inside the synced device subtree.
#[derive(Debug, Clone)]
pub struct RemoteDoc {
    /// Directory path relative to the sync root (`""` = the root).
    pub rel_dir: String,
    /// Visible name (no extension).
    pub name: String,
    pub uuid: String,
    pub doc_type: RemoteType,
    pub last_modified: i64,
    pub size_bytes: Option<u64>,
    /// SHA-256 of the payload file, when it was (lazily) fetched —
    /// see [`device_hash_candidates`]. Always `None` for notebooks.
    pub payload_hash: Option<String>,
}

impl RemoteDoc {
    /// Local relative path a pull of this document produces.
    pub fn local_rel_path(&self) -> String {
        let filename = format!("{}.{}", self.name, self.doc_type.local_extension());
        join_rel(&self.rel_dir, &filename)
    }
}

/// The synced subtree of the device.
#[derive(Debug, Default, Clone)]
pub struct RemoteSnapshot {
    pub docs: Vec<RemoteDoc>,
    /// Folder rel path → UUID; `""` maps to the sync root folder.
    pub folders: BTreeMap<String, String>,
    /// Items excluded up-front (duplicate names, unusable names, ...).
    pub skips: Vec<SyncAction>,
    /// Present but unsafe to sync, including descendants of excluded folders.
    pub excluded_uuids: std::collections::HashSet<String>,
    /// Excluded logical names. Neither these paths nor paths below them are
    /// safe targets, even when the previously mapped UUID is now absent.
    pub excluded_paths: std::collections::BTreeSet<String>,
}

impl RemoteSnapshot {
    fn excludes_path(&self, path: &str) -> bool {
        self.excluded_paths
            .iter()
            .any(|excluded| path == excluded || path_under(excluded, path))
    }

    fn excludes_local(&self, path: &str, state: Option<&StateEntry>) -> bool {
        let (dir, name) = split_target(path);
        self.excludes_path(&join_rel(dir, &name))
            || state.is_some_and(|st| self.excluded_uuids.contains(&st.uuid))
    }
}

/// Build the device-side snapshot from a full listing, scoped to the
/// folder `root_uuid` (`""` = device root). Duplicate sibling names and
/// names unusable as filenames are excluded with `Skip` actions,
/// consistent with the repo-wide "never guess on ambiguity" invariant.
pub fn remote_snapshot(items: &[Item], root_uuid: &str) -> RemoteSnapshot {
    fn usable_name(name: &str) -> bool {
        !name.is_empty() && !name.contains('/') && !name.starts_with('.') && name != ".."
    }

    fn exclude(
        children: &HashMap<&str, Vec<&Item>>,
        item: &Item,
        path: String,
        reason: String,
        snapshot: &mut RemoteSnapshot,
    ) {
        fn mark_subtree(
            children: &HashMap<&str, Vec<&Item>>,
            uuid: &str,
            excluded: &mut std::collections::HashSet<String>,
        ) {
            if excluded.insert(uuid.to_string()) {
                children.get(uuid).into_iter().flatten().for_each(|child| {
                    mark_subtree(children, &child.uuid, excluded);
                });
            }
        }
        mark_subtree(children, &item.uuid, &mut snapshot.excluded_uuids);
        snapshot.excluded_paths.insert(path.clone());
        snapshot.skips.push(SyncAction::Skip { path, reason });
    }

    fn walk(
        children: &HashMap<&str, Vec<&Item>>,
        parent_uuid: &str,
        rel_dir: &str,
        snapshot: &mut RemoteSnapshot,
    ) {
        let kids = children.get(parent_uuid).map(Vec::as_slice).unwrap_or(&[]);
        let name_counts = kids.iter().fold(HashMap::<&str, usize>::new(), |mut m, i| {
            *m.entry(i.visible_name.as_str()).or_default() += 1;
            m
        });
        kids.iter().for_each(|item| {
            let path = join_rel(rel_dir, &item.visible_name);
            if name_counts[item.visible_name.as_str()] > 1 {
                exclude(
                    children,
                    item,
                    path,
                    "duplicate sibling name on device".to_string(),
                    snapshot,
                );
                return;
            }
            if !usable_name(&item.visible_name) {
                exclude(
                    children,
                    item,
                    path,
                    "name is not usable as a filename".to_string(),
                    snapshot,
                );
                return;
            }
            if item.is_folder() {
                snapshot.folders.insert(path.clone(), item.uuid.clone());
                walk(children, &item.uuid, &path, snapshot);
                return;
            }
            let doc_type = match item.file_type.as_deref() {
                Some("pdf") => RemoteType::Pdf,
                Some("epub") => RemoteType::Epub,
                Some("notebook") | None => RemoteType::Notebook,
                Some(other) => {
                    exclude(
                        children,
                        item,
                        path,
                        format!("unsupported document type '{other}'"),
                        snapshot,
                    );
                    return;
                }
            };
            snapshot.docs.push(RemoteDoc {
                rel_dir: rel_dir.to_string(),
                name: item.visible_name.clone(),
                uuid: item.uuid.clone(),
                doc_type,
                last_modified: item.last_modified,
                size_bytes: item.size_bytes,
                payload_hash: None,
            });
        });
    }

    let mut snapshot = RemoteSnapshot::default();
    snapshot
        .folders
        .insert(String::new(), root_uuid.to_string());
    walk(
        &xochitl::children_map(items, false),
        root_uuid,
        "",
        &mut snapshot,
    );
    snapshot
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Split a relative file path into (directory, stem-without-extension).
fn split_target(rel_path: &str) -> (&str, String) {
    let (dir, filename) = rel_path.rsplit_once('/').unwrap_or(("", rel_path));
    let stem = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| filename.to_string());
    (dir, stem)
}

// ---------------------------------------------------------------------------
// Sync state
// ---------------------------------------------------------------------------

/// Last-synced knowledge for one mapped file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateEntry {
    /// Device document UUID; empty for fs↔fs pairs (identity is the
    /// path itself).
    pub uuid: String,
    pub kind: DocKind,
    pub local_size: u64,
    pub local_mtime_ms: i64,
    /// Device `lastModified`, or the B side's mtime for fs↔fs pairs.
    pub remote_last_modified: i64,
    /// The UUID was rebound, but the successor's content has not yet
    /// been reconciled. Its timestamp may equal the old document's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote_pending: bool,
    /// B-side size for fs↔fs pairs (absent for device pairs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_size: Option<u64>,
    /// SHA-256 of the local file at last sync. The stat stamp
    /// (mtime+size) gates re-hashing, Unison-fastcheck style: a file
    /// whose stamp matches is never re-read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_hash: Option<String>,
    /// SHA-256 of the device payload (`<uuid>.pdf`/`.epub`) at last
    /// sync. Absent for notebooks/bundles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_hash: Option<String>,
}

/// A new UUID is a change signal even when its timestamp is unchanged.
/// After rebinding, persist that signal until content is reconciled.
fn remote_stamp_changed(st: &StateEntry, doc: &RemoteDoc) -> bool {
    st.remote_pending || st.uuid != doc.uuid || doc.last_modified != st.remote_last_modified
}

/// The sync archive: `local rel path ↔ device UUID` plus what both
/// sides looked like at last sync. Enables three-way diffing; see
/// `docs/sync-design.md`. Stored under `$XDG_STATE_HOME/rmu/`, keyed
/// by the endpoint pair — never inside the synced tree.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub version: u32,
    pub entries: BTreeMap<String, StateEntry>,
}

impl SyncState {
    pub fn parse(text: &str) -> Result<Self> {
        serde_json::from_str(text).map_err(|source| Error::Json {
            path: "sync state".to_string(),
            source,
        })
    }

    pub fn serialize(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            path: "sync state".to_string(),
            source,
        })
    }

    /// Load from a state file; missing file = fresh state.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                version: 2,
                entries: BTreeMap::new(),
            });
        }
        Self::parse(&fs::read_to_string(path)?)
    }

    /// Save atomically (temp + rename), creating parent directories.
    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomically(path, self.serialize()?.as_bytes())
    }
}

/// Write via a sibling temp file + atomic rename so a crash never
/// leaves a truncated file.
fn write_atomically(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    Ok(fs::rename(&tmp, path)?)
}

/// Where the archive for a pair of sync endpoints lives:
/// `$XDG_STATE_HOME/rmu/` (or `~/.local/state/rmu/`), keyed
/// order-independently by the two endpoint labels so `push` and
/// `pull` over the same pair share state.
pub fn sync_state_path(label_a: &str, label_b: &str) -> PathBuf {
    state_dir().join(format!("sync-{}.json", pair_key_hash(label_a, label_b)))
}

// ---------------------------------------------------------------------------
// Filesystem endpoints
// ---------------------------------------------------------------------------

/// A file-tree side of a sync: a local directory or a directory on a
/// generic ssh host. The device side is *not* an `FsEndpoint` — it has
/// a logical document model instead (see `RemoteSnapshot`).
pub trait FsEndpoint {
    /// Human-readable identity for messages.
    fn label(&self) -> String;
    /// Create the root directory if missing.
    fn ensure_root(&self) -> Result<()>;
    /// Whether the root exists, without creating it. Missing writable
    /// roots can be represented by an empty snapshot during planning.
    fn root_exists(&self) -> Result<bool> {
        match self.stat("") {
            Ok(_) => Ok(true),
            Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
    /// Walk the tree: (syncable files, unsupported-file count).
    fn snapshot(&self) -> Result<(Vec<LocalEntry>, usize)>;
    fn read(&self, rel_path: &str) -> Result<Vec<u8>>;
    /// Write a file, creating parent directories.
    fn write(&self, rel_path: &str, data: &[u8]) -> Result<()>;
    /// Remove a file; idempotent.
    fn remove(&self, rel_path: &str) -> Result<()>;
    /// Move/rename a file, creating target parent directories.
    fn rename(&self, from: &str, to: &str) -> Result<()>;
    /// Remove a directory **only if empty**; returns whether it was
    /// removed. Missing or non-empty directories are left alone
    /// without error — the emptiness check is the safety mechanism.
    fn remove_dir(&self, rel_dir: &str) -> Result<bool>;
    /// (size, mtime in ms).
    fn stat(&self, rel_path: &str) -> Result<(u64, i64)>;
    /// SHA-256 of a file's content, if this endpoint can compute it
    /// cheaply enough to be worth it.
    fn content_hash(&self, rel_path: &str) -> Result<Option<String>>;
    /// Prepare a local output path for a streamed write, rejecting symlinks
    /// below the endpoint root and creating missing parent directories.
    /// Endpoints without this fast path use `write` instead.
    fn prepare_local_write(&self, rel_path: &str) -> Result<Option<PathBuf>> {
        let _ = rel_path;
        Ok(None)
    }
    /// For local endpoints: the real path, enabling streamed reads.
    /// This is not a validated output path; use `prepare_local_write` to write.
    fn as_local_path(&self, rel_path: &str) -> Option<PathBuf> {
        let _ = rel_path;
        None
    }
}

/// A directory on this machine.
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// Require a nonempty, root-relative path before checking filesystem entries.
/// In particular, `..` must not undo a checked component.
fn sync_path_parts(rel_path: &str) -> Result<Vec<&str>> {
    crate::relative_path::split_safe_relative(rel_path)
        .filter(|parts| {
            parts.iter().all(|part| {
                matches!(
                    Path::new(part).components().next(),
                    Some(std::path::Component::Normal(_))
                )
            })
        })
        .ok_or_else(|| Error::InvalidName(format!("unsafe sync path: {rel_path}")))
}

/// Check every component beneath the selected root, including dangling links.
/// The root itself may deliberately be a symlink (as may its own ancestors).
/// These write-time checks reject collisions omitted by snapshots; they are
/// not a sandbox against concurrent, hostile filesystem namespace changes.
fn checked_local_path(root: &Path, rel_path: &str) -> Result<PathBuf> {
    let parts = sync_path_parts(rel_path)?;
    parts.iter().enumerate().try_fold(
        root.to_path_buf(),
        |path, (index, part)| -> Result<PathBuf> {
            let path = path.join(part);
            match fs::symlink_metadata(&path) {
                Ok(meta) => {
                    let valid_type = if index + 1 == parts.len() {
                        meta.is_file()
                    } else {
                        meta.is_dir()
                    };
                    if meta.file_type().is_symlink() || !valid_type {
                        return Err(std::io::Error::other(format!(
                            "unsafe sync path (symlink or wrong file type): {}",
                            path.display()
                        ))
                        .into());
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            Ok(path)
        },
    )
}

impl FsEndpoint for LocalFs {
    fn label(&self) -> String {
        self.root.display().to_string()
    }

    fn ensure_root(&self) -> Result<()> {
        Ok(fs::create_dir_all(&self.root)?)
    }

    fn root_exists(&self) -> Result<bool> {
        // Strip trailing separators/dots so lstat sees a dangling root link
        // itself, rather than following it. Keep `..` components intact.
        let root: PathBuf = self.root.components().collect();
        root.ancestors()
            .enumerate()
            .find_map(|(depth, path)| {
                let path = if path.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    path
                };
                match fs::symlink_metadata(path) {
                    // Follow an existing link only after identifying it. A
                    // dangling link is an error, not a creatable missing root.
                    // Valid directory links at/above the root remain supported.
                    Ok(_) => Some(
                        fs::metadata(path)
                            .and_then(|metadata| {
                                if metadata.is_dir() {
                                    Ok(depth == 0)
                                } else {
                                    Err(std::io::Error::new(
                                        std::io::ErrorKind::NotADirectory,
                                        format!("not a sync root directory: {}", path.display()),
                                    ))
                                }
                            })
                            .map_err(Error::from),
                    ),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                    Err(err) => Some(Err(err.into())),
                }
            })
            .unwrap_or(Ok(false))
    }

    fn snapshot(&self) -> Result<(Vec<LocalEntry>, usize)> {
        local_snapshot(&self.root)
    }

    fn read(&self, rel_path: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.root.join(rel_path))?)
    }

    fn write(&self, rel_path: &str, data: &[u8]) -> Result<()> {
        let path = self.prepare_local_write(rel_path)?.expect("local endpoint");
        Ok(fs::write(path, data)?)
    }

    fn remove(&self, rel_path: &str) -> Result<()> {
        match fs::remove_file(self.root.join(rel_path)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let source = checked_local_path(&self.root, from)?;
        let target = self.prepare_local_write(to)?.expect("local endpoint");
        Ok(fs::rename(source, target)?)
    }

    fn remove_dir(&self, rel_dir: &str) -> Result<bool> {
        match fs::remove_dir(self.root.join(rel_dir)) {
            Ok(()) => Ok(true),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                Ok(false)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn stat(&self, rel_path: &str) -> Result<(u64, i64)> {
        let metadata = fs::metadata(self.root.join(rel_path))?;
        Ok((metadata.len(), mtime_ms(&metadata)))
    }

    fn content_hash(&self, rel_path: &str) -> Result<Option<String>> {
        hash_file(&self.root.join(rel_path)).map(Some)
    }

    fn prepare_local_write(&self, rel_path: &str) -> Result<Option<PathBuf>> {
        let path = checked_local_path(&self.root, rel_path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Check again after directory creation, immediately before the write.
        checked_local_path(&self.root, rel_path).map(Some)
    }

    fn as_local_path(&self, rel_path: &str) -> Option<PathBuf> {
        Some(self.root.join(rel_path))
    }
}

/// A directory on a generic ssh host (not a tablet). Uses POSIX
/// commands; `stat` flags are probed once (GNU vs BSD).
pub struct SshFs {
    session: SshSession,
    /// Remote root; empty = the ssh user's home directory (scp
    /// semantics for `host:`).
    root: String,
    progress: Arc<dyn Progress>,
}

impl SshFs {
    pub fn new(session: SshSession, root: impl Into<String>, progress: Arc<dyn Progress>) -> Self {
        Self {
            session,
            root: root.into(),
            progress,
        }
    }

    fn join(&self, rel_path: &str) -> String {
        if self.root.is_empty() {
            rel_path.to_string()
        } else if self.root.ends_with('/') {
            format!("{}{rel_path}", self.root)
        } else {
            format!("{}/{rel_path}", self.root)
        }
    }

    /// Enter the selected root, then reject links in every relative component
    /// in the same shell invocation as the mutation (not just at snapshot time).
    fn mutation_preamble(&self) -> String {
        format!(
            "mkdir -p {root} && cd {root} || exit 9\n",
            root = shell_quote(&self.root_for_shell())
        )
    }

    fn write_command(&self, rel_path: &str) -> Result<String> {
        let guard = shell_path_guard(rel_path, true)?;
        Ok(format!(
            "{}{guard}cat > {}",
            self.mutation_preamble(),
            shell_quote(&format!("./{rel_path}"))
        ))
    }

    fn rename_command(&self, from: &str, to: &str) -> Result<String> {
        let source_guard = shell_path_guard(from, false)?;
        let target_guard = shell_path_guard(to, true)?;
        Ok(format!(
            "{}{source_guard}{target_guard}mv -- {from} {to}",
            self.mutation_preamble(),
            from = shell_quote(&format!("./{from}")),
            to = shell_quote(&format!("./{to}")),
        ))
    }

    fn root_for_shell(&self) -> String {
        if self.root.is_empty() {
            ".".to_string()
        } else {
            self.root.clone()
        }
    }
}

/// POSIX shell counterpart of `checked_local_path`. Each parent is checked
/// before mkdir can traverse it. `-L` also catches dangling symlinks; checking
/// the leaf type prevents `mv` from treating a directory as a target container.
fn shell_path_guard(rel_path: &str, create_parents: bool) -> Result<String> {
    let parts = sync_path_parts(rel_path)?;
    let (_, script) = parts.iter().enumerate().fold(
        (String::from("."), String::new()),
        |(mut path, mut script), (index, part)| {
            path.push('/');
            path.push_str(part);
            let quoted = shell_quote(&path);
            let is_parent = index + 1 < parts.len();
            let kind = if is_parent { "d" } else { "f" };
            script.push_str(&format!(
                "if [ -L {quoted} ] || {{ [ -e {quoted} ] && [ ! -{kind} {quoted} ]; }}; then\n\
                 printf '%s\\n' {message} >&2; exit 9\nfi\n",
                message = shell_quote(&format!(
                    "unsafe sync path (symlink or wrong file type): {path}"
                )),
            ));
            if is_parent && create_parents {
                script.push_str(&format!("mkdir -p {quoted} || exit 9\n"));
            }
            (path, script)
        },
    );
    Ok(script)
}

impl FsEndpoint for SshFs {
    fn label(&self) -> String {
        format!("{}:{}", self.session.target(), self.root)
    }

    fn ensure_root(&self) -> Result<()> {
        self.session
            .run_checked(&format!("mkdir -p {}", shell_quote(&self.root_for_shell())))?;
        Ok(())
    }

    fn root_exists(&self) -> Result<bool> {
        let root = shell_quote(&self.root_for_shell());
        // A failed existence test can also mean ENOTDIR or EACCES.
        // Before accepting absence, find the nearest existing ancestor
        // and require it to be a searchable directory.
        let output = self.session.run_checked(&format!(
            "if test -e {root} || test -L {root}; then printf exists; exit 0; fi\n\
             parent={root}\n\
             while ! test -e \"$parent\" && ! test -L \"$parent\"; do\n\
             case \"$parent\" in\n\
             */*) parent=${{parent%/*}}; test -n \"$parent\" || parent=/ ;;\n\
             *) parent=. ;;\n\
             esac\n\
             done\n\
             test -d \"$parent\" && test -x \"$parent\" || {{\n\
             printf 'not a searchable directory: %s\\n' \"$parent\" >&2; exit 9;\n\
             }}"
        ))?;
        Ok(output == "exists")
    }

    fn snapshot(&self) -> Result<(Vec<LocalEntry>, usize)> {
        // One round trip: list every regular file with size and mtime.
        // GNU stat (-c) vs BSD/macOS stat (-f) is probed inline.
        let script = format!(
            "cd {root} || exit 9\n\
             if stat -c %s . >/dev/null 2>&1; then\n\
             find . -type f -exec stat -c '%s %Y %n' {{}} +\n\
             else\n\
             find . -type f -exec stat -f '%z %m %N' {{}} +\n\
             fi\n",
            root = shell_quote(&self.root_for_shell()),
        );
        let output = self.session.run_checked(&script)?;
        Ok(parse_fs_listing(&output))
    }

    fn read(&self, rel_path: &str) -> Result<Vec<u8>> {
        self.session.run_checked_bytes(
            &format!("cat {}", shell_quote(&self.join(rel_path))),
            &*self.progress,
        )
    }

    fn write(&self, rel_path: &str, data: &[u8]) -> Result<()> {
        self.session
            .run_checked_with_stdin(&self.write_command(rel_path)?, data, &*self.progress)
    }

    fn remove(&self, rel_path: &str) -> Result<()> {
        self.session
            .run_checked(&format!("rm -f -- {}", shell_quote(&self.join(rel_path))))?;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.session.run_checked(&self.rename_command(from, to)?)?;
        Ok(())
    }

    fn remove_dir(&self, rel_dir: &str) -> Result<bool> {
        // POSIX rmdir refuses non-empty directories; that refusal is
        // the safety mechanism, so a non-zero exit is "kept", not an
        // error.
        let output = self
            .session
            .run(&format!("rmdir -- {}", shell_quote(&self.join(rel_dir))))?;
        Ok(output.status.success())
    }

    fn stat(&self, rel_path: &str) -> Result<(u64, i64)> {
        let quoted = shell_quote(&self.join(rel_path));
        let output = self.session.run_checked(&format!(
            "stat -c '%s %Y' -- {quoted} 2>/dev/null || stat -f '%z %m' {quoted}"
        ))?;
        let mut parts = output.split_whitespace();
        let size = parts.next().and_then(|s| s.parse().ok());
        let mtime_s = parts.next().and_then(|s| s.parse::<i64>().ok());
        match (size, mtime_s) {
            (Some(size), Some(mtime)) => Ok((size, mtime * 1000)),
            _ => Err(Error::Remote {
                status: -1,
                stderr: format!("unparsable stat output: {}", output.trim()),
            }),
        }
    }

    fn content_hash(&self, rel_path: &str) -> Result<Option<String>> {
        // One round trip per file; callers hash lazily (only files
        // whose stat stamp moved), so this stays proportional to the
        // number of *changes*.
        let output = self.session.run(&format!(
            "sha256sum -- {} 2>/dev/null",
            shell_quote(&self.join(rel_path))
        ))?;
        if !output.status.success() {
            return Ok(None); // no sha256sum on the host, or file gone
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .map(str::to_string))
    }
}

/// Parse `SIZE MTIME ./rel/path` lines from the snapshot script.
/// Dotfiles/dot-directories are skipped; unsupported extensions are
/// counted. Paths containing newlines are silently dropped (they
/// arrive as unparsable lines).
fn parse_fs_listing(output: &str) -> (Vec<LocalEntry>, usize) {
    let mut ignored = 0usize;
    let mut entries: Vec<LocalEntry> = output
        .lines()
        .filter_map(|line| {
            let (size, rest) = line.split_once(' ')?;
            let (mtime, path) = rest.split_once(' ')?;
            let size: u64 = size.trim().parse().ok()?;
            let mtime_s: i64 = mtime.trim().parse().ok()?;
            let rel = path.strip_prefix("./").unwrap_or(path);
            if rel.is_empty() || rel.split('/').any(|part| part.starts_with('.')) {
                return None;
            }
            let kind = Path::new(rel)
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .and_then(|ext| DocKind::from_extension(&ext));
            match kind {
                Some(kind) => Some(LocalEntry {
                    rel_path: rel.to_string(),
                    kind,
                    size,
                    mtime_ms: mtime_s * 1000,
                    hash: None,
                }),
                None => {
                    ignored += 1;
                    None
                }
            }
        })
        .collect();
    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    (entries, ignored)
}

// ---------------------------------------------------------------------------
// Planner (pure)
// ---------------------------------------------------------------------------

/// Sync mode. In one-way modes the destination is never treated as a
/// source of changes; two-way propagates changes in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Local → device.
    Push,
    /// Device → local.
    Pull,
    /// Bidirectional.
    TwoWay,
}

/// What to do when both sides of a mapped file changed (or when
/// unmapped files collide). `PreferLocal`/`PreferRemote` are produced
/// by the CLI from `--conflict src|dst` plus the argument order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// Report and touch nothing (default; loses nothing).
    #[default]
    Skip,
    /// The side with the newer timestamp wins (local mtime vs. device
    /// `lastModified`; ties go to local; beware clock skew).
    Newest,
    PreferLocal,
    PreferRemote,
}

#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    pub mode: Mode,
    /// Propagate deletions of **mapped** files (never-synced files are
    /// never deleted, unlike rsync `--delete`).
    pub delete: bool,
    pub conflict: ConflictPolicy,
}

/// One planned operation. `Skip`/`Conflict` are informational.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncAction {
    CreateRemoteFolder {
        rel_dir: String,
    },
    Upload {
        local: String,
        kind: DocKind,
        remote_dir: String,
        name: String,
        /// Content hash of the local file (recorded into state; for
        /// pdf/epub the uploaded payload is byte-identical).
        hash: Option<String>,
    },
    UpdateRemote {
        local: String,
        kind: DocKind,
        uuid: String,
        hash: Option<String>,
    },
    Download {
        local: String,
        uuid: String,
        doc_type: RemoteType,
        size_bytes: Option<u64>,
        /// Remote `lastModified` at planning time, recorded into state.
        last_modified: i64,
    },
    /// Only emitted with `--delete`, and only for mapped files.
    DeleteRemote {
        path: String,
        uuid: String,
    },
    /// Only emitted with `--delete`, and only for mapped files.
    DeleteLocal {
        path: String,
    },
    /// Remove a device folder emptied by this plan's deletions.
    /// Only emitted with `--delete`; ordered after document
    /// deletions, children before parents.
    DeleteRemoteFolder {
        rel_dir: String,
        uuid: String,
    },
    /// Remove a local directory emptied by this plan's deletions.
    /// Only emitted with `--delete`; executed as remove-if-empty, so
    /// a directory still holding files sync cannot see (unsupported
    /// types, dotfiles) survives.
    DeleteLocalDir {
        rel_dir: String,
    },
    /// Drop a stale state mapping (e.g. both sides gone).
    Forget {
        path: String,
    },
    /// Re-link a mapping whose document was replaced on the device by
    /// a same-name, same-kind document (e.g. an interrupted sync that
    /// uploaded but never recorded state). State-only; no transfer.
    /// Changes identity only, preserving the last agreed content baseline.
    Rebind {
        path: String,
        uuid: String,
        /// Observed successor stamp, not a new content-agreement baseline.
        last_modified: i64,
    },
    /// Pair an unmapped local file with an unmapped device document
    /// whose content is **hash-verified identical** (e.g. after state
    /// loss). State-only; no transfer, no policy needed.
    Adopt {
        path: String,
        kind: DocKind,
        uuid: String,
        last_modified: i64,
        hash: String,
    },
    /// A mapped file whose stamp moved but whose content did not
    /// (touched file, or device `lastModified` bumped by annotations).
    /// Updates the recorded stamps so the next run is silent; no
    /// transfer.
    Refresh {
        path: String,
        kind: DocKind,
        uuid: String,
        last_modified: i64,
        local_hash: Option<String>,
        payload_hash: Option<String>,
    },
    /// The local file moved (hash-paired with a vanished mapped path):
    /// relocate the device document with one metadata write. Zero
    /// bytes transferred, annotations preserved.
    MoveRemote {
        from: String,
        to: String,
        kind: DocKind,
        uuid: String,
        remote_dir: String,
        name: String,
        local_hash: Option<String>,
        payload_hash: Option<String>,
    },
    /// An entire local folder moved/renamed (every mapped file under
    /// it reappeared under the new prefix, hash-verified): relocate
    /// the device folder with **one** metadata write — the whole
    /// subtree follows, the folder keeps its UUID.
    MoveRemoteFolder {
        from_dir: String,
        to_dir: String,
        uuid: String,
        /// Parent of `to_dir` (`""` = sync root).
        remote_dir: String,
        name: String,
    },
    /// The document moved on the device (UUID identity): relocate the
    /// local file to mirror it. Zero bytes transferred.
    MoveLocal {
        from: String,
        to: String,
        kind: DocKind,
        uuid: String,
        last_modified: i64,
        local_hash: Option<String>,
        payload_hash: Option<String>,
    },
    /// Create a device document by copying an identical payload that
    /// is **already on the device** (hash-verified). Nothing uploaded.
    CopyRemote {
        local: String,
        kind: DocKind,
        from_uuid: String,
        remote_dir: String,
        name: String,
        size_bytes: Option<u64>,
        hash: String,
    },
    /// Materialize a device document by copying an identical local
    /// file (hash-verified). Nothing downloaded.
    CopyLocal {
        from: String,
        to: String,
        kind: DocKind,
        uuid: String,
        last_modified: i64,
        hash: String,
    },
    /// A conflict the policy did not resolve; nothing is changed.
    Conflict {
        path: String,
        reason: String,
    },
    Skip {
        path: String,
        reason: String,
    },
}

#[derive(Debug, Default)]
pub struct Plan {
    pub actions: Vec<SyncAction>,
}

impl SyncAction {
    /// Whether executing it writes to the tablet (documents or folders;
    /// state-only and local actions do not).
    pub fn writes_remote(&self) -> bool {
        match self {
            SyncAction::CreateRemoteFolder { .. }
            | SyncAction::Upload { .. }
            | SyncAction::UpdateRemote { .. }
            | SyncAction::DeleteRemote { .. }
            | SyncAction::DeleteRemoteFolder { .. }
            | SyncAction::MoveRemote { .. }
            | SyncAction::MoveRemoteFolder { .. }
            | SyncAction::CopyRemote { .. } => true,
            SyncAction::Download { .. }
            | SyncAction::DeleteLocal { .. }
            | SyncAction::DeleteLocalDir { .. }
            | SyncAction::Forget { .. }
            | SyncAction::Rebind { .. }
            | SyncAction::Adopt { .. }
            | SyncAction::Refresh { .. }
            | SyncAction::MoveLocal { .. }
            | SyncAction::CopyLocal { .. }
            | SyncAction::Conflict { .. }
            | SyncAction::Skip { .. } => false,
        }
    }

    /// Whether it deletes something on the tablet.
    pub fn deletes_remote(&self) -> bool {
        matches!(
            self,
            SyncAction::DeleteRemote { .. } | SyncAction::DeleteRemoteFolder { .. }
        )
    }
}

impl Plan {
    /// Number of actions that would change something.
    pub fn changes(&self) -> usize {
        self.actions
            .iter()
            .filter(|action| {
                !matches!(
                    action,
                    SyncAction::Skip { .. } | SyncAction::Conflict { .. }
                )
            })
            .count()
    }
}

/// Which side a conflict resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Winner {
    Local,
    Remote,
}

/// Resolve a conflict between a local version (`local_ms`) and a
/// remote version (`remote_ms`). Deleted sides pass `i64::MIN` so
/// `Newest` lets the surviving change win.
fn winner(policy: ConflictPolicy, local_ms: i64, remote_ms: i64) -> Option<Winner> {
    match policy {
        ConflictPolicy::Skip => None,
        ConflictPolicy::PreferLocal => Some(Winner::Local),
        ConflictPolicy::PreferRemote => Some(Winner::Remote),
        ConflictPolicy::Newest => Some(if local_ms >= remote_ms {
            Winner::Local
        } else {
            Winner::Remote
        }),
    }
}

fn can_write_remote(mode: Mode) -> bool {
    matches!(mode, Mode::Push | Mode::TwoWay)
}

fn can_write_local(mode: Mode) -> bool {
    matches!(mode, Mode::Pull | Mode::TwoWay)
}

/// Per-key view of the three-way diff.
struct View<'a> {
    local: Option<&'a LocalEntry>,
    remote: Option<&'a RemoteDoc>,
    state: Option<&'a StateEntry>,
}

impl View<'_> {
    fn empty() -> Self {
        View {
            local: None,
            remote: None,
            state: None,
        }
    }
}

/// Name-collision guards and content indexes for upload candidates.
struct Guards {
    /// (dir, name) pairs already occupied on the device.
    taken: std::collections::HashSet<(String, String)>,
    /// Upload candidates per (dir, name); >1 means local files compete
    /// for the same device name (e.g. `n.epub` + `n.pdf`).
    upload_counts: HashMap<(String, String), usize>,
    /// Copy-by-fingerprint sources on the device: payload hash →
    /// (uuid, kind, size). A new local file whose hash is here is
    /// materialized by an on-device `cp` instead of an upload.
    copy_sources: HashMap<String, (String, DocKind, Option<u64>)>,
    /// Local files by content hash: a new device document whose
    /// payload hash is here is materialized by a local copy instead
    /// of a download.
    local_by_hash: HashMap<String, String>,
}

/// Action buckets, concatenated in execution-safe order: rebinds →
/// folders → transfers → deletions → forgets → notes. Rebinds go
/// first so a transfer's own state update (written after it runs)
/// is not clobbered by the planning-time rebind values.
#[derive(Default)]
struct Buckets {
    rebinds: Vec<SyncAction>,
    moves: Vec<SyncAction>,
    transfers: Vec<SyncAction>,
    deletes: Vec<SyncAction>,
    forgets: Vec<SyncAction>,
    notes: Vec<SyncAction>,
}

/// Whether a document kind mirrors its payload byte-for-byte between
/// the fs side and the device (notebooks/bundles do not).
fn payload_mirrored(doc_type: RemoteType) -> bool {
    matches!(doc_type, RemoteType::Pdf | RemoteType::Epub)
}

/// Which device payloads are worth hashing before planning (lazy,
/// batched by the caller into one round trip). Pure. Candidates:
/// - mapped docs whose remote stamp changed (including pending UUID
///   replacements) while a recorded payload hash exists (annotation-only
///   changes and identical successors can then skip the transfer);
/// - unmapped docs whose pull path collides with an unmapped local
///   file of the same payload kind (hash-verified adoption);
/// - unmapped docs whose size matches some local file (hash-verified
///   local copy instead of a download — equal hashes require equal
///   sizes, so the size check is a free prefilter).
pub fn device_hash_candidates(
    local: &[LocalEntry],
    remote: &RemoteSnapshot,
    state: &SyncState,
) -> Vec<(String, &'static str)> {
    let local_by_path: HashMap<&str, &LocalEntry> =
        local.iter().map(|e| (e.rel_path.as_str(), e)).collect();
    let local_sizes: std::collections::HashSet<u64> = local.iter().map(|e| e.size).collect();
    let mapped_uuids: std::collections::HashSet<&str> =
        state.entries.values().map(|st| st.uuid.as_str()).collect();
    let by_uuid: HashMap<&str, &RemoteDoc> =
        remote.docs.iter().map(|d| (d.uuid.as_str(), d)).collect();

    let by_target: HashMap<(&str, &str), &RemoteDoc> = remote
        .docs
        .iter()
        .map(|d| ((d.rel_dir.as_str(), d.name.as_str()), d))
        .collect();
    let mapped = state.entries.iter().filter_map(|(key, st)| {
        let (dir, stem) = split_target(key);
        let doc = by_uuid.get(st.uuid.as_str()).or_else(|| {
            by_target.get(&(dir, stem.as_str())).filter(|doc| {
                !mapped_uuids.contains(doc.uuid.as_str()) && kind_matches(st.kind, doc.doc_type)
            })
        })?;
        (payload_mirrored(doc.doc_type)
            && st.payload_hash.is_some()
            && remote_stamp_changed(st, doc))
        .then_some(*doc)
    });
    let adoption = remote
        .docs
        .iter()
        .filter(|doc| payload_mirrored(doc.doc_type) && !mapped_uuids.contains(doc.uuid.as_str()))
        .filter(|doc| {
            let twin = local_by_path
                .get(doc.local_rel_path().as_str())
                .is_some_and(|entry| {
                    !state.entries.contains_key(&entry.rel_path)
                        && kind_matches(entry.kind, doc.doc_type)
                });
            let size_match = doc
                .size_bytes
                .is_some_and(|size| local_sizes.contains(&size));
            twin || size_match
        });

    let mut out: Vec<(String, &'static str)> = mapped
        .chain(adoption)
        .map(|doc| {
            let ext = match doc.doc_type {
                RemoteType::Pdf => "pdf",
                RemoteType::Epub => "epub",
                RemoteType::Notebook => unreachable!("filtered to mirrored payloads"),
            };
            (doc.uuid.clone(), ext)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Attach lazily fetched payload hashes to a snapshot.
pub fn attach_payload_hashes(remote: &mut RemoteSnapshot, hashes: &HashMap<String, String>) {
    remote.docs.iter_mut().for_each(|doc| {
        doc.payload_hash = hashes.get(&doc.uuid).cloned();
    });
}

/// Output of the move-detection pre-pass: the move actions plus
/// working copies of the archive and local snapshot with moved
/// entries **re-keyed to their new paths**, so the three-way table
/// below sees moves as already-agreed facts instead of
/// delete-and-create pairs.
struct MovePrepass {
    actions: Vec<SyncAction>,
    notes: Vec<SyncAction>,
    state: SyncState,
    local: Vec<LocalEntry>,
    remote: RemoteSnapshot,
}

/// Pair moved files by content identity. Pure.
///
/// Local→device (`MoveRemote`): a vanished mapped path and a new
/// unmapped file with the same hash and kind, strict 1:1 (filename
/// tie-break); requires the device copy unchanged — a racing device
/// edit falls back to the ordinary rules. Device→local (`MoveLocal`):
/// the document UUID *is* the identity, so a mapped doc whose device
/// path moved simply drags the local file along; the `lastModified`
/// bump a move causes is absorbed only when the payload hash proves
/// the content unchanged.
fn detect_moves(
    options: SyncOptions,
    local: &[LocalEntry],
    remote: &RemoteSnapshot,
    state: &SyncState,
) -> MovePrepass {
    let mut out = MovePrepass {
        actions: Vec::new(),
        notes: Vec::new(),
        state: SyncState {
            version: state.version,
            entries: state.entries.clone(),
        },
        local: local.to_vec(),
        remote: remote.clone(),
    };
    let docs_by_uuid: HashMap<&str, &RemoteDoc> =
        remote.docs.iter().map(|d| (d.uuid.as_str(), d)).collect();
    let local_by_path: HashMap<&str, &LocalEntry> =
        local.iter().map(|e| (e.rel_path.as_str(), e)).collect();
    let mut claimed_names = device_taken(remote);

    // ---- local folder moves → MoveRemoteFolder --------------------------
    //
    // A whole folder moved when *every* mapped file under it vanished
    // locally and reappeared under one new prefix at the same relative
    // sub-paths with the same hashes and kinds. One metadata write
    // relocates the device folder (UUID kept, subtree follows); the
    // adjusted copies are rewritten so the file-level pass and the
    // table below see nothing left to do.
    if can_write_remote(options.mode) {
        let mut moved_prefixes: Vec<String> = Vec::new();
        let folder_list: Vec<(String, String)> = remote
            .folders
            .iter()
            .filter(|(dir, _)| !dir.is_empty())
            .map(|(dir, uuid)| (dir.clone(), uuid.clone()))
            .collect();
        folder_list.into_iter().for_each(|(from_dir, folder_uuid)| {
            if moved_prefixes.iter().any(|p| path_under(p, &from_dir)) {
                return;
            }
            let mapped: Vec<(&String, &StateEntry)> = state
                .entries
                .iter()
                .filter(|(key, _)| path_under(&from_dir, key))
                .collect();
            if mapped.is_empty()
                || remote
                    .excluded_paths
                    .iter()
                    .any(|path| path_under(&from_dir, path))
                || mapped
                    .iter()
                    .any(|(key, st)| remote.excludes_local(key, Some(st)))
                || mapped.iter().any(|(_, st)| st.local_hash.is_none())
                || local.iter().any(|e| path_under(&from_dir, &e.rel_path))
            {
                return;
            }
            // Hypothesize target prefixes from the first mapped file,
            // then verify every other one against each hypothesis.
            let sub0 = mapped[0]
                .0
                .strip_prefix(&format!("{from_dir}/"))
                .expect("filtered on prefix")
                .to_string();
            let hash0 = mapped[0].1.local_hash.clone().expect("filtered on Some");
            let verifies = |g: &str| -> bool {
                !g.is_empty()
                    && !remote.excludes_path(g)
                    && !out.remote.folders.contains_key(g)
                    && !state.entries.keys().any(|k| path_under(g, k) || k == g)
                    && mapped.iter().all(|(key, st)| {
                        let sub = key
                            .strip_prefix(&format!("{from_dir}/"))
                            .expect("filtered on prefix");
                        local_by_path
                            .get(join_rel(g, sub).as_str())
                            .is_some_and(|e| {
                                !state.entries.contains_key(&e.rel_path)
                                    && e.kind == st.kind
                                    && e.hash.is_some()
                                    && e.hash == st.local_hash
                            })
                    })
            };
            let mut candidates: Vec<String> = local
                .iter()
                .filter(|e| {
                    !state.entries.contains_key(&e.rel_path)
                        && e.kind == mapped[0].1.kind
                        && e.hash.as_deref() == Some(hash0.as_str())
                })
                .filter_map(|e| {
                    e.rel_path
                        .strip_suffix(&sub0)
                        .and_then(|prefix| prefix.strip_suffix('/'))
                        .map(str::to_string)
                })
                .filter(|g| verifies(g))
                .collect();
            candidates.sort();
            candidates.dedup();
            match &candidates[..] {
                [to_dir] => {
                    let (parent, name) = to_dir.rsplit_once('/').unwrap_or(("", to_dir));
                    let target_name = (parent.to_string(), name.to_string());
                    if claimed_names.contains(&target_name) {
                        out.notes.push(skip(
                            to_dir,
                            "moved folder's device name is already taken; \
                             treating contents as new files",
                        ));
                        return;
                    }
                    claimed_names.insert(target_name);
                    out.actions.push(SyncAction::MoveRemoteFolder {
                        from_dir: from_dir.clone(),
                        to_dir: to_dir.clone(),
                        uuid: folder_uuid,
                        remote_dir: parent.to_string(),
                        name: name.to_string(),
                    });
                    rewrite_prefix(&mut out, &from_dir, to_dir, &local_by_path);
                    moved_prefixes.push(from_dir);
                }
                [] => {}
                _ => out.notes.push(skip(
                    &from_dir,
                    "every file under this folder moved, but multiple target \
                     folders match; treating contents as new files",
                )),
            }
        });
    }

    // ---- local moves → MoveRemote --------------------------------------
    if can_write_remote(options.mode) {
        // (hash, kind) → vanished mapped paths / new unmapped paths.
        type Group = (Vec<String>, Vec<String>);
        let mut groups: BTreeMap<(String, DocKind), Group> = BTreeMap::new();
        out.state
            .entries
            .iter()
            .filter(|(key, st)| {
                !local_by_path.contains_key(key.as_str())
                    && !remote.excludes_local(key, Some(st))
                    && st.local_hash.is_some()
                    && docs_by_uuid
                        .get(st.uuid.as_str())
                        .is_some_and(|doc| !remote_stamp_changed(st, doc))
            })
            .for_each(|(key, st)| {
                let hash = st.local_hash.clone().expect("filtered on Some");
                groups
                    .entry((hash, st.kind))
                    .or_default()
                    .0
                    .push(key.clone());
            });
        local
            .iter()
            .filter(|entry| {
                !out.state.entries.contains_key(&entry.rel_path)
                    && !remote.excludes_local(&entry.rel_path, None)
            })
            .filter_map(|entry| entry.hash.clone().map(|hash| (entry, hash)))
            .for_each(|(entry, hash)| {
                if let Some(group) = groups.get_mut(&(hash, entry.kind)) {
                    group.1.push(entry.rel_path.clone());
                }
            });

        groups.into_values().for_each(|(mut sources, mut targets)| {
            if targets.is_empty() {
                return;
            }
            // Filename tie-break first, then an unambiguous 1:1 pair.
            let mut pairs: Vec<(String, String)> = Vec::new();
            targets.retain(|target| {
                let stem = split_target(target).1;
                let matching: Vec<usize> = sources
                    .iter()
                    .enumerate()
                    .filter(|(_, source)| split_target(source).1 == stem)
                    .map(|(i, _)| i)
                    .collect();
                if let [only] = matching[..] {
                    pairs.push((sources.remove(only), target.clone()));
                    return false;
                }
                true
            });
            if let ([source], [target]) = (&sources[..], &targets[..]) {
                pairs.push((source.clone(), target.clone()));
                targets.clear();
            }
            targets
                .iter()
                .filter(|_| !sources.is_empty())
                .for_each(|target| {
                    out.notes.push(skip(
                        target,
                        "content matches a vanished synced file, but the move \
                     pairing is ambiguous; treating as a new file",
                    ));
                });

            pairs.into_iter().for_each(|(old_key, new_path)| {
                let st = out.state.entries[&old_key].clone();
                let (dir, stem) = split_target(&new_path);
                let target_name = (dir.to_string(), stem.clone());
                if claimed_names.contains(&target_name) {
                    out.notes.push(skip(
                        &new_path,
                        "moved file's device name is already taken; treating as a new file",
                    ));
                    return;
                }
                claimed_names.insert(target_name);
                let entry = local_by_path[new_path.as_str()];
                out.actions.push(SyncAction::MoveRemote {
                    from: old_key.clone(),
                    to: new_path.clone(),
                    kind: st.kind,
                    uuid: st.uuid.clone(),
                    remote_dir: dir.to_string(),
                    name: stem,
                    local_hash: st.local_hash.clone(),
                    payload_hash: st.payload_hash.clone(),
                });
                // Re-key, absorbing the new stat stamp so the table
                // sees a clean mapped entry at the new path. The
                // executor records the authoritative state (fresh
                // device lastModified) when the move runs.
                out.state.entries.remove(&old_key);
                out.state.entries.insert(
                    new_path,
                    StateEntry {
                        local_size: entry.size,
                        local_mtime_ms: entry.mtime_ms,
                        ..st
                    },
                );
            });
        });
    }

    // ---- device moves → MoveLocal ---------------------------------------
    if can_write_local(options.mode) {
        // Consult the *adjusted* snapshot so device paths already
        // rewritten by a folder move are not re-processed.
        let adjusted_docs: HashMap<String, (String, i64, Option<String>)> = out
            .remote
            .docs
            .iter()
            .map(|d| {
                (
                    d.uuid.clone(),
                    (d.local_rel_path(), d.last_modified, d.payload_hash.clone()),
                )
            })
            .collect();
        let keys: Vec<String> = out.state.entries.keys().cloned().collect();
        keys.into_iter().for_each(|key| {
            let st = out.state.entries[&key].clone();
            let Some((new_key, last_modified, payload_hash)) = adjusted_docs.get(&st.uuid) else {
                return;
            };
            // Reconcile a rebound successor before following its moves.
            // MoveLocal records agreement, which is not yet established.
            if st.remote_pending
                || *new_key == key
                || !local_by_path.contains_key(key.as_str())
                || remote.excludes_local(&key, Some(&st))
                || remote.excludes_local(new_key, None)
            {
                return;
            }
            if local_by_path.contains_key(new_key.as_str())
                || out.state.entries.contains_key(new_key)
            {
                out.notes.push(skip(
                    &key,
                    "the device moved this document, but the target local path is occupied",
                ));
                return;
            }
            // A device move bumps lastModified; absorb the bump only
            // when the payload hash proves the content unchanged —
            // otherwise the recorded lastModified stays put so the
            // table schedules the content transfer at the new path.
            let verified_same = payload_hash.is_some() && *payload_hash == st.payload_hash;
            let recorded_lm = if verified_same {
                *last_modified
            } else {
                st.remote_last_modified
            };
            out.actions.push(SyncAction::MoveLocal {
                from: key.clone(),
                to: new_key.clone(),
                kind: st.kind,
                uuid: st.uuid.clone(),
                last_modified: recorded_lm,
                local_hash: st.local_hash.clone(),
                payload_hash: st.payload_hash.clone(),
            });
            let mut new_st = st.clone();
            new_st.remote_last_modified = recorded_lm;
            out.state.entries.remove(&key);
            out.state.entries.insert(new_key.clone(), new_st);
            if let Some(entry) = out.local.iter_mut().find(|e| e.rel_path == key) {
                entry.rel_path = new_key.clone();
            }
        });
    }

    out
}

/// Rewrite a folder prefix across the pre-pass working copies: state
/// keys, device folder paths, and doc directories all move from
/// `from_dir/...` to `to_dir/...`. Stat stamps are absorbed from the
/// local files now at the new paths so the table sees clean entries;
/// the executor re-stats when the move runs.
fn rewrite_prefix(
    out: &mut MovePrepass,
    from_dir: &str,
    to_dir: &str,
    local_by_path: &HashMap<&str, &LocalEntry>,
) {
    let rekey = |path: &str| -> Option<String> {
        path.strip_prefix(&format!("{from_dir}/"))
            .map(|sub| join_rel(to_dir, sub))
    };
    let moved_keys: Vec<String> = out
        .state
        .entries
        .keys()
        .filter(|key| path_under(from_dir, key))
        .cloned()
        .collect();
    moved_keys.into_iter().for_each(|old_key| {
        let mut st = out.state.entries.remove(&old_key).expect("key listed");
        let new_key = rekey(&old_key).expect("filtered on prefix");
        if let Some(entry) = local_by_path.get(new_key.as_str()) {
            st.local_size = entry.size;
            st.local_mtime_ms = entry.mtime_ms;
        }
        out.state.entries.insert(new_key, st);
    });
    out.remote.folders = std::mem::take(&mut out.remote.folders)
        .into_iter()
        .map(|(dir, uuid)| {
            if dir == from_dir {
                (to_dir.to_string(), uuid)
            } else if let Some(new_dir) = rekey(&dir) {
                (new_dir, uuid)
            } else {
                (dir, uuid)
            }
        })
        .collect();
    out.remote.docs.iter_mut().for_each(|doc| {
        if doc.rel_dir == from_dir {
            doc.rel_dir = to_dir.to_string();
        } else if let Some(new_dir) = rekey(&doc.rel_dir) {
            doc.rel_dir = new_dir;
        }
    });
}

/// (dir, name) pairs already occupied on the device.
fn device_taken(remote: &RemoteSnapshot) -> std::collections::HashSet<(String, String)> {
    remote
        .docs
        .iter()
        .map(|d| (d.rel_dir.clone(), d.name.clone()))
        .chain(remote.folders.keys().filter(|p| !p.is_empty()).map(|path| {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            (dir.to_string(), name.to_string())
        }))
        .collect()
}

/// Compute the ordered action plan. Pure: no I/O.
pub fn plan(
    options: SyncOptions,
    local: &[LocalEntry],
    remote: &RemoteSnapshot,
    state: &SyncState,
) -> Plan {
    // Move-detection pre-pass: from here on, `state`, `local`, and
    // `remote` are working copies with moved entries re-keyed to
    // their new paths (folder moves rewrite whole prefixes).
    let prepass = detect_moves(options, local, remote, state);
    let state = &prepass.state;
    let local = &prepass.local[..];
    let remote = &prepass.remote;

    let local_by_path: HashMap<&str, &LocalEntry> =
        local.iter().map(|e| (e.rel_path.as_str(), e)).collect();
    let docs_by_uuid: HashMap<&str, &RemoteDoc> =
        remote.docs.iter().map(|d| (d.uuid.as_str(), d)).collect();
    let mapped_uuids: std::collections::HashSet<&str> =
        state.entries.values().map(|st| st.uuid.as_str()).collect();

    let mut buckets = Buckets {
        moves: prepass.actions,
        notes: remote.skips.iter().cloned().chain(prepass.notes).collect(),
        ..Buckets::default()
    };

    // Successor candidates for dangling mappings: unmapped remote
    // docs by (dir, name).
    let unmapped_by_target: HashMap<(String, String), &RemoteDoc> = remote
        .docs
        .iter()
        .filter(|doc| !mapped_uuids.contains(doc.uuid.as_str()))
        .map(|doc| ((doc.rel_dir.clone(), doc.name.clone()), doc))
        .collect();
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Unified per-key views (BTreeMap: deterministic order).
    let mut views: BTreeMap<String, View> = state
        .entries
        .iter()
        .map(|(key, st)| {
            let mut remote_doc = docs_by_uuid.get(st.uuid.as_str()).copied();
            if remote_doc.is_none() && !remote.excludes_local(key, Some(st)) {
                // The mapped document vanished. If a same-name,
                // same-kind unmapped document took its place (typical
                // after an interrupted sync uploaded but never saved
                // state), re-adopt it instead of wedging on name
                // collisions.
                let (dir, stem) = split_target(key);
                if let Some(successor) = unmapped_by_target.get(&(dir.to_string(), stem))
                    && kind_matches(st.kind, successor.doc_type)
                    && !claimed.contains(successor.uuid.as_str())
                {
                    claimed.insert(successor.uuid.clone());
                    buckets.rebinds.push(SyncAction::Rebind {
                        path: key.clone(),
                        uuid: successor.uuid.clone(),
                        last_modified: successor.last_modified,
                    });
                    remote_doc = Some(successor);
                }
            }
            (
                key.clone(),
                View {
                    local: local_by_path.get(key.as_str()).copied(),
                    remote: remote_doc,
                    state: Some(st),
                },
            )
        })
        .collect();
    local
        .iter()
        .filter(|entry| !state.entries.contains_key(&entry.rel_path))
        .for_each(|entry| {
            views
                .entry(entry.rel_path.clone())
                .or_insert_with(View::empty)
                .local = Some(entry);
        });
    remote
        .docs
        .iter()
        .filter(|doc| !mapped_uuids.contains(doc.uuid.as_str()) && !claimed.contains(&doc.uuid))
        .for_each(|doc| {
            let key = doc.local_rel_path();
            let view = views.entry(key.clone()).or_insert_with(View::empty);
            if view.state.is_some() || view.remote.is_some() {
                buckets.notes.push(SyncAction::Skip {
                    path: key,
                    reason: "target path is already tracked by another document".to_string(),
                });
            } else {
                view.remote = Some(doc);
            }
        });

    let mut guards = Guards {
        taken: device_taken(remote),
        copy_sources: HashMap::new(),
        local_by_hash: HashMap::new(),
        upload_counts: views
            .values()
            .filter(|v| v.local.is_some() && v.remote.is_none())
            .fold(HashMap::new(), |mut counts, view| {
                let entry = view.local.expect("filtered on local");
                let (dir, stem) = split_target(&entry.rel_path);
                *counts.entry((dir.to_string(), stem)).or_default() += 1;
                counts
            }),
    };

    // First decide without copy optimizations so the indexes below
    // exclude every payload/path this plan will overwrite. Checking
    // only snapshot stamps is insufficient: an unchanged destination
    // can be updated before a later copy reads its old fingerprint.
    let mut without_copies = Buckets::default();
    views
        .iter()
        .filter(|(key, view)| !remote.excludes_local(key, view.state))
        .for_each(|(key, view)| decide(options, key, view, &guards, &mut without_copies));
    let remote_writes: std::collections::HashSet<&str> = without_copies
        .transfers
        .iter()
        .filter_map(|action| match action {
            SyncAction::UpdateRemote { uuid, .. } => Some(uuid.as_str()),
            _ => None,
        })
        .collect();
    let local_writes: std::collections::HashSet<&str> = without_copies
        .transfers
        .iter()
        .filter_map(|action| match action {
            SyncAction::Download { local, .. } => Some(local.as_str()),
            _ => None,
        })
        .collect();
    // Moves already re-keyed these snapshots, and run before transfers:
    // local sources use their new paths; device sources keep their UUIDs.
    // Deletions run after all copies, so condemned sources remain usable.
    guards.copy_sources = state
        .entries
        .values()
        .filter(|st| !remote_writes.contains(st.uuid.as_str()))
        .filter_map(|st| {
            let doc = docs_by_uuid.get(st.uuid.as_str())?;
            let hash = st.payload_hash.clone()?;
            (payload_mirrored(doc.doc_type) && !remote_stamp_changed(st, doc))
                .then(|| (hash, (st.uuid.clone(), st.kind, doc.size_bytes)))
        })
        .collect();
    guards.local_by_hash = local
        .iter()
        .filter(|entry| !local_writes.contains(entry.rel_path.as_str()))
        .filter_map(|entry| entry.hash.clone().map(|h| (h, entry.rel_path.clone())))
        .collect();

    views.iter().for_each(|(key, view)| {
        if remote.excludes_local(key, view.state) {
            buckets.notes.push(skip(
                key,
                "device entry or target is excluded; preserving the sync mapping",
            ));
        } else {
            decide(options, key, view, &guards, &mut buckets);
        }
    });

    if options.delete {
        append_dir_deletes(options, local, remote, &docs_by_uuid, &mut buckets);
    }

    // Folder creation for upload/move/copy targets, parents before
    // children.
    let mut needed: Vec<String> = buckets
        .transfers
        .iter()
        .chain(buckets.moves.iter())
        .filter_map(|action| match action {
            SyncAction::Upload { remote_dir, .. }
            | SyncAction::CopyRemote { remote_dir, .. }
            | SyncAction::MoveRemote { remote_dir, .. }
            | SyncAction::MoveRemoteFolder { remote_dir, .. } => Some(remote_dir.as_str()),
            _ => None,
        })
        .flat_map(path_prefixes)
        .filter(|dir| !remote.folders.contains_key(dir.as_str()))
        .collect();
    needed.sort();
    needed.dedup();

    Plan {
        actions: buckets
            .rebinds
            .into_iter()
            .chain(
                needed
                    .into_iter()
                    .map(|rel_dir| SyncAction::CreateRemoteFolder { rel_dir }),
            )
            .chain(buckets.moves)
            .chain(buckets.transfers)
            .chain(buckets.deletes)
            .chain(buckets.forgets)
            .chain(buckets.notes)
            .collect(),
    }
}

/// Whether a state entry's kind and a device document type describe
/// the same payload family (used for successor rebinding).
fn kind_matches(kind: DocKind, doc_type: RemoteType) -> bool {
    matches!(
        (kind, doc_type),
        (DocKind::Pdf, RemoteType::Pdf)
            | (DocKind::Epub, RemoteType::Epub)
            | (DocKind::Rmdoc, RemoteType::Notebook)
    )
}

/// With `--delete`, a folder emptied *by this plan's deletions* is
/// deleted too (children before parents), on whichever side lost the
/// documents. Only folders that lost content to this plan qualify —
/// a pre-existing empty folder was never synced, so it is never
/// touched (the "never delete what was never synced" invariant).
fn append_dir_deletes(
    options: SyncOptions,
    local: &[LocalEntry],
    remote: &RemoteSnapshot,
    docs_by_uuid: &HashMap<&str, &RemoteDoc>,
    buckets: &mut Buckets,
) {
    /// `sub` (a dir or file path) is at or below directory `dir`.
    fn within(dir: &str, sub: &str) -> bool {
        sub == dir
            || (sub.len() > dir.len() && sub.starts_with(dir) && sub.as_bytes()[dir.len()] == b'/')
    }

    let deleted_local: std::collections::HashSet<String> = buckets
        .deletes
        .iter()
        .filter_map(|action| match action {
            SyncAction::DeleteLocal { path } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let deleted_uuids: std::collections::HashSet<String> = buckets
        .deletes
        .iter()
        .filter_map(|action| match action {
            SyncAction::DeleteRemote { uuid, .. } => Some(uuid.clone()),
            _ => None,
        })
        .collect();
    // Local files that survive this plan (deleted ones excluded).
    let surviving_local: Vec<&str> = local
        .iter()
        .map(|entry| entry.rel_path.as_str())
        .filter(|path| !deleted_local.contains(*path))
        .collect();

    // Device folders emptied by this plan's `DeleteRemote` actions.
    // Reverse lexicographic order visits subfolders before their
    // parents, so a parent emptied only via a condemned child is
    // caught too.
    let mut condemned: std::collections::HashSet<&str> = std::collections::HashSet::new();
    if can_write_remote(options.mode) {
        let deleted_dirs: Vec<&str> = deleted_uuids
            .iter()
            .filter_map(|uuid| docs_by_uuid.get(uuid.as_str()))
            .map(|doc| doc.rel_dir.as_str())
            .collect();
        remote
            .folders
            .iter()
            .rev()
            .filter(|(dir, _)| !dir.is_empty())
            .for_each(|(dir, uuid)| {
                let lost_content = deleted_dirs.iter().any(|d| within(dir, d));
                let occupied = remote.docs.iter().any(|doc| {
                    !deleted_uuids.contains(&doc.uuid) && within(dir, &doc.rel_dir)
                }) || remote.skips.iter().any(|action| {
                    matches!(action, SyncAction::Skip { path, .. } if within(dir, path))
                }) || remote.folders.keys().any(|sub| {
                    sub != dir && within(dir, sub) && !condemned.contains(sub.as_str())
                }) || surviving_local.iter().any(|path| within(dir, path));
                if lost_content && !occupied {
                    condemned.insert(dir.as_str());
                    buckets.deletes.push(SyncAction::DeleteRemoteFolder {
                        rel_dir: dir.clone(),
                        uuid: uuid.clone(),
                    });
                }
            });
    }

    // Local directories emptied by this plan's `DeleteLocal` actions.
    // The planner only names candidates; the executor removes a
    // directory only if it is actually empty (files sync cannot see
    // keep it alive).
    if can_write_local(options.mode) {
        let download_targets: Vec<&str> = buckets
            .transfers
            .iter()
            .filter_map(|action| match action {
                SyncAction::Download { local, .. } => Some(local.as_str()),
                _ => None,
            })
            .collect();
        let mut candidates: Vec<String> = deleted_local
            .iter()
            .filter_map(|path| path.rsplit_once('/').map(|(dir, _)| dir))
            .flat_map(path_prefixes)
            .collect();
        candidates.sort();
        candidates.dedup();
        candidates
            .into_iter()
            .rev()
            .filter(|dir| {
                let alive_on_device =
                    remote.folders.contains_key(dir) && !condemned.contains(dir.as_str());
                !alive_on_device
                    && !surviving_local.iter().any(|path| within(dir, path))
                    && !download_targets.iter().any(|path| within(dir, path))
            })
            .for_each(|rel_dir| buckets.deletes.push(SyncAction::DeleteLocalDir { rel_dir }));
    }
}

/// `sub` (a dir or file path) is strictly below directory `dir`.
fn path_under(dir: &str, sub: &str) -> bool {
    sub.len() > dir.len() && sub.starts_with(dir) && sub.as_bytes()[dir.len()] == b'/'
}

/// All non-empty prefixes of a relative dir path: `a/b/c` → `a`,
/// `a/b`, `a/b/c`.
fn path_prefixes(path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    path.match_indices('/')
        .map(|(i, _)| path[..i].to_string())
        .chain(std::iter::once(path.to_string()))
        .collect()
}

/// The decision table: one key, three-way presence, mode + options.
fn decide(options: SyncOptions, key: &str, view: &View, guards: &Guards, out: &mut Buckets) {
    match (view.state, view.local, view.remote) {
        (Some(st), Some(entry), Some(doc)) => mapped_both(options, key, st, entry, doc, out),
        (Some(st), Some(entry), None) => mapped_remote_gone(options, key, st, entry, guards, out),
        (Some(st), None, Some(doc)) => mapped_local_gone(options, key, st, doc, out),
        // Both sides gone: the mapping is stale.
        (Some(_), None, None) => out.forgets.push(SyncAction::Forget {
            path: key.to_string(),
        }),
        (None, Some(entry), None) => {
            if can_write_remote(options.mode) {
                upload_with_guards(key, entry, guards, out);
            }
            // Pull: untracked destination files stay untouched.
        }
        (None, None, Some(doc)) => {
            if can_write_local(options.mode) {
                // Identical bytes already exist locally: copy instead
                // of downloading (hash-verified).
                if let Some(payload_hash) = &doc.payload_hash
                    && let Some(from) = guards.local_by_hash.get(payload_hash)
                    && from != key
                {
                    out.transfers.push(SyncAction::CopyLocal {
                        from: from.clone(),
                        to: key.to_string(),
                        kind: doc.doc_type.pulled_kind(),
                        uuid: doc.uuid.clone(),
                        last_modified: doc.last_modified,
                        hash: payload_hash.clone(),
                    });
                } else {
                    out.transfers.push(download(key, doc));
                }
            }
            // Push: untracked device documents stay untouched.
        }
        (None, Some(entry), Some(doc)) => collision(options, key, entry, doc, out),
        (None, None, None) => unreachable!("views are only built from existing entries"),
    }
}

/// Mapped, present on both sides: the core three-way diff.
fn mapped_both(
    options: SyncOptions,
    key: &str,
    st: &StateEntry,
    entry: &LocalEntry,
    doc: &RemoteDoc,
    out: &mut Buckets,
) {
    let stamp_local = entry.size != st.local_size || entry.mtime_ms != st.local_mtime_ms;
    let stamp_remote = remote_stamp_changed(st, doc);
    // Content verdicts override stamp verdicts when hashes are known:
    // a touched-but-identical file, or a device `lastModified` bumped
    // by annotations, is not a change worth transferring.
    let same_local_content =
        entry.hash.is_some() && st.local_hash.is_some() && entry.hash == st.local_hash;
    let same_remote_content = doc.payload_hash.is_some()
        && st.payload_hash.is_some()
        && doc.payload_hash == st.payload_hash;
    let local_changed = stamp_local && !same_local_content;
    let remote_changed = stamp_remote && !same_remote_content;
    // What each side's changes can flow into, given mode and kind.
    let push_ok = can_write_remote(options.mode) && st.kind != DocKind::Rmdoc;
    let pull_ok = can_write_local(options.mode);

    // Stamps moved but content did not (on every side that moved):
    // record the new stamps so the next run does not re-hash.
    if !local_changed && !remote_changed && (stamp_local || stamp_remote) {
        out.rebinds.push(SyncAction::Refresh {
            path: key.to_string(),
            kind: st.kind,
            uuid: doc.uuid.clone(),
            last_modified: doc.last_modified,
            local_hash: entry.hash.clone().or_else(|| st.local_hash.clone()),
            payload_hash: doc.payload_hash.clone().or_else(|| st.payload_hash.clone()),
        });
        return;
    }

    match (local_changed, remote_changed) {
        (false, false) => {}
        (true, false) => {
            if push_ok {
                out.transfers.push(update_remote(key, entry, &doc.uuid));
            } else if can_write_remote(options.mode) {
                out.notes.push(skip(
                    key,
                    "mapped .rmdoc files are pull-only (the tablet's ink wins)",
                ));
            } else {
                // Pull mode: destination drift.
                match winner(options.conflict, entry.mtime_ms, doc.last_modified) {
                    None => out
                        .notes
                        .push(conflict(key, "destination changed since last sync")),
                    Some(Winner::Remote) => out.transfers.push(download(key, doc)),
                    Some(Winner::Local) => {} // destination kept
                }
            }
        }
        (false, true) => {
            if pull_ok {
                out.transfers.push(download(key, doc));
            }
            // Push mode: nothing. Remote lastModified moves for benign
            // reasons (annotations); only local changes push.
        }
        (true, true) => match winner(options.conflict, entry.mtime_ms, doc.last_modified) {
            None => out
                .notes
                .push(conflict(key, "both sides changed since last sync")),
            Some(Winner::Local) => {
                if push_ok {
                    out.transfers.push(update_remote(key, entry, &doc.uuid));
                } else if can_write_remote(options.mode) {
                    out.notes.push(skip(
                        key,
                        "mapped .rmdoc files are pull-only (the tablet's ink wins)",
                    ));
                }
                // Pull mode: destination (local) kept.
            }
            Some(Winner::Remote) => {
                if pull_ok {
                    out.transfers.push(download(key, doc));
                }
                // Push mode: destination (device) kept.
            }
        },
    }
}

/// Mapped, but the device copy vanished.
fn mapped_remote_gone(
    options: SyncOptions,
    key: &str,
    st: &StateEntry,
    entry: &LocalEntry,
    guards: &Guards,
    out: &mut Buckets,
) {
    let local_changed = entry.size != st.local_size || entry.mtime_ms != st.local_mtime_ms;
    match options.mode {
        // Deleting the *source* is never propagated by a one-way sync;
        // recopy (rsync semantics).
        Mode::Push => upload_with_guards(key, entry, guards, out),
        Mode::Pull => {
            if !options.delete {
                return; // stale mapping kept; local file untouched
            }
            if !local_changed {
                out.deletes.push(SyncAction::DeleteLocal {
                    path: key.to_string(),
                });
            } else {
                // Deleted on the device but changed locally. A deleted
                // side has no timestamp: pass i64::MIN so Newest lets
                // the surviving change win.
                match winner(options.conflict, entry.mtime_ms, i64::MIN) {
                    None => out
                        .notes
                        .push(conflict(key, "deleted on the device but changed locally")),
                    Some(Winner::Local) => {
                        out.forgets.push(SyncAction::Forget {
                            path: key.to_string(),
                        });
                        out.notes.push(skip(
                            key,
                            "kept local copy of a document deleted on the device",
                        ));
                    }
                    Some(Winner::Remote) => out.deletes.push(SyncAction::DeleteLocal {
                        path: key.to_string(),
                    }),
                }
            }
        }
        Mode::TwoWay => {
            if !options.delete {
                upload_with_guards(key, entry, guards, out); // recopy
            } else if !local_changed {
                out.deletes.push(SyncAction::DeleteLocal {
                    path: key.to_string(),
                });
            } else {
                match winner(options.conflict, entry.mtime_ms, i64::MIN) {
                    None => out
                        .notes
                        .push(conflict(key, "deleted on the device but changed locally")),
                    Some(Winner::Local) => upload_with_guards(key, entry, guards, out),
                    Some(Winner::Remote) => out.deletes.push(SyncAction::DeleteLocal {
                        path: key.to_string(),
                    }),
                }
            }
        }
    }
}

/// Mapped, but the local copy vanished.
fn mapped_local_gone(
    options: SyncOptions,
    key: &str,
    st: &StateEntry,
    doc: &RemoteDoc,
    out: &mut Buckets,
) {
    let remote_changed = remote_stamp_changed(st, doc);
    match options.mode {
        // Source still has it: recopy.
        Mode::Pull => out.transfers.push(download(key, doc)),
        Mode::Push => {
            if !options.delete {
                return; // device keeps the document
            }
            if !remote_changed {
                out.deletes.push(SyncAction::DeleteRemote {
                    path: key.to_string(),
                    uuid: doc.uuid.clone(),
                });
            } else {
                match winner(options.conflict, i64::MIN, doc.last_modified) {
                    None => out
                        .notes
                        .push(conflict(key, "deleted locally but changed on the device")),
                    Some(Winner::Remote) => {
                        out.forgets.push(SyncAction::Forget {
                            path: key.to_string(),
                        });
                        out.notes
                            .push(skip(key, "kept device copy of a locally deleted document"));
                    }
                    Some(Winner::Local) => out.deletes.push(SyncAction::DeleteRemote {
                        path: key.to_string(),
                        uuid: doc.uuid.clone(),
                    }),
                }
            }
        }
        Mode::TwoWay => {
            if !options.delete {
                out.transfers.push(download(key, doc)); // recopy
            } else if !remote_changed {
                out.deletes.push(SyncAction::DeleteRemote {
                    path: key.to_string(),
                    uuid: doc.uuid.clone(),
                });
            } else {
                match winner(options.conflict, i64::MIN, doc.last_modified) {
                    None => out
                        .notes
                        .push(conflict(key, "deleted locally but changed on the device")),
                    Some(Winner::Remote) => out.transfers.push(download(key, doc)),
                    Some(Winner::Local) => out.deletes.push(SyncAction::DeleteRemote {
                        path: key.to_string(),
                        uuid: doc.uuid.clone(),
                    }),
                }
            }
        }
    }
}

/// Unmapped files at the same path on both sides. Policies may adopt
/// the pairing (the state file then maps them); `Skip` reports it.
fn collision(
    options: SyncOptions,
    key: &str,
    entry: &LocalEntry,
    doc: &RemoteDoc,
    out: &mut Buckets,
) {
    // Hash-verified identical content: adopt the pairing silently,
    // regardless of mode or policy (state-only, loses nothing). This
    // is what makes archive loss benign.
    if let (Some(local_hash), Some(payload_hash)) = (&entry.hash, &doc.payload_hash)
        && local_hash == payload_hash
        && kind_matches(entry.kind, doc.doc_type)
    {
        out.rebinds.push(SyncAction::Adopt {
            path: key.to_string(),
            kind: entry.kind,
            uuid: doc.uuid.clone(),
            last_modified: doc.last_modified,
            hash: local_hash.clone(),
        });
        return;
    }
    match winner(options.conflict, entry.mtime_ms, doc.last_modified) {
        None => out.notes.push(conflict(
            key,
            "exists on both sides (no sync state to match them)",
        )),
        Some(Winner::Local) => {
            if !can_write_remote(options.mode) {
                return; // pull: destination (local) kept
            }
            match (entry.kind, doc.doc_type) {
                (DocKind::Pdf, RemoteType::Pdf) | (DocKind::Epub, RemoteType::Epub) => {
                    out.transfers.push(update_remote(key, entry, &doc.uuid))
                }
                _ => out.notes.push(conflict(
                    key,
                    "cannot overwrite the device copy (handwriting or mismatched types)",
                )),
            }
        }
        Some(Winner::Remote) => {
            if can_write_local(options.mode) {
                out.transfers.push(download(key, doc));
            }
            // Push: destination (device) kept.
        }
    }
}

fn upload_with_guards(key: &str, entry: &LocalEntry, guards: &Guards, out: &mut Buckets) {
    let (dir, stem) = split_target(key);
    let target = (dir.to_string(), stem.clone());
    if guards.taken.contains(&target) {
        out.notes.push(skip(
            key,
            "an item with this name already exists on the device (no sync state to match it)",
        ));
    } else if guards.upload_counts.get(&target).copied().unwrap_or(0) > 1 {
        out.notes.push(skip(
            key,
            "multiple local files map to the same device name",
        ));
    } else if let Some((from_uuid, source_kind, size_bytes)) = entry
        .hash
        .as_ref()
        .and_then(|hash| guards.copy_sources.get(hash))
        && *source_kind == entry.kind
    {
        // Identical bytes already live on the device: one on-device
        // `cp` instead of an upload.
        out.transfers.push(SyncAction::CopyRemote {
            local: key.to_string(),
            kind: entry.kind,
            from_uuid: from_uuid.clone(),
            remote_dir: dir.to_string(),
            name: stem,
            size_bytes: *size_bytes,
            hash: entry.hash.clone().expect("checked above"),
        });
    } else {
        out.transfers.push(SyncAction::Upload {
            local: key.to_string(),
            kind: entry.kind,
            remote_dir: dir.to_string(),
            name: stem,
            hash: entry.hash.clone(),
        });
    }
}

fn update_remote(key: &str, entry: &LocalEntry, uuid: &str) -> SyncAction {
    SyncAction::UpdateRemote {
        local: key.to_string(),
        kind: entry.kind,
        uuid: uuid.to_string(),
        hash: entry.hash.clone(),
    }
}

fn download(key: &str, doc: &RemoteDoc) -> SyncAction {
    SyncAction::Download {
        local: key.to_string(),
        uuid: doc.uuid.clone(),
        doc_type: doc.doc_type,
        size_bytes: doc.size_bytes,
        last_modified: doc.last_modified,
    }
}

fn skip(key: &str, reason: &str) -> SyncAction {
    SyncAction::Skip {
        path: key.to_string(),
        reason: reason.to_string(),
    }
}

fn conflict(key: &str, reason: &str) -> SyncAction {
    SyncAction::Conflict {
        path: key.to_string(),
        reason: reason.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Result of applying a plan.
#[derive(Debug, Default)]
pub struct Outcome {
    pub folders_created: usize,
    pub uploaded: usize,
    pub updated: usize,
    pub downloaded: usize,
    /// Metadata-only device moves (zero bytes transferred).
    pub moved_remote: usize,
    /// Whole device folders moved with one metadata write.
    pub moved_folders: usize,
    /// Local file moves mirroring device-side moves.
    pub moved_local: usize,
    /// On-device payload copies (upload avoided).
    pub copied_remote: usize,
    /// Local copies of identical content (download avoided).
    pub copied_local: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    /// Emptied device folders removed (`--delete`).
    pub deleted_remote_folders: usize,
    /// Emptied local directories removed (`--delete`).
    pub deleted_local_dirs: usize,
    pub skipped: Vec<(String, String)>,
    pub conflicts: Vec<(String, String)>,
    /// Whether the device was modified (drives the xochitl restart).
    pub modified_remote: bool,
}

/// Apply a plan. State is saved after every action so an interrupted
/// sync resumes cleanly. The caller restarts xochitl afterwards if
/// `modified_remote` (once, not per file).
pub fn execute(
    client: &Client,
    progress: &dyn Progress,
    fs_side: &dyn FsEndpoint,
    plan: &Plan,
    folders: &mut BTreeMap<String, String>,
    state: &mut SyncState,
    state_path: &Path,
) -> Result<Outcome> {
    let total = plan.changes();
    let mut outcome = Outcome::default();
    let mut done = 0usize;

    plan.actions.iter().try_for_each(|action| -> Result<()> {
        if !matches!(
            action,
            SyncAction::Skip { .. } | SyncAction::Conflict { .. }
        ) {
            done += 1;
            progress.step(&format!("[{done}/{total}] {}", describe(action)));
        }
        match action {
            SyncAction::CreateRemoteFolder { rel_dir } => {
                let (parent_dir, name) = rel_dir.rsplit_once('/').unwrap_or(("", rel_dir));
                let parent_uuid = folders
                    .get(parent_dir)
                    .cloned()
                    .ok_or_else(|| Error::PathNotFound(parent_dir.to_string()))?;
                let item = client.create_folder_in(name, &parent_uuid)?;
                folders.insert(rel_dir.clone(), item.uuid);
                outcome.folders_created += 1;
                outcome.modified_remote = true;
            }
            SyncAction::Upload {
                local,
                kind,
                remote_dir,
                name,
                hash,
            } => {
                let parent_uuid = folders
                    .get(remote_dir)
                    .cloned()
                    .ok_or_else(|| Error::PathNotFound(remote_dir.to_string()))?;
                // Streamed when the fs side is a local directory;
                // buffered through memory for remote fs endpoints.
                let item = match (kind, fs_side.as_local_path(local)) {
                    (DocKind::Pdf, Some(path)) => {
                        client.store_payload(&path, &parent_uuid, name, "pdf")?
                    }
                    (DocKind::Epub, Some(path)) => {
                        client.store_payload(&path, &parent_uuid, name, "epub")?
                    }
                    (DocKind::Pdf, None) => client.store_payload_bytes(
                        &fs_side.read(local)?,
                        &parent_uuid,
                        name,
                        "pdf",
                    )?,
                    (DocKind::Epub, None) => client.store_payload_bytes(
                        &fs_side.read(local)?,
                        &parent_uuid,
                        name,
                        "epub",
                    )?,
                    (DocKind::Rmdoc, _) => {
                        let rmdoc = bundle::parse_rmdoc(&fs_side.read(local)?)?;
                        client.restore_bundle(rmdoc, &parent_uuid, name)?
                    }
                };
                // pdf/epub payloads are byte-identical to the local
                // file, so the local hash doubles as the payload hash.
                let payload = payload_mirrored_kind(*kind).then(|| hash.clone()).flatten();
                record_state_with(
                    state,
                    state_path,
                    fs_side,
                    local,
                    *kind,
                    &item.uuid,
                    item.last_modified,
                    hash.clone(),
                    payload,
                )?;
                outcome.uploaded += 1;
                outcome.modified_remote = true;
            }
            SyncAction::UpdateRemote {
                local,
                kind,
                uuid,
                hash,
            } => {
                let last_modified = match (kind, fs_side.as_local_path(local)) {
                    (DocKind::Pdf, Some(path)) => {
                        client.update_payload_from_file(uuid, "pdf", &path)?
                    }
                    (DocKind::Epub, Some(path)) => {
                        client.update_payload_from_file(uuid, "epub", &path)?
                    }
                    (DocKind::Pdf, None) => {
                        client.update_payload_bytes(uuid, "pdf", &fs_side.read(local)?)?
                    }
                    (DocKind::Epub, None) => {
                        client.update_payload_bytes(uuid, "epub", &fs_side.read(local)?)?
                    }
                    // The planner never emits this.
                    (DocKind::Rmdoc, _) => unreachable!("mapped .rmdoc files are pull-only"),
                };
                let payload = payload_mirrored_kind(*kind).then(|| hash.clone()).flatten();
                record_state_with(
                    state,
                    state_path,
                    fs_side,
                    local,
                    *kind,
                    uuid,
                    last_modified,
                    hash.clone(),
                    payload,
                )?;
                outcome.updated += 1;
                outcome.modified_remote = true;
            }
            SyncAction::Download {
                local,
                uuid,
                doc_type,
                size_bytes,
                last_modified,
            } => {
                match (doc_type, fs_side.prepare_local_write(local)?) {
                    (_, Some(dest)) => match doc_type {
                        RemoteType::Notebook => client.download_bundle_to(uuid, &dest)?,
                        RemoteType::Pdf => {
                            client.download_payload_to(uuid, "pdf", &dest, *size_bytes)?
                        }
                        RemoteType::Epub => {
                            client.download_payload_to(uuid, "epub", &dest, *size_bytes)?
                        }
                    },
                    (RemoteType::Notebook, None) => {
                        fs_side.write(local, &client.download_bundle_bytes(uuid)?)?
                    }
                    (RemoteType::Pdf, None) => fs_side.write(
                        local,
                        &client.download_payload_bytes(uuid, "pdf", *size_bytes)?,
                    )?,
                    (RemoteType::Epub, None) => fs_side.write(
                        local,
                        &client.download_payload_bytes(uuid, "epub", *size_bytes)?,
                    )?,
                }
                // Hash what we just wrote: for pdf/epub it is also
                // the payload hash (byte-identical by construction).
                let written_hash = fs_side.content_hash(local)?;
                let payload = payload_mirrored(*doc_type)
                    .then(|| written_hash.clone())
                    .flatten();
                record_state_with(
                    state,
                    state_path,
                    fs_side,
                    local,
                    doc_type.pulled_kind(),
                    uuid,
                    *last_modified,
                    written_hash,
                    payload,
                )?;
                outcome.downloaded += 1;
            }
            SyncAction::DeleteRemote { path, uuid } => {
                client.delete_document(uuid)?;
                state.entries.remove(path);
                state.save(state_path)?;
                outcome.deleted_remote += 1;
                outcome.modified_remote = true;
            }
            SyncAction::DeleteLocal { path } => {
                fs_side.remove(path)?;
                state.entries.remove(path);
                state.save(state_path)?;
                outcome.deleted_local += 1;
            }
            SyncAction::DeleteRemoteFolder { rel_dir, uuid } => {
                // The planner guarantees this folder has no children
                // left once the preceding deletions ran.
                client.delete_document(uuid)?;
                folders.remove(rel_dir);
                outcome.deleted_remote_folders += 1;
                outcome.modified_remote = true;
            }
            SyncAction::DeleteLocalDir { rel_dir } => {
                // Remove-if-empty: files sync cannot see (unsupported
                // types, dotfiles) keep the directory alive.
                if fs_side.remove_dir(rel_dir)? {
                    outcome.deleted_local_dirs += 1;
                }
            }
            SyncAction::Forget { path } => {
                state.entries.remove(path);
                state.save(state_path)?;
            }
            SyncAction::Adopt {
                path,
                kind,
                uuid,
                last_modified,
                hash,
            } => {
                let (size, mtime) = fs_side.stat(path)?;
                state.entries.insert(
                    path.clone(),
                    StateEntry {
                        uuid: uuid.clone(),
                        kind: *kind,
                        local_size: size,
                        local_mtime_ms: mtime,
                        remote_last_modified: *last_modified,
                        remote_pending: false,
                        remote_size: None,
                        local_hash: Some(hash.clone()),
                        // Verified equal to the local hash by the planner.
                        payload_hash: Some(hash.clone()),
                    },
                );
                state.save(state_path)?;
            }
            SyncAction::Refresh {
                path,
                kind,
                uuid,
                last_modified,
                local_hash,
                payload_hash,
            } => {
                let (size, mtime) = fs_side.stat(path)?;
                state.entries.insert(
                    path.clone(),
                    StateEntry {
                        uuid: uuid.clone(),
                        kind: *kind,
                        local_size: size,
                        local_mtime_ms: mtime,
                        remote_last_modified: *last_modified,
                        remote_pending: false,
                        remote_size: None,
                        local_hash: local_hash.clone(),
                        payload_hash: payload_hash.clone(),
                    },
                );
                state.save(state_path)?;
            }
            SyncAction::MoveRemote {
                from,
                to,
                kind,
                uuid,
                remote_dir,
                name,
                local_hash,
                payload_hash,
            } => {
                let parent_uuid = folders
                    .get(remote_dir)
                    .cloned()
                    .ok_or_else(|| Error::PathNotFound(remote_dir.to_string()))?;
                let last_modified = client.move_document(uuid, &parent_uuid, name)?;
                state.entries.remove(from);
                record_state_with(
                    state,
                    state_path,
                    fs_side,
                    to,
                    *kind,
                    uuid,
                    last_modified,
                    local_hash.clone(),
                    payload_hash.clone(),
                )?;
                outcome.moved_remote += 1;
                outcome.modified_remote = true;
            }
            SyncAction::MoveRemoteFolder {
                from_dir,
                to_dir,
                uuid,
                remote_dir,
                name,
            } => {
                let parent_uuid = folders
                    .get(remote_dir)
                    .cloned()
                    .ok_or_else(|| Error::PathNotFound(remote_dir.to_string()))?;
                client.move_document(uuid, &parent_uuid, name)?;
                // Re-key the folder map (the whole subtree followed).
                *folders = std::mem::take(folders)
                    .into_iter()
                    .map(|(dir, folder_uuid)| {
                        let new_dir = if dir == *from_dir {
                            to_dir.clone()
                        } else if let Some(sub) = dir.strip_prefix(&format!("{from_dir}/")) {
                            join_rel(to_dir, sub)
                        } else {
                            dir
                        };
                        (new_dir, folder_uuid)
                    })
                    .collect();
                // Re-key every archive entry under the moved prefix,
                // refreshing local stamps from the files' new homes.
                let moved_keys: Vec<String> = state
                    .entries
                    .keys()
                    .filter(|key| path_under(from_dir, key))
                    .cloned()
                    .collect();
                moved_keys
                    .into_iter()
                    .try_for_each(|old_key| -> Result<()> {
                        let mut entry = state.entries.remove(&old_key).expect("key listed");
                        let sub = old_key
                            .strip_prefix(&format!("{from_dir}/"))
                            .expect("filtered on prefix");
                        let new_key = join_rel(to_dir, sub);
                        if let Ok((size, mtime)) = fs_side.stat(&new_key) {
                            entry.local_size = size;
                            entry.local_mtime_ms = mtime;
                        }
                        state.entries.insert(new_key, entry);
                        Ok(())
                    })?;
                state.save(state_path)?;
                outcome.moved_folders += 1;
                outcome.modified_remote = true;
            }
            SyncAction::MoveLocal {
                from,
                to,
                last_modified,
                ..
            } => {
                let mut entry = state
                    .entries
                    .get(from)
                    .cloned()
                    .ok_or_else(|| Error::PathNotFound(from.clone()))?;
                fs_side.rename(from, to)?;
                // Moving does not reconcile content. Keep the local stamp
                // paired with its agreed hash so unresolved edits are still
                // detected on the next run. The planner only advances the
                // remote timestamp when the payload is verified unchanged.
                entry.remote_last_modified = *last_modified;
                state.entries.remove(from);
                state.entries.insert(to.clone(), entry);
                state.save(state_path)?;
                outcome.moved_local += 1;
            }
            SyncAction::CopyRemote {
                local,
                kind,
                from_uuid,
                remote_dir,
                name,
                size_bytes,
                hash,
            } => {
                let parent_uuid = folders
                    .get(remote_dir)
                    .cloned()
                    .ok_or_else(|| Error::PathNotFound(remote_dir.to_string()))?;
                let ext = match kind {
                    DocKind::Pdf => "pdf",
                    DocKind::Epub => "epub",
                    DocKind::Rmdoc => unreachable!("copy sources are payload-mirrored"),
                };
                let item = client.copy_payload_on_device(
                    from_uuid,
                    ext,
                    &parent_uuid,
                    name,
                    *size_bytes,
                )?;
                record_state_with(
                    state,
                    state_path,
                    fs_side,
                    local,
                    *kind,
                    &item.uuid,
                    item.last_modified,
                    Some(hash.clone()),
                    Some(hash.clone()),
                )?;
                outcome.copied_remote += 1;
                outcome.modified_remote = true;
            }
            SyncAction::CopyLocal {
                from,
                to,
                kind,
                uuid,
                last_modified,
                hash,
            } => {
                match (
                    fs_side.as_local_path(from),
                    fs_side.prepare_local_write(to)?,
                ) {
                    (Some(src), Some(dest)) => {
                        fs::copy(&src, &dest)?;
                    }
                    _ => {
                        let data = fs_side.read(from)?;
                        fs_side.write(to, &data)?;
                    }
                }
                record_state_with(
                    state,
                    state_path,
                    fs_side,
                    to,
                    *kind,
                    uuid,
                    *last_modified,
                    Some(hash.clone()),
                    Some(hash.clone()),
                )?;
                outcome.copied_local += 1;
            }
            SyncAction::Rebind { path, uuid, .. } => {
                if let Some(entry) = state.entries.get_mut(path) {
                    entry.uuid = uuid.clone();
                    // Identity is known, content agreement is not. Keep
                    // both baselines until a refresh or transfer succeeds.
                    entry.remote_pending = true;
                }
                state.save(state_path)?;
                // A rebind means a previous interrupted run wrote this
                // document to the device and likely died before its
                // xochitl restart — without one now, the document stays
                // invisible in the UI. A redundant restart is cheap;
                // an invisible document is not.
                outcome.modified_remote = true;
            }
            SyncAction::Conflict { path, reason } => {
                outcome.conflicts.push((path.clone(), reason.clone()));
            }
            SyncAction::Skip { path, reason } => {
                outcome.skipped.push((path.clone(), reason.clone()));
            }
        }
        Ok(())
    })?;

    progress.finished();
    Ok(outcome)
}

/// Payload-mirrored [`DocKind`]s (see [`payload_mirrored`]).
fn payload_mirrored_kind(kind: DocKind) -> bool {
    matches!(kind, DocKind::Pdf | DocKind::Epub)
}

/// Stat the fs-side file and persist the new state entry.
#[allow(clippy::too_many_arguments)]
fn record_state_with(
    state: &mut SyncState,
    state_path: &Path,
    fs_side: &dyn FsEndpoint,
    rel_path: &str,
    kind: DocKind,
    uuid: &str,
    remote_last_modified: i64,
    local_hash: Option<String>,
    payload_hash: Option<String>,
) -> Result<()> {
    let (size, mtime) = fs_side.stat(rel_path)?;
    state.entries.insert(
        rel_path.to_string(),
        StateEntry {
            uuid: uuid.to_string(),
            kind,
            local_size: size,
            local_mtime_ms: mtime,
            remote_last_modified,
            remote_pending: false,
            remote_size: None,
            local_hash,
            payload_hash,
        },
    );
    state.save(state_path)
}

// ---------------------------------------------------------------------------
// fs ↔ fs sync (local↔local, local↔ssh host, ssh↔ssh)
// ---------------------------------------------------------------------------
//
// Plain file mirroring between two `FsEndpoint`s. Deliberately limited
// to the same supported document types as device sync — rmu is a
// document tool, not an rsync replacement (no delta transfer; bytes
// flow through the initiating machine). Sides are called A and B; A is
// the side holding the state file. No device rules apply: identity is
// the path itself (no uuids, no rebind), and any kind may overwrite
// its counterpart.

/// One planned fs↔fs operation.
#[derive(Debug, Clone, PartialEq)]
pub enum FileAction {
    CopyToB { path: String },
    CopyToA { path: String },
    DeleteA { path: String },
    DeleteB { path: String },
    Forget { path: String },
    Conflict { path: String, reason: String },
}

/// Compute the fs↔fs plan. `Mode`/`ConflictPolicy` are interpreted
/// relative to the A (state-holding) side: `Push` = A→B, `Pull` =
/// B→A, `PreferLocal` = prefer A. Pure: no I/O.
pub fn plan_files(
    options: SyncOptions,
    a: &[LocalEntry],
    b: &[LocalEntry],
    state: &SyncState,
) -> Vec<FileAction> {
    let a_by_path: HashMap<&str, &LocalEntry> =
        a.iter().map(|e| (e.rel_path.as_str(), e)).collect();
    let b_by_path: HashMap<&str, &LocalEntry> =
        b.iter().map(|e| (e.rel_path.as_str(), e)).collect();

    let keys: std::collections::BTreeSet<&str> = a_by_path
        .keys()
        .chain(b_by_path.keys())
        .copied()
        .chain(state.entries.keys().map(String::as_str))
        .collect();

    let can_a = can_write_local(options.mode); // A plays the "local" role
    let can_b = can_write_remote(options.mode);

    keys.iter()
        .filter_map(|&key| {
            let entry_a = a_by_path.get(key).copied();
            let entry_b = b_by_path.get(key).copied();
            let st = state.entries.get(key);
            decide_file(options, can_a, can_b, key, entry_a, entry_b, st)
        })
        .collect()
}

/// Side-agnostic outcome of the pair decision table; mapped to
/// concrete actions by [`plan_files`] (fs↔fs) and [`plan_docs`]
/// (tablet↔tablet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    CopyToB,
    CopyToA,
    DeleteA,
    DeleteB,
    Forget,
    Conflict(&'static str),
}

/// The shared three-way decision table for symmetric pairings (two
/// file trees, or two tablets): each side is just (present, changed,
/// version timestamp). `mapped` = a state entry exists for this key.
fn decide_pair(
    options: SyncOptions,
    can_a: bool,
    can_b: bool,
    a: Option<(bool, i64)>,
    b: Option<(bool, i64)>,
    mapped: bool,
) -> Option<Disposition> {
    use Disposition::*;
    match (mapped, a, b) {
        (true, Some((a_changed, a_ms)), Some((b_changed, b_ms))) => {
            match (a_changed, b_changed) {
                (false, false) => None,
                (true, false) => {
                    if can_b {
                        Some(CopyToB)
                    } else {
                        // One-way toward A: destination (A) drifted.
                        match winner(options.conflict, a_ms, b_ms) {
                            None => Some(Conflict("destination changed since last sync")),
                            Some(Winner::Remote) => Some(CopyToA),
                            Some(Winner::Local) => None, // destination kept
                        }
                    }
                }
                (false, true) => {
                    if can_a {
                        Some(CopyToA)
                    } else {
                        match winner(options.conflict, a_ms, b_ms) {
                            None => Some(Conflict("destination changed since last sync")),
                            Some(Winner::Local) => Some(CopyToB),
                            Some(Winner::Remote) => None,
                        }
                    }
                }
                (true, true) => match winner(options.conflict, a_ms, b_ms) {
                    None => Some(Conflict("both sides changed since last sync")),
                    Some(Winner::Local) if can_b => Some(CopyToB),
                    Some(Winner::Remote) if can_a => Some(CopyToA),
                    Some(_) => None, // winner is the untouchable destination: kept
                },
            }
        }
        // B deleted it. Deletions, like copies, may only write to a
        // destination allowed by the mode; a missing destination must
        // never cause a one-way source to be deleted.
        (true, Some((a_changed, a_ms)), None) => {
            if can_b && !options.delete {
                return Some(CopyToB); // recopy
            }
            if !options.delete {
                return None; // one-way toward A without --delete: stale state kept
            }
            if !a_changed {
                return can_a.then_some(DeleteA);
            }
            match winner(options.conflict, a_ms, i64::MIN) {
                None => Some(Conflict("deleted on one side but changed on the other")),
                Some(Winner::Local) if can_b => Some(CopyToB),
                Some(Winner::Local) => Some(Forget),
                Some(Winner::Remote) => can_a.then_some(DeleteA),
            }
        }
        // A deleted it (mirror).
        (true, None, Some((b_changed, b_ms))) => {
            if can_a && !options.delete {
                return Some(CopyToA);
            }
            if !options.delete {
                return None;
            }
            if !b_changed {
                return can_b.then_some(DeleteB);
            }
            match winner(options.conflict, i64::MIN, b_ms) {
                None => Some(Conflict("deleted on one side but changed on the other")),
                Some(Winner::Remote) if can_a => Some(CopyToA),
                Some(Winner::Remote) => Some(Forget),
                Some(Winner::Local) => can_b.then_some(DeleteB),
            }
        }
        (true, None, None) => Some(Forget),
        (false, Some(_), None) => can_b.then_some(CopyToB),
        (false, None, Some(_)) => can_a.then_some(CopyToA),
        // Unmapped collision: policies adopt, skip reports.
        (false, Some((_, a_ms)), Some((_, b_ms))) => match winner(options.conflict, a_ms, b_ms) {
            None => Some(Conflict(
                "exists on both sides (no sync state to match them)",
            )),
            Some(Winner::Local) => can_b.then_some(CopyToB),
            Some(Winner::Remote) => can_a.then_some(CopyToA),
        },
        (false, None, None) => unreachable!("keys come from existing entries"),
    }
}

fn decide_file(
    options: SyncOptions,
    can_a: bool,
    can_b: bool,
    key: &str,
    a: Option<&LocalEntry>,
    b: Option<&LocalEntry>,
    st: Option<&StateEntry>,
) -> Option<FileAction> {
    let a_view = a.map(|e| {
        let changed =
            st.is_none_or(|st| e.size != st.local_size || e.mtime_ms != st.local_mtime_ms);
        (changed, e.mtime_ms)
    });
    let b_view = b.map(|e| {
        let changed = st.is_none_or(|st| {
            e.mtime_ms != st.remote_last_modified || st.remote_size.is_some_and(|s| e.size != s)
        });
        (changed, e.mtime_ms)
    });
    let path = key.to_string();
    decide_pair(options, can_a, can_b, a_view, b_view, st.is_some()).map(|d| match d {
        Disposition::CopyToB => FileAction::CopyToB { path },
        Disposition::CopyToA => FileAction::CopyToA { path },
        Disposition::DeleteA => FileAction::DeleteA { path },
        Disposition::DeleteB => FileAction::DeleteB { path },
        Disposition::Forget => FileAction::Forget { path },
        Disposition::Conflict(reason) => FileAction::Conflict {
            path,
            reason: reason.to_string(),
        },
    })
}

/// Result of applying an fs↔fs plan.
#[derive(Debug, Default)]
pub struct FileOutcome {
    pub copied_to_a: usize,
    pub copied_to_b: usize,
    pub deleted_a: usize,
    pub deleted_b: usize,
    pub conflicts: Vec<(String, String)>,
}

/// Apply an fs↔fs plan; `a` is the state-holding side. State is saved
/// after every action (interrupted syncs resume cleanly).
pub fn execute_files(
    a: &dyn FsEndpoint,
    b: &dyn FsEndpoint,
    progress: &dyn Progress,
    plan: &[FileAction],
    state: &mut SyncState,
    state_path: &Path,
) -> Result<FileOutcome> {
    let total = plan
        .iter()
        .filter(|action| !matches!(action, FileAction::Conflict { .. }))
        .count();
    let mut outcome = FileOutcome::default();
    let mut done = 0usize;

    let copy = |state: &mut SyncState, path: &str, to_b: bool| -> Result<()> {
        let (source, destination) = if to_b { (a, b) } else { (b, a) };
        let source_stamp = source.stat(path)?;
        let data = source.read(path)?;
        let verify_source = || -> Result<()> {
            if source.stat(path)? != source_stamp || data.len() as u64 != source_stamp.0 {
                return Err(std::io::Error::other(format!(
                    "source changed during sync transfer: {path}; archive entry not updated"
                ))
                .into());
            }
            Ok(())
        };
        // Reject a changing read before touching the destination. Recheck after
        // the write too: a source edit during that write must remain pending.
        verify_source()?;
        destination.write(path, &data)?;
        let destination_stamp = destination.stat(path)?;
        verify_source()?;
        // Record the captured version, never a later source stamp. Even an edit
        // after verification will then differ from the archived source version.
        let ((a_size, a_mtime), (b_size, b_mtime)) = if to_b {
            (source_stamp, destination_stamp)
        } else {
            (destination_stamp, source_stamp)
        };
        let kind = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .and_then(|ext| DocKind::from_extension(&ext))
            .unwrap_or(DocKind::Pdf);
        state.entries.insert(
            path.to_string(),
            StateEntry {
                uuid: String::new(),
                kind,
                local_size: a_size,
                local_mtime_ms: a_mtime,
                remote_last_modified: b_mtime,
                remote_pending: false,
                remote_size: Some(b_size),
                local_hash: None,
                payload_hash: None,
            },
        );
        state.save(state_path)
    };

    plan.iter().try_for_each(|action| -> Result<()> {
        if !matches!(action, FileAction::Conflict { .. }) {
            done += 1;
            progress.step(&format!("[{done}/{total}] {}", describe_file(action)));
        }
        match action {
            FileAction::CopyToB { path } => {
                copy(state, path, true)?;
                outcome.copied_to_b += 1;
            }
            FileAction::CopyToA { path } => {
                copy(state, path, false)?;
                outcome.copied_to_a += 1;
            }
            FileAction::DeleteA { path } => {
                a.remove(path)?;
                state.entries.remove(path);
                state.save(state_path)?;
                outcome.deleted_a += 1;
            }
            FileAction::DeleteB { path } => {
                b.remove(path)?;
                state.entries.remove(path);
                state.save(state_path)?;
                outcome.deleted_b += 1;
            }
            FileAction::Forget { path } => {
                state.entries.remove(path);
                state.save(state_path)?;
            }
            FileAction::Conflict { path, reason } => {
                outcome.conflicts.push((path.clone(), reason.clone()));
            }
        }
        Ok(())
    })?;

    progress.finished();
    Ok(outcome)
}

/// One-line description of an fs↔fs action for `--dry-run`/progress.
pub fn describe_file(action: &FileAction) -> String {
    match action {
        FileAction::CopyToB { path } => format!("copy → B  {path}"),
        FileAction::CopyToA { path } => format!("copy → A  {path}"),
        FileAction::DeleteA { path } => format!("delete A  {path}"),
        FileAction::DeleteB { path } => format!("delete B  {path}"),
        FileAction::Forget { path } => format!("forget    {path} (stale sync mapping)"),
        FileAction::Conflict { path, reason } => format!("conflict  {path} ({reason})"),
    }
}

// ---------------------------------------------------------------------------
// tablet ↔ tablet sync
// ---------------------------------------------------------------------------
//
// Copies logical documents between two devices via `.rmdoc` bundle
// streaming (full fidelity: notebooks, annotations, everything).
// Identity is the logical path (folder path + name); each side has its
// own UUID for a document, recorded in a pair-state file kept on the
// initiating computer. Change detection: a side changed when its UUID
// *or* `lastModified` differs from the recorded one — a replaced
// document is just a changed document. "Updating" a document replaces
// it wholesale (delete + fresh restore): bundles carry everything, and
// ink cannot be merged anyway.

/// Last-synced knowledge for one document across a tablet pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairEntry {
    pub uuid_a: String,
    pub lm_a: i64,
    pub uuid_b: String,
    pub lm_b: i64,
}

/// Pair-state file for tablet↔tablet sync, keyed by logical path.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PairState {
    pub version: u32,
    pub entries: BTreeMap<String, PairEntry>,
}

impl PairState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                version: 1,
                entries: BTreeMap::new(),
            });
        }
        let text = fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(|source| Error::Json {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            path: path.display().to_string(),
            source,
        })?;
        write_atomically(path, text.as_bytes())
    }
}

/// Where the pair-state for two tablet endpoints lives on the
/// initiating computer: `$XDG_STATE_HOME/rmu/` (or `~/.local/state/rmu/`),
/// keyed order-independently by the endpoint pair.
pub fn pair_state_path(endpoint_a: &str, endpoint_b: &str) -> PathBuf {
    state_dir().join(format!(
        "sync-pair-{}.json",
        pair_key_hash(endpoint_a, endpoint_b)
    ))
}

/// Where the incremental listing cache for a device lives (keyed by
/// ssh destination + xochitl dir, so profiles never collide).
pub fn listing_cache_path(destination: &str, xochitl_dir: &str) -> PathBuf {
    state_dir().join(format!(
        "listing-{:016x}.json",
        fnv1a(&format!("{destination}\n{xochitl_dir}"))
    ))
}

/// The rmu state directory: `$XDG_STATE_HOME/rmu` (or
/// `~/.local/state/rmu`).
fn state_dir() -> PathBuf {
    // Empty variables count as set here (unlike the hub's cache dir).
    xdg::base_dir(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        ".local/state",
    )
    .unwrap_or_else(|| PathBuf::from("."))
    .join("rmu")
}

/// Order-independent stable hash of an endpoint pair.
fn pair_key_hash(label_a: &str, label_b: &str) -> String {
    let (first, second) = if label_a <= label_b {
        (label_a, label_b)
    } else {
        (label_b, label_a)
    };
    format!("{:016x}", fnv1a(&format!("{first}\n{second}")))
}

/// FNV-1a: tiny, dependency-free, and stable across releases (unlike
/// `DefaultHasher`, whose output is not guaranteed between versions).
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The two tablets of a tablet↔tablet sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocSide {
    A,
    B,
}

impl DocAction {
    /// Whether executing it writes to tablet `side` (a copy that
    /// replaces a document also deletes the old one there).
    pub fn writes(&self, side: DocSide) -> bool {
        match self {
            DocAction::CreateFolderOnA { .. }
            | DocAction::CopyToA { .. }
            | DocAction::DeleteOnA { .. } => side == DocSide::A,
            DocAction::CreateFolderOnB { .. }
            | DocAction::CopyToB { .. }
            | DocAction::DeleteOnB { .. } => side == DocSide::B,
            DocAction::Forget { .. } | DocAction::Conflict { .. } => false,
        }
    }

    /// Whether it deletes a document on tablet `side` (not counting a
    /// copy replacing its older version).
    pub fn deletes(&self, side: DocSide) -> bool {
        match self {
            DocAction::DeleteOnA { .. } => side == DocSide::A,
            DocAction::DeleteOnB { .. } => side == DocSide::B,
            _ => false,
        }
    }
}

/// One planned tablet↔tablet operation.
#[derive(Debug, Clone, PartialEq)]
pub enum DocAction {
    CreateFolderOnA {
        rel_dir: String,
    },
    CreateFolderOnB {
        rel_dir: String,
    },
    CopyToB {
        key: String,
        rel_dir: String,
        name: String,
        from_uuid: String,
        from_lm: i64,
        /// Existing destination document to replace (delete first).
        replace_uuid: Option<String>,
    },
    CopyToA {
        key: String,
        rel_dir: String,
        name: String,
        from_uuid: String,
        from_lm: i64,
        replace_uuid: Option<String>,
    },
    DeleteOnA {
        key: String,
        uuid: String,
    },
    DeleteOnB {
        key: String,
        uuid: String,
    },
    Forget {
        key: String,
    },
    Conflict {
        key: String,
        reason: String,
    },
}

/// Compute the tablet↔tablet plan. `Mode`/`ConflictPolicy` are
/// interpreted relative to side A (`Push` = A→B, `PreferLocal` =
/// prefer A). Pure: no I/O.
pub fn plan_docs(
    options: SyncOptions,
    a: &RemoteSnapshot,
    b: &RemoteSnapshot,
    state: &PairState,
) -> Vec<DocAction> {
    let a_by_key: HashMap<String, &RemoteDoc> = a
        .docs
        .iter()
        .map(|doc| (join_rel(&doc.rel_dir, &doc.name), doc))
        .collect();
    let b_by_key: HashMap<String, &RemoteDoc> = b
        .docs
        .iter()
        .map(|doc| (join_rel(&doc.rel_dir, &doc.name), doc))
        .collect();

    let keys: std::collections::BTreeSet<&str> = a_by_key
        .keys()
        .chain(b_by_key.keys())
        .map(String::as_str)
        .chain(state.entries.keys().map(String::as_str))
        .collect();

    let can_a = can_write_local(options.mode); // A plays the "local" role
    let can_b = can_write_remote(options.mode);

    let mut copies: Vec<DocAction> = Vec::new();
    let mut deletes: Vec<DocAction> = Vec::new();
    let mut forgets: Vec<DocAction> = Vec::new();
    let mut notes: Vec<DocAction> = Vec::new();

    keys.iter().for_each(|&key| {
        let doc_a = a_by_key.get(key).copied();
        let doc_b = b_by_key.get(key).copied();
        let st = state.entries.get(key);
        if a.excludes_path(key)
            || b.excludes_path(key)
            || st.is_some_and(|s| {
                a.excluded_uuids.contains(&s.uuid_a) || b.excluded_uuids.contains(&s.uuid_b)
            })
        {
            notes.push(DocAction::Conflict {
                key: key.to_string(),
                reason: "device entry or target is excluded; preserving the sync mapping"
                    .to_string(),
            });
            return;
        }
        // Changed = replaced (uuid drift) or modified (lastModified).
        let a_view = doc_a.map(|d| {
            let changed = st.is_none_or(|s| s.uuid_a != d.uuid || s.lm_a != d.last_modified);
            (changed, d.last_modified)
        });
        let b_view = doc_b.map(|d| {
            let changed = st.is_none_or(|s| s.uuid_b != d.uuid || s.lm_b != d.last_modified);
            (changed, d.last_modified)
        });
        let Some(disposition) = decide_pair(options, can_a, can_b, a_view, b_view, st.is_some())
        else {
            return;
        };
        let key = key.to_string();
        match disposition {
            Disposition::CopyToB => {
                let doc = doc_a.expect("CopyToB requires a document on A");
                copies.push(DocAction::CopyToB {
                    key,
                    rel_dir: doc.rel_dir.clone(),
                    name: doc.name.clone(),
                    from_uuid: doc.uuid.clone(),
                    from_lm: doc.last_modified,
                    replace_uuid: doc_b.map(|d| d.uuid.clone()),
                });
            }
            Disposition::CopyToA => {
                let doc = doc_b.expect("CopyToA requires a document on B");
                copies.push(DocAction::CopyToA {
                    key,
                    rel_dir: doc.rel_dir.clone(),
                    name: doc.name.clone(),
                    from_uuid: doc.uuid.clone(),
                    from_lm: doc.last_modified,
                    replace_uuid: doc_a.map(|d| d.uuid.clone()),
                });
            }
            Disposition::DeleteA => deletes.push(DocAction::DeleteOnA {
                key,
                uuid: doc_a
                    .expect("DeleteA requires a document on A")
                    .uuid
                    .clone(),
            }),
            Disposition::DeleteB => deletes.push(DocAction::DeleteOnB {
                key,
                uuid: doc_b
                    .expect("DeleteB requires a document on B")
                    .uuid
                    .clone(),
            }),
            Disposition::Forget => forgets.push(DocAction::Forget { key }),
            Disposition::Conflict(reason) => notes.push(DocAction::Conflict {
                key,
                reason: reason.to_string(),
            }),
        }
    });

    // Folder creation per destination side, parents before children.
    let folder_actions = |snapshot: &RemoteSnapshot, dirs: Vec<&str>| -> Vec<String> {
        let mut needed: Vec<String> = dirs
            .into_iter()
            .flat_map(path_prefixes)
            .filter(|dir| !snapshot.folders.contains_key(dir.as_str()))
            .collect();
        needed.sort();
        needed.dedup();
        needed
    };
    let dirs_to_b: Vec<&str> = copies
        .iter()
        .filter_map(|action| match action {
            DocAction::CopyToB { rel_dir, .. } => Some(rel_dir.as_str()),
            _ => None,
        })
        .collect();
    let dirs_to_a: Vec<&str> = copies
        .iter()
        .filter_map(|action| match action {
            DocAction::CopyToA { rel_dir, .. } => Some(rel_dir.as_str()),
            _ => None,
        })
        .collect();

    folder_actions(b, dirs_to_b)
        .into_iter()
        .map(|rel_dir| DocAction::CreateFolderOnB { rel_dir })
        .chain(
            folder_actions(a, dirs_to_a)
                .into_iter()
                .map(|rel_dir| DocAction::CreateFolderOnA { rel_dir }),
        )
        .chain(copies)
        .chain(deletes)
        .chain(forgets)
        .chain(notes)
        .chain(
            a.skips
                .iter()
                .chain(b.skips.iter())
                .filter_map(|skip| match skip {
                    SyncAction::Skip { path, reason } => Some(DocAction::Conflict {
                        key: path.clone(),
                        reason: reason.clone(),
                    }),
                    _ => None,
                }),
        )
        .collect()
}

/// Result of applying a tablet↔tablet plan.
#[derive(Debug, Default)]
pub struct DocOutcome {
    pub copied_to_a: usize,
    pub copied_to_b: usize,
    pub deleted_a: usize,
    pub deleted_b: usize,
    pub folders_created: usize,
    pub conflicts: Vec<(String, String)>,
    pub modified_a: bool,
    pub modified_b: bool,
}

/// Apply a tablet↔tablet plan. Pair state is saved after every action.
/// The caller restarts xochitl on each modified device afterwards.
#[allow(clippy::too_many_arguments)]
pub fn execute_docs(
    client_a: &Client,
    client_b: &Client,
    progress: &dyn Progress,
    plan: &[DocAction],
    folders_a: &mut BTreeMap<String, String>,
    folders_b: &mut BTreeMap<String, String>,
    state: &mut PairState,
    state_path: &Path,
) -> Result<DocOutcome> {
    let total = plan
        .iter()
        .filter(|action| !matches!(action, DocAction::Conflict { .. }))
        .count();
    let mut outcome = DocOutcome::default();
    let mut done = 0usize;

    fn create_folder(
        client: &Client,
        folders: &mut BTreeMap<String, String>,
        rel_dir: &str,
    ) -> Result<()> {
        let (parent_dir, name) = rel_dir.rsplit_once('/').unwrap_or(("", rel_dir));
        let parent_uuid = folders
            .get(parent_dir)
            .cloned()
            .ok_or_else(|| Error::PathNotFound(parent_dir.to_string()))?;
        let item = client.create_folder_in(name, &parent_uuid)?;
        folders.insert(rel_dir.to_string(), item.uuid);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_doc(
        from: &Client,
        to: &Client,
        to_folders: &BTreeMap<String, String>,
        rel_dir: &str,
        name: &str,
        from_uuid: &str,
        replace_uuid: Option<&str>,
    ) -> Result<Item> {
        let bundle_bytes = from.download_bundle_bytes(from_uuid)?;
        let rmdoc = bundle::parse_rmdoc(&bundle_bytes)?;
        if let Some(old_uuid) = replace_uuid {
            to.delete_document(old_uuid)?;
        }
        let parent_uuid = to_folders
            .get(rel_dir)
            .cloned()
            .ok_or_else(|| Error::PathNotFound(rel_dir.to_string()))?;
        to.restore_bundle(rmdoc, &parent_uuid, name)
    }

    plan.iter().try_for_each(|action| -> Result<()> {
        if !matches!(action, DocAction::Conflict { .. }) {
            done += 1;
            progress.step(&format!("[{done}/{total}] {}", describe_doc(action)));
        }
        match action {
            DocAction::CreateFolderOnA { rel_dir } => {
                create_folder(client_a, folders_a, rel_dir)?;
                outcome.folders_created += 1;
                outcome.modified_a = true;
            }
            DocAction::CreateFolderOnB { rel_dir } => {
                create_folder(client_b, folders_b, rel_dir)?;
                outcome.folders_created += 1;
                outcome.modified_b = true;
            }
            DocAction::CopyToB {
                key,
                rel_dir,
                name,
                from_uuid,
                from_lm,
                replace_uuid,
            } => {
                let item = copy_doc(
                    client_a,
                    client_b,
                    folders_b,
                    rel_dir,
                    name,
                    from_uuid,
                    replace_uuid.as_deref(),
                )?;
                state.entries.insert(
                    key.clone(),
                    PairEntry {
                        uuid_a: from_uuid.clone(),
                        lm_a: *from_lm,
                        uuid_b: item.uuid,
                        lm_b: item.last_modified,
                    },
                );
                state.save(state_path)?;
                outcome.copied_to_b += 1;
                outcome.modified_b = true;
            }
            DocAction::CopyToA {
                key,
                rel_dir,
                name,
                from_uuid,
                from_lm,
                replace_uuid,
            } => {
                let item = copy_doc(
                    client_b,
                    client_a,
                    folders_a,
                    rel_dir,
                    name,
                    from_uuid,
                    replace_uuid.as_deref(),
                )?;
                state.entries.insert(
                    key.clone(),
                    PairEntry {
                        uuid_a: item.uuid,
                        lm_a: item.last_modified,
                        uuid_b: from_uuid.clone(),
                        lm_b: *from_lm,
                    },
                );
                state.save(state_path)?;
                outcome.copied_to_a += 1;
                outcome.modified_a = true;
            }
            DocAction::DeleteOnA { key, uuid } => {
                client_a.delete_document(uuid)?;
                state.entries.remove(key);
                state.save(state_path)?;
                outcome.deleted_a += 1;
                outcome.modified_a = true;
            }
            DocAction::DeleteOnB { key, uuid } => {
                client_b.delete_document(uuid)?;
                state.entries.remove(key);
                state.save(state_path)?;
                outcome.deleted_b += 1;
                outcome.modified_b = true;
            }
            DocAction::Forget { key } => {
                state.entries.remove(key);
                state.save(state_path)?;
            }
            DocAction::Conflict { key, reason } => {
                outcome.conflicts.push((key.clone(), reason.clone()));
            }
        }
        Ok(())
    })?;

    progress.finished();
    Ok(outcome)
}

/// One-line description of a tablet↔tablet action.
pub fn describe_doc(action: &DocAction) -> String {
    match action {
        DocAction::CreateFolderOnA { rel_dir } => format!("mkdir A   {rel_dir}/"),
        DocAction::CreateFolderOnB { rel_dir } => format!("mkdir B   {rel_dir}/"),
        DocAction::CopyToB {
            key, replace_uuid, ..
        } => match replace_uuid {
            Some(_) => format!("replace → B  {key}"),
            None => format!("copy → B  {key}"),
        },
        DocAction::CopyToA {
            key, replace_uuid, ..
        } => match replace_uuid {
            Some(_) => format!("replace → A  {key}"),
            None => format!("copy → A  {key}"),
        },
        DocAction::DeleteOnA { key, .. } => format!("delete A  {key}"),
        DocAction::DeleteOnB { key, .. } => format!("delete B  {key}"),
        DocAction::Forget { key } => format!("forget    {key} (stale sync mapping)"),
        DocAction::Conflict { key, reason } => format!("conflict  {key} ({reason})"),
    }
}

/// Human-readable one-line description of an action (used for progress
/// and `--dry-run` output).
pub fn describe(action: &SyncAction) -> String {
    match action {
        SyncAction::CreateRemoteFolder { rel_dir } => format!("mkdir    {rel_dir}/"),
        SyncAction::Upload { local, kind, .. } => match kind {
            DocKind::Rmdoc => format!("restore  {local}"),
            _ => format!("upload   {local}"),
        },
        SyncAction::UpdateRemote { local, .. } => format!("update   {local}"),
        SyncAction::Download {
            local, doc_type, ..
        } => match doc_type {
            RemoteType::Notebook => format!("download {local} (notebook bundle)"),
            _ => format!("download {local}"),
        },
        SyncAction::DeleteRemote { path, .. } => format!("delete   {path} (on device)"),
        SyncAction::DeleteLocal { path } => format!("delete   {path} (local)"),
        SyncAction::DeleteRemoteFolder { rel_dir, .. } => {
            format!("rmdir    {rel_dir}/ (emptied, on device)")
        }
        SyncAction::DeleteLocalDir { rel_dir } => {
            format!("rmdir    {rel_dir}/ (emptied, local, if empty)")
        }
        SyncAction::Forget { path } => format!("forget   {path} (stale sync mapping)"),
        SyncAction::Rebind { path, .. } => {
            format!("rebind   {path} (re-linked to replacement on device)")
        }
        SyncAction::Adopt { path, .. } => {
            format!("adopt    {path} (identical content on both sides; state only)")
        }
        SyncAction::Refresh { path, .. } => {
            format!("refresh  {path} (content unchanged; state only)")
        }
        SyncAction::MoveRemote { from, to, .. } => {
            format!("move     {from} -> {to} (on device, metadata only)")
        }
        SyncAction::MoveRemoteFolder {
            from_dir, to_dir, ..
        } => {
            format!("move     {from_dir}/ -> {to_dir}/ (device folder, one metadata write)")
        }
        SyncAction::MoveLocal { from, to, .. } => {
            format!("move     {from} -> {to} (local)")
        }
        SyncAction::CopyRemote { local, .. } => {
            format!("copy     {local} (device-side copy of identical payload)")
        }
        SyncAction::CopyLocal { from, to, .. } => {
            format!("copy     {from} -> {to} (local copy of identical content)")
        }
        SyncAction::Conflict { path, reason } => format!("conflict {path} ({reason})"),
        SyncAction::Skip { path, reason } => format!("skip     {path} ({reason})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xochitl::ItemKind;

    fn folder(uuid: &str, name: &str, parent: &str) -> Item {
        Item {
            uuid: uuid.to_string(),
            visible_name: name.to_string(),
            parent: parent.to_string(),
            kind: ItemKind::Folder,
            file_type: None,
            created_time: 0,
            last_modified: 0,
            size_bytes: None,
        }
    }

    fn doc(uuid: &str, name: &str, parent: &str, file_type: &str, modified: i64) -> Item {
        Item {
            uuid: uuid.to_string(),
            visible_name: name.to_string(),
            parent: parent.to_string(),
            kind: ItemKind::Document,
            file_type: Some(file_type.to_string()),
            created_time: 0,
            last_modified: modified,
            size_bytes: Some(100),
        }
    }

    fn local(rel: &str, kind: DocKind, size: u64, mtime: i64) -> LocalEntry {
        LocalEntry {
            rel_path: rel.to_string(),
            kind,
            size,
            mtime_ms: mtime,
            hash: None,
        }
    }

    fn with_hash(mut entry: LocalEntry, hash: &str) -> LocalEntry {
        entry.hash = Some(hash.to_string());
        entry
    }

    fn entry(uuid: &str, kind: DocKind, size: u64, mtime: i64, remote: i64) -> StateEntry {
        StateEntry {
            uuid: uuid.to_string(),
            kind,
            local_size: size,
            local_mtime_ms: mtime,
            remote_last_modified: remote,
            remote_pending: false,
            remote_size: None,
            local_hash: None,
            payload_hash: None,
        }
    }

    fn entry_hashed(
        uuid: &str,
        kind: DocKind,
        size: u64,
        mtime: i64,
        remote: i64,
        local_hash: &str,
        payload_hash: &str,
    ) -> StateEntry {
        StateEntry {
            local_hash: Some(local_hash.to_string()),
            payload_hash: Some(payload_hash.to_string()),
            ..entry(uuid, kind, size, mtime, remote)
        }
    }

    fn push() -> SyncOptions {
        SyncOptions {
            mode: Mode::Push,
            delete: false,
            conflict: ConflictPolicy::Skip,
        }
    }

    fn pull() -> SyncOptions {
        SyncOptions {
            mode: Mode::Pull,
            delete: false,
            conflict: ConflictPolicy::Skip,
        }
    }

    fn two_way() -> SyncOptions {
        SyncOptions {
            mode: Mode::TwoWay,
            delete: false,
            conflict: ConflictPolicy::Skip,
        }
    }

    fn with_delete(mut options: SyncOptions) -> SyncOptions {
        options.delete = true;
        options
    }

    fn with_policy(mut options: SyncOptions, conflict: ConflictPolicy) -> SyncOptions {
        options.conflict = conflict;
        options
    }

    fn skips(plan: &Plan) -> Vec<&str> {
        plan.actions
            .iter()
            .filter_map(|a| match a {
                SyncAction::Skip { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect()
    }

    fn conflicts(plan: &Plan) -> Vec<&str> {
        plan.actions
            .iter()
            .filter_map(|a| match a {
                SyncAction::Conflict { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect()
    }

    // ---- endpoint parsing -------------------------------------------------

    #[test]
    fn endpoint_parsing_follows_scp_rules() {
        assert_eq!(
            parse_endpoint("./books"),
            Endpoint::Local("./books".to_string())
        );
        assert_eq!(
            parse_endpoint("/abs/path"),
            Endpoint::Local("/abs/path".to_string())
        );
        assert_eq!(
            parse_endpoint("remarkable:/books"),
            Endpoint::Remote {
                destination: "remarkable".to_string(),
                path: "/books".to_string()
            }
        );
        assert_eq!(
            parse_endpoint("root@10.11.99.1:Books/Math"),
            Endpoint::Remote {
                destination: "root@10.11.99.1".to_string(),
                path: "Books/Math".to_string()
            }
        );
        // Empty path after ':' = endpoint root.
        assert_eq!(
            parse_endpoint("rm:"),
            Endpoint::Remote {
                destination: "rm".to_string(),
                path: String::new()
            }
        );
        // Colon after a slash: local (scp rule).
        assert_eq!(
            parse_endpoint("./weird:name"),
            Endpoint::Local("./weird:name".to_string())
        );
        // Leading colon: local.
        assert_eq!(
            parse_endpoint(":oops"),
            Endpoint::Local(":oops".to_string())
        );
    }

    // ---- remote snapshot --------------------------------------------------

    #[test]
    fn snapshot_scopes_to_root_and_flags_duplicates() {
        let items = vec![
            folder("b", "Books", ""),
            doc("d1", "Doc", "b", "pdf", 5),
            doc("d2", "Doc", "b", "epub", 6), // duplicate sibling name
            doc("d3", "Note", "b", "notebook", 7),
            doc("bad", "we/ird", "b", "pdf", 8), // unusable name
            doc("outside", "Elsewhere", "", "pdf", 9),
        ];
        let snapshot = remote_snapshot(&items, "b");
        let names: Vec<&str> = snapshot.docs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Note"]);
        assert_eq!(snapshot.docs[0].local_rel_path(), "Note.rmdoc");
        assert_eq!(snapshot.skips.len(), 3); // two duplicates + one bad name
        assert_eq!(snapshot.folders[""], "b");
        assert!(snapshot.excluded_uuids.contains("d1"));
        assert!(snapshot.excluded_uuids.contains("d2"));
        assert!(snapshot.excluded_uuids.contains("bad"));
        assert!(!snapshot.excluded_uuids.contains("outside"));
    }

    #[test]
    fn snapshot_builds_nested_paths() {
        let items = vec![
            folder("a", "A", ""),
            folder("bb", "B", "a"),
            doc("d", "Deep", "bb", "pdf", 1),
        ];
        let snapshot = remote_snapshot(&items, "");
        assert_eq!(snapshot.folders["A/B"], "bb");
        assert_eq!(snapshot.docs[0].local_rel_path(), "A/B/Deep.pdf");
    }

    // Each case retains the mapped UUID (u1), but excludes it from docs.
    // Renamed cases also leave a usable successor at the archived path:
    // UUID exclusion must prevent rebinding/replacing that successor.
    fn excluded_cases() -> Vec<(&'static str, Vec<Item>)> {
        vec![
            (
                "n",
                vec![doc("u1", "n", "", "pdf", 5), doc("dup", "n", "", "pdf", 5)],
            ),
            (
                "A/B/n",
                vec![
                    folder("a", "A", ""),
                    folder("dup", "A", ""),
                    folder("b", "B", "a"),
                    doc("u1", "n", "b", "pdf", 5),
                ],
            ),
            ("n", vec![doc("u1", "n", "", "unsupported", 5)]),
            (
                "n",
                vec![
                    doc("u1", ".hidden", "", "pdf", 5),
                    doc("new", "n", "", "pdf", 9),
                ],
            ),
            (
                "A/n",
                vec![folder("a", ".hidden", ""), doc("u1", "n", "a", "pdf", 5)],
            ),
            (
                "n",
                vec![
                    doc("u1", "moved", "", "pdf", 5),
                    doc("dup", "moved", "", "pdf", 5),
                    doc("new", "n", "", "pdf", 9),
                ],
            ),
        ]
    }

    #[test]
    fn excluded_mappings_never_transfer_delete_move_or_rebind() {
        excluded_cases().into_iter().for_each(|(key, items)| {
            let path = format!("{key}.pdf");
            let snapshot = remote_snapshot(&items, "");
            assert!(snapshot.excluded_uuids.contains("u1"));
            let mut state = SyncState::default();
            state.entries.insert(
                path.clone(),
                entry_hashed("u1", DocKind::Pdf, 100, 1, 5, "h", "h"),
            );
            [push(), pull(), two_way()].into_iter().for_each(|mode| {
                [false, true].into_iter().for_each(|delete| {
                    [
                        ConflictPolicy::Skip,
                        ConflictPolicy::Newest,
                        ConflictPolicy::PreferLocal,
                        ConflictPolicy::PreferRemote,
                    ]
                    .into_iter()
                    .for_each(|conflict| {
                        // Unchanged, edited, and absent local copies must all
                        // preserve the mapping rather than infer deletion.
                        [
                            vec![local(&path, DocKind::Pdf, 100, 1)],
                            vec![local(&path, DocKind::Pdf, 200, 99)],
                            vec![],
                        ]
                        .into_iter()
                        .for_each(|locals| {
                            let result = plan(
                                SyncOptions {
                                    delete,
                                    conflict,
                                    ..mode
                                },
                                &locals,
                                &snapshot,
                                &state,
                            );
                            assert!(
                                result
                                    .actions
                                    .iter()
                                    .all(|a| matches!(a, SyncAction::Skip { .. })),
                                "{key}: {:?}",
                                result.actions
                            );
                        });
                    });
                });
            });
        });
    }

    #[test]
    fn duplicate_documents_and_subtrees_preserve_successful_sync_across_runs() {
        [false, true].into_iter().for_each(|nested| {
            let (key, mut items) = if nested {
                (
                    "A/B/n.pdf",
                    vec![
                        folder("a", "A", ""),
                        folder("b", "B", "a"),
                        doc("u1", "n", "b", "pdf", 5),
                    ],
                )
            } else {
                ("n.pdf", vec![doc("u1", "n", "", "pdf", 5)])
            };
            let clean = remote_snapshot(&items, "");
            let initial = plan(pull(), &[], &clean, &SyncState::default());
            assert!(
                initial
                    .actions
                    .iter()
                    .any(|a| matches!(a, SyncAction::Download { local, .. } if local == key))
            );
            // Archive and local copy produced by the successful initial download.
            let mut state = SyncState::default();
            state
                .entries
                .insert(key.to_string(), entry("u1", DocKind::Pdf, 100, 1, 5));
            let locals = vec![local(key, DocKind::Pdf, 100, 1)];
            assert!(plan(pull(), &locals, &clean, &state).actions.is_empty());
            items.push(if nested {
                folder("dup", "A", "")
            } else {
                doc("dup", "n", "", "pdf", 5)
            });
            let excluded = remote_snapshot(&items, "");
            (0..2).for_each(|_| {
                let result = plan(with_delete(pull()), &locals, &excluded, &state);
                assert!(
                    result
                        .actions
                        .iter()
                        .all(|a| matches!(a, SyncAction::Skip { .. }))
                );
                // A skipped run writes no new entry; round-trip the retained archive.
                state = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
            });
            assert!(
                plan(with_delete(pull()), &locals, &clean, &state)
                    .actions
                    .is_empty()
            );
            let gone = remote_snapshot(&[], "");
            assert!(
                plan(with_delete(pull()), &locals, &gone, &state)
                    .actions
                    .iter()
                    .any(|a| matches!(a, SyncAction::DeleteLocal { path } if path == key))
            );
        });
    }

    #[test]
    fn excluded_paths_block_new_targets_and_absent_descendant_mappings() {
        let snapshot = remote_snapshot(
            &[
                folder("a", "A", ""),
                folder("dup", "A", ""),
                doc("unsupported", "n", "", "unsupported", 5),
            ],
            "",
        );
        // No live UUID to match: the excluded subtree still makes absence unknown.
        ["A/missing.pdf", "n.pdf"].into_iter().for_each(|key| {
            let locals = vec![local(key, DocKind::Pdf, 100, 1)];
            let mut state = SyncState::default();
            state
                .entries
                .insert(key.to_string(), entry("gone", DocKind::Pdf, 100, 1, 5));
            [push(), pull(), two_way()].into_iter().for_each(|options| {
                [&state, &SyncState::default()]
                    .into_iter()
                    .for_each(|archive| {
                        let result = plan(with_delete(options), &locals, &snapshot, archive);
                        assert!(
                            result
                                .actions
                                .iter()
                                .all(|a| matches!(a, SyncAction::Skip { .. }))
                        );
                    });
                let result = plan(with_delete(options), &[], &snapshot, &state);
                assert!(
                    result
                        .actions
                        .iter()
                        .all(|a| matches!(a, SyncAction::Skip { .. }))
                );
            });
        });
    }

    #[test]
    fn exclusions_do_not_block_unrelated_true_deletions() {
        let snapshot = remote_snapshot(
            &[doc("u1", "n", "", "pdf", 5), doc("dup", "n", "", "pdf", 5)],
            "",
        );
        let locals = vec![
            local("n.pdf", DocKind::Pdf, 100, 1),
            local("gone.pdf", DocKind::Pdf, 100, 1),
        ];
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 100, 1, 5));
        state.entries.insert(
            "gone.pdf".to_string(),
            entry("gone", DocKind::Pdf, 100, 1, 5),
        );
        let result = plan(with_delete(pull()), &locals, &snapshot, &state);
        let changes: Vec<_> = result
            .actions
            .into_iter()
            .filter(|a| !matches!(a, SyncAction::Skip { .. }))
            .collect();
        assert_eq!(
            changes,
            vec![SyncAction::DeleteLocal {
                path: "gone.pdf".to_string()
            }]
        );

        let peer = remote_snapshot(
            &[
                doc("peer", "n", "", "pdf", 5),
                doc("gone-peer", "gone", "", "pdf", 5),
            ],
            "",
        );
        let mut state = PairState::default();
        state
            .entries
            .insert("n".to_string(), pair_entry("u1", 5, "peer", 5));
        state
            .entries
            .insert("gone".to_string(), pair_entry("gone", 5, "gone-peer", 5));
        let changes: Vec<_> = plan_docs(with_delete(two_way()), &snapshot, &peer, &state)
            .into_iter()
            .filter(|a| !matches!(a, DocAction::Conflict { .. }))
            .collect();
        assert_eq!(
            changes,
            vec![DocAction::DeleteOnB {
                key: "gone".to_string(),
                uuid: "gone-peer".to_string()
            }]
        );
    }

    #[test]
    fn excluded_contents_and_targets_block_move_prepass() {
        let mut state = SyncState::default();
        state.entries.insert(
            "A/n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 100, 1, 5, "h", "h"),
        );
        let locals = vec![with_hash(local("B/n.pdf", DocKind::Pdf, 100, 1), "h")];
        let cases = [
            // A folder move must not carry excluded, even untracked, content.
            vec![
                folder("a", "A", ""),
                doc("u1", "n", "a", "pdf", 5),
                doc("bad", "bad", "a", "unsupported", 5),
            ],
            // Neither folder nor file moves may target an excluded subtree.
            vec![
                folder("a", "A", ""),
                doc("u1", "n", "a", "pdf", 5),
                folder("b", "B", ""),
                folder("dup", "B", ""),
            ],
        ];
        cases.into_iter().enumerate().for_each(|(index, items)| {
            let result = plan(push(), &locals, &remote_snapshot(&items, ""), &state);
            assert!(
                !result
                    .actions
                    .iter()
                    .any(|a| matches!(a, SyncAction::MoveRemoteFolder { .. }))
            );
            if index == 1 {
                assert!(!result.actions.iter().any(|a| matches!(
                    a,
                    SyncAction::MoveRemote { .. }
                        | SyncAction::Upload { .. }
                        | SyncAction::CopyRemote { .. }
                        | SyncAction::CreateRemoteFolder { .. }
                )));
            }
        });
    }

    // ---- push planning ----------------------------------------------------

    #[test]
    fn push_first_sync_uploads_and_creates_folders() {
        let locals = vec![
            local("a/b/notes.epub", DocKind::Epub, 10, 1),
            local("top.pdf", DocKind::Pdf, 20, 2),
        ];
        let snapshot = remote_snapshot(&[], "");
        let plan = plan(push(), &locals, &snapshot, &SyncState::default());
        assert_eq!(
            plan.actions,
            vec![
                SyncAction::CreateRemoteFolder {
                    rel_dir: "a".to_string()
                },
                SyncAction::CreateRemoteFolder {
                    rel_dir: "a/b".to_string()
                },
                SyncAction::Upload {
                    local: "a/b/notes.epub".to_string(),
                    kind: DocKind::Epub,
                    remote_dir: "a/b".to_string(),
                    name: "notes".to_string(),
                    hash: None
                },
                SyncAction::Upload {
                    local: "top.pdf".to_string(),
                    kind: DocKind::Pdf,
                    remote_dir: String::new(),
                    name: "top".to_string(),
                    hash: None
                },
            ]
        );
    }

    #[test]
    fn push_mapped_unchanged_is_noop() {
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)];
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let plan = plan(push(), &locals, &snapshot, &state);
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn push_local_change_updates_in_place() {
        let locals = vec![local("n.epub", DocKind::Epub, 99, 9)];
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "epub", 5)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.epub".to_string(), entry("u1", DocKind::Epub, 10, 1, 5));
        let plan = plan(push(), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![SyncAction::UpdateRemote {
                local: "n.epub".to_string(),
                kind: DocKind::Epub,
                uuid: "u1".to_string(),
                hash: None
            }]
        );
    }

    #[test]
    fn push_both_changed_is_conflict() {
        let locals = vec![local("n.pdf", DocKind::Pdf, 99, 9)];
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 6)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let plan = plan(push(), &locals, &snapshot, &state);
        assert_eq!(conflicts(&plan), ["n.pdf"]);
    }

    #[test]
    fn push_mapped_rmdoc_never_pushes() {
        let locals = vec![local("n.rmdoc", DocKind::Rmdoc, 99, 9)];
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "notebook", 5)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.rmdoc".to_string(), entry("u1", DocKind::Rmdoc, 10, 1, 5));
        let plan = plan(push(), &locals, &snapshot, &state);
        assert_eq!(skips(&plan), ["n.rmdoc"]);
    }

    #[test]
    fn push_unmapped_rmdoc_is_a_restore() {
        let locals = vec![local("backup.rmdoc", DocKind::Rmdoc, 10, 1)];
        let snapshot = remote_snapshot(&[], "");
        let plan = plan(push(), &locals, &snapshot, &SyncState::default());
        assert_eq!(
            plan.actions,
            vec![SyncAction::Upload {
                local: "backup.rmdoc".to_string(),
                kind: DocKind::Rmdoc,
                remote_dir: String::new(),
                name: "backup".to_string(),
                hash: None
            }]
        );
    }

    #[test]
    fn push_name_taken_on_device_skips() {
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)];
        let snapshot = remote_snapshot(&[doc("other", "n", "", "epub", 5)], "");
        let plan = plan(push(), &locals, &snapshot, &SyncState::default());
        assert_eq!(skips(&plan), ["n.pdf"]);
    }

    #[test]
    fn push_folder_name_collision_skips() {
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)];
        let snapshot = remote_snapshot(&[folder("f", "n", "")], "");
        let plan = plan(push(), &locals, &snapshot, &SyncState::default());
        assert_eq!(skips(&plan), ["n.pdf"]);
    }

    #[test]
    fn push_duplicate_local_targets_skip() {
        let locals = vec![
            local("n.epub", DocKind::Epub, 10, 1),
            local("n.pdf", DocKind::Pdf, 10, 1),
        ];
        let snapshot = remote_snapshot(&[], "");
        let plan = plan(push(), &locals, &snapshot, &SyncState::default());
        assert_eq!(skips(&plan).len(), 2);
    }

    #[test]
    fn push_recopies_when_device_copy_vanished() {
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)];
        let snapshot = remote_snapshot(&[], ""); // mapped doc gone
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("gone", DocKind::Pdf, 10, 1, 5));
        let plan = plan(push(), &locals, &snapshot, &state);
        assert!(matches!(plan.actions[0], SyncAction::Upload { .. }));
    }

    // ---- pull planning ----------------------------------------------------

    #[test]
    fn pull_first_sync_downloads_everything() {
        let items = vec![
            folder("b", "Books", ""),
            doc("d1", "Paper", "b", "pdf", 5),
            doc("d2", "Sketch", "", "notebook", 6),
        ];
        let snapshot = remote_snapshot(&items, "");
        let plan = plan(pull(), &[], &snapshot, &SyncState::default());
        let downloads: Vec<&str> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                SyncAction::Download { local, .. } => Some(local.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(downloads, ["Books/Paper.pdf", "Sketch.rmdoc"]);
    }

    #[test]
    fn pull_remote_change_downloads() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "notebook", 9)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.rmdoc".to_string(), entry("u1", DocKind::Rmdoc, 10, 1, 5));
        let locals = vec![local("n.rmdoc", DocKind::Rmdoc, 10, 1)];
        let plan = plan(pull(), &locals, &snapshot, &state);
        assert!(matches!(
            plan.actions[0],
            SyncAction::Download {
                doc_type: RemoteType::Notebook,
                last_modified: 9,
                ..
            }
        ));
    }

    #[test]
    fn pull_conflicts_and_drift_are_reported() {
        // Both changed.
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 9)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let locals = vec![local("n.pdf", DocKind::Pdf, 99, 9)];
        let plan = plan(pull(), &locals, &snapshot, &state);
        assert_eq!(conflicts(&plan), ["n.pdf"]);

        // Destination (local) drift only.
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        let plan = super::plan(pull(), &locals, &snapshot, &state);
        assert_eq!(conflicts(&plan), ["n.pdf"]);
    }

    #[test]
    fn pull_recopies_locally_deleted_file() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let plan = plan(pull(), &[], &snapshot, &state);
        assert!(matches!(plan.actions[0], SyncAction::Download { .. }));
    }

    #[test]
    fn pull_unmapped_local_collision_conflicts() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)];
        let plan = plan(pull(), &locals, &snapshot, &SyncState::default());
        assert_eq!(conflicts(&plan), ["n.pdf"]);
    }

    // ---- two-way planning -------------------------------------------------

    #[test]
    fn two_way_first_sync_merges_both_directions() {
        let locals = vec![local("only-local.pdf", DocKind::Pdf, 10, 1)];
        let snapshot = remote_snapshot(&[doc("u1", "only-remote", "", "notebook", 5)], "");
        let plan = plan(two_way(), &locals, &snapshot, &SyncState::default());
        assert!(plan.actions.iter().any(|a| matches!(
            a,
            SyncAction::Upload { local, .. } if local == "only-local.pdf"
        )));
        assert!(plan.actions.iter().any(|a| matches!(
            a,
            SyncAction::Download { local, .. } if local == "only-remote.rmdoc"
        )));
    }

    #[test]
    fn two_way_propagates_each_side() {
        let mut state = SyncState::default();
        state
            .entries
            .insert("a.pdf".to_string(), entry("ua", DocKind::Pdf, 10, 1, 5));
        state
            .entries
            .insert("b.pdf".to_string(), entry("ub", DocKind::Pdf, 10, 1, 5));
        let locals = vec![
            local("a.pdf", DocKind::Pdf, 99, 9), // local changed
            local("b.pdf", DocKind::Pdf, 10, 1), // unchanged
        ];
        let snapshot = remote_snapshot(
            &[doc("ua", "a", "", "pdf", 5), doc("ub", "b", "", "pdf", 9)], // ub changed
            "",
        );
        let plan = plan(two_way(), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![
                SyncAction::UpdateRemote {
                    local: "a.pdf".to_string(),
                    kind: DocKind::Pdf,
                    uuid: "ua".to_string(),
                    hash: None
                },
                SyncAction::Download {
                    local: "b.pdf".to_string(),
                    uuid: "ub".to_string(),
                    doc_type: RemoteType::Pdf,
                    size_bytes: Some(100),
                    last_modified: 9
                },
            ]
        );
    }

    #[test]
    fn two_way_conflict_policies() {
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 50)], "");
        let locals = vec![local("n.pdf", DocKind::Pdf, 99, 100)]; // local newer

        // Default: conflict, nothing happens.
        let plan_skip = plan(two_way(), &locals, &snapshot, &state);
        assert_eq!(conflicts(&plan_skip), ["n.pdf"]);

        // Newest: local (mtime 100 > lastModified 50) wins.
        let plan_newest = plan(
            with_policy(two_way(), ConflictPolicy::Newest),
            &locals,
            &snapshot,
            &state,
        );
        assert!(matches!(
            plan_newest.actions[0],
            SyncAction::UpdateRemote { .. }
        ));

        // Newest with remote newer: download.
        let snapshot_newer = remote_snapshot(&[doc("u1", "n", "", "pdf", 500)], "");
        let plan_remote = plan(
            with_policy(two_way(), ConflictPolicy::Newest),
            &locals,
            &snapshot_newer,
            &state,
        );
        assert!(matches!(
            plan_remote.actions[0],
            SyncAction::Download { .. }
        ));

        // Explicit sides.
        let plan_local = plan(
            with_policy(two_way(), ConflictPolicy::PreferLocal),
            &locals,
            &snapshot,
            &state,
        );
        assert!(matches!(
            plan_local.actions[0],
            SyncAction::UpdateRemote { .. }
        ));
        let plan_remote = plan(
            with_policy(two_way(), ConflictPolicy::PreferRemote),
            &locals,
            &snapshot,
            &state,
        );
        assert!(matches!(
            plan_remote.actions[0],
            SyncAction::Download { .. }
        ));
    }

    #[test]
    fn two_way_rmdoc_conflict_can_only_resolve_to_device() {
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.rmdoc".to_string(), entry("u1", DocKind::Rmdoc, 10, 1, 5));
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "notebook", 50)], "");
        let locals = vec![local("n.rmdoc", DocKind::Rmdoc, 99, 100)];

        // Local can never win for handwriting, even when preferred.
        let plan_local = plan(
            with_policy(two_way(), ConflictPolicy::PreferLocal),
            &locals,
            &snapshot,
            &state,
        );
        assert_eq!(skips(&plan_local), ["n.rmdoc"]);
        let plan_remote = plan(
            with_policy(two_way(), ConflictPolicy::PreferRemote),
            &locals,
            &snapshot,
            &state,
        );
        assert!(matches!(
            plan_remote.actions[0],
            SyncAction::Download { .. }
        ));
    }

    // ---- content hashes (fastcheck, refresh, adoption) ---------------------

    #[test]
    fn cached_hash_gates_rehashing() {
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "abc", "abc"),
        );
        // Stamp matches: reuse without I/O.
        assert_eq!(
            cached_hash(&local("n.pdf", DocKind::Pdf, 10, 1), &state),
            Some("abc".to_string())
        );
        // Stamp moved: must re-hash.
        assert_eq!(
            cached_hash(&local("n.pdf", DocKind::Pdf, 10, 9), &state),
            None
        );
        // Unknown file: must hash.
        assert_eq!(
            cached_hash(&local("x.pdf", DocKind::Pdf, 10, 1), &state),
            None
        );
    }

    #[test]
    fn touched_but_identical_local_file_refreshes_instead_of_uploading() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "abc", "abc"),
        );
        // mtime moved, content identical.
        let locals = vec![with_hash(local("n.pdf", DocKind::Pdf, 10, 9), "abc")];
        let plan_refresh = plan(push(), &locals, &snapshot, &state);
        assert!(matches!(
            plan_refresh.actions[0],
            SyncAction::Refresh { .. }
        ));
        assert_eq!(plan_refresh.actions.len(), 1);

        // Content actually changed: normal update.
        let locals = vec![with_hash(local("n.pdf", DocKind::Pdf, 10, 9), "zzz")];
        let plan_update = plan(push(), &locals, &snapshot, &state);
        assert!(matches!(
            plan_update.actions[0],
            SyncAction::UpdateRemote { .. }
        ));
    }

    #[test]
    fn annotation_only_remote_change_refreshes_instead_of_downloading() {
        // lastModified moved (annotations) but the payload hash did not.
        let mut items_snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 9)], "");
        items_snapshot.docs[0].payload_hash = Some("abc".to_string());
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "abc", "abc"),
        );
        let locals = vec![with_hash(local("n.pdf", DocKind::Pdf, 10, 1), "abc")];
        let plan_refresh = plan(pull(), &locals, &items_snapshot, &state);
        assert!(matches!(
            plan_refresh.actions[0],
            SyncAction::Refresh {
                last_modified: 9,
                ..
            }
        ));
        assert_eq!(plan_refresh.actions.len(), 1);

        // Payload hash unknown: conservative, downloads as before.
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 9)], "");
        let plan_dl = plan(pull(), &locals, &snapshot, &state);
        assert!(matches!(plan_dl.actions[0], SyncAction::Download { .. }));
    }

    #[test]
    fn identical_unmapped_content_is_adopted_without_policy() {
        // Same path, no state (archive lost), hash-verified identical:
        // silently re-paired even under the default Skip policy.
        let mut snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        snapshot.docs[0].payload_hash = Some("abc".to_string());
        let locals = vec![with_hash(local("n.pdf", DocKind::Pdf, 10, 1), "abc")];
        let plan_adopt = plan(two_way(), &locals, &snapshot, &SyncState::default());
        assert_eq!(
            plan_adopt.actions,
            vec![SyncAction::Adopt {
                path: "n.pdf".to_string(),
                kind: DocKind::Pdf,
                uuid: "u1".to_string(),
                last_modified: 5,
                hash: "abc".to_string()
            }]
        );

        // Different content: stays a conflict under Skip.
        let locals = vec![with_hash(local("n.pdf", DocKind::Pdf, 10, 1), "zzz")];
        let plan_conflict = plan(two_way(), &locals, &snapshot, &SyncState::default());
        assert_eq!(conflicts(&plan_conflict), ["n.pdf"]);
    }

    #[test]
    fn device_hash_candidates_are_lazy() {
        let mut state = SyncState::default();
        state.entries.insert(
            "changed.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "a", "a"),
        );
        state.entries.insert(
            "same.pdf".to_string(),
            entry_hashed("u2", DocKind::Pdf, 10, 1, 5, "b", "b"),
        );
        let items = vec![
            doc("u1", "changed", "", "pdf", 9),   // lastModified moved
            doc("u2", "same", "", "pdf", 5),      // unchanged
            doc("u3", "twin", "", "epub", 5),     // unmapped, local twin
            doc("u4", "lonely", "", "pdf", 5),    // unmapped, no local twin
            doc("u5", "note", "", "notebook", 9), // never hashed
        ];
        let snapshot = remote_snapshot(&items, "");
        let locals = vec![local("twin.epub", DocKind::Epub, 10, 1)];
        let wanted = device_hash_candidates(&locals, &snapshot, &state);
        assert_eq!(
            wanted,
            [("u1".to_string(), "pdf"), ("u3".to_string(), "epub")]
        );
    }

    #[test]
    fn sync_state_path_is_order_independent() {
        assert_eq!(
            sync_state_path("/home/u/books", "rm:/Books"),
            sync_state_path("rm:/Books", "/home/u/books")
        );
        assert_ne!(
            sync_state_path("/home/u/books", "rm:/Books"),
            sync_state_path("/home/u/books", "rm:/Other")
        );
    }

    // ---- moves and copies ---------------------------------------------------

    fn doc_at(uuid: &str, name: &str, parent: &str, file_type: &str, modified: i64) -> Item {
        doc(uuid, name, parent, file_type, modified)
    }

    #[test]
    fn local_move_becomes_metadata_only_device_move() {
        // n.pdf (mapped, hash h) vanished; a/moved.pdf appeared with
        // the same hash. Expect: mkdir a, MoveRemote — no upload, no
        // delete, even with --delete.
        let snapshot = remote_snapshot(&[doc_at("u1", "n", "", "pdf", 5)], "");
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h", "h"),
        );
        let locals = vec![with_hash(local("a/moved.pdf", DocKind::Pdf, 10, 7), "h")];

        let plan = plan(with_delete(push()), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![
                SyncAction::CreateRemoteFolder {
                    rel_dir: "a".to_string()
                },
                SyncAction::MoveRemote {
                    from: "n.pdf".to_string(),
                    to: "a/moved.pdf".to_string(),
                    kind: DocKind::Pdf,
                    uuid: "u1".to_string(),
                    remote_dir: "a".to_string(),
                    name: "moved".to_string(),
                    local_hash: Some("h".to_string()),
                    payload_hash: Some("h".to_string()),
                },
            ]
        );
    }

    #[test]
    fn local_move_with_racing_device_change_falls_back() {
        // Device copy changed since last sync (lm 9 != 5): moving
        // would mask the change, so the ordinary rules apply instead.
        let snapshot = remote_snapshot(&[doc_at("u1", "n", "", "pdf", 9)], "");
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h", "h"),
        );
        let locals = vec![with_hash(local("moved.pdf", DocKind::Pdf, 10, 7), "h")];
        let plan = plan(push(), &locals, &snapshot, &state);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::MoveRemote { .. }))
        );
    }

    #[test]
    fn ambiguous_moves_tie_break_by_filename_then_note() {
        // Two identical files (same hash) moved into a folder: the
        // stem tie-break pairs both.
        let items = vec![
            doc_at("u1", "one", "", "pdf", 5),
            doc_at("u2", "two", "", "pdf", 5),
        ];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state.entries.insert(
            "one.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h", "h"),
        );
        state.entries.insert(
            "two.pdf".to_string(),
            entry_hashed("u2", DocKind::Pdf, 10, 1, 5, "h", "h"),
        );
        let locals = vec![
            with_hash(local("a/one.pdf", DocKind::Pdf, 10, 7), "h"),
            with_hash(local("a/two.pdf", DocKind::Pdf, 10, 7), "h"),
        ];
        let plan_tie = plan(push(), &locals, &snapshot, &state);
        assert_eq!(
            plan_tie
                .actions
                .iter()
                .filter(|a| matches!(a, SyncAction::MoveRemote { .. }))
                .count(),
            2
        );

        // Renamed to unrelated names: truly ambiguous — no move, a
        // note, and (thanks to copy-by-fingerprint) device-side copies
        // instead of uploads.
        let locals = vec![
            with_hash(local("a/x.pdf", DocKind::Pdf, 10, 7), "h"),
            with_hash(local("a/y.pdf", DocKind::Pdf, 10, 7), "h"),
        ];
        let plan_amb = plan(push(), &locals, &snapshot, &state);
        assert!(
            !plan_amb
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::MoveRemote { .. }))
        );
        assert_eq!(
            plan_amb
                .actions
                .iter()
                .filter(|a| matches!(a, SyncAction::CopyRemote { .. }))
                .count(),
            2
        );
        assert_eq!(skips(&plan_amb).len(), 2);
    }

    #[test]
    fn device_move_drags_the_local_file_along() {
        // Mapped doc u1 moved into device folder A; payload hash
        // verified unchanged → MoveLocal only, nothing transferred.
        let items = vec![folder("a", "A", ""), doc_at("u1", "n", "a", "pdf", 9)];
        let mut snapshot = remote_snapshot(&items, "");
        snapshot.docs[0].payload_hash = Some("h".to_string());
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h", "h"),
        );
        let locals = vec![with_hash(local("n.pdf", DocKind::Pdf, 10, 1), "h")];
        let plan_move = plan(pull(), &locals, &snapshot, &state);
        assert_eq!(
            plan_move.actions,
            vec![SyncAction::MoveLocal {
                from: "n.pdf".to_string(),
                to: "A/n.pdf".to_string(),
                kind: DocKind::Pdf,
                uuid: "u1".to_string(),
                last_modified: 9,
                local_hash: Some("h".to_string()),
                payload_hash: Some("h".to_string()),
            }]
        );

        // Payload also changed (hash differs): move, then download at
        // the new path.
        snapshot.docs[0].payload_hash = Some("zzz".to_string());
        let plan_both = plan(pull(), &locals, &snapshot, &state);
        assert!(matches!(plan_both.actions[0], SyncAction::MoveLocal { .. }));
        assert!(plan_both.actions.iter().any(|a| matches!(
            a,
            SyncAction::Download { local, .. } if local == "A/n.pdf"
        )));
    }

    // These plans only perform local operations. Disable SSH multiplexing
    // (including its drop-time cleanup), and prohibit any network connection
    // should a regression unexpectedly introduce a device operation.
    fn with_local_executor(test: impl FnOnce(&LocalFs, &Path, &Client)) {
        let root = std::env::temp_dir().join(format!("rmu-move-test-{}", uuid::Uuid::new_v4()));
        let fs_side = LocalFs::new(root.join("files"));
        fs_side.ensure_root().unwrap();
        let session = SshSession::new(crate::ssh::SshOptions {
            destination: "unused.invalid".to_string(),
            multiplex: false,
            extra_options: vec!["ProxyCommand=false".to_string()],
            ..Default::default()
        })
        .unwrap();
        let client = Client::new(session, "/unused");
        test(&fs_side, &root.join("state.json"), &client);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn device_move_keeps_local_edit_conflicting_on_next_run() {
        [pull(), two_way()].into_iter().for_each(|options| {
            // A changed payload and an unavailable payload hash must both
            // retain the unresolved device change across the local move.
            [Some(hash_bytes(b"device edit")), None]
                .into_iter()
                .for_each(|remote_hash| {
                    with_local_executor(|fs_side, state_path, client| {
                        fs_side.write("n.pdf", b"agreed").unwrap();
                        let (size, mtime) = fs_side.stat("n.pdf").unwrap();
                        let hash = hash_bytes(b"agreed");
                        let baseline =
                            entry_hashed("u1", DocKind::Pdf, size, mtime, 5, &hash, &hash);
                        let mut state = SyncState::default();
                        state.entries.insert("n.pdf".to_string(), baseline.clone());
                        state.save(state_path).unwrap();

                        // Different size makes the edit detectable even on
                        // filesystems with coarse timestamp resolution.
                        let edited = b"unresolved local edit";
                        fs_side.write("n.pdf", edited).unwrap();
                        let mut remote = remote_snapshot(&[doc("u1", "renamed", "", "pdf", 9)], "");
                        remote.docs[0].payload_hash = remote_hash.clone();
                        let (mut locals, _) = fs_side.snapshot().unwrap();
                        attach_hashes(fs_side, &mut locals, &state).unwrap();
                        let first = plan(options, &locals, &remote, &state);
                        assert!(matches!(first.actions.as_slice(), [
                            SyncAction::MoveLocal { from, to, last_modified: 5, .. },
                            SyncAction::Conflict { path, .. },
                        ] if from == "n.pdf" && to == "renamed.pdf" && path == to));
                        let outcome = execute(
                            client,
                            &crate::progress::NoProgress,
                            fs_side,
                            &first,
                            &mut remote.folders,
                            &mut state,
                            state_path,
                        )
                        .unwrap();
                        assert_eq!(outcome.moved_local, 1);
                        assert_eq!(outcome.conflicts.len(), 1);
                        assert!(!outcome.modified_remote);
                        assert!(!fs_side.as_local_path("n.pdf").unwrap().exists());
                        assert_eq!(fs_side.read("renamed.pdf").unwrap(), edited);

                        // Reload the actual persisted archive, then run the
                        // production hash-cache path rather than supplying a
                        // hand-written local hash to the second planner pass.
                        let mut state = SyncState::load(state_path).unwrap();
                        assert_eq!(state.entries.len(), 1);
                        let saved = &state.entries["renamed.pdf"];
                        assert_eq!(saved.local_size, baseline.local_size);
                        assert_eq!(saved.local_mtime_ms, baseline.local_mtime_ms);
                        assert_eq!(saved.local_hash, baseline.local_hash);
                        assert_eq!(saved.payload_hash, baseline.payload_hash);
                        assert_eq!(saved.remote_last_modified, 5);
                        let (mut locals, _) = fs_side.snapshot().unwrap();
                        assert_eq!(cached_hash(&locals[0], &state), None);
                        attach_hashes(fs_side, &mut locals, &state).unwrap();
                        assert_eq!(locals[0].hash, Some(hash_bytes(edited)));
                        let second = plan(options, &locals, &remote, &state);
                        assert!(matches!(second.actions.as_slice(), [
                            SyncAction::Conflict { path, .. },
                        ] if path == "renamed.pdf"));
                        let outcome = execute(
                            client,
                            &crate::progress::NoProgress,
                            fs_side,
                            &second,
                            &mut remote.folders,
                            &mut state,
                            state_path,
                        )
                        .unwrap();
                        assert_eq!(outcome.conflicts.len(), 1);
                        assert_eq!(outcome.downloaded, 0);
                        assert_eq!(fs_side.read("renamed.pdf").unwrap(), edited);
                    });
                });
        });
    }

    #[test]
    fn device_move_records_verified_rename_timestamp() {
        with_local_executor(|fs_side, state_path, client| {
            fs_side.write("n.pdf", b"agreed").unwrap();
            let (size, mtime) = fs_side.stat("n.pdf").unwrap();
            let hash = hash_bytes(b"agreed");
            let mut state = SyncState::default();
            state.entries.insert(
                "n.pdf".to_string(),
                entry_hashed("u1", DocKind::Pdf, size, mtime, 5, &hash, &hash),
            );
            let mut remote = remote_snapshot(&[doc("u1", "renamed", "", "pdf", 9)], "");
            remote.docs[0].payload_hash = Some(hash.clone());
            let (mut locals, _) = fs_side.snapshot().unwrap();
            attach_hashes(fs_side, &mut locals, &state).unwrap();
            let first = plan(two_way(), &locals, &remote, &state);
            assert!(matches!(
                first.actions.as_slice(),
                [SyncAction::MoveLocal {
                    last_modified: 9,
                    ..
                },]
            ));
            execute(
                client,
                &crate::progress::NoProgress,
                fs_side,
                &first,
                &mut remote.folders,
                &mut state,
                state_path,
            )
            .unwrap();

            let state = SyncState::load(state_path).unwrap();
            assert_eq!(state.entries.len(), 1);
            assert_eq!(state.entries["renamed.pdf"].remote_last_modified, 9);
            let (mut locals, _) = fs_side.snapshot().unwrap();
            assert_eq!(cached_hash(&locals[0], &state), Some(hash));
            attach_hashes(fs_side, &mut locals, &state).unwrap();
            assert!(plan(two_way(), &locals, &remote, &state).actions.is_empty());
            assert_eq!(fs_side.read("renamed.pdf").unwrap(), b"agreed");
        });
    }

    #[test]
    fn device_move_to_occupied_path_is_noted_not_forced() {
        let items = vec![folder("a", "A", ""), doc_at("u1", "n", "a", "pdf", 9)];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h", "h"),
        );
        // Target A/n.pdf already exists locally.
        let locals = vec![
            with_hash(local("n.pdf", DocKind::Pdf, 10, 1), "h"),
            with_hash(local("A/n.pdf", DocKind::Pdf, 20, 2), "other"),
        ];
        let plan = plan(pull(), &locals, &snapshot, &state);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::MoveLocal { .. }))
        );
        assert!(skips(&plan).contains(&"n.pdf"));
    }

    #[test]
    fn new_local_file_matching_device_payload_copies_on_device() {
        // copy.pdf duplicates the mapped n.pdf's content: one on-device
        // cp instead of an upload.
        let snapshot = remote_snapshot(&[doc_at("u1", "n", "", "pdf", 5)], "");
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h", "h"),
        );
        let locals = vec![
            with_hash(local("n.pdf", DocKind::Pdf, 10, 1), "h"),
            with_hash(local("copy.pdf", DocKind::Pdf, 10, 3), "h"),
        ];
        let plan = plan(push(), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![SyncAction::CopyRemote {
                local: "copy.pdf".to_string(),
                kind: DocKind::Pdf,
                from_uuid: "u1".to_string(),
                remote_dir: String::new(),
                name: "copy".to_string(),
                size_bytes: Some(100),
                hash: "h".to_string(),
            }]
        );
    }

    #[test]
    fn new_device_doc_matching_local_content_copies_locally() {
        // Device doc "dup" has the same payload hash as the local
        // n.pdf: local copy instead of a download.
        let items = vec![
            doc_at("u1", "n", "", "pdf", 5),
            doc_at("u2", "dup", "", "pdf", 6),
        ];
        let mut snapshot = remote_snapshot(&items, "");
        snapshot
            .docs
            .iter_mut()
            .find(|d| d.uuid == "u2")
            .expect("dup doc present")
            .payload_hash = Some("h".to_string());
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h", "h"),
        );
        let locals = vec![with_hash(local("n.pdf", DocKind::Pdf, 10, 1), "h")];
        let plan = plan(pull(), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![SyncAction::CopyLocal {
                from: "n.pdf".to_string(),
                to: "dup.pdf".to_string(),
                kind: DocKind::Pdf,
                uuid: "u2".to_string(),
                last_modified: 6,
                hash: "h".to_string(),
            }]
        );
    }

    #[test]
    fn fingerprint_copies_choose_an_unmodified_alternative() {
        [push(), pull(), two_way()].into_iter().for_each(|options| {
            let pushing = options.mode != Mode::Pull;
            let mut snapshot = remote_snapshot(
                &[
                    doc("u1", "a", "", "pdf", if pushing { 5 } else { 9 }),
                    doc("u2", "b", "", "pdf", 5),
                ],
                "",
            );
            let mut state = SyncState::default();
            [ ("a.pdf", "u1"), ("b.pdf", "u2") ].into_iter().for_each(|(path, uuid)| {
                state.entries.insert(
                    path.to_string(),
                    entry_hashed(uuid, DocKind::Pdf, 10, 1, 5, "h1", "h1"),
                );
            });
            let mut locals = vec![
                with_hash(local("a.pdf", DocKind::Pdf, 10, if pushing { 2 } else { 1 }), if pushing { "h2" } else { "h1" }),
                with_hash(local("b.pdf", DocKind::Pdf, 10, 1), "h1"),
            ];
            if pushing {
                locals.push(with_hash(local("z.pdf", DocKind::Pdf, 10, 1), "h1"));
            } else {
                let mut added = remote_snapshot(&[doc("u3", "z", "", "pdf", 5)], "");
                added.docs[0].payload_hash = Some("h1".to_string());
                snapshot.docs.extend(added.docs);
            }
            let actions = plan(options, &locals, &snapshot, &state).actions;
            assert_eq!(actions.len(), 2);
            if pushing {
                assert!(matches!(&actions[0], SyncAction::UpdateRemote { uuid, .. } if uuid == "u1"));
                assert!(matches!(&actions[1], SyncAction::CopyRemote { from_uuid, .. } if from_uuid == "u2"));
            } else {
                assert!(matches!(&actions[0], SyncAction::Download { local, .. } if local == "a.pdf"));
                assert!(matches!(&actions[1], SyncAction::CopyLocal { from, .. } if from == "b.pdf"));
            }
        });
    }

    #[test]
    fn pending_successor_and_overwritten_copy_sources_are_both_excluded() {
        let snapshot = remote_snapshot(
            &[doc("u1", "a", "", "pdf", 5), doc("u2", "b", "", "pdf", 5)],
            "",
        );
        let mut state = SyncState::default();
        state.entries.insert(
            "a.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h1", "h1"),
        );
        state.entries.insert(
            "b.pdf".to_string(),
            StateEntry {
                // The successor retained its predecessor's timestamp,
                // but its payload has not yet been reconciled.
                remote_pending: true,
                ..entry_hashed("u2", DocKind::Pdf, 10, 1, 5, "h1", "h1")
            },
        );
        let locals = vec![
            with_hash(local("a.pdf", DocKind::Pdf, 10, 2), "h2"),
            with_hash(local("b.pdf", DocKind::Pdf, 10, 1), "h1"),
            with_hash(local("z.pdf", DocKind::Pdf, 10, 1), "h1"),
        ];
        let actions = plan(push(), &locals, &snapshot, &state).actions;
        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], SyncAction::UpdateRemote { uuid, .. } if uuid == "u1"));
        assert!(matches!(&actions[1], SyncAction::Upload { local, .. } if local == "z.pdf"));
    }

    #[test]
    fn excluded_views_do_not_invalidate_unchanged_local_copy_sources() {
        let mut snapshot = remote_snapshot(
            &[
                folder("folder-a", "A", ""),
                folder("duplicate-a", "A", ""),
                doc("u1", "moved", "", "pdf", 9),
                doc("u2", "b", "", "pdf", 9),
                doc("u3", "z", "", "pdf", 5),
            ],
            "",
        );
        snapshot.docs.iter_mut().for_each(|doc| {
            doc.payload_hash = Some(if doc.uuid == "u3" { "h1" } else { "h2" }.to_string());
        });
        let mut state = SyncState::default();
        [("A/source.pdf", "u1"), ("b.pdf", "u2")]
            .into_iter()
            .for_each(|(path, uuid)| {
                state.entries.insert(
                    path.to_string(),
                    entry_hashed(uuid, DocKind::Pdf, 10, 1, 5, "h1", "h1"),
                );
            });
        let locals = vec![
            with_hash(local("A/source.pdf", DocKind::Pdf, 10, 1), "h1"),
            with_hash(local("b.pdf", DocKind::Pdf, 10, 1), "h1"),
        ];
        [pull(), two_way()].into_iter().for_each(|options| {
            let result = plan(options, &locals, &snapshot, &state);
            assert!(skips(&result).contains(&"A/source.pdf"));
            let changes: Vec<_> = result
                .actions
                .iter()
                .filter(|action| !matches!(action, SyncAction::Skip { .. }))
                .collect();
            assert_eq!(changes.len(), 2);
            assert!(matches!(changes[0], SyncAction::Download { local, .. } if local == "b.pdf"));
            // The excluded entry is not moved or downloaded in either
            // pass. Its local bytes remain usable; b.pdf's do not.
            assert!(matches!(changes[1], SyncAction::CopyLocal { from, to, .. }
                if from == "A/source.pdf" && to == "z.pdf"));
        });
    }

    #[test]
    fn moved_local_copy_source_is_usable_only_without_a_pending_download() {
        ["h1", "h2"].into_iter().for_each(|payload| {
            let mut snapshot = remote_snapshot(
                &[doc("u1", "a", "", "pdf", 9), doc("u2", "z", "", "pdf", 5)],
                "",
            );
            snapshot.docs.iter_mut().for_each(|doc| {
                doc.payload_hash = Some(if doc.uuid == "u1" { payload } else { "h1" }.to_string());
            });
            let mut state = SyncState::default();
            state.entries.insert(
                "old.pdf".to_string(),
                entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h1", "h1"),
            );
            let locals = vec![with_hash(local("old.pdf", DocKind::Pdf, 10, 1), "h1")];
            let actions = plan(pull(), &locals, &snapshot, &state).actions;
            assert!(matches!(&actions[0], SyncAction::MoveLocal { from, to, .. } if from == "old.pdf" && to == "a.pdf"));
            if payload == "h1" {
                assert_eq!(actions.len(), 2);
                assert!(matches!(&actions[1], SyncAction::CopyLocal { from, to, .. } if from == "a.pdf" && to == "z.pdf"));
            } else {
                assert_eq!(actions.len(), 3);
                assert!(matches!(&actions[1], SyncAction::Download { local, .. } if local == "a.pdf"));
                assert!(matches!(&actions[2], SyncAction::Download { local, .. } if local == "z.pdf"));
            }
        });
    }

    #[test]
    fn remote_copy_source_survives_a_metadata_only_move() {
        let snapshot = remote_snapshot(&[doc("u1", "a", "", "pdf", 5)], "");
        let mut state = SyncState::default();
        state.entries.insert(
            "a.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h1", "h1"),
        );
        let locals = vec![
            with_hash(local("moved/a.pdf", DocKind::Pdf, 10, 1), "h1"),
            with_hash(local("z.pdf", DocKind::Pdf, 10, 1), "h1"),
        ];
        let actions = plan(push(), &locals, &snapshot, &state).actions;
        assert_eq!(actions.len(), 3);
        assert!(matches!(&actions[1], SyncAction::MoveRemote { uuid, .. } if uuid == "u1"));
        assert!(
            matches!(&actions[2], SyncAction::CopyRemote { from_uuid, local, .. } if from_uuid == "u1" && local == "z.pdf")
        );
    }

    #[test]
    fn copies_can_read_sources_deleted_after_transfers() {
        let mut state = SyncState::default();
        state.entries.insert(
            "a.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "h1", "h1"),
        );
        let snapshot = remote_snapshot(&[doc("u1", "a", "", "pdf", 5)], "");
        // Two targets make move pairing ambiguous: both copy before deletion.
        let locals = vec![
            with_hash(local("y.pdf", DocKind::Pdf, 10, 1), "h1"),
            with_hash(local("z.pdf", DocKind::Pdf, 10, 1), "h1"),
        ];
        let actions = plan(with_delete(push()), &locals, &snapshot, &state).actions;
        assert!(
            matches!(&actions[0], SyncAction::CopyRemote { from_uuid, .. } if from_uuid == "u1")
        );
        assert!(
            matches!(&actions[1], SyncAction::CopyRemote { from_uuid, .. } if from_uuid == "u1")
        );
        assert!(matches!(&actions[2], SyncAction::DeleteRemote { uuid, .. } if uuid == "u1"));

        let mut snapshot = remote_snapshot(&[doc("u2", "z", "", "pdf", 5)], "");
        snapshot.docs[0].payload_hash = Some("h1".to_string());
        let locals = vec![with_hash(local("a.pdf", DocKind::Pdf, 10, 1), "h1")];
        let actions = plan(with_delete(pull()), &locals, &snapshot, &state).actions;
        assert_eq!(actions.len(), 2);
        assert!(
            matches!(&actions[0], SyncAction::CopyLocal { from, to, .. } if from == "a.pdf" && to == "z.pdf")
        );
        assert!(matches!(&actions[1], SyncAction::DeleteLocal { path } if path == "a.pdf"));
    }

    #[test]
    fn size_prefilter_extends_hash_candidates() {
        // Unmapped device doc whose size matches a local file is a
        // hash candidate (potential local copy source).
        let items = vec![doc_at("u1", "new", "", "pdf", 5)]; // size 100
        let snapshot = remote_snapshot(&items, "");
        let locals = vec![local("other.pdf", DocKind::Pdf, 100, 1)];
        let wanted = device_hash_candidates(&locals, &snapshot, &SyncState::default());
        assert_eq!(wanted, [("u1".to_string(), "pdf")]);

        // No size match, no path twin: not hashed.
        let locals = vec![local("other.pdf", DocKind::Pdf, 33, 1)];
        let wanted = device_hash_candidates(&locals, &snapshot, &SyncState::default());
        assert!(wanted.is_empty());
    }

    #[test]
    fn whole_folder_move_is_one_metadata_write() {
        // Device folder A holds two mapped docs; locally A/ became B/
        // wholesale. Expect exactly one MoveRemoteFolder — no per-file
        // moves, no uploads, no deletes (with --delete too).
        let items = vec![
            folder("fa", "A", ""),
            doc_at("u1", "x", "fa", "pdf", 5),
            doc_at("u2", "y", "fa", "pdf", 6),
        ];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state.entries.insert(
            "A/x.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "hx", "hx"),
        );
        state.entries.insert(
            "A/y.pdf".to_string(),
            entry_hashed("u2", DocKind::Pdf, 11, 2, 6, "hy", "hy"),
        );
        let locals = vec![
            with_hash(local("B/x.pdf", DocKind::Pdf, 10, 1), "hx"),
            with_hash(local("B/y.pdf", DocKind::Pdf, 11, 2), "hy"),
        ];
        let plan = plan(with_delete(push()), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![SyncAction::MoveRemoteFolder {
                from_dir: "A".to_string(),
                to_dir: "B".to_string(),
                uuid: "fa".to_string(),
                remote_dir: String::new(),
                name: "B".to_string(),
            }]
        );
    }

    #[test]
    fn nested_folder_move_pairs_the_outermost_folder() {
        // A/Sub moved as part of A → B: only A is paired; the subtree
        // follows for free.
        let items = vec![
            folder("fa", "A", ""),
            folder("fs", "Sub", "fa"),
            doc_at("u1", "x", "fa", "pdf", 5),
            doc_at("u2", "y", "fs", "pdf", 6),
        ];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state.entries.insert(
            "A/x.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "hx", "hx"),
        );
        state.entries.insert(
            "A/Sub/y.pdf".to_string(),
            entry_hashed("u2", DocKind::Pdf, 11, 2, 6, "hy", "hy"),
        );
        let locals = vec![
            with_hash(local("B/x.pdf", DocKind::Pdf, 10, 1), "hx"),
            with_hash(local("B/Sub/y.pdf", DocKind::Pdf, 11, 2), "hy"),
        ];
        let plan = plan(push(), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions
                .iter()
                .filter(|a| matches!(a, SyncAction::MoveRemoteFolder { .. }))
                .count(),
            1
        );
        assert_eq!(plan.actions.len(), 1);
    }

    #[test]
    fn partial_folder_move_falls_back_to_file_moves() {
        // Only one of two files left folder A: not a folder move — the
        // single file gets a file-level metadata move instead.
        let items = vec![
            folder("fa", "A", ""),
            doc_at("u1", "x", "fa", "pdf", 5),
            doc_at("u2", "y", "fa", "pdf", 6),
        ];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state.entries.insert(
            "A/x.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "hx", "hx"),
        );
        state.entries.insert(
            "A/y.pdf".to_string(),
            entry_hashed("u2", DocKind::Pdf, 11, 2, 6, "hy", "hy"),
        );
        let locals = vec![
            with_hash(local("A/x.pdf", DocKind::Pdf, 10, 1), "hx"), // stayed
            with_hash(local("B/y.pdf", DocKind::Pdf, 11, 2), "hy"), // moved
        ];
        let plan = plan(push(), &locals, &snapshot, &state);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::MoveRemoteFolder { .. }))
        );
        assert!(plan.actions.iter().any(|a| matches!(
            a,
            SyncAction::MoveRemote { from, .. } if from == "A/y.pdf"
        )));
    }

    #[test]
    fn folder_move_to_taken_name_falls_back() {
        // Target folder B already exists on the device: no folder
        // move; file-level moves into the existing B instead.
        let items = vec![
            folder("fa", "A", ""),
            folder("fb", "B", ""),
            doc_at("u1", "x", "fa", "pdf", 5),
        ];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state.entries.insert(
            "A/x.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "hx", "hx"),
        );
        let locals = vec![with_hash(local("B/x.pdf", DocKind::Pdf, 10, 1), "hx")];
        let plan = plan(push(), &locals, &snapshot, &state);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::MoveRemoteFolder { .. }))
        );
        assert!(plan.actions.iter().any(|a| matches!(
            a,
            SyncAction::MoveRemote { to, .. } if to == "B/x.pdf"
        )));
    }

    #[test]
    fn ambiguous_folder_move_is_noted() {
        // Two identical candidate target folders: cannot decide.
        let items = vec![folder("fa", "A", ""), doc_at("u1", "x", "fa", "pdf", 5)];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state.entries.insert(
            "A/x.pdf".to_string(),
            entry_hashed("u1", DocKind::Pdf, 10, 1, 5, "hx", "hx"),
        );
        let locals = vec![
            with_hash(local("B/x.pdf", DocKind::Pdf, 10, 1), "hx"),
            with_hash(local("C/x.pdf", DocKind::Pdf, 10, 1), "hx"),
        ];
        let plan = plan(push(), &locals, &snapshot, &state);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::MoveRemoteFolder { .. }))
        );
        assert!(skips(&plan).contains(&"A"));
    }

    // ---- deletions --------------------------------------------------------

    #[test]
    fn push_delete_propagates_local_deletion() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));

        // Without --delete: device keeps the document.
        let plan_keep = plan(push(), &[], &snapshot, &state);
        assert!(plan_keep.actions.is_empty());

        // With --delete: remove on device.
        let plan_delete = plan(with_delete(push()), &[], &snapshot, &state);
        assert_eq!(
            plan_delete.actions,
            vec![SyncAction::DeleteRemote {
                path: "n.pdf".to_string(),
                uuid: "u1".to_string()
            }]
        );
    }

    #[test]
    fn push_delete_vs_remote_change_is_conflict() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 9)], ""); // changed
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));

        let plan_skip = plan(with_delete(push()), &[], &snapshot, &state);
        assert_eq!(conflicts(&plan_skip), ["n.pdf"]);

        // Deletion side preferred: delete anyway.
        let plan_del = plan(
            with_policy(with_delete(push()), ConflictPolicy::PreferLocal),
            &[],
            &snapshot,
            &state,
        );
        assert!(matches!(
            plan_del.actions[0],
            SyncAction::DeleteRemote { .. }
        ));

        // Change side preferred (and Newest): keep the device copy,
        // forget the mapping.
        let plan_keep = plan(
            with_policy(with_delete(push()), ConflictPolicy::Newest),
            &[],
            &snapshot,
            &state,
        );
        assert!(
            plan_keep
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::Forget { .. }))
        );
    }

    #[test]
    fn pull_delete_propagates_device_deletion() {
        let snapshot = remote_snapshot(&[], ""); // doc gone from device
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)]; // unchanged

        let plan_keep = plan(pull(), &locals, &snapshot, &state);
        assert!(plan_keep.actions.is_empty());

        let plan_delete = plan(with_delete(pull()), &locals, &snapshot, &state);
        assert_eq!(
            plan_delete.actions,
            vec![SyncAction::DeleteLocal {
                path: "n.pdf".to_string()
            }]
        );
    }

    #[test]
    fn push_delete_removes_emptied_folders_children_first() {
        let items = vec![
            folder("a", "A", ""),
            folder("b", "B", "a"),
            doc("u1", "n", "b", "pdf", 5),
        ];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("A/B/n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));

        // Without --delete nothing happens (existing semantics).
        assert!(plan(push(), &[], &snapshot, &state).actions.is_empty());

        // With --delete: the document, then the emptied folders,
        // children before parents. The sync root is never deleted.
        let plan_delete = plan(with_delete(push()), &[], &snapshot, &state);
        assert_eq!(
            plan_delete.actions,
            vec![
                SyncAction::DeleteRemote {
                    path: "A/B/n.pdf".to_string(),
                    uuid: "u1".to_string()
                },
                SyncAction::DeleteRemoteFolder {
                    rel_dir: "A/B".to_string(),
                    uuid: "b".to_string()
                },
                SyncAction::DeleteRemoteFolder {
                    rel_dir: "A".to_string(),
                    uuid: "a".to_string()
                },
            ]
        );
    }

    #[test]
    fn push_delete_keeps_folders_with_surviving_content() {
        // Folder A keeps an untracked device doc; folder C keeps
        // skipped (duplicate-name) items; folder D keeps a local file.
        let items = vec![
            folder("a", "A", ""),
            doc("u1", "n", "a", "pdf", 5),
            doc("u2", "untracked", "a", "pdf", 5),
            folder("c", "C", ""),
            doc("u3", "m", "c", "pdf", 5),
            doc("d1", "dup", "c", "pdf", 5),
            doc("d2", "dup", "c", "epub", 5),
            folder("d", "D", ""),
            doc("u4", "k", "d", "pdf", 5),
        ];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("A/n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        state
            .entries
            .insert("C/m.pdf".to_string(), entry("u3", DocKind::Pdf, 10, 1, 5));
        state
            .entries
            .insert("D/k.pdf".to_string(), entry("u4", DocKind::Pdf, 10, 1, 5));
        let locals = vec![local("D/other.pdf", DocKind::Pdf, 10, 1)];

        let plan = plan(with_delete(push()), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions
                .iter()
                .filter(|a| matches!(a, SyncAction::DeleteRemote { .. }))
                .count(),
            3
        );
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::DeleteRemoteFolder { .. }))
        );
    }

    #[test]
    fn push_delete_ignores_preexisting_empty_folders() {
        // "Empty" lost nothing to this plan: it was never synced, so
        // it is never deleted — even though it is empty and absent
        // locally.
        let items = vec![folder("e", "Empty", ""), doc("u1", "n", "", "pdf", 5)];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));

        let plan = plan(with_delete(push()), &[], &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![SyncAction::DeleteRemote {
                path: "n.pdf".to_string(),
                uuid: "u1".to_string()
            }]
        );
    }

    #[test]
    fn pull_delete_removes_emptied_local_dirs() {
        // The document and its folder are gone from the device.
        let snapshot = remote_snapshot(&[], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("A/B/n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let locals = vec![local("A/B/n.pdf", DocKind::Pdf, 10, 1)]; // unchanged

        let plan_delete = plan(with_delete(pull()), &locals, &snapshot, &state);
        assert_eq!(
            plan_delete.actions,
            vec![
                SyncAction::DeleteLocal {
                    path: "A/B/n.pdf".to_string()
                },
                SyncAction::DeleteLocalDir {
                    rel_dir: "A/B".to_string()
                },
                SyncAction::DeleteLocalDir {
                    rel_dir: "A".to_string()
                },
            ]
        );
    }

    #[test]
    fn pull_delete_keeps_local_dirs_alive_on_device() {
        // The document is gone but its folder still exists on the
        // device: the local directory mirrors it and stays.
        let snapshot = remote_snapshot(&[folder("a", "A", "")], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("A/n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let locals = vec![local("A/n.pdf", DocKind::Pdf, 10, 1)];

        let plan_delete = plan(with_delete(pull()), &locals, &snapshot, &state);
        assert_eq!(
            plan_delete.actions,
            vec![SyncAction::DeleteLocal {
                path: "A/n.pdf".to_string()
            }]
        );
    }

    #[test]
    fn two_way_delete_removes_folders_emptied_on_both_sides() {
        // Local n.pdf was deleted; the device folder empties and goes;
        // the local dir for a device-side deletion goes too.
        let items = vec![folder("a", "A", ""), doc("u1", "n", "a", "pdf", 5)];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("A/n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        state
            .entries
            .insert("B/m.pdf".to_string(), entry("u2", DocKind::Pdf, 10, 1, 5));
        let locals = vec![local("B/m.pdf", DocKind::Pdf, 10, 1)];

        let plan = plan(with_delete(two_way()), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![
                SyncAction::DeleteRemote {
                    path: "A/n.pdf".to_string(),
                    uuid: "u1".to_string()
                },
                SyncAction::DeleteLocal {
                    path: "B/m.pdf".to_string()
                },
                SyncAction::DeleteRemoteFolder {
                    rel_dir: "A".to_string(),
                    uuid: "a".to_string()
                },
                SyncAction::DeleteLocalDir {
                    rel_dir: "B".to_string()
                },
            ]
        );
    }

    #[test]
    fn two_way_deletion_vs_change() {
        // Deleted on device, changed locally.
        let snapshot = remote_snapshot(&[], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let locals = vec![local("n.pdf", DocKind::Pdf, 99, 9)]; // changed

        let plan_skip = plan(with_delete(two_way()), &locals, &snapshot, &state);
        assert_eq!(conflicts(&plan_skip), ["n.pdf"]);

        // Newest: the surviving change wins → restore to device.
        let plan_restore = plan(
            with_policy(with_delete(two_way()), ConflictPolicy::Newest),
            &locals,
            &snapshot,
            &state,
        );
        assert!(matches!(plan_restore.actions[0], SyncAction::Upload { .. }));

        // Deletion side preferred: delete locally.
        let plan_del = plan(
            with_policy(with_delete(two_way()), ConflictPolicy::PreferRemote),
            &locals,
            &snapshot,
            &state,
        );
        assert!(matches!(
            plan_del.actions[0],
            SyncAction::DeleteLocal { .. }
        ));
    }

    #[test]
    fn forget_when_both_sides_gone() {
        let snapshot = remote_snapshot(&[], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let plan = plan(two_way(), &[], &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![SyncAction::Forget {
                path: "n.pdf".to_string()
            }]
        );
    }

    // ---- unmapped collisions (adoption) ------------------------------------

    #[test]
    fn collision_adoption_by_policy() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 50)], "");
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 100)]; // local newer

        // Default: conflict.
        let plan_skip = plan(two_way(), &locals, &snapshot, &SyncState::default());
        assert_eq!(conflicts(&plan_skip), ["n.pdf"]);

        // Newest: local wins → adopt the device doc and overwrite it.
        let plan_adopt = plan(
            with_policy(two_way(), ConflictPolicy::Newest),
            &locals,
            &snapshot,
            &SyncState::default(),
        );
        assert_eq!(
            plan_adopt.actions,
            vec![SyncAction::UpdateRemote {
                local: "n.pdf".to_string(),
                kind: DocKind::Pdf,
                uuid: "u1".to_string(),
                hash: None
            }]
        );

        // Remote preferred: download over the local file (adopts too).
        let plan_pull = plan(
            with_policy(two_way(), ConflictPolicy::PreferRemote),
            &locals,
            &snapshot,
            &SyncState::default(),
        );
        assert!(matches!(plan_pull.actions[0], SyncAction::Download { .. }));
    }

    #[test]
    fn collision_never_overwrites_handwriting() {
        // Local n.rmdoc vs device notebook "n": local can never win.
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "notebook", 50)], "");
        let locals = vec![local("n.rmdoc", DocKind::Rmdoc, 10, 100)];
        let plan = plan(
            with_policy(two_way(), ConflictPolicy::PreferLocal),
            &locals,
            &snapshot,
            &SyncState::default(),
        );
        assert_eq!(conflicts(&plan), ["n.rmdoc"]);
    }

    // ---- interrupted-sync recovery (rebind) ---------------------------------

    #[test]
    fn rebind_recovers_interrupted_upload() {
        // A previous sync uploaded test/Notebook.rmdoc but was killed
        // before recording state: the mapping points at a gone uuid
        // while the restored notebook sits on the device unmapped.
        let items = vec![
            folder("f", "test", ""),
            doc("new-uuid", "Notebook", "f", "notebook", 7),
        ];
        let snapshot = remote_snapshot(&items, "");
        let mut state = SyncState::default();
        state.entries.insert(
            "test/Notebook.rmdoc".to_string(),
            entry("old-uuid", DocKind::Rmdoc, 10, 1, 5),
        );
        let locals = vec![local("test/Notebook.rmdoc", DocKind::Rmdoc, 10, 1)];

        let plan = plan(push(), &locals, &snapshot, &state);
        // No wedge: just a state rebind, nothing skipped.
        assert_eq!(
            plan.actions,
            vec![SyncAction::Rebind {
                path: "test/Notebook.rmdoc".to_string(),
                uuid: "new-uuid".to_string(),
                last_modified: 7
            }]
        );
    }

    #[test]
    fn rebind_then_pull_refreshes_content() {
        // Same recovery in pull mode: rebind, then re-download since
        // the successor's content has not been verified.
        let snapshot = remote_snapshot(&[doc("new-uuid", "n", "", "pdf", 7)], "");
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry("old-uuid", DocKind::Pdf, 10, 1, 5),
        );
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)];
        let plan = plan(pull(), &locals, &snapshot, &state);
        assert!(matches!(plan.actions[0], SyncAction::Rebind { .. }));
        assert!(matches!(plan.actions[1], SyncAction::Download { .. }));
    }

    /// Real local files and archives; the client is deliberately unable
    /// to connect. These tests execute only state actions and conflicts.
    struct RebindFixture {
        root: PathBuf,
        fs: LocalFs,
        client: Client,
        state_path: PathBuf,
        state: SyncState,
        remote: RemoteSnapshot,
    }

    impl RebindFixture {
        fn new(last_modified: i64) -> Self {
            let root = std::env::temp_dir().join(format!("rmu-rebind-{}", uuid::Uuid::new_v4()));
            let fs = LocalFs::new(root.join("files"));
            fs.ensure_root().unwrap();
            fs.write("n.pdf", b"agreed").unwrap();
            let state_path = root.join("state.json");
            let mut state = SyncState::default();
            let hash = hash_bytes(b"agreed");
            record_state_with(
                &mut state,
                &state_path,
                &fs,
                "n.pdf",
                DocKind::Pdf,
                "old-uuid",
                5,
                Some(hash.clone()),
                Some(hash),
            )
            .unwrap();
            let session = crate::ssh::SshSession::new(crate::ssh::SshOptions {
                destination: "rebind-test.invalid".to_string(),
                multiplex: false,
                extra_options: vec![
                    "ProxyCommand=false".to_string(),
                    "BatchMode=yes".to_string(),
                ],
                ..Default::default()
            })
            .unwrap();
            Self {
                root,
                fs,
                client: Client::new(session, "/synthetic-xochitl"),
                state_path,
                state,
                remote: remote_snapshot(&[doc("new-uuid", "n", "", "pdf", last_modified)], ""),
            }
        }

        fn locals(&self) -> Vec<LocalEntry> {
            let (mut locals, _) = self.fs.snapshot().unwrap();
            attach_hashes(&self.fs, &mut locals, &self.state).unwrap();
            locals
        }

        fn execute(&mut self, plan: &Plan) -> Outcome {
            let outcome = execute(
                &self.client,
                &crate::progress::NoProgress,
                &self.fs,
                plan,
                &mut BTreeMap::new(),
                &mut self.state,
                &self.state_path,
            )
            .unwrap();
            // Every next planner pass uses the persisted archive, as a
            // new invocation would, rather than only the in-memory state.
            self.state = SyncState::load(&self.state_path).unwrap();
            outcome
        }
    }

    impl Drop for RebindFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn rebind_skipped_conflict_survives_two_runs() {
        [5, 7].into_iter().for_each(|last_modified| {
            let mut fixture = RebindFixture::new(last_modified);
            fixture.fs.write("n.pdf", b"unresolved local edit").unwrap();
            fixture.remote.docs[0].payload_hash = Some(hash_bytes(b"replacement"));
            let before = fixture.state.serialize().unwrap();
            let first = plan(
                two_way(),
                &fixture.locals(),
                &fixture.remote,
                &fixture.state,
            );
            assert!(matches!(first.actions[0], SyncAction::Rebind { .. }));
            assert_eq!(conflicts(&first), ["n.pdf"]);
            assert_eq!(first.actions.len(), 2);
            assert_eq!(fixture.execute(&first).conflicts.len(), 1);

            let rebound = &fixture.state.entries["n.pdf"];
            let baseline = SyncState::parse(&before).unwrap();
            let agreed = &baseline.entries["n.pdf"];
            assert_eq!(rebound.uuid, "new-uuid");
            assert!(rebound.remote_pending);
            assert_eq!(rebound.remote_last_modified, agreed.remote_last_modified);
            assert_eq!(rebound.payload_hash, agreed.payload_hash);
            assert_eq!(rebound.local_hash, agreed.local_hash);
            assert_eq!(rebound.local_size, agreed.local_size);
            assert_eq!(rebound.local_mtime_ms, agreed.local_mtime_ms);
            assert_eq!(
                device_hash_candidates(&fixture.locals(), &fixture.remote, &fixture.state),
                [("new-uuid".to_string(), "pdf")]
            );

            let second = plan(
                two_way(),
                &fixture.locals(),
                &fixture.remote,
                &fixture.state,
            );
            assert_eq!(conflicts(&second), ["n.pdf"]);
            assert_eq!(second.actions.len(), 1);
            assert_eq!(fixture.execute(&second).conflicts.len(), 1);
            assert_eq!(fixture.fs.read("n.pdf").unwrap(), b"unresolved local edit");
            assert!(fixture.state.entries["n.pdf"].remote_pending);
        });
    }

    #[test]
    fn rebind_interrupted_pull_keeps_successor_content_pending() {
        [5, 7].into_iter().for_each(|last_modified| {
            let mut fixture = RebindFixture::new(last_modified);
            // No payload hash: even equal timestamps cannot prove that
            // two different UUIDs have the same content.
            let first = plan(pull(), &fixture.locals(), &fixture.remote, &fixture.state);
            assert!(matches!(first.actions[0], SyncAction::Rebind { .. }));
            assert!(matches!(&first.actions[1], SyncAction::Download { uuid, .. } if uuid == "new-uuid"));
            // Simulate interruption at the durable action boundary,
            // before the planned download executes.
            fixture.execute(&Plan {
                actions: first.actions[..1].to_vec(),
            });
            let resumed = plan(pull(), &fixture.locals(), &fixture.remote, &fixture.state);
            assert!(matches!(&resumed.actions[..], [SyncAction::Download { uuid, .. }] if uuid == "new-uuid"));
            assert!(fixture.state.entries["n.pdf"].remote_pending);
            assert_eq!(fixture.fs.read("n.pdf").unwrap(), b"agreed");

            // The successful transfer's archive writer must clear the
            // pending flag; model the downloaded bytes locally.
            fixture.fs.write("n.pdf", b"replacement").unwrap();
            let hash = hash_bytes(b"replacement");
            record_state_with(
                &mut fixture.state,
                &fixture.state_path,
                &fixture.fs,
                "n.pdf",
                DocKind::Pdf,
                "new-uuid",
                last_modified,
                Some(hash.clone()),
                Some(hash),
            )
            .unwrap();
            fixture.state = SyncState::load(&fixture.state_path).unwrap();
            assert!(!fixture.state.entries["n.pdf"].remote_pending);
            assert!(plan(pull(), &fixture.locals(), &fixture.remote, &fixture.state).actions.is_empty());
        });
    }

    #[test]
    fn rebind_pull_refresh_uses_successor_and_survives_interruption() {
        let mut fixture = RebindFixture::new(5);
        // Size differs from the local file, so the copy-size heuristic
        // cannot accidentally supply the replacement hash for this test.
        assert_eq!(
            device_hash_candidates(&fixture.locals(), &fixture.remote, &fixture.state),
            [("new-uuid".to_string(), "pdf")]
        );
        fixture.remote.docs[0].payload_hash = Some(hash_bytes(b"agreed"));
        let first = plan(pull(), &fixture.locals(), &fixture.remote, &fixture.state);
        assert!(
            matches!(&first.actions[..], [SyncAction::Rebind { .. }, SyncAction::Refresh { uuid, .. }] if uuid == "new-uuid")
        );
        fixture.execute(&Plan {
            actions: first.actions[..1].to_vec(),
        });
        let resumed = plan(pull(), &fixture.locals(), &fixture.remote, &fixture.state);
        assert!(
            matches!(&resumed.actions[..], [SyncAction::Refresh { uuid, .. }] if uuid == "new-uuid")
        );
        fixture.execute(&resumed);
        let st = &fixture.state.entries["n.pdf"];
        assert_eq!(st.uuid, "new-uuid");
        assert!(!st.remote_pending);
        assert_eq!(st.payload_hash, Some(hash_bytes(b"agreed")));
        assert!(
            plan(pull(), &fixture.locals(), &fixture.remote, &fixture.state)
                .actions
                .is_empty()
        );
        assert!(
            device_hash_candidates(&fixture.locals(), &fixture.remote, &fixture.state).is_empty()
        );
    }

    #[test]
    fn rebind_push_resolution_targets_successor_not_missing_uuid() {
        [false, true].into_iter().for_each(|remote_unchanged| {
            let fixture = &mut RebindFixture::new(5);
            fixture.fs.write("n.pdf", b"local edit").unwrap();
            fixture.remote.docs[0].payload_hash = Some(hash_bytes(if remote_unchanged {
                b"agreed"
            } else {
                b"replacement"
            }));
            let options = if remote_unchanged {
                two_way()
            } else {
                with_policy(two_way(), ConflictPolicy::PreferLocal)
            };
            let result = plan(options, &fixture.locals(), &fixture.remote, &fixture.state);
            assert!(matches!(&result.actions[..], [SyncAction::Rebind { .. }, SyncAction::UpdateRemote { uuid, .. }] if uuid == "new-uuid"));
        });
    }

    #[test]
    fn rebind_with_local_deletion_preserves_successor_on_both_runs() {
        let mut fixture = RebindFixture::new(5);
        fixture.fs.remove("n.pdf").unwrap();
        let options = with_delete(two_way());
        let first = plan(options, &fixture.locals(), &fixture.remote, &fixture.state);
        assert!(matches!(
            &first.actions[..],
            [SyncAction::Rebind { .. }, SyncAction::Conflict { .. }]
        ));
        fixture.execute(&first);
        let second = plan(options, &fixture.locals(), &fixture.remote, &fixture.state);
        assert!(matches!(&second.actions[..], [SyncAction::Conflict { .. }]));
    }

    #[test]
    fn rebind_pending_field_defaults_for_existing_archives() {
        let fixture = RebindFixture::new(5);
        let json = fixture.state.serialize().unwrap();
        assert!(!json.contains("remote_pending"));
        assert!(!SyncState::parse(&json).unwrap().entries["n.pdf"].remote_pending);
    }

    #[test]
    fn rebind_requires_matching_kind() {
        // Dangling pdf mapping; a *notebook* with the same name is not
        // a valid successor.
        let snapshot = remote_snapshot(&[doc("new-uuid", "n", "", "notebook", 7)], "");
        let mut state = SyncState::default();
        state.entries.insert(
            "n.pdf".to_string(),
            entry("old-uuid", DocKind::Pdf, 10, 1, 5),
        );
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)];
        let plan = plan(push(), &locals, &snapshot, &state);
        assert!(
            plan.actions
                .iter()
                .all(|a| !matches!(a, SyncAction::Rebind { .. }))
        );
    }

    // ---- fs ↔ fs planning ---------------------------------------------------

    #[test]
    fn files_first_sync_copies_toward_destination() {
        let a = vec![local("x/a.pdf", DocKind::Pdf, 10, 1)];
        let b = vec![local("y/b.epub", DocKind::Epub, 20, 2)];

        // Push: only A-side files copy.
        let plan = plan_files(push(), &a, &b, &SyncState::default());
        assert_eq!(
            plan,
            vec![FileAction::CopyToB {
                path: "x/a.pdf".to_string()
            }]
        );

        // Two-way: both copy.
        let plan = plan_files(two_way(), &a, &b, &SyncState::default());
        assert_eq!(
            plan,
            vec![
                FileAction::CopyToB {
                    path: "x/a.pdf".to_string()
                },
                FileAction::CopyToA {
                    path: "y/b.epub".to_string()
                },
            ]
        );
    }

    #[test]
    fn pair_deletions_respect_direction_and_policy() {
        use ConflictPolicy::{Newest, PreferLocal, PreferRemote, Skip};
        use Disposition::{CopyToA, CopyToB, DeleteA, DeleteB, Forget};
        use Mode::{Pull, Push, TwoWay};

        let conflict = Some(Disposition::Conflict(
            "deleted on one side but changed on the other",
        ));
        // Each row specifies: mode, surviving side, unchanged outcome,
        // changed outcomes (skip/newest/prefer A/prefer B), and recopy
        // without --delete. Blocked deletions retain the mapping.
        let cases = [
            (
                Push,
                true,
                None,
                [conflict, Some(CopyToB), Some(CopyToB), None],
                Some(CopyToB),
            ),
            (
                Pull,
                true,
                Some(DeleteA),
                [conflict, Some(Forget), Some(Forget), Some(DeleteA)],
                None,
            ),
            (
                TwoWay,
                true,
                Some(DeleteA),
                [conflict, Some(CopyToB), Some(CopyToB), Some(DeleteA)],
                Some(CopyToB),
            ),
            (
                Push,
                false,
                Some(DeleteB),
                [conflict, Some(Forget), Some(DeleteB), Some(Forget)],
                None,
            ),
            (
                Pull,
                false,
                None,
                [conflict, Some(CopyToA), None, Some(CopyToA)],
                Some(CopyToA),
            ),
            (
                TwoWay,
                false,
                Some(DeleteB),
                [conflict, Some(CopyToA), Some(DeleteB), Some(CopyToA)],
                Some(CopyToA),
            ),
        ];
        cases
            .into_iter()
            .for_each(|(mode, survives_a, unchanged, changed_outcomes, recopy)| {
                [false, true].into_iter().for_each(|changed| {
                    [false, true].into_iter().for_each(|delete| {
                        [Skip, Newest, PreferLocal, PreferRemote]
                            .into_iter()
                            .zip(changed_outcomes)
                            .for_each(|(policy, changed_outcome)| {
                                let options = SyncOptions {
                                    mode,
                                    delete,
                                    conflict: policy,
                                };
                                let expected = if !delete {
                                    recopy
                                } else if changed {
                                    changed_outcome
                                } else {
                                    unchanged
                                };
                                let lm = if changed { 200 } else { 100 };
                                let file = local("n.pdf", DocKind::Pdf, 10, lm);
                                let (a, b) = if survives_a {
                                    (vec![file], vec![])
                                } else {
                                    (vec![], vec![file])
                                };
                                let mut state = SyncState::default();
                                state
                                    .entries
                                    .insert("n.pdf".to_string(), file_entry(10, 100, 100, 10));
                                let path = "n.pdf".to_string();
                                let expected_files: Vec<_> = expected
                                    .into_iter()
                                    .map(|d| match d {
                                        CopyToA => FileAction::CopyToA { path: path.clone() },
                                        CopyToB => FileAction::CopyToB { path: path.clone() },
                                        DeleteA => FileAction::DeleteA { path: path.clone() },
                                        DeleteB => FileAction::DeleteB { path: path.clone() },
                                        Forget => FileAction::Forget { path: path.clone() },
                                        Disposition::Conflict(reason) => FileAction::Conflict {
                                            path: path.clone(),
                                            reason: reason.to_string(),
                                        },
                                    })
                                    .collect();
                                assert_eq!(
                                    plan_files(options, &a, &b, &state),
                                    expected_files,
                                    "files: {options:?}, survives_a={survives_a}, changed={changed}"
                                );

                                let present = remote_snapshot(
                                    &[doc(
                                        if survives_a { "a1" } else { "b1" },
                                        "n",
                                        "",
                                        "notebook",
                                        lm,
                                    )],
                                    "",
                                );
                                let empty = remote_snapshot(&[], "");
                                let (a, b) = if survives_a {
                                    (&present, &empty)
                                } else {
                                    (&empty, &present)
                                };
                                let mut state = PairState::default();
                                state
                                    .entries
                                    .insert("n".to_string(), pair_entry("a1", 100, "b1", 100));
                                let key = "n".to_string();
                                let expected_docs: Vec<_> = expected
                                    .into_iter()
                                    .map(|d| match d {
                                        CopyToA => DocAction::CopyToA {
                                            key: key.clone(),
                                            rel_dir: String::new(),
                                            name: key.clone(),
                                            from_uuid: "b1".to_string(),
                                            from_lm: lm,
                                            replace_uuid: None,
                                        },
                                        CopyToB => DocAction::CopyToB {
                                            key: key.clone(),
                                            rel_dir: String::new(),
                                            name: key.clone(),
                                            from_uuid: "a1".to_string(),
                                            from_lm: lm,
                                            replace_uuid: None,
                                        },
                                        DeleteA => DocAction::DeleteOnA {
                                            key: key.clone(),
                                            uuid: "a1".to_string(),
                                        },
                                        DeleteB => DocAction::DeleteOnB {
                                            key: key.clone(),
                                            uuid: "b1".to_string(),
                                        },
                                        Forget => DocAction::Forget { key: key.clone() },
                                        Disposition::Conflict(reason) => DocAction::Conflict {
                                            key: key.clone(),
                                            reason: reason.to_string(),
                                        },
                                    })
                                    .collect();
                                assert_eq!(
                                    plan_docs(options, a, b, &state),
                                    expected_docs,
                                    "docs: {options:?}, survives_a={survives_a}, changed={changed}"
                                );
                            });
                    });
                });
            });
    }

    #[test]
    fn files_destination_deletion_keeps_source_across_runs() {
        [Mode::Push, Mode::Pull].into_iter().for_each(|mode| {
            [
                ConflictPolicy::Skip,
                ConflictPolicy::Newest,
                ConflictPolicy::PreferLocal,
                ConflictPolicy::PreferRemote,
            ]
            .into_iter()
            .for_each(|conflict| {
                [false, true].into_iter().for_each(|changed| {
                    let root = std::env::temp_dir()
                        .join(format!("rmu-delete-direction-{}", uuid::Uuid::new_v4()));
                    let a = LocalFs::new(root.join("a"));
                    let b = LocalFs::new(root.join("b"));
                    a.ensure_root().unwrap();
                    b.ensure_root().unwrap();
                    let (source, destination) = if mode == Mode::Push {
                        (&a, &b)
                    } else {
                        (&b, &a)
                    };
                    source
                        .write("n.pdf", b"original synthetic document")
                        .unwrap();
                    let state_path = root.join("state.json");
                    let mut state = SyncState::default();
                    let options = SyncOptions {
                        mode,
                        delete: true,
                        conflict,
                    };
                    let actions = plan_files(
                        options,
                        &a.snapshot().unwrap().0,
                        &b.snapshot().unwrap().0,
                        &state,
                    );
                    execute_files(
                        &a,
                        &b,
                        &crate::progress::NoProgress,
                        &actions,
                        &mut state,
                        &state_path,
                    )
                    .unwrap();
                    assert_eq!(
                        source.read("n.pdf").unwrap(),
                        destination.read("n.pdf").unwrap()
                    );
                    destination.remove("n.pdf").unwrap();
                    if changed {
                        // A size change avoids relying on filesystem timestamp precision.
                        source
                            .write("n.pdf", b"edited synthetic document with a different size")
                            .unwrap();
                    }
                    let expected = source.read("n.pdf").unwrap();
                    (0..2).for_each(|_| {
                        let mut state = SyncState::load(&state_path).unwrap();
                        let actions = plan_files(
                            options,
                            &a.snapshot().unwrap().0,
                            &b.snapshot().unwrap().0,
                            &state,
                        );
                        let outcome = execute_files(
                            &a,
                            &b,
                            &crate::progress::NoProgress,
                            &actions,
                            &mut state,
                            &state_path,
                        )
                        .unwrap();
                        assert_eq!(
                            outcome.deleted_a + outcome.deleted_b,
                            0,
                            "{options:?}, changed={changed}"
                        );
                        assert_eq!(source.read("n.pdf").unwrap(), expected);
                        assert!(state.entries.contains_key("n.pdf"));
                    });
                    fs::remove_dir_all(root).unwrap();
                });
            });
        });
    }

    #[test]
    fn files_delete_propagates_a_side_deletion() {
        let mut state = SyncState::default();
        state
            .entries
            .insert("doc.pdf".to_string(), file_entry(11, 100, 200, 11));
        let a: Vec<LocalEntry> = vec![];
        let b = vec![local("doc.pdf", DocKind::Pdf, 11, 200)];
        let plan = plan_files(with_delete(push()), &a, &b, &state);
        assert_eq!(
            plan,
            vec![FileAction::DeleteB {
                path: "doc.pdf".to_string()
            }]
        );
    }

    fn file_entry(size: u64, a_mtime: i64, b_mtime: i64, b_size: u64) -> StateEntry {
        StateEntry {
            uuid: String::new(),
            kind: DocKind::Pdf,
            local_size: size,
            local_mtime_ms: a_mtime,
            remote_last_modified: b_mtime,
            remote_pending: false,
            remote_size: Some(b_size),
            local_hash: None,
            payload_hash: None,
        }
    }

    #[test]
    fn files_mapped_changes_propagate() {
        let mut state = SyncState::default();
        state
            .entries
            .insert("a.pdf".to_string(), file_entry(10, 1, 1, 10));

        // Unchanged: no-op.
        let same_a = vec![local("a.pdf", DocKind::Pdf, 10, 1)];
        let same_b = vec![local("a.pdf", DocKind::Pdf, 10, 1)];
        assert!(plan_files(two_way(), &same_a, &same_b, &state).is_empty());

        // A changed: copy to B.
        let changed_a = vec![local("a.pdf", DocKind::Pdf, 99, 9)];
        let plan = plan_files(two_way(), &changed_a, &same_b, &state);
        assert_eq!(
            plan,
            vec![FileAction::CopyToB {
                path: "a.pdf".to_string()
            }]
        );

        // Both changed: conflict by default; newest wins with policy.
        let changed_b = vec![local("a.pdf", DocKind::Pdf, 50, 100)];
        let plan = plan_files(two_way(), &changed_a, &changed_b, &state);
        assert!(matches!(plan[0], FileAction::Conflict { .. }));
        let plan = plan_files(
            with_policy(two_way(), ConflictPolicy::Newest),
            &changed_a,
            &changed_b,
            &state,
        );
        // B mtime 100 > A mtime 9: B wins.
        assert_eq!(
            plan,
            vec![FileAction::CopyToA {
                path: "a.pdf".to_string()
            }]
        );
    }

    #[test]
    fn files_deletions_propagate_only_with_delete() {
        let mut state = SyncState::default();
        state
            .entries
            .insert("a.pdf".to_string(), file_entry(10, 1, 1, 10));
        let a = vec![local("a.pdf", DocKind::Pdf, 10, 1)];

        // B deleted, no --delete: recopy in two-way.
        let plan = plan_files(two_way(), &a, &[], &state);
        assert_eq!(
            plan,
            vec![FileAction::CopyToB {
                path: "a.pdf".to_string()
            }]
        );

        // With --delete: propagate to A.
        let plan = plan_files(with_delete(two_way()), &a, &[], &state);
        assert_eq!(
            plan,
            vec![FileAction::DeleteA {
                path: "a.pdf".to_string()
            }]
        );

        // Deleted everywhere: forget.
        let plan = plan_files(two_way(), &[], &[], &state);
        assert_eq!(
            plan,
            vec![FileAction::Forget {
                path: "a.pdf".to_string()
            }]
        );
    }

    #[test]
    fn files_unmapped_collision_uses_policy() {
        let a = vec![local("a.pdf", DocKind::Pdf, 10, 100)];
        let b = vec![local("a.pdf", DocKind::Pdf, 20, 50)];
        let plan = plan_files(two_way(), &a, &b, &SyncState::default());
        assert!(matches!(plan[0], FileAction::Conflict { .. }));
        let plan = plan_files(
            with_policy(two_way(), ConflictPolicy::Newest),
            &a,
            &b,
            &SyncState::default(),
        );
        assert_eq!(
            plan,
            vec![FileAction::CopyToB {
                path: "a.pdf".to_string()
            }]
        );
    }

    // ---- fs ↔ fs execution --------------------------------------------------

    #[derive(Default)]
    struct TransferFs<'a> {
        files: std::cell::RefCell<BTreeMap<String, (Vec<u8>, i64)>>,
        edit_after_read: std::cell::RefCell<Option<Vec<u8>>>,
        after_write: std::cell::RefCell<Option<Box<dyn FnOnce() + 'a>>>,
    }

    impl TransferFs<'_> {
        fn put(&self, path: &str, data: &[u8]) {
            let mut files = self.files.borrow_mut();
            let stamp = files.get(path).map_or(1, |(_, stamp)| stamp + 1);
            files.insert(path.to_string(), (data.to_vec(), stamp));
        }
    }

    impl FsEndpoint for TransferFs<'_> {
        fn label(&self) -> String {
            "synthetic transfer endpoint".to_string()
        }
        fn ensure_root(&self) -> Result<()> {
            Ok(())
        }
        fn snapshot(&self) -> Result<(Vec<LocalEntry>, usize)> {
            Ok((
                self.files
                    .borrow()
                    .iter()
                    .map(|(path, (data, stamp))| {
                        local(path, DocKind::Pdf, data.len() as u64, *stamp)
                    })
                    .collect(),
                0,
            ))
        }
        fn read(&self, path: &str) -> Result<Vec<u8>> {
            let data = self.files.borrow()[path].0.clone();
            if let Some(edit) = self.edit_after_read.borrow_mut().take() {
                self.put(path, &edit);
            }
            Ok(data)
        }
        fn write(&self, path: &str, data: &[u8]) -> Result<()> {
            self.put(path, data);
            if let Some(hook) = self.after_write.borrow_mut().take() {
                hook();
            }
            Ok(())
        }
        fn stat(&self, path: &str) -> Result<(u64, i64)> {
            let files = self.files.borrow();
            let (data, stamp) = &files[path];
            Ok((data.len() as u64, *stamp))
        }
        fn content_hash(&self, _: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn remove(&self, _: &str) -> Result<()> {
            panic!("unexpected removal")
        }
        fn rename(&self, _: &str, _: &str) -> Result<()> {
            panic!("unexpected rename")
        }
        fn remove_dir(&self, _: &str) -> Result<bool> {
            panic!("unexpected directory removal")
        }
    }

    struct TransferArchive(PathBuf);

    impl TransferArchive {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("rmu-transfer-test-{}.json", uuid::Uuid::new_v4())),
            )
        }
    }

    impl Drop for TransferArchive {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn transfer_plan(a: &dyn FsEndpoint, b: &dyn FsEndpoint, state: &SyncState) -> Vec<FileAction> {
        plan_files(
            two_way(),
            &a.snapshot().unwrap().0,
            &b.snapshot().unwrap().0,
            state,
        )
    }

    #[test]
    fn files_source_edits_during_transfer_are_not_archived() {
        // Both directions, first/existing agreements, edits during read/write,
        // and both size-changing and same-size edits with a changed stamp.
        [false, true].into_iter().for_each(|to_b| {
            [false, true].into_iter().for_each(|mapped| {
                [false, true].into_iter().for_each(|during_write| {
                    [b"new".as_slice(), b"longer edit".as_slice()]
                        .into_iter()
                        .for_each(|edit| {
                            let source = TransferFs::default();
                            let destination = TransferFs::default();
                            let (a, b): (&dyn FsEndpoint, &dyn FsEndpoint) = if to_b {
                                (&source, &destination)
                            } else {
                                (&destination, &source)
                            };
                            let archive = TransferArchive::new();
                            let mut state = SyncState::default();
                            source.put("doc.pdf", b"old");
                            if mapped {
                                let plan = transfer_plan(a, b, &state);
                                execute_files(
                                    a,
                                    b,
                                    &crate::progress::NoProgress,
                                    &plan,
                                    &mut state,
                                    &archive.0,
                                )
                                .unwrap();
                                state = SyncState::load(&archive.0).unwrap();
                                assert!(transfer_plan(a, b, &state).is_empty());
                            }
                            source.put("doc.pdf", b"src");
                            let plan = transfer_plan(a, b, &state);
                            assert_eq!(plan.len(), 1);
                            let before = state.serialize().unwrap();
                            if during_write {
                                *destination.after_write.borrow_mut() =
                                    Some(Box::new(|| source.put("doc.pdf", edit)));
                            } else {
                                *source.edit_after_read.borrow_mut() = Some(edit.to_vec());
                            }
                            let err = execute_files(
                                a,
                                b,
                                &crate::progress::NoProgress,
                                &plan,
                                &mut state,
                                &archive.0,
                            )
                            .unwrap_err();
                            assert!(
                                err.to_string()
                                    .contains("source changed during sync transfer")
                            );
                            assert_eq!(state.serialize().unwrap(), before);
                            if mapped {
                                assert_eq!(
                                    SyncState::load(&archive.0).unwrap().serialize().unwrap(),
                                    before
                                );
                            } else {
                                assert!(!archive.0.exists());
                            }
                            if during_write {
                                assert_eq!(destination.read("doc.pdf").unwrap(), b"src");
                            } else if mapped {
                                assert_eq!(destination.read("doc.pdf").unwrap(), b"old");
                            } else {
                                assert!(destination.files.borrow().is_empty());
                            }
                            // Reload as a new invocation would: the source edit
                            // must still be pending, never a no-op.
                            state = SyncState::load(&archive.0).unwrap();
                            assert!(!transfer_plan(a, b, &state).is_empty());
                            // An opposite-side edit must conflict, not overwrite
                            // the untransferred source version on the next run.
                            destination.put("doc.pdf", b"dst");
                            let next = transfer_plan(a, b, &state);
                            assert!(matches!(next.as_slice(), [FileAction::Conflict { .. }]));
                            let outcome = execute_files(
                                a,
                                b,
                                &crate::progress::NoProgress,
                                &next,
                                &mut state,
                                &archive.0,
                            )
                            .unwrap();
                            assert_eq!(outcome.conflicts.len(), 1);
                            assert_eq!(source.read("doc.pdf").unwrap(), edit);
                            assert_eq!(destination.read("doc.pdf").unwrap(), b"dst");
                        });
                });
            });
        });
    }

    #[test]
    fn files_source_race_preserves_prior_incremental_saves() {
        let source = TransferFs::default();
        let destination = TransferFs::default();
        let archive = TransferArchive::new();
        let mut state = SyncState::default();
        source.put("a.pdf", b"first");
        source.put("b.pdf", b"second");
        source.put("c.pdf", b"third");
        // Arm the second transfer's read race after the first write succeeds.
        *destination.after_write.borrow_mut() = Some(Box::new(|| {
            *source.edit_after_read.borrow_mut() = Some(b"edited".to_vec());
        }));
        let plan = transfer_plan(&source, &destination, &state);
        assert!(
            execute_files(
                &source,
                &destination,
                &crate::progress::NoProgress,
                &plan,
                &mut state,
                &archive.0
            )
            .is_err()
        );
        let mut saved = SyncState::load(&archive.0).unwrap();
        assert_eq!(
            saved.entries.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["a.pdf"]
        );
        assert_eq!(saved.serialize().unwrap(), state.serialize().unwrap());
        assert_eq!(destination.read("a.pdf").unwrap(), b"first");
        assert_eq!(destination.files.borrow().len(), 1);
        let retry = transfer_plan(&source, &destination, &saved);
        assert_eq!(
            retry,
            vec![
                FileAction::CopyToB {
                    path: "b.pdf".to_string()
                },
                FileAction::CopyToB {
                    path: "c.pdf".to_string()
                }
            ]
        );
        execute_files(
            &source,
            &destination,
            &crate::progress::NoProgress,
            &retry,
            &mut saved,
            &archive.0,
        )
        .unwrap();
        let saved = SyncState::load(&archive.0).unwrap();
        assert!(transfer_plan(&source, &destination, &saved).is_empty());
        assert_eq!(destination.read("b.pdf").unwrap(), b"edited");
    }

    // ---- tablet ↔ tablet planning ---------------------------------------------

    fn pair_entry(uuid_a: &str, lm_a: i64, uuid_b: &str, lm_b: i64) -> PairEntry {
        PairEntry {
            uuid_a: uuid_a.to_string(),
            lm_a,
            uuid_b: uuid_b.to_string(),
            lm_b,
        }
    }

    #[test]
    fn docs_first_sync_copies_and_creates_folders() {
        let a = remote_snapshot(
            &[
                folder("fa", "Books", ""),
                doc("a1", "Novel", "fa", "notebook", 5),
            ],
            "",
        );
        let b = remote_snapshot(&[doc("b1", "OnlyB", "", "pdf", 6)], "");

        let plan = plan_docs(two_way(), &a, &b, &PairState::default());
        assert_eq!(
            plan,
            vec![
                DocAction::CreateFolderOnB {
                    rel_dir: "Books".to_string()
                },
                DocAction::CopyToB {
                    key: "Books/Novel".to_string(),
                    rel_dir: "Books".to_string(),
                    name: "Novel".to_string(),
                    from_uuid: "a1".to_string(),
                    from_lm: 5,
                    replace_uuid: None,
                },
                DocAction::CopyToA {
                    key: "OnlyB".to_string(),
                    rel_dir: String::new(),
                    name: "OnlyB".to_string(),
                    from_uuid: "b1".to_string(),
                    from_lm: 6,
                    replace_uuid: None,
                },
            ]
        );

        // One-way A->B copies only A's documents.
        let plan = plan_docs(push(), &a, &b, &PairState::default());
        assert!(
            plan.iter()
                .all(|action| !matches!(action, DocAction::CopyToA { .. }))
        );
    }

    #[test]
    fn docs_change_replaces_destination_copy() {
        let a = remote_snapshot(&[doc("a1", "n", "", "notebook", 9)], ""); // changed
        let b = remote_snapshot(&[doc("b1", "n", "", "notebook", 5)], "");
        let mut state = PairState::default();
        state
            .entries
            .insert("n".to_string(), pair_entry("a1", 5, "b1", 5));

        let plan = plan_docs(two_way(), &a, &b, &state);
        assert_eq!(
            plan,
            vec![DocAction::CopyToB {
                key: "n".to_string(),
                rel_dir: String::new(),
                name: "n".to_string(),
                from_uuid: "a1".to_string(),
                from_lm: 9,
                replace_uuid: Some("b1".to_string()),
            }]
        );
    }

    #[test]
    fn docs_uuid_drift_counts_as_change() {
        // The document on A was replaced (deleted + recreated): same
        // lastModified would be a coincidence, but even then the uuid
        // differs from the recorded one → treated as changed.
        let a = remote_snapshot(&[doc("a2", "n", "", "pdf", 5)], "");
        let b = remote_snapshot(&[doc("b1", "n", "", "pdf", 5)], "");
        let mut state = PairState::default();
        state
            .entries
            .insert("n".to_string(), pair_entry("a1", 5, "b1", 5));
        let plan = plan_docs(two_way(), &a, &b, &state);
        assert!(matches!(plan[0], DocAction::CopyToB { .. }));
    }

    #[test]
    fn docs_conflict_and_deletions() {
        // Both changed: conflict by default, newest wins by policy.
        let a = remote_snapshot(&[doc("a1", "n", "", "pdf", 100)], "");
        let b = remote_snapshot(&[doc("b1", "n", "", "pdf", 200)], "");
        let mut state = PairState::default();
        state
            .entries
            .insert("n".to_string(), pair_entry("a1", 5, "b1", 5));
        let plan = plan_docs(two_way(), &a, &b, &state);
        assert!(matches!(plan[0], DocAction::Conflict { .. }));
        let plan = plan_docs(
            with_policy(two_way(), ConflictPolicy::Newest),
            &a,
            &b,
            &state,
        );
        assert!(matches!(plan[0], DocAction::CopyToA { .. })); // B newer

        // Deleted on B, unchanged on A, --delete: propagate.
        let b_empty = remote_snapshot(&[], "");
        let a_unchanged = remote_snapshot(&[doc("a1", "n", "", "pdf", 5)], "");
        let plan = plan_docs(with_delete(two_way()), &a_unchanged, &b_empty, &state);
        assert_eq!(
            plan,
            vec![DocAction::DeleteOnA {
                key: "n".to_string(),
                uuid: "a1".to_string()
            }]
        );

        // Gone everywhere: forget.
        let a_empty = remote_snapshot(&[], "");
        let plan = plan_docs(two_way(), &a_empty, &b_empty, &state);
        assert_eq!(
            plan,
            vec![DocAction::Forget {
                key: "n".to_string()
            }]
        );
    }

    #[test]
    fn docs_excluded_mappings_block_both_sides_and_preserve_archives() {
        excluded_cases().into_iter().for_each(|(key, items)| {
            let excluded = remote_snapshot(&items, "");
            let (dir, name) = key.rsplit_once('/').unwrap_or(("", key));
            let peer = RemoteSnapshot {
                docs: vec![RemoteDoc {
                    rel_dir: dir.to_string(),
                    name: name.to_string(),
                    uuid: "peer".to_string(),
                    doc_type: RemoteType::Pdf,
                    last_modified: 5,
                    size_bytes: Some(100),
                    payload_hash: None,
                }],
                ..RemoteSnapshot::default()
            };
            [false, true].into_iter().for_each(|reverse| {
                let (a, b, entry) = if reverse {
                    (&peer, &excluded, pair_entry("peer", 5, "u1", 5))
                } else {
                    (&excluded, &peer, pair_entry("u1", 5, "peer", 5))
                };
                let mut state = PairState::default();
                state.entries.insert(key.to_string(), entry);
                [push(), pull(), two_way()].into_iter().for_each(|mode| {
                    [false, true].into_iter().for_each(|delete| {
                        [
                            ConflictPolicy::Skip,
                            ConflictPolicy::Newest,
                            ConflictPolicy::PreferLocal,
                            ConflictPolicy::PreferRemote,
                        ]
                        .into_iter()
                        .for_each(|conflict| {
                            let result = plan_docs(
                                SyncOptions {
                                    delete,
                                    conflict,
                                    ..mode
                                },
                                a,
                                b,
                                &state,
                            );
                            assert!(
                                result
                                    .iter()
                                    .all(|a| matches!(a, DocAction::Conflict { .. })),
                                "{key}: {result:?}"
                            );
                        });
                    });
                });
            });
        });
    }

    #[test]
    fn docs_duplicate_subtrees_preserve_successful_sync_across_runs() {
        [false, true].into_iter().for_each(|nested| {
            let (key, mut items) = if nested {
                (
                    "A/n",
                    vec![folder("a", "A", ""), doc("u1", "n", "a", "pdf", 5)],
                )
            } else {
                ("n", vec![doc("u1", "n", "", "pdf", 5)])
            };
            let clean = remote_snapshot(&items, "");
            let empty = remote_snapshot(&[], "");
            assert!(
                plan_docs(push(), &clean, &empty, &PairState::default())
                    .iter()
                    .any(|a| matches!(a, DocAction::CopyToB { key: k, .. } if k == key))
            );
            let mut peer = clean.clone();
            peer.docs[0].uuid = "peer".to_string();
            let mut state = PairState::default();
            state
                .entries
                .insert(key.to_string(), pair_entry("u1", 5, "peer", 5));
            assert!(plan_docs(two_way(), &clean, &peer, &state).is_empty());
            items.push(if nested {
                folder("dup", "A", "")
            } else {
                doc("dup", "n", "", "pdf", 5)
            });
            let excluded = remote_snapshot(&items, "");
            (0..2).for_each(|_| {
                let result = plan_docs(with_delete(two_way()), &excluded, &peer, &state);
                assert!(
                    result
                        .iter()
                        .all(|a| matches!(a, DocAction::Conflict { .. }))
                );
                state = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
            });
            assert!(plan_docs(with_delete(two_way()), &clean, &peer, &state).is_empty());
            assert_eq!(
                plan_docs(with_delete(two_way()), &empty, &peer, &state),
                vec![DocAction::DeleteOnB {
                    key: key.to_string(),
                    uuid: "peer".to_string()
                }]
            );
            // Neither a missing descendant UUID nor loss of the archive licenses
            // copying into an ambiguous destination subtree.
            items.retain(|item| item.uuid != "u1");
            if !nested {
                items.push(doc("dup2", "n", "", "pdf", 5));
            }
            let excluded = remote_snapshot(&items, "");
            [&state, &PairState::default()]
                .into_iter()
                .for_each(|archive| {
                    assert!(
                        plan_docs(two_way(), &excluded, &peer, archive)
                            .iter()
                            .all(|a| matches!(a, DocAction::Conflict { .. }))
                    );
                });
        });
    }

    #[test]
    fn pair_state_path_is_order_independent() {
        let one = pair_state_path("rm1:/a", "rm2:/b");
        let two = pair_state_path("rm2:/b", "rm1:/a");
        assert_eq!(one, two);
        assert_ne!(one, pair_state_path("rm1:/a", "rm2:/c"));
    }

    // ---- ssh fs listing parsing ---------------------------------------------

    #[test]
    fn fs_listing_parses_and_filters() {
        let output = "\
1024 1700000000 ./docs/paper.pdf\n\
99 1700000001 ./notes/with space.epub\n\
5 1700000002 ./.hidden/secret.pdf\n\
7 1700000003 ./script.py\n\
not a valid line\n";
        let (entries, ignored) = parse_fs_listing(output);
        let paths: Vec<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(paths, ["docs/paper.pdf", "notes/with space.epub"]);
        assert_eq!(entries[0].size, 1024);
        assert_eq!(entries[0].mtime_ms, 1_700_000_000_000);
        assert_eq!(entries[1].kind, DocKind::Epub);
        assert_eq!(ignored, 1); // script.py; dotdirs skipped silently
    }

    // ---- helpers ----------------------------------------------------------

    #[test]
    fn prefix_and_target_helpers() {
        assert_eq!(path_prefixes(""), Vec::<String>::new());
        assert_eq!(path_prefixes("a/b/c"), ["a", "a/b", "a/b/c"]);
        assert_eq!(split_target("a/b/notes.pdf"), ("a/b", "notes".to_string()));
        assert_eq!(split_target("top.pdf"), ("", "top".to_string()));
    }
}
