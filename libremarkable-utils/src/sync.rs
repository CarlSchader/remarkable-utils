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
use crate::epub::{self, TextKind};
use crate::error::{Error, Result};
use crate::progress::Progress;
use crate::ssh::{SshSession, shell_quote};
use crate::xochitl::{self, Item};

/// Name of the sync-state file kept in the local sync root.
pub const STATE_FILE_NAME: &str = ".rmu-sync.json";

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocKind {
    Pdf,
    Epub,
    Markdown,
    Text,
    Rmdoc,
}

impl DocKind {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "pdf" => Some(Self::Pdf),
            "epub" => Some(Self::Epub),
            "md" | "markdown" => Some(Self::Markdown),
            "txt" => Some(Self::Text),
            "rmdoc" => Some(Self::Rmdoc),
            _ => None,
        }
    }

    /// The payload `fileType` this kind maps to on the device.
    pub fn device_file_type(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Epub | Self::Markdown | Self::Text => "epub",
            Self::Rmdoc => "notebook",
        }
    }
}

/// A syncable local file (directories are implicit in the paths).
#[derive(Debug, Clone)]
pub struct LocalEntry {
    /// Relative path with `/` separators, e.g. `Books/notes.md`.
    pub rel_path: String,
    pub kind: DocKind,
    pub size: u64,
    pub mtime_ms: i64,
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
}

impl RemoteDoc {
    /// Local relative path a pull of this document produces.
    pub fn local_rel_path(&self) -> String {
        let filename = format!("{}.{}", self.name, self.doc_type.local_extension());
        join_rel(&self.rel_dir, &filename)
    }
}

/// The synced subtree of the device.
#[derive(Debug, Default)]
pub struct RemoteSnapshot {
    pub docs: Vec<RemoteDoc>,
    /// Folder rel path → UUID; `""` maps to the sync root folder.
    pub folders: BTreeMap<String, String>,
    /// Items excluded up-front (duplicate names, unusable names, ...).
    pub skips: Vec<SyncAction>,
}

/// Build the device-side snapshot from a full listing, scoped to the
/// folder `root_uuid` (`""` = device root). Duplicate sibling names and
/// names unusable as filenames are excluded with `Skip` actions,
/// consistent with the repo-wide "never guess on ambiguity" invariant.
pub fn remote_snapshot(items: &[Item], root_uuid: &str) -> RemoteSnapshot {
    fn usable_name(name: &str) -> bool {
        !name.is_empty() && !name.contains('/') && !name.starts_with('.') && name != ".."
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
                snapshot.skips.push(SyncAction::Skip {
                    path,
                    reason: "duplicate sibling name on device".to_string(),
                });
                return;
            }
            if !usable_name(&item.visible_name) {
                snapshot.skips.push(SyncAction::Skip {
                    path,
                    reason: "name is not usable as a filename".to_string(),
                });
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
                    snapshot.skips.push(SyncAction::Skip {
                        path,
                        reason: format!("unsupported document type '{other}'"),
                    });
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
    /// B-side size for fs↔fs pairs (absent for device pairs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_size: Option<u64>,
}

/// The sync-state file: `local rel path ↔ device UUID` plus what both
/// sides looked like at last sync. Enables three-way diffing; see
/// `docs/sync-design.md`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub version: u32,
    pub entries: BTreeMap<String, StateEntry>,
}

impl SyncState {
    pub fn parse(text: &str) -> Result<Self> {
        serde_json::from_str(text).map_err(|source| Error::Json {
            path: STATE_FILE_NAME.to_string(),
            source,
        })
    }

    pub fn serialize(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            path: STATE_FILE_NAME.to_string(),
            source,
        })
    }

    /// Load from the state-holding endpoint; missing file = fresh state.
    pub fn load_from(fs: &dyn FsEndpoint) -> Result<Self> {
        match fs.read_state()? {
            Some(text) => Self::parse(&text),
            None => Ok(Self {
                version: 1,
                entries: BTreeMap::new(),
            }),
        }
    }

    pub fn save_to(&self, fs: &dyn FsEndpoint) -> Result<()> {
        fs.write_state(&self.serialize()?)
    }
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
    /// Walk the tree: (syncable files, unsupported-file count).
    fn snapshot(&self) -> Result<(Vec<LocalEntry>, usize)>;
    fn read(&self, rel_path: &str) -> Result<Vec<u8>>;
    /// Write a file, creating parent directories.
    fn write(&self, rel_path: &str, data: &[u8]) -> Result<()>;
    /// Remove a file; idempotent.
    fn remove(&self, rel_path: &str) -> Result<()>;
    /// (size, mtime in ms).
    fn stat(&self, rel_path: &str) -> Result<(u64, i64)>;
    /// Read the sync-state file; `None` if absent.
    fn read_state(&self) -> Result<Option<String>>;
    fn write_state(&self, text: &str) -> Result<()>;
    /// For local endpoints: the real path, enabling streamed transfers
    /// instead of in-memory buffering.
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

impl FsEndpoint for LocalFs {
    fn label(&self) -> String {
        self.root.display().to_string()
    }

    fn ensure_root(&self) -> Result<()> {
        Ok(fs::create_dir_all(&self.root)?)
    }

    fn snapshot(&self) -> Result<(Vec<LocalEntry>, usize)> {
        local_snapshot(&self.root)
    }

    fn read(&self, rel_path: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.root.join(rel_path))?)
    }

    fn write(&self, rel_path: &str, data: &[u8]) -> Result<()> {
        let path = self.root.join(rel_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(fs::write(path, data)?)
    }

    fn remove(&self, rel_path: &str) -> Result<()> {
        match fs::remove_file(self.root.join(rel_path)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn stat(&self, rel_path: &str) -> Result<(u64, i64)> {
        let metadata = fs::metadata(self.root.join(rel_path))?;
        Ok((metadata.len(), mtime_ms(&metadata)))
    }

    fn read_state(&self) -> Result<Option<String>> {
        let path = self.root.join(STATE_FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read_to_string(path)?))
    }

    fn write_state(&self, text: &str) -> Result<()> {
        Ok(fs::write(self.root.join(STATE_FILE_NAME), text)?)
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

    fn root_for_shell(&self) -> String {
        if self.root.is_empty() {
            ".".to_string()
        } else {
            self.root.clone()
        }
    }
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
        let path = self.join(rel_path);
        let parent = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(".");
        self.session.run_checked_with_stdin(
            &format!(
                "mkdir -p {parent} && cat > {path}",
                parent = shell_quote(parent),
                path = shell_quote(&path),
            ),
            data,
            &*self.progress,
        )
    }

    fn remove(&self, rel_path: &str) -> Result<()> {
        self.session
            .run_checked(&format!("rm -f -- {}", shell_quote(&self.join(rel_path))))?;
        Ok(())
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

    fn read_state(&self) -> Result<Option<String>> {
        let output = self
            .session
            .run(&format!("cat {}", shell_quote(&self.join(STATE_FILE_NAME))))?;
        match output.status.code() {
            Some(0) => Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned())),
            // Missing file (cat exits 1/2 depending on the shell).
            Some(1) | Some(2) => Ok(None),
            code => Err(Error::Remote {
                status: code.unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }

    fn write_state(&self, text: &str) -> Result<()> {
        self.write(STATE_FILE_NAME, text.as_bytes())
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
    },
    UpdateRemote {
        local: String,
        kind: DocKind,
        uuid: String,
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
    /// Drop a stale state mapping (e.g. both sides gone).
    Forget {
        path: String,
    },
    /// Re-link a mapping whose document was replaced on the device by
    /// a same-name, same-kind document (e.g. an interrupted sync that
    /// uploaded but never recorded state). State-only; no transfer.
    Rebind {
        path: String,
        uuid: String,
        last_modified: i64,
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

/// Name-collision guards for upload candidates.
struct Guards {
    /// (dir, name) pairs already occupied on the device.
    taken: std::collections::HashSet<(String, String)>,
    /// Upload candidates per (dir, name); >1 means local files compete
    /// for the same device name (e.g. `n.md` + `n.pdf`).
    upload_counts: HashMap<(String, String), usize>,
}

/// Action buckets, concatenated in execution-safe order: rebinds →
/// folders → transfers → deletions → forgets → notes. Rebinds go
/// first so a transfer's own state update (written after it runs)
/// is not clobbered by the planning-time rebind values.
#[derive(Default)]
struct Buckets {
    rebinds: Vec<SyncAction>,
    transfers: Vec<SyncAction>,
    deletes: Vec<SyncAction>,
    forgets: Vec<SyncAction>,
    notes: Vec<SyncAction>,
}

/// Compute the ordered action plan. Pure: no I/O.
pub fn plan(
    options: SyncOptions,
    local: &[LocalEntry],
    remote: &RemoteSnapshot,
    state: &SyncState,
) -> Plan {
    let local_by_path: HashMap<&str, &LocalEntry> =
        local.iter().map(|e| (e.rel_path.as_str(), e)).collect();
    let docs_by_uuid: HashMap<&str, &RemoteDoc> =
        remote.docs.iter().map(|d| (d.uuid.as_str(), d)).collect();
    let mapped_uuids: std::collections::HashSet<&str> =
        state.entries.values().map(|st| st.uuid.as_str()).collect();

    let mut buckets = Buckets {
        notes: remote.skips.clone(),
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
            if remote_doc.is_none() {
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

    let guards = Guards {
        taken: remote
            .docs
            .iter()
            .map(|d| (d.rel_dir.clone(), d.name.clone()))
            .chain(remote.folders.keys().filter(|p| !p.is_empty()).map(|path| {
                let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
                (dir.to_string(), name.to_string())
            }))
            .collect(),
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

    views
        .iter()
        .for_each(|(key, view)| decide(options, key, view, &guards, &mut buckets));

    // Folder creation for upload targets, parents before children.
    let mut needed: Vec<String> = buckets
        .transfers
        .iter()
        .filter_map(|action| match action {
            SyncAction::Upload { remote_dir, .. } => Some(remote_dir.as_str()),
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
            | (
                DocKind::Epub | DocKind::Markdown | DocKind::Text,
                RemoteType::Epub
            )
            | (DocKind::Rmdoc, RemoteType::Notebook)
    )
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
                out.transfers.push(download(key, doc));
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
    let local_changed = entry.size != st.local_size || entry.mtime_ms != st.local_mtime_ms;
    let remote_changed = doc.last_modified != st.remote_last_modified;
    let text_import = matches!(st.kind, DocKind::Markdown | DocKind::Text);
    // What each side's changes can flow into, given mode and kind.
    let push_ok = can_write_remote(options.mode) && st.kind != DocKind::Rmdoc;
    let pull_ok = can_write_local(options.mode) && !text_import;

    match (local_changed, remote_changed) {
        (false, false) => {}
        (true, false) => {
            if push_ok {
                out.transfers.push(update_remote(key, entry.kind, &st.uuid));
            } else if can_write_remote(options.mode) {
                out.notes.push(skip(
                    key,
                    "mapped .rmdoc files are pull-only (the tablet's ink wins)",
                ));
            } else if !text_import {
                // Pull mode: destination drift. (Text imports are
                // expected to change locally; push is their flow.)
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
            } else if can_write_local(options.mode) && text_import {
                out.notes
                    .push(skip(key, "text import; device-side changes are not pulled"));
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
                    out.transfers.push(update_remote(key, entry.kind, &st.uuid));
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
                } else if can_write_local(options.mode) && text_import {
                    out.notes.push(conflict(
                        key,
                        "resolved to the device side, but device-side changes \
                         cannot be pulled into a text import",
                    ));
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
    let remote_changed = doc.last_modified != st.remote_last_modified;
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
                (DocKind::Pdf, RemoteType::Pdf) | (DocKind::Epub, RemoteType::Epub) => out
                    .transfers
                    .push(update_remote(key, entry.kind, &doc.uuid)),
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
    } else {
        out.transfers.push(SyncAction::Upload {
            local: key.to_string(),
            kind: entry.kind,
            remote_dir: dir.to_string(),
            name: stem,
        });
    }
}

fn update_remote(key: &str, kind: DocKind, uuid: &str) -> SyncAction {
    SyncAction::UpdateRemote {
        local: key.to_string(),
        kind,
        uuid: uuid.to_string(),
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
    pub deleted_local: usize,
    pub deleted_remote: usize,
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
                    (DocKind::Markdown, _) => client.store_text_source(
                        &read_utf8(fs_side, local)?,
                        &parent_uuid,
                        name,
                        TextKind::Markdown,
                    )?,
                    (DocKind::Text, _) => client.store_text_source(
                        &read_utf8(fs_side, local)?,
                        &parent_uuid,
                        name,
                        TextKind::Plain,
                    )?,
                    (DocKind::Rmdoc, _) => {
                        let rmdoc = bundle::parse_rmdoc(&fs_side.read(local)?)?;
                        client.restore_bundle(rmdoc, &parent_uuid, name)?
                    }
                };
                record_state(state, fs_side, local, *kind, &item.uuid, item.last_modified)?;
                outcome.uploaded += 1;
                outcome.modified_remote = true;
            }
            SyncAction::UpdateRemote { local, kind, uuid } => {
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
                    (DocKind::Markdown | DocKind::Text, _) => {
                        let (_, stem) = split_target(local);
                        let text_kind = if *kind == DocKind::Markdown {
                            TextKind::Markdown
                        } else {
                            TextKind::Plain
                        };
                        let source = read_utf8(fs_side, local)?;
                        let bytes = epub::text_to_epub(&stem, text_kind, &source)?;
                        client.update_payload_bytes(uuid, "epub", &bytes)?
                    }
                    // The planner never emits this.
                    (DocKind::Rmdoc, _) => unreachable!("mapped .rmdoc files are pull-only"),
                };
                record_state(state, fs_side, local, *kind, uuid, last_modified)?;
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
                match (doc_type, fs_side.as_local_path(local)) {
                    (_, Some(dest)) => {
                        if let Some(parent) = dest.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        match doc_type {
                            RemoteType::Notebook => client.download_bundle_to(uuid, &dest)?,
                            RemoteType::Pdf => {
                                client.download_payload_to(uuid, "pdf", &dest, *size_bytes)?
                            }
                            RemoteType::Epub => {
                                client.download_payload_to(uuid, "epub", &dest, *size_bytes)?
                            }
                        }
                    }
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
                record_state(
                    state,
                    fs_side,
                    local,
                    doc_type.pulled_kind(),
                    uuid,
                    *last_modified,
                )?;
                outcome.downloaded += 1;
            }
            SyncAction::DeleteRemote { path, uuid } => {
                client.delete_document(uuid)?;
                state.entries.remove(path);
                state.save_to(fs_side)?;
                outcome.deleted_remote += 1;
                outcome.modified_remote = true;
            }
            SyncAction::DeleteLocal { path } => {
                fs_side.remove(path)?;
                state.entries.remove(path);
                state.save_to(fs_side)?;
                outcome.deleted_local += 1;
            }
            SyncAction::Forget { path } => {
                state.entries.remove(path);
                state.save_to(fs_side)?;
            }
            SyncAction::Rebind {
                path,
                uuid,
                last_modified,
            } => {
                if let Some(entry) = state.entries.get_mut(path) {
                    entry.uuid = uuid.clone();
                    entry.remote_last_modified = *last_modified;
                }
                state.save_to(fs_side)?;
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

/// Read a file from an fs endpoint as UTF-8 (text imports are UTF-8
/// only; anything else is rejected rather than mangled).
fn read_utf8(fs_side: &dyn FsEndpoint, rel_path: &str) -> Result<String> {
    String::from_utf8(fs_side.read(rel_path)?).map_err(|_| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{rel_path} is not valid UTF-8"),
        ))
    })
}

/// Stat the fs-side file and persist the new state entry.
fn record_state(
    state: &mut SyncState,
    fs_side: &dyn FsEndpoint,
    rel_path: &str,
    kind: DocKind,
    uuid: &str,
    remote_last_modified: i64,
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
            remote_size: None,
        },
    );
    state.save_to(fs_side)
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
        // B deleted it.
        (true, Some((a_changed, a_ms)), None) => {
            if can_b && !options.delete {
                return Some(CopyToB); // recopy
            }
            if !options.delete {
                return None; // one-way toward A without --delete: stale state kept
            }
            if !a_changed {
                return Some(DeleteA);
            }
            match winner(options.conflict, a_ms, i64::MIN) {
                None => Some(Conflict("deleted on one side but changed on the other")),
                Some(Winner::Local) if can_b => Some(CopyToB),
                Some(Winner::Local) => Some(Forget),
                Some(Winner::Remote) => Some(DeleteA),
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
                return Some(DeleteB);
            }
            match winner(options.conflict, i64::MIN, b_ms) {
                None => Some(Conflict("deleted on one side but changed on the other")),
                Some(Winner::Remote) if can_a => Some(CopyToA),
                Some(Winner::Remote) => Some(Forget),
                Some(Winner::Local) => Some(DeleteB),
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
) -> Result<FileOutcome> {
    let total = plan
        .iter()
        .filter(|action| !matches!(action, FileAction::Conflict { .. }))
        .count();
    let mut outcome = FileOutcome::default();
    let mut done = 0usize;

    let record = |state: &mut SyncState, path: &str| -> Result<()> {
        let (a_size, a_mtime) = a.stat(path)?;
        let (b_size, b_mtime) = b.stat(path)?;
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
                remote_size: Some(b_size),
            },
        );
        state.save_to(a)
    };

    plan.iter().try_for_each(|action| -> Result<()> {
        if !matches!(action, FileAction::Conflict { .. }) {
            done += 1;
            progress.step(&format!("[{done}/{total}] {}", describe_file(action)));
        }
        match action {
            FileAction::CopyToB { path } => {
                b.write(path, &a.read(path)?)?;
                record(state, path)?;
                outcome.copied_to_b += 1;
            }
            FileAction::CopyToA { path } => {
                a.write(path, &b.read(path)?)?;
                record(state, path)?;
                outcome.copied_to_a += 1;
            }
            FileAction::DeleteA { path } => {
                a.remove(path)?;
                state.entries.remove(path);
                state.save_to(a)?;
                outcome.deleted_a += 1;
            }
            FileAction::DeleteB { path } => {
                b.remove(path)?;
                state.entries.remove(path);
                state.save_to(a)?;
                outcome.deleted_b += 1;
            }
            FileAction::Forget { path } => {
                state.entries.remove(path);
                state.save_to(a)?;
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            path: path.display().to_string(),
            source,
        })?;
        Ok(fs::write(path, text)?)
    }
}

/// Where the pair-state for two tablet endpoints lives on the
/// initiating computer: `$XDG_STATE_HOME/rmu/` (or `~/.local/state/rmu/`),
/// keyed order-independently by the endpoint pair.
pub fn pair_state_path(endpoint_a: &str, endpoint_b: &str) -> PathBuf {
    let (first, second) = if endpoint_a <= endpoint_b {
        (endpoint_a, endpoint_b)
    } else {
        (endpoint_b, endpoint_a)
    };
    let hash = fnv1a(&format!("{first}\n{second}"));
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("rmu").join(format!("sync-pair-{hash:016x}.json"))
}

/// FNV-1a: tiny, dependency-free, and stable across releases (unlike
/// `DefaultHasher`, whose output is not guaranteed between versions).
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
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
            DocKind::Markdown | DocKind::Text => format!("upload   {local} (as EPUB)"),
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
        SyncAction::Forget { path } => format!("forget   {path} (stale sync mapping)"),
        SyncAction::Rebind { path, .. } => {
            format!("rebind   {path} (re-linked to replacement on device)")
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
        }
    }

    fn entry(uuid: &str, kind: DocKind, size: u64, mtime: i64, remote: i64) -> StateEntry {
        StateEntry {
            uuid: uuid.to_string(),
            kind,
            local_size: size,
            local_mtime_ms: mtime,
            remote_last_modified: remote,
            remote_size: None,
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

    // ---- push planning ----------------------------------------------------

    #[test]
    fn push_first_sync_uploads_and_creates_folders() {
        let locals = vec![
            local("a/b/notes.md", DocKind::Markdown, 10, 1),
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
                    local: "a/b/notes.md".to_string(),
                    kind: DocKind::Markdown,
                    remote_dir: "a/b".to_string(),
                    name: "notes".to_string()
                },
                SyncAction::Upload {
                    local: "top.pdf".to_string(),
                    kind: DocKind::Pdf,
                    remote_dir: String::new(),
                    name: "top".to_string()
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
        let locals = vec![local("n.md", DocKind::Markdown, 99, 9)];
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "epub", 5)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.md".to_string(), entry("u1", DocKind::Markdown, 10, 1, 5));
        let plan = plan(push(), &locals, &snapshot, &state);
        assert_eq!(
            plan.actions,
            vec![SyncAction::UpdateRemote {
                local: "n.md".to_string(),
                kind: DocKind::Markdown,
                uuid: "u1".to_string()
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
                name: "backup".to_string()
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
            local("n.md", DocKind::Markdown, 10, 1),
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
    fn pull_mapped_text_import_is_noop_and_warns_on_remote_change() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "epub", 9)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.md".to_string(), entry("u1", DocKind::Markdown, 10, 1, 5));
        let locals = vec![local("n.md", DocKind::Markdown, 10, 1)];
        let plan = plan(pull(), &locals, &snapshot, &state);
        assert_eq!(skips(&plan), ["n.md"]); // warned, not downloaded

        // Unchanged remote: fully silent.
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "epub", 5)], "");
        let plan = super::plan(pull(), &locals, &snapshot, &state);
        assert!(plan.actions.is_empty());
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
                    uuid: "ua".to_string()
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
                uuid: "u1".to_string()
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
        // the successor's lastModified differs from the recorded one.
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
        let b = vec![local("y/b.md", DocKind::Markdown, 20, 2)];

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
                    path: "y/b.md".to_string()
                },
            ]
        );
    }

    fn file_entry(size: u64, a_mtime: i64, b_mtime: i64, b_size: u64) -> StateEntry {
        StateEntry {
            uuid: String::new(),
            kind: DocKind::Pdf,
            local_size: size,
            local_mtime_ms: a_mtime,
            remote_last_modified: b_mtime,
            remote_size: Some(b_size),
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
99 1700000001 ./notes/with space.md\n\
5 1700000002 ./.hidden/secret.pdf\n\
7 1700000003 ./script.py\n\
not a valid line\n";
        let (entries, ignored) = parse_fs_listing(output);
        let paths: Vec<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(paths, ["docs/paper.pdf", "notes/with space.md"]);
        assert_eq!(entries[0].size, 1024);
        assert_eq!(entries[0].mtime_ms, 1_700_000_000_000);
        assert_eq!(entries[1].kind, DocKind::Markdown);
        assert_eq!(ignored, 1); // script.py; dotdirs skipped silently
    }

    // ---- helpers ----------------------------------------------------------

    #[test]
    fn prefix_and_target_helpers() {
        assert_eq!(path_prefixes(""), Vec::<String>::new());
        assert_eq!(path_prefixes("a/b/c"), ["a", "a/b", "a/b/c"]);
        assert_eq!(split_target("a/b/notes.md"), ("a/b", "notes".to_string()));
        assert_eq!(split_target("top.pdf"), ("", "top".to_string()));
    }
}
