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
use std::path::Path;
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
    pub uuid: String,
    pub kind: DocKind,
    pub local_size: u64,
    pub local_mtime_ms: i64,
    pub remote_last_modified: i64,
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
    pub fn load(local_root: &Path) -> Result<Self> {
        let path = local_root.join(STATE_FILE_NAME);
        if !path.exists() {
            return Ok(Self {
                version: 1,
                entries: BTreeMap::new(),
            });
        }
        let text = fs::read_to_string(&path)?;
        serde_json::from_str(&text).map_err(|source| Error::Json {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn save(&self, local_root: &Path) -> Result<()> {
        let path = local_root.join(STATE_FILE_NAME);
        let text = serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            path: path.display().to_string(),
            source,
        })?;
        Ok(fs::write(path, text)?)
    }
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

/// Action buckets, concatenated in execution-safe order: folders →
/// transfers → deletions → forgets → notes.
#[derive(Default)]
struct Buckets {
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

    // Unified per-key views (BTreeMap: deterministic order).
    let mut views: BTreeMap<String, View> = state
        .entries
        .iter()
        .map(|(key, st)| {
            (
                key.clone(),
                View {
                    local: local_by_path.get(key.as_str()).copied(),
                    remote: docs_by_uuid.get(st.uuid.as_str()).copied(),
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
        .filter(|doc| !mapped_uuids.contains(doc.uuid.as_str()))
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
        actions: needed
            .into_iter()
            .map(|rel_dir| SyncAction::CreateRemoteFolder { rel_dir })
            .chain(buckets.transfers)
            .chain(buckets.deletes)
            .chain(buckets.forgets)
            .chain(buckets.notes)
            .collect(),
    }
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
    local_root: &Path,
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
                let path = local_root.join(local);
                let item = match kind {
                    DocKind::Pdf => client.store_payload(&path, &parent_uuid, name, "pdf")?,
                    DocKind::Epub => client.store_payload(&path, &parent_uuid, name, "epub")?,
                    DocKind::Markdown => {
                        client.store_text(&path, &parent_uuid, name, TextKind::Markdown)?
                    }
                    DocKind::Text => {
                        client.store_text(&path, &parent_uuid, name, TextKind::Plain)?
                    }
                    DocKind::Rmdoc => {
                        let rmdoc = bundle::parse_rmdoc(&fs::read(&path)?)?;
                        client.restore_bundle(rmdoc, &parent_uuid, name)?
                    }
                };
                record_state(
                    state,
                    local_root,
                    local,
                    *kind,
                    &item.uuid,
                    item.last_modified,
                )?;
                outcome.uploaded += 1;
                outcome.modified_remote = true;
            }
            SyncAction::UpdateRemote { local, kind, uuid } => {
                let path = local_root.join(local);
                let last_modified = match kind {
                    DocKind::Pdf => client.update_payload_from_file(uuid, "pdf", &path)?,
                    DocKind::Epub => client.update_payload_from_file(uuid, "epub", &path)?,
                    DocKind::Markdown | DocKind::Text => {
                        let (_, stem) = split_target(local);
                        let text_kind = if *kind == DocKind::Markdown {
                            TextKind::Markdown
                        } else {
                            TextKind::Plain
                        };
                        let source = fs::read_to_string(&path)?;
                        let bytes = epub::text_to_epub(&stem, text_kind, &source)?;
                        client.update_payload_bytes(uuid, "epub", &bytes)?
                    }
                    // The planner never emits this.
                    DocKind::Rmdoc => unreachable!("mapped .rmdoc files are pull-only"),
                };
                record_state(state, local_root, local, *kind, uuid, last_modified)?;
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
                let dest = local_root.join(local);
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
                record_state(
                    state,
                    local_root,
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
                state.save(local_root)?;
                outcome.deleted_remote += 1;
                outcome.modified_remote = true;
            }
            SyncAction::DeleteLocal { path } => {
                match fs::remove_file(local_root.join(path)) {
                    Ok(()) => {}
                    // Already gone: deletion is idempotent.
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.into()),
                }
                state.entries.remove(path);
                state.save(local_root)?;
                outcome.deleted_local += 1;
            }
            SyncAction::Forget { path } => {
                state.entries.remove(path);
                state.save(local_root)?;
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

/// Stat the local file and persist the new state entry.
fn record_state(
    state: &mut SyncState,
    local_root: &Path,
    rel_path: &str,
    kind: DocKind,
    uuid: &str,
    remote_last_modified: i64,
) -> Result<()> {
    let metadata = fs::metadata(local_root.join(rel_path))?;
    state.entries.insert(
        rel_path.to_string(),
        StateEntry {
            uuid: uuid.to_string(),
            kind,
            local_size: metadata.len(),
            local_mtime_ms: mtime_ms(&metadata),
            remote_last_modified,
        },
    );
    state.save(local_root)
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

    // ---- helpers ----------------------------------------------------------

    #[test]
    fn prefix_and_target_helpers() {
        assert_eq!(path_prefixes(""), Vec::<String>::new());
        assert_eq!(path_prefixes("a/b/c"), ["a", "a/b", "a/b/c"]);
        assert_eq!(split_target("a/b/notes.md"), ("a/b", "notes".to_string()));
        assert_eq!(split_target("top.pdf"), ("", "top".to_string()));
    }
}
