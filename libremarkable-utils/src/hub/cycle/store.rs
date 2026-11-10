//! The local cache and reading it.
//!
//! The cache is a bare clone of origin at
//! `$XDG_CACHE_HOME/rmu/hub/<repo-id>.git` with origin's object format,
//! guarded by an `flock` on `<repo-id>.lock` next to it, so two runs on
//! one computer never write it at once. It is disposable: a missing or
//! mismatched one is re-created and fetched again.
//!
//! [`Store`] reads it through one `cat-file --batch` process: commits,
//! trees (raw tree objects), document versions, blobs; its caches are
//! bounded ([`Bounded`]). Searches over history go through the
//! [`HistoryIndex`] of origin `main` (one `rev-list` and one `diff-tree`
//! process), never a tree read per commit. It implements the merge
//! engine's [`BlobSource`] and, through [`StoreHistory`], its
//! [`History`].

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CycleError;
use super::history::HistoryIndex;
use super::plan::JsonBlobs;
use crate::error::{Error, Result};
use crate::git::{
    self, CatFile, CommitObject, Expected, Git, ObjectFormat, ObjectKind, Oid, Repo, TreeEntry,
};
use crate::hub::merge::{BlobSource, History};
use crate::hub::model::{
    self, DeviceId, DocId, DocVersion, MirrorRules, REPO_CONFIG_PATH, RepoId, RepoTree,
};

/// Origin's shared branch.
pub(super) const MAIN: &str = "refs/heads/main";
/// Where the cache keeps origin's `main`.
pub(super) const LOCAL_MAIN: &str = "refs/remotes/origin/main";

/// A device ref's name on origin, preferred form first: forges that
/// refuse custom refs get the branch-shaped fallback.
pub(super) fn device_ref_names(device: &DeviceId) -> [String; 2] {
    [
        format!("refs/rmu/devices/{device}"),
        format!("refs/heads/rmu/devices/{device}"),
    ]
}

/// Where the cache keeps a device ref.
pub(super) fn local_device_ref(device: &DeviceId) -> String {
    format!("refs/remotes/origin/devices/{device}")
}

/// A ref on origin and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteRef {
    pub name: String,
    pub oid: Oid,
}

/// What a fetch found on origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Remote {
    /// `main`, as fetched (`None`: origin has no `main`).
    pub main: Option<Oid>,
    /// The device ref, as fetched.
    pub device: Option<RemoteRef>,
}

/// Where the last pre-read of a tablet (`device`, or `None` before it has
/// a device id) against the cache of `repo_id` keeps what it read: next
/// to the cache, under its `flock`.
pub(super) fn pre_read_path(root: &Path, repo_id: &RepoId, device: Option<&DeviceId>) -> PathBuf {
    let tablet = device.map_or_else(|| "new".to_string(), ToString::to_string);
    root.join(format!("{repo_id}.pre-read-{tablet}.json"))
}

/// A saved pre-read (see [`save_pre_read`]); empty if there is none or
/// it is unreadable (it is only a hint: every entry is checked against
/// the tablet's stat and the cache before use).
pub(super) fn load_pre_read(path: &Path) -> BTreeMap<model::XochitlPath, model::ManifestEntry> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Keep what a pre-read read (each file's stat and blob), replacing the
/// last one atomically. Best effort.
pub(super) fn save_pre_read(
    path: &Path,
    entries: &BTreeMap<model::XochitlPath, model::ManifestEntry>,
) {
    let temp = path.with_extension("json.tmp");
    let _ = serde_json::to_vec(entries)
        .map_err(io_error)
        .and_then(|bytes| fs::write(&temp, bytes))
        .and_then(|()| fs::rename(&temp, path));
}

fn io_error(error: serde_json::Error) -> std::io::Error {
    std::io::Error::other(error)
}

/// The local cache, held under its `flock` for as long as this lives.
pub(super) struct Cache {
    pub repo: Repo,
    _lock: File,
}

fn lock_path(root: &Path, repo_id: &RepoId) -> PathBuf {
    root.join(format!("{repo_id}.lock"))
}

/// How long to wait for the cache lock before reporting it busy. A child
/// process forked by another thread holds a copy of the descriptor (and
/// so the lock) until it execs, so a brief wait is not contention.
const LOCK_PATIENCE: Duration = Duration::from_secs(5);

fn take_lock(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    let deadline = Instant::now() + LOCK_PATIENCE;
    let taken = std::iter::repeat_with(|| match file.try_lock() {
        Ok(()) => Some(Ok(())),
        Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
            std::thread::sleep(Duration::from_millis(20));
            None
        }
        Err(TryLockError::WouldBlock) => {
            Some(Err(CycleError::CacheBusy(path.to_path_buf()).into()))
        }
        Err(TryLockError::Error(error)) => Some(Err(Error::from(error))),
    })
    .find_map(|attempt| attempt);
    taken.unwrap_or_else(|| Err(CycleError::CacheBusy(path.to_path_buf()).into()))?;
    Ok(file)
}

/// An existing cache of `format`, else a fresh one (a missing or
/// mismatched cache is replaced: it is disposable).
fn open_or_create(git: &Git, path: &Path, format: ObjectFormat) -> Result<Repo> {
    if path.exists() {
        match git.open(path) {
            Ok(repo) if repo.object_format()? == format => return Ok(repo),
            _ => fs::remove_dir_all(path)?,
        }
    }
    git.init_bare(path, format)
}

/// Open (or create) the cache of `repo_id` under `root`, matching the
/// object format origin advertises (`url`), and take its lock. Returns
/// the advertisement too, for the first [`fetch`] to reuse.
pub(super) fn open_cache(
    git: &Git,
    root: &Path,
    repo_id: &RepoId,
    url: &str,
) -> Result<(Cache, Vec<git::RemoteRef>)> {
    let lock = take_lock(&lock_path(root, repo_id))?;
    let path = root.join(format!("{repo_id}.git"));
    let repo = open_or_create(git, &path, ObjectFormat::Sha1)?;
    let refs = repo.ls_remote(url)?;
    let repo = match refs.first().map(|reference| reference.oid.object_format()) {
        Some(format) if format != repo.object_format()? => open_or_create(git, &path, format)?,
        _ => repo,
    };
    Ok((Cache { repo, _lock: lock }, refs))
}

/// Remove `incoming-*.git` directories that no clone is using any more
/// (one killed mid-fetch leaves its own, possibly gigabytes): each has an
/// `incoming-*.lock` its clone holds while it runs.
fn sweep_incoming(root: &Path) -> Result<()> {
    fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix("incoming-")?
                .strip_suffix(".git")
                .map(str::to_string)
        })
        .try_for_each(|token| {
            let lock = root.join(format!("incoming-{token}.lock"));
            let file = File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock)?;
            if file.try_lock().is_ok() {
                fs::remove_dir_all(root.join(format!("incoming-{token}.git")))?;
                drop(file);
                let _ = fs::remove_file(&lock);
            }
            Ok(())
        })
}

/// For a first contact (`clone`), when the repo id is not known yet:
/// fetch origin's `main` into a temporary repository, read its repo id,
/// then make that the cache (or fetch from it into an existing cache).
pub(super) fn bootstrap_cache(git: &Git, root: &Path, url: &str) -> Result<(Cache, Oid)> {
    fs::create_dir_all(root)?;
    sweep_incoming(root)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let temp = root.join(format!("incoming-{token}.git"));
    let temp_lock = root.join(format!("incoming-{token}.lock"));
    let held = take_lock(&temp_lock)?;
    let result = (|| {
        let probe = git.init_bare(&temp, ObjectFormat::Sha1)?;
        let refs = probe.ls_remote(url)?;
        let main = refs
            .iter()
            .find(|reference| reference.name == MAIN)
            .ok_or_else(|| CycleError::NotInitialised {
                url: url.to_string(),
            })?;
        let probe = match main.oid.object_format() {
            ObjectFormat::Sha1 => probe,
            format => {
                fs::remove_dir_all(&temp)?;
                git.init_bare(&temp, format)?
            }
        };
        probe.fetch(url, &[&format!("+{MAIN}:{LOCAL_MAIN}")], false)?;
        let main = probe
            .rev_parse(LOCAL_MAIN)?
            .ok_or_else(|| CycleError::NotInitialised {
                url: url.to_string(),
            })?;
        let config = Store::new(probe.clone())?.repo_config(&main)?;
        let lock = take_lock(&lock_path(root, &config.repo_id))?;
        let path = root.join(format!("{}.git", config.repo_id));
        let format = main.object_format();
        let repo = match git.open(&path) {
            Ok(repo) if repo.object_format()? == format => {
                repo.fetch(
                    temp.to_str().ok_or_else(|| {
                        Error::InvalidGitInput("cache path is not UTF-8".to_string())
                    })?,
                    &[&format!("+{LOCAL_MAIN}:{LOCAL_MAIN}")],
                    false,
                )?;
                repo
            }
            _ => {
                if path.exists() {
                    fs::remove_dir_all(&path)?;
                }
                fs::rename(&temp, &path)?;
                git.open(&path)?
            }
        };
        Ok((Cache { repo, _lock: lock }, main))
    })();
    if temp.exists() {
        let _ = fs::remove_dir_all(&temp);
    }
    drop(held);
    let _ = fs::remove_file(&temp_lock);
    result
}

/// `ls-remote` origin (unless `advertised` is the advertisement just
/// read), then fetch `main` and the device ref (if origin has them) into
/// the cache.
pub(super) fn fetch(
    repo: &Repo,
    url: &str,
    device: Option<&DeviceId>,
    advertised: Option<Vec<git::RemoteRef>>,
) -> Result<Remote> {
    let refs = match advertised {
        Some(refs) => refs,
        None => repo.ls_remote(url)?,
    };
    let advertised = |name: &str| refs.iter().any(|reference| reference.name == name);
    let has_main = advertised(MAIN);
    let device_name = device.and_then(|device| {
        device_ref_names(device)
            .into_iter()
            .find(|name| advertised(name))
            .map(|name| (name, local_device_ref(device)))
    });
    let specs: Vec<String> = has_main
        .then(|| format!("+{MAIN}:{LOCAL_MAIN}"))
        .into_iter()
        .chain(
            device_name
                .iter()
                .map(|(name, local)| format!("+{name}:{local}")),
        )
        .collect();
    if !specs.is_empty() {
        let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
        repo.fetch(url, &specs, false)?;
    }
    let main = if has_main {
        repo.rev_parse(LOCAL_MAIN)?
    } else {
        None
    };
    let device = match device_name {
        Some((name, local)) => repo.rev_parse(&local)?.map(|oid| RemoteRef { name, oid }),
        None => None,
    };
    Ok(Remote { main, device })
}

/// Push `commit` to `dst` on origin and mirror the new value in the
/// cache's ref `local`.
pub(super) fn push(
    repo: &Repo,
    url: &str,
    commit: &Oid,
    dst: &str,
    lease: Option<&Expected>,
    local: &str,
) -> Result<()> {
    repo.push(url, commit, dst, lease)?;
    repo.update_ref(local, commit, None)
}

/// Reads the cache (see the module docs). Not `Sync`: one per run.
/// A cache holding about `capacity` weight at most, in two generations:
/// when the young one reaches half the capacity, the old one is dropped
/// and the young one takes its place; a hit in the old one moves the
/// entry back to the young one. Memory stays bounded however long the
/// history, and entries in use stay cached.
pub(super) struct Bounded<V> {
    capacity: usize,
    young: HashMap<Oid, (V, usize)>,
    old: HashMap<Oid, (V, usize)>,
    weight: usize,
}

impl<V: Clone> Bounded<V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            young: HashMap::new(),
            old: HashMap::new(),
            weight: 0,
        }
    }

    pub fn get(&mut self, key: &Oid) -> Option<V> {
        if let Some((value, _)) = self.young.get(key) {
            return Some(value.clone());
        }
        let (value, weight) = self.old.remove(key)?;
        self.insert(key.clone(), value.clone(), weight);
        Some(value)
    }

    pub fn insert(&mut self, key: Oid, value: V, weight: usize) {
        if self.weight + weight > self.capacity / 2 && !self.young.is_empty() {
            self.old = std::mem::take(&mut self.young);
            self.weight = 0;
        }
        self.weight += weight;
        self.young.insert(key, (value, weight));
    }

    /// Entries held (both generations).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.young.len() + self.old.len()
    }
}

/// Cache bounds: commits by count, trees by entries, JSON by the bytes it
/// was parsed from.
const COMMIT_CACHE: usize = 16 * 1024;
const TREE_CACHE: usize = 256 * 1024;
const JSON_CACHE: usize = 64 * 1024 * 1024;

pub(super) struct Store {
    pub repo: Repo,
    format: ObjectFormat,
    cat: RefCell<Option<CatFile>>,
    commits: RefCell<Bounded<Rc<CommitObject>>>,
    trees: RefCell<Bounded<Rc<Vec<TreeEntry>>>>,
    json: RefCell<Bounded<Option<Rc<Value>>>>,
    /// The first-parent chain of the last tip asked for.
    chain: RefCell<Option<(Oid, Rc<Vec<Oid>>)>>,
    /// The history index of the last tip asked for.
    history: RefCell<Option<Rc<HistoryIndex>>>,
    /// The ancestors of the last tip asked for (when it has no index).
    ancestry: RefCell<Option<(Oid, Rc<std::collections::HashSet<Oid>>)>>,
}

impl Store {
    pub fn new(repo: Repo) -> Result<Self> {
        let format = repo.object_format()?;
        Ok(Self {
            repo,
            format,
            cat: RefCell::new(None),
            commits: RefCell::new(Bounded::new(COMMIT_CACHE)),
            trees: RefCell::new(Bounded::new(TREE_CACHE)),
            json: RefCell::new(Bounded::new(JSON_CACHE)),
            chain: RefCell::default(),
            history: RefCell::default(),
            ancestry: RefCell::default(),
        })
    }

    fn with_cat<T>(&self, f: impl FnOnce(&mut CatFile) -> Result<T>) -> Result<T> {
        let mut cat = self.cat.borrow_mut();
        if cat.is_none() {
            *cat = Some(self.repo.cat_file()?);
        }
        let result = f(cat.as_mut().expect("started above"));
        if result.is_err() {
            // A failed process cannot be reused.
            *cat = None;
        }
        result
    }

    /// An object's content, which must exist and be of `kind`.
    fn object(&self, oid: &Oid, kind: ObjectKind) -> Result<Vec<u8>> {
        match self.with_cat(|cat| cat.read(oid))? {
            Some((found, data)) if found == kind => Ok(data),
            Some((found, _)) => Err(Error::GitOutput {
                command: "cat-file".into(),
                detail: format!("{oid} is a {found:?}, expected a {kind:?}"),
            }),
            None => Err(CycleError::MissingObject(oid.clone()).into()),
        }
    }

    pub fn blob(&self, oid: &Oid) -> Result<Vec<u8>> {
        self.object(oid, ObjectKind::Blob)
    }

    /// Stream a blob: `sink` gets its size and a reader over exactly
    /// that many bytes.
    pub fn stream_blob(
        &self,
        oid: &Oid,
        sink: &mut dyn FnMut(u64, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        self.with_cat(|cat| match cat.object(oid)? {
            Some(mut object) if object.kind == ObjectKind::Blob => {
                let size = object.size;
                sink(size, &mut object)
            }
            Some(_) | None => Err(CycleError::MissingObject(oid.clone()).into()),
        })
    }

    /// The SHA-256 (lowercase hex) of a blob's content, as the tablet's
    /// `sha256sum` reports it.
    pub fn blob_sha256(&self, oid: &Oid) -> Result<String> {
        let mut hasher = Sha256::new();
        self.stream_blob(oid, &mut |_, reader| {
            std::io::copy(reader, &mut hasher)?;
            Ok(())
        })?;
        Ok(format!("{:x}", hasher.finalize()))
    }

    pub fn commit(&self, oid: &Oid) -> Result<Rc<CommitObject>> {
        if let Some(found) = self.commits.borrow_mut().get(oid) {
            return Ok(found);
        }
        let commit = Rc::new(git::parse_commit(&self.object(oid, ObjectKind::Commit)?)?);
        self.commits
            .borrow_mut()
            .insert(oid.clone(), Rc::clone(&commit), 1);
        Ok(commit)
    }

    /// Whether `oid` names a commit the cache has.
    pub fn has_commit(&self, oid: &Oid) -> Result<bool> {
        Ok(self.repo.rev_parse(&format!("{oid}^{{commit}}"))?.as_ref() == Some(oid))
    }

    fn tree(&self, oid: &Oid) -> Result<Rc<Vec<TreeEntry>>> {
        if let Some(found) = self.trees.borrow_mut().get(oid) {
            return Ok(found);
        }
        let entries = Rc::new(git::parse_tree(
            &self.object(oid, ObjectKind::Tree)?,
            self.format,
        )?);
        self.trees
            .borrow_mut()
            .insert(oid.clone(), Rc::clone(&entries), entries.len() + 1);
        Ok(entries)
    }

    /// The entry `name` of tree `oid`.
    fn child(&self, oid: &Oid, name: &str) -> Result<Option<TreeEntry>> {
        Ok(self
            .tree(oid)?
            .iter()
            .find(|entry| entry.path == name.as_bytes())
            .cloned())
    }

    /// The tree id of `docs/<doc>` in `commit`, if it has one: from the
    /// history index when one is built that has the commit, else by
    /// reading its trees.
    pub fn doc_subtree(&self, commit: &Oid, doc: &DocId) -> Result<Option<Oid>> {
        if let Some(found) = self
            .history
            .borrow()
            .as_ref()
            .and_then(|index| index.subtree_at(commit, doc))
        {
            return Ok(found.cloned());
        }
        let root = self.commit(commit)?.tree.clone();
        let Some(docs) = self
            .child(&root, "docs")?
            .filter(|entry| entry.kind == ObjectKind::Tree)
        else {
            return Ok(None);
        };
        Ok(self
            .child(&docs.oid, doc.as_str())?
            .filter(|entry| entry.kind == ObjectKind::Tree)
            .map(|entry| entry.oid))
    }

    /// Every non-tree entry below tree `oid`, paths prefixed.
    fn walk(&self, oid: &Oid, prefix: &[u8]) -> Result<Vec<TreeEntry>> {
        self.tree(oid)?
            .iter()
            .map(|entry| {
                let path = [prefix, b"/", &entry.path].concat();
                match entry.kind {
                    ObjectKind::Tree => self.walk(&entry.oid, &path),
                    _ => Ok(vec![TreeEntry {
                        path,
                        ..entry.clone()
                    }]),
                }
            })
            .collect::<Result<Vec<_>>>()
            .map(|nested| nested.into_iter().flatten().collect())
    }

    /// The document version held by subtree `oid` of `docs/<doc>`.
    pub fn doc_version(&self, rules: &MirrorRules, doc: &DocId, oid: &Oid) -> Result<DocVersion> {
        let listing = model::tree_listing(self.walk(oid, format!("docs/{doc}").as_bytes())?)?;
        Ok(rules
            .group_tree(listing)?
            .docs
            .remove(doc)
            .unwrap_or_default())
    }

    /// `doc`'s version in `commit`, if the commit has its subtree.
    pub fn version_at(
        &self,
        rules: &MirrorRules,
        commit: &Oid,
        doc: &DocId,
    ) -> Result<Option<DocVersion>> {
        self.doc_subtree(commit, doc)?
            .map(|subtree| self.doc_version(rules, doc, &subtree))
            .transpose()
    }

    /// The whole tree of `commit`, grouped by document.
    pub fn tree_of(&self, rules: &MirrorRules, commit: &Oid) -> Result<RepoTree> {
        let listing = model::tree_listing(self.repo.ls_tree_recursive(commit.as_str())?)?;
        Ok(rules.group_tree(listing)?)
    }

    /// The history index of `tip` (origin `main`), built once per tip.
    pub fn history(&self, tip: &Oid) -> Result<Rc<HistoryIndex>> {
        if let Some(found) = self
            .history
            .borrow()
            .as_ref()
            .filter(|index| index.tip() == Some(tip))
        {
            return Ok(Rc::clone(found));
        }
        let commits = self.repo.rev_list_parents(tip.as_str())?;
        let changes = self.repo.first_parent_changes(&commits, &["docs"])?;
        let index = Rc::new(HistoryIndex::build(&commits, &changes)?);
        *self.history.borrow_mut() = Some(Rc::clone(&index));
        Ok(index)
    }

    /// Whether `commit` is `tip` or one of its ancestors (absent from the
    /// cache: no), from `tip`'s history index if one is built, else from
    /// one `rev-list` of `tip` (both kept for the next question).
    pub fn is_ancestor(&self, commit: &Oid, tip: &Oid) -> Result<bool> {
        if let Some(index) = self
            .history
            .borrow()
            .as_ref()
            .filter(|index| index.tip() == Some(tip))
        {
            return Ok(index.contains(commit));
        }
        if let Some((_, set)) = self.ancestry.borrow().as_ref().filter(|(at, _)| at == tip) {
            return Ok(set.contains(commit));
        }
        let set: Rc<std::collections::HashSet<Oid>> = Rc::new(
            self.repo
                .rev_list(&[tip.as_str()], false)?
                .into_iter()
                .collect(),
        );
        let found = set.contains(commit);
        *self.ancestry.borrow_mut() = Some((tip.clone(), set));
        Ok(found)
    }

    /// The newest ancestor of `tip` (children before parents; device refs
    /// are never searched, but snapshots merged into `main` are its
    /// ancestors) whose version of `doc` satisfies `matches`. Each
    /// distinct version is judged once, newest first, until one matches.
    pub fn newest_matching(
        &self,
        rules: &MirrorRules,
        tip: &Oid,
        doc: &DocId,
        matches: &mut dyn FnMut(&DocVersion) -> Result<bool>,
    ) -> Result<Option<Oid>> {
        self.history(tip)?
            .versions(doc)
            .into_iter()
            .filter_map(|(commit, subtree)| Some((commit, subtree?)))
            .map(|(commit, subtree)| -> Result<Option<Oid>> {
                let version = self.doc_version(rules, doc, subtree)?;
                Ok(matches(&version)?.then(|| commit.clone()))
            })
            .find_map(Result::transpose)
            .transpose()
    }

    /// `tip` and its first-parent ancestors, newest first.
    pub fn first_parents(&self, tip: &Oid) -> Result<Rc<Vec<Oid>>> {
        if let Some((_, found)) = self.chain.borrow().as_ref().filter(|(at, _)| at == tip) {
            return Ok(Rc::clone(found));
        }
        let chain = Rc::new(self.repo.rev_list(&[tip.as_str()], true)?);
        *self.chain.borrow_mut() = Some((tip.clone(), Rc::clone(&chain)));
        Ok(chain)
    }

    /// A file of `commit` by repository path, if present.
    pub fn file_at(&self, commit: &Oid, path: &str) -> Result<Option<Vec<u8>>> {
        let root = self.commit(commit)?.tree.clone();
        let found = path.split('/').try_fold(
            Some(TreeEntry {
                mode: 0o040000,
                kind: ObjectKind::Tree,
                oid: root,
                size: None,
                path: Vec::new(),
            }),
            |entry, name| match entry {
                Some(entry) if entry.kind == ObjectKind::Tree => self.child(&entry.oid, name),
                _ => Ok(None),
            },
        )?;
        found
            .filter(|entry| entry.kind == ObjectKind::Blob)
            .map(|entry| self.blob(&entry.oid))
            .transpose()
    }

    /// Origin's `.rmu/hub.json` as of `commit`.
    pub fn repo_config(&self, commit: &Oid) -> Result<model::RepoConfig> {
        let bytes = self.file_at(commit, REPO_CONFIG_PATH)?.ok_or_else(|| {
            CycleError::InvalidRepoConfig(format!("{REPO_CONFIG_PATH} is missing"))
        })?;
        model::RepoConfig::parse(&bytes).map_err(|error| match error {
            unsupported @ (model::ModelError::UnsupportedFormat { .. }
            | model::ModelError::UnsupportedRules { .. }) => unsupported.into(),
            other => CycleError::InvalidRepoConfig(other.to_string()).into(),
        })
    }

    /// The trailers of every commit reachable from `revisions`.
    pub fn messages(&self, revisions: &[&str], first_parent: bool) -> Result<Vec<String>> {
        self.repo
            .rev_list(revisions, first_parent)?
            .iter()
            .map(|oid| Ok(self.commit(oid)?.message.clone()))
            .collect()
    }
}

impl BlobSource for Store {
    fn read(&self, id: &Oid) -> Result<Vec<u8>> {
        self.blob(id)
    }
}

impl JsonBlobs for Store {
    fn json(&self, id: &Oid) -> Result<Option<Rc<Value>>> {
        if let Some(found) = self.json.borrow_mut().get(id) {
            return Ok(found);
        }
        let bytes = self.blob(id)?;
        let parsed = serde_json::from_slice::<Value>(&bytes).ok().map(Rc::new);
        self.json
            .borrow_mut()
            .insert(id.clone(), parsed.clone(), bytes.len() + 1);
        Ok(parsed)
    }
}

/// Origin's history as the merge engine reads it: `tip` (origin `main`)
/// and its first-parent ancestors, never device refs.
pub(super) struct StoreHistory<'a> {
    pub store: &'a Store,
    pub rules: &'a MirrorRules,
    pub tip: &'a Oid,
}

impl History for StoreHistory<'_> {
    /// `doc`'s present versions along `tip`'s first-parent chain, newest
    /// first, from the history index.
    fn present_versions(&self, doc: &DocId) -> Result<Vec<DocVersion>> {
        self.store
            .history(self.tip)?
            .first_parent_versions(doc)
            .into_iter()
            .flatten()
            .map(|subtree| self.store.doc_version(self.rules, doc, subtree))
            .filter(|version| !matches!(version, Ok(version) if !version.is_present()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{CommitMeta, FileMode, Signature};
    use test_support::Scratch;

    fn doc(n: u32) -> DocId {
        DocId::new(format!("8c1f0b9e-0000-4000-8000-{n:012x}")).unwrap()
    }

    /// A commit whose tree is exactly `files` (path, content).
    fn commit(repo: &Repo, parents: &[&Oid], files: &[(String, &str)]) -> Oid {
        let mut writer = repo
            .commit_writer(&crate::progress::NoProgress, None)
            .unwrap();
        files.iter().for_each(|(path, data)| {
            writer
                .add_bytes(path.as_str(), FileMode::Regular, data.as_bytes())
                .unwrap();
        });
        let who = Signature {
            name: "Test".to_string(),
            email: "test@example.invalid".to_string(),
            time: 1_800_000_000,
            offset_minutes: 0,
        };
        writer
            .commit(&CommitMeta {
                parents: parents.iter().map(|&parent| parent.clone()).collect(),
                author: who.clone(),
                committer: who,
                message: "test\n".to_string(),
            })
            .unwrap()
    }

    /// `docs/<doc>`'s subtree in `commit`, by a full tree listing.
    fn listed(repo: &Repo, commit: &Oid, doc: &DocId) -> Option<Oid> {
        let path = format!("docs/{doc}");
        repo.ls_tree_recursive(commit.as_str())
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == path.as_bytes() && entry.kind == ObjectKind::Tree)
            .map(|entry| entry.oid)
    }

    /// The history index against full tree reads, on a history with a
    /// root adding documents, a deletion and a re-add of the same
    /// subtree, a blob at `docs/<uuid>` turning into a tree and back, and
    /// a merge taking the side branch's version.
    #[test]
    fn the_history_index_agrees_with_tree_reads() {
        let scratch = Scratch::new("history-index");
        let git = crate::git::Git::new()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", scratch.path());
        let repo = git
            .init_bare(&scratch.path().join("r.git"), ObjectFormat::Sha1)
            .unwrap();
        let (d1, d2, d3) = (doc(1), doc(2), doc(3));
        let meta = |d: &DocId, v: &'static str| (format!("docs/{d}/metadata"), v);
        let blob = |d: &DocId, v: &'static str| (format!("docs/{d}"), v);
        let root = commit(&repo, &[], &[meta(&d1, "1"), meta(&d2, "2")]);
        let a = commit(&repo, &[&root], &[meta(&d1, "1a")]);
        let b = commit(
            &repo,
            &[&a],
            &[meta(&d1, "1a"), meta(&d2, "2"), blob(&d3, "x")],
        );
        let side = commit(
            &repo,
            &[&root],
            &[meta(&d1, "1s"), meta(&d2, "2"), meta(&d3, "3")],
        );
        let merge = commit(
            &repo,
            &[&b, &side],
            &[meta(&d1, "1s"), meta(&d2, "2"), meta(&d3, "3")],
        );
        let tip = commit(&repo, &[&merge], &[meta(&d2, "2"), blob(&d3, "y")]);
        let store = Store::new(repo.clone()).unwrap();
        let index = store.history(&tip).unwrap();
        let commits = repo.rev_list_topo(&[tip.as_str()]).unwrap();
        let chain = repo.rev_list(&[tip.as_str()], true).unwrap();
        [&d1, &d2, &d3, &doc(4)].into_iter().for_each(|d| {
            let truth: Vec<Option<Oid>> = commits.iter().map(|c| listed(&repo, c, d)).collect();
            commits.iter().zip(&truth).for_each(|(c, expected)| {
                assert_eq!(
                    index.subtree_at(c, d),
                    Some(expected.as_ref()),
                    "{d} at {c}"
                );
                assert_eq!(&store.doc_subtree(c, d).unwrap(), expected, "{d} at {c}");
            });
            // Each distinct version (absent included) at its newest holder.
            let mut seen = std::collections::HashSet::new();
            let versions: Vec<(&Oid, Option<&Oid>)> = commits
                .iter()
                .zip(&truth)
                .filter(|(_, version)| seen.insert(*version))
                .map(|(c, version)| (c, version.as_ref()))
                .collect();
            assert_eq!(index.versions(d), versions, "{d}");
            // The first-parent chain's present versions, newest first, one
            // per stretch.
            let (stretches, _) = chain.iter().map(|c| listed(&repo, c, d)).fold(
                (Vec::new(), None::<Option<Oid>>),
                |(mut out, previous), version| {
                    if previous.as_ref() != Some(&version) {
                        out.extend(version.clone());
                    }
                    (out, Some(version))
                },
            );
            let indexed: Vec<Oid> = index
                .first_parent_versions(d)
                .into_iter()
                .flatten()
                .cloned()
                .collect();
            assert_eq!(indexed, stretches, "{d}");
        });
        // The ancestry check: from the index of `tip`, and for another
        // tip from one rev-list (the index stays `tip`'s).
        assert!(store.is_ancestor(&side, &tip).unwrap());
        assert!(!store.is_ancestor(&tip, &merge).unwrap());
        assert!(store.is_ancestor(&root, &a).unwrap());
        assert!(!store.is_ancestor(&side, &a).unwrap());
        let unknown: Oid = "f".repeat(40).parse().unwrap();
        assert!(!store.is_ancestor(&unknown, &tip).unwrap());
        assert!(!store.is_ancestor(&unknown, &a).unwrap());
        // A subtree at a commit the index lacks is read from its trees.
        let detached = commit(&repo, &[], &[meta(&d1, "z")]);
        assert_eq!(
            store.doc_subtree(&detached, &d1).unwrap(),
            listed(&repo, &detached, &d1)
        );
    }

    fn oid(n: u32) -> Oid {
        format!("{n:040x}").parse().unwrap()
    }

    #[test]
    fn bounded_caches_drop_the_old_generation_and_keep_what_is_used() {
        let mut cache = Bounded::new(4);
        (1..=2).for_each(|n| cache.insert(oid(n), n, 1));
        // The young generation is full: 3 starts a new one.
        cache.insert(oid(3), 3, 1);
        assert_eq!(cache.get(&oid(1)), Some(1));
        cache.insert(oid(4), 4, 1);
        assert!(cache.len() <= 4);
        // 1 was used, so it survived the drop; 2 was not and is gone.
        assert_eq!(cache.get(&oid(2)), None);
        assert_eq!(cache.get(&oid(1)), Some(1));
        // An entry heavier than the capacity is still cached alone.
        cache.insert(oid(9), 9, 100);
        assert_eq!(cache.get(&oid(9)), Some(9));
    }
}
