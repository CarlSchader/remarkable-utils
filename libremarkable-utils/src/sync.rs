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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Local → device.
    Push,
    /// Device → local.
    Pull,
}

/// One planned operation. `Skip` entries are informational.
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
            .filter(|action| !matches!(action, SyncAction::Skip { .. }))
            .count()
    }
}

/// Compute the ordered action plan. Pure: no I/O.
pub fn plan(
    direction: Direction,
    local: &[LocalEntry],
    remote: &RemoteSnapshot,
    state: &SyncState,
) -> Plan {
    let mut plan = match direction {
        Direction::Push => plan_push(local, remote, state),
        Direction::Pull => plan_pull(local, remote, state),
    };
    plan.actions.extend(remote.skips.iter().cloned());
    plan
}

fn plan_push(local: &[LocalEntry], remote: &RemoteSnapshot, state: &SyncState) -> Plan {
    let docs_by_uuid: HashMap<&str, &RemoteDoc> =
        remote.docs.iter().map(|d| (d.uuid.as_str(), d)).collect();
    // Existing (dir, name) pairs on the device: docs and folders.
    let taken: std::collections::HashSet<(String, String)> = remote
        .docs
        .iter()
        .map(|d| (d.rel_dir.clone(), d.name.clone()))
        .chain(remote.folders.keys().filter(|p| !p.is_empty()).map(|path| {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            (dir.to_string(), name.to_string())
        }))
        .collect();
    // Local files competing for the same device name (e.g. notes.md +
    // notes.pdf): count unmapped targets per (dir, stem).
    let target_counts: HashMap<(String, String), usize> = local
        .iter()
        .filter(|entry| !state.entries.contains_key(&entry.rel_path))
        .fold(HashMap::new(), |mut counts, entry| {
            let (dir, stem) = split_target(&entry.rel_path);
            *counts.entry((dir.to_string(), stem)).or_default() += 1;
            counts
        });

    let mut actions: Vec<SyncAction> = Vec::new();
    local.iter().for_each(|entry| {
        let (dir, stem) = split_target(&entry.rel_path);
        let mapped = state
            .entries
            .get(&entry.rel_path)
            .and_then(|st| docs_by_uuid.get(st.uuid.as_str()).map(|doc| (st, *doc)));
        match mapped {
            Some((st, doc)) => {
                let local_changed =
                    entry.size != st.local_size || entry.mtime_ms != st.local_mtime_ms;
                let remote_changed = doc.last_modified != st.remote_last_modified;
                if !local_changed {
                    return;
                }
                if st.kind == DocKind::Rmdoc {
                    actions.push(SyncAction::Skip {
                        path: entry.rel_path.clone(),
                        reason: "mapped .rmdoc files are pull-only (the tablet's ink wins)"
                            .to_string(),
                    });
                } else if remote_changed {
                    actions.push(SyncAction::Skip {
                        path: entry.rel_path.clone(),
                        reason: "destination changed since last sync".to_string(),
                    });
                } else {
                    actions.push(SyncAction::UpdateRemote {
                        local: entry.rel_path.clone(),
                        kind: entry.kind,
                        uuid: st.uuid.clone(),
                    });
                }
            }
            // Unmapped, or the mapped document vanished from the device
            // (rsync-without-`--delete` semantics: recopy).
            None => {
                let key = (dir.to_string(), stem.clone());
                if taken.contains(&key) {
                    actions.push(SyncAction::Skip {
                        path: entry.rel_path.clone(),
                        reason: "an item with this name already exists on the device \
                                 (no sync state to match it)"
                            .to_string(),
                    });
                } else if target_counts.get(&key).copied().unwrap_or(0) > 1 {
                    actions.push(SyncAction::Skip {
                        path: entry.rel_path.clone(),
                        reason: "multiple local files map to the same device name".to_string(),
                    });
                } else {
                    actions.push(SyncAction::Upload {
                        local: entry.rel_path.clone(),
                        kind: entry.kind,
                        remote_dir: dir.to_string(),
                        name: stem,
                    });
                }
            }
        }
    });

    // Folder creation for upload targets, parents before children.
    let mut needed: Vec<String> = actions
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
    let folder_actions: Vec<SyncAction> = needed
        .into_iter()
        .map(|rel_dir| SyncAction::CreateRemoteFolder { rel_dir })
        .collect();

    Plan {
        actions: folder_actions.into_iter().chain(actions).collect(),
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

fn plan_pull(local: &[LocalEntry], remote: &RemoteSnapshot, state: &SyncState) -> Plan {
    let local_by_path: HashMap<&str, &LocalEntry> =
        local.iter().map(|e| (e.rel_path.as_str(), e)).collect();
    let state_by_uuid: HashMap<&str, (&String, &StateEntry)> = state
        .entries
        .iter()
        .map(|(key, st)| (st.uuid.as_str(), (key, st)))
        .collect();

    let actions = remote
        .docs
        .iter()
        .filter_map(|doc| {
            match state_by_uuid.get(doc.uuid.as_str()) {
                Some((key, st)) => {
                    let remote_changed = doc.last_modified != st.remote_last_modified;
                    // Text imports have no local representation of
                    // device-side changes; pull is a documented no-op.
                    if matches!(st.kind, DocKind::Markdown | DocKind::Text) {
                        return remote_changed.then(|| SyncAction::Skip {
                            path: (*key).clone(),
                            reason: "text import; device-side changes are not pulled".to_string(),
                        });
                    }
                    match local_by_path.get(key.as_str()) {
                        // Deleted locally: recopy (rsync semantics).
                        None => Some(SyncAction::Download {
                            local: (*key).clone(),
                            uuid: doc.uuid.clone(),
                            doc_type: doc.doc_type,
                            size_bytes: doc.size_bytes,
                            last_modified: doc.last_modified,
                        }),
                        Some(entry) => {
                            let local_changed =
                                entry.size != st.local_size || entry.mtime_ms != st.local_mtime_ms;
                            match (remote_changed, local_changed) {
                                (false, false) => None,
                                (false, true) => Some(SyncAction::Skip {
                                    path: (*key).clone(),
                                    reason: "destination changed since last sync".to_string(),
                                }),
                                (true, false) => Some(SyncAction::Download {
                                    local: (*key).clone(),
                                    uuid: doc.uuid.clone(),
                                    doc_type: doc.doc_type,
                                    size_bytes: doc.size_bytes,
                                    last_modified: doc.last_modified,
                                }),
                                (true, true) => Some(SyncAction::Skip {
                                    path: (*key).clone(),
                                    reason: "both sides changed since last sync".to_string(),
                                }),
                            }
                        }
                    }
                }
                None => {
                    let target = doc.local_rel_path();
                    if local_by_path.contains_key(target.as_str()) {
                        Some(SyncAction::Skip {
                            path: target,
                            reason: "exists on both sides (no sync state to match them)"
                                .to_string(),
                        })
                    } else {
                        Some(SyncAction::Download {
                            local: target,
                            uuid: doc.uuid.clone(),
                            doc_type: doc.doc_type,
                            size_bytes: doc.size_bytes,
                            last_modified: doc.last_modified,
                        })
                    }
                }
            }
        })
        .collect();

    Plan { actions }
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
    pub skipped: Vec<(String, String)>,
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
        if !matches!(action, SyncAction::Skip { .. }) {
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
        SyncAction::CreateRemoteFolder { rel_dir } => format!("mkdir   {rel_dir}/"),
        SyncAction::Upload { local, kind, .. } => match kind {
            DocKind::Markdown | DocKind::Text => format!("upload  {local} (as EPUB)"),
            DocKind::Rmdoc => format!("restore {local}"),
            _ => format!("upload  {local}"),
        },
        SyncAction::UpdateRemote { local, .. } => format!("update  {local}"),
        SyncAction::Download {
            local, doc_type, ..
        } => match doc_type {
            RemoteType::Notebook => format!("download {local} (notebook bundle)"),
            _ => format!("download {local}"),
        },
        SyncAction::Skip { path, reason } => format!("skip    {path} ({reason})"),
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

    fn skips(plan: &Plan) -> Vec<&str> {
        plan.actions
            .iter()
            .filter_map(|a| match a {
                SyncAction::Skip { path, .. } => Some(path.as_str()),
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
        let plan = plan(Direction::Push, &locals, &snapshot, &SyncState::default());
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
        let plan = plan(Direction::Push, &locals, &snapshot, &state);
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
        let plan = plan(Direction::Push, &locals, &snapshot, &state);
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
    fn push_both_changed_is_conflict_skip() {
        let locals = vec![local("n.pdf", DocKind::Pdf, 99, 9)];
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 6)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let plan = plan(Direction::Push, &locals, &snapshot, &state);
        assert_eq!(skips(&plan), ["n.pdf"]);
    }

    #[test]
    fn push_mapped_rmdoc_never_pushes() {
        let locals = vec![local("n.rmdoc", DocKind::Rmdoc, 99, 9)];
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "notebook", 5)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.rmdoc".to_string(), entry("u1", DocKind::Rmdoc, 10, 1, 5));
        let plan = plan(Direction::Push, &locals, &snapshot, &state);
        assert_eq!(skips(&plan), ["n.rmdoc"]);
    }

    #[test]
    fn push_unmapped_rmdoc_is_a_restore() {
        let locals = vec![local("backup.rmdoc", DocKind::Rmdoc, 10, 1)];
        let snapshot = remote_snapshot(&[], "");
        let plan = plan(Direction::Push, &locals, &snapshot, &SyncState::default());
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
        let plan = plan(Direction::Push, &locals, &snapshot, &SyncState::default());
        assert_eq!(skips(&plan), ["n.pdf"]);
    }

    #[test]
    fn push_folder_name_collision_skips() {
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)];
        let snapshot = remote_snapshot(&[folder("f", "n", "")], "");
        let plan = plan(Direction::Push, &locals, &snapshot, &SyncState::default());
        assert_eq!(skips(&plan), ["n.pdf"]);
    }

    #[test]
    fn push_duplicate_local_targets_skip() {
        let locals = vec![
            local("n.md", DocKind::Markdown, 10, 1),
            local("n.pdf", DocKind::Pdf, 10, 1),
        ];
        let snapshot = remote_snapshot(&[], "");
        let plan = plan(Direction::Push, &locals, &snapshot, &SyncState::default());
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
        let plan = plan(Direction::Push, &locals, &snapshot, &state);
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
        let plan = plan(Direction::Pull, &[], &snapshot, &SyncState::default());
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
        let plan = plan(Direction::Pull, &locals, &snapshot, &state);
        assert_eq!(skips(&plan), ["n.md"]); // warned, not downloaded

        // Unchanged remote: fully silent.
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "epub", 5)], "");
        let plan = super::plan(Direction::Pull, &locals, &snapshot, &state);
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
        let plan = plan(Direction::Pull, &locals, &snapshot, &state);
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
    fn pull_conflicts_and_drift_skip() {
        // Both changed.
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 9)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let locals = vec![local("n.pdf", DocKind::Pdf, 99, 9)];
        let plan = plan(Direction::Pull, &locals, &snapshot, &state);
        assert_eq!(skips(&plan), ["n.pdf"]);

        // Destination (local) drift only.
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        let plan = super::plan(Direction::Pull, &locals, &snapshot, &state);
        assert_eq!(skips(&plan), ["n.pdf"]);
    }

    #[test]
    fn pull_recopies_locally_deleted_file() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        let mut state = SyncState::default();
        state
            .entries
            .insert("n.pdf".to_string(), entry("u1", DocKind::Pdf, 10, 1, 5));
        let plan = plan(Direction::Pull, &[], &snapshot, &state);
        assert!(matches!(plan.actions[0], SyncAction::Download { .. }));
    }

    #[test]
    fn pull_unmapped_local_collision_skips() {
        let snapshot = remote_snapshot(&[doc("u1", "n", "", "pdf", 5)], "");
        let locals = vec![local("n.pdf", DocKind::Pdf, 10, 1)];
        let plan = plan(Direction::Pull, &locals, &snapshot, &SyncState::default());
        assert_eq!(skips(&plan), ["n.pdf"]);
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
