//! The local cache and reading it.
//!
//! The cache is a bare clone of origin at
//! `$XDG_CACHE_HOME/rmu/hub/<repo-id>.git` with origin's object format,
//! guarded by an `flock` on `<repo-id>.lock` next to it, so two runs on
//! one computer never write it at once. It is disposable: a missing or
//! mismatched one is re-created and fetched again.
//!
//! [`Store`] reads it through one `cat-file --batch` process: commits,
//! trees (raw tree objects, so walking a history needs no process per
//! commit), document versions, blobs. It implements the merge engine's
//! [`BlobSource`] and, through [`StoreHistory`], its [`History`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CycleError;
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
/// object format origin advertises (`url`), and take its lock.
pub(super) fn open_cache(git: &Git, root: &Path, repo_id: &RepoId, url: &str) -> Result<Cache> {
    let lock = take_lock(&lock_path(root, repo_id))?;
    let path = root.join(format!("{repo_id}.git"));
    let repo = open_or_create(git, &path, ObjectFormat::Sha1)?;
    let advertised = advertised_format(&repo, url)?;
    let repo = match advertised {
        Some(format) if format != repo.object_format()? => open_or_create(git, &path, format)?,
        _ => repo,
    };
    Ok(Cache { repo, _lock: lock })
}

/// The object format of origin's refs (`None` if it advertises none).
fn advertised_format(repo: &Repo, url: &str) -> Result<Option<ObjectFormat>> {
    Ok(repo
        .ls_remote(url)?
        .first()
        .map(|reference| reference.oid.object_format()))
}

/// For a first contact (`clone`), when the repo id is not known yet:
/// fetch origin's `main` into a temporary repository, read its repo id,
/// then make that the cache (or fetch from it into an existing cache).
pub(super) fn bootstrap_cache(git: &Git, root: &Path, url: &str) -> Result<(Cache, Oid)> {
    fs::create_dir_all(root)?;
    let temp = root.join(format!("incoming-{}.git", uuid::Uuid::new_v4().simple()));
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
    result
}

/// `ls-remote` origin, then fetch `main` and the device ref (if origin
/// has them) into the cache.
pub(super) fn fetch(repo: &Repo, url: &str, device: Option<&DeviceId>) -> Result<Remote> {
    let refs = repo.ls_remote(url)?;
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
    let main = match has_main {
        true => repo.rev_parse(LOCAL_MAIN)?,
        false => None,
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
pub(super) struct Store {
    pub repo: Repo,
    format: ObjectFormat,
    cat: RefCell<Option<CatFile>>,
    commits: RefCell<HashMap<Oid, Rc<CommitObject>>>,
    trees: RefCell<HashMap<Oid, Rc<Vec<TreeEntry>>>>,
    json: RefCell<HashMap<Oid, Option<Rc<Value>>>>,
    chains: RefCell<HashMap<Oid, Rc<Vec<Oid>>>>,
}

impl Store {
    pub fn new(repo: Repo) -> Result<Self> {
        let format = repo.object_format()?;
        Ok(Self {
            repo,
            format,
            cat: RefCell::new(None),
            commits: RefCell::default(),
            trees: RefCell::default(),
            json: RefCell::default(),
            chains: RefCell::default(),
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
        if let Some(found) = self.commits.borrow().get(oid) {
            return Ok(Rc::clone(found));
        }
        let commit = Rc::new(git::parse_commit(&self.object(oid, ObjectKind::Commit)?)?);
        self.commits
            .borrow_mut()
            .insert(oid.clone(), Rc::clone(&commit));
        Ok(commit)
    }

    /// Whether `oid` names a commit the cache has.
    pub fn has_commit(&self, oid: &Oid) -> Result<bool> {
        Ok(self.repo.rev_parse(&format!("{oid}^{{commit}}"))?.as_ref() == Some(oid))
    }

    fn tree(&self, oid: &Oid) -> Result<Rc<Vec<TreeEntry>>> {
        if let Some(found) = self.trees.borrow().get(oid) {
            return Ok(Rc::clone(found));
        }
        let entries = Rc::new(git::parse_tree(
            &self.object(oid, ObjectKind::Tree)?,
            self.format,
        )?);
        self.trees
            .borrow_mut()
            .insert(oid.clone(), Rc::clone(&entries));
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

    /// The tree id of `docs/<doc>` in `commit`, if it has one.
    pub fn doc_subtree(&self, commit: &Oid, doc: &DocId) -> Result<Option<Oid>> {
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

    /// `tip` and its first-parent ancestors, newest first.
    pub fn first_parents(&self, tip: &Oid) -> Result<Rc<Vec<Oid>>> {
        if let Some(found) = self.chains.borrow().get(tip) {
            return Ok(Rc::clone(found));
        }
        let chain = Rc::new(self.repo.rev_list(&[tip.as_str()], true)?);
        self.chains
            .borrow_mut()
            .insert(tip.clone(), Rc::clone(&chain));
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
        if let Some(found) = self.json.borrow().get(id) {
            return Ok(found.clone());
        }
        let parsed = serde_json::from_slice::<Value>(&self.blob(id)?)
            .ok()
            .map(Rc::new);
        self.json.borrow_mut().insert(id.clone(), parsed.clone());
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

impl StoreHistory<'_> {
    /// `doc`'s distinct versions along the first-parent chain, newest
    /// first, with the commit each was found at (consecutive commits
    /// sharing a subtree are visited once).
    pub fn versions(&self, doc: &DocId) -> Result<Vec<(Oid, DocVersion)>> {
        let chain = self.store.first_parents(self.tip)?;
        let subtrees = chain
            .iter()
            .map(|commit| Ok((commit, self.store.doc_subtree(commit, doc)?)))
            .collect::<Result<Vec<_>>>()?;
        let (versions, _) = subtrees.into_iter().try_fold(
            (Vec::new(), None::<Oid>),
            |(mut versions, previous), (commit, subtree)| -> Result<_> {
                if let Some(subtree) = subtree.as_ref().filter(|s| previous.as_ref() != Some(*s)) {
                    versions.push((
                        commit.clone(),
                        self.store.doc_version(self.rules, doc, subtree)?,
                    ));
                }
                Ok((versions, subtree))
            },
        )?;
        Ok(versions)
    }
}

impl History for StoreHistory<'_> {
    fn present_versions(&self, doc: &DocId) -> Result<Vec<DocVersion>> {
        Ok(self
            .versions(doc)?
            .into_iter()
            .map(|(_, version)| version)
            .filter(DocVersion::is_present)
            .collect())
    }
}
