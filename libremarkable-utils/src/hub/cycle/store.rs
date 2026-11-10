//! The local cache and reading it.
//!
//! The cache is a bare clone of origin at
//! `$XDG_CACHE_HOME/rmu/hub/<repo-id>.git` with origin's object format,
//! guarded by an `flock` on `<repo-id>.lock` next to it, so two runs on
//! one computer never write it at once. A missing one is created and
//! fetched; one that holds refs is never deleted (it may hold the last
//! copy of a replaced origin's history), and one of another object
//! format than origin's is refused. It records the origin URLs it was
//! opened with (`rmu-origin` inside it, a few), so a `clone` from one of
//! them fetches into it instead of transferring the library again.
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
    self, CatFile, CommitChanges, CommitObject, Expected, Git, ObjectFormat, ObjectKind, Oid,
    RawChange, Repo, TreeEntry,
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

/// Where the last pre-read of a tablet against the cache of `repo_id`
/// keeps what it read: next to the cache, under its `flock`.
pub(super) fn pre_read_path(root: &Path, repo_id: &RepoId, tablet: PreReadOf) -> PathBuf {
    let key = match tablet {
        PreReadOf::Device(device) => device.to_string(),
        PreReadOf::Destination(destination) => {
            let digest = Sha256::digest(destination.as_bytes());
            let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
            format!("at-{hex}")
        }
    };
    root.join(format!("{repo_id}.pre-read-{key}.json"))
}

/// Whose pre-read: an attached tablet's (its device id), or one not
/// attached yet (where this computer reaches it, [`Tablet::destination`]).
pub(super) enum PreReadOf<'a> {
    Device(&'a DeviceId),
    Destination(&'a str),
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

/// The file inside a cache recording the origin URLs it was opened with,
/// one per line, the most recent first.
const ORIGIN_FILE: &str = "rmu-origin";

/// How many origin URLs a cache remembers (one origin is often reached
/// through several: an ssh alias here, a full URL there, `--origin`).
const ORIGIN_URLS: usize = 8;

/// The URLs recorded in the cache at `git_dir`.
fn recorded_urls(git_dir: &Path) -> Vec<String> {
    fs::read_to_string(git_dir.join(ORIGIN_FILE))
        .map(|text| {
            text.lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `urls` with `url` first, without repeats, at most [`ORIGIN_URLS`].
fn with_url(urls: Vec<String>, url: &str) -> Vec<String> {
    std::iter::once(url.to_string())
        .chain(urls.into_iter().filter(|known| known != url))
        .take(ORIGIN_URLS)
        .collect()
}

/// Replace the record atomically (a reader never sees it half written);
/// no URLs: no record. Best effort.
fn write_urls(git_dir: &Path, urls: &[String]) {
    let file = git_dir.join(ORIGIN_FILE);
    if urls.is_empty() {
        let _ = fs::remove_file(&file);
        return;
    }
    let temp = git_dir.join(format!("{ORIGIN_FILE}.tmp"));
    let _ = fs::write(&temp, urls.join("\n") + "\n").and_then(|()| fs::rename(&temp, &file));
}

/// Under the cache's lock, before any writer exists: record `url`, drop
/// a [`BOOTSTRAP_REF`] an interrupted clone left (it would pin a whole
/// library), and remove the `.keep` files a killed `fast-import` left
/// (best effort).
fn opened(repo: &Repo, url: &str) {
    if !url.contains(char::is_control) {
        write_urls(
            repo.git_dir(),
            &with_url(recorded_urls(repo.git_dir()), url),
        );
    }
    let _ = repo.delete_ref(BOOTSTRAP_REF);
    let _ = repo.remove_fast_import_keeps();
}

/// Forget that the cache at `git_dir` is reached at `url` (another
/// repository answers there now).
fn forget_url(git_dir: &Path, url: &str) {
    let urls: Vec<String> = recorded_urls(git_dir)
        .into_iter()
        .filter(|known| known != url)
        .collect();
    write_urls(git_dir, &urls);
}

/// Open (or create) the cache of `repo_id` under `root`, matching the
/// object format origin advertises (`url`), and take its lock. Returns
/// the advertisement too, for the first [`fetch`] to reuse. An existing
/// cache is never deleted once it holds refs: a cache of another format
/// than origin's is refused (`CacheFormatMismatch`: a repository cannot
/// change format, so another answers there, or a converted one), and any
/// error opening it is returned as is.
pub(super) fn open_cache(
    git: &Git,
    root: &Path,
    repo_id: &RepoId,
    url: &str,
) -> Result<(Cache, Vec<git::RemoteRef>)> {
    let lock = take_lock(&lock_path(root, repo_id))?;
    let path = root.join(format!("{repo_id}.git"));
    // A new cache starts as SHA-1: it only runs the `ls-remote` before it
    // is re-created in origin's format, if that differs.
    let repo = if path.exists() {
        git.open(&path)?
    } else {
        git.init_bare(&path, ObjectFormat::Sha1)?
    };
    let refs = repo.ls_remote(url)?;
    let repo = match refs.first().map(|reference| reference.oid.object_format()) {
        Some(format) if format != repo.object_format()? => {
            if repo.has_refs()? {
                return Err(CycleError::CacheFormatMismatch {
                    cache: path,
                    url: url.to_string(),
                }
                .into());
            }
            fs::remove_dir_all(&path)?;
            git.init_bare(&path, format)?
        }
        _ => repo,
    };
    opened(&repo, url);
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

/// For a first contact (`clone`), when the repo id is not known yet.
/// - A cache this computer keeps for `url` (a recorded origin URL) whose
///   format is origin's is fetched into, if `main` there still has that
///   cache's repo id ([`known_cache`]).
/// - Otherwise origin's `main` goes into a temporary repository, whose
///   repo id names the cache: the temporary one becomes it, or a cache
///   with that id is fetched into from it locally. If a recorded cache
///   turned out stale, the temporary repository borrows what that cache
///   just fetched instead of fetching it again.
///
/// Either way the library crosses the network once. An existing cache is
/// replaced only when it opens and has another format than origin's with
/// the same repo id (origin holds the converted history).
pub(super) fn bootstrap_cache(git: &Git, root: &Path, url: &str) -> Result<(Cache, Oid)> {
    fs::create_dir_all(root)?;
    sweep_incoming(root)?;
    let stale = match known_cache(git, root, url)? {
        Known::Found(cache, main) => return Ok((cache, main)),
        Known::Stale { cache, .. } => Some(cache),
        Known::None => None,
    };
    let token = uuid::Uuid::new_v4().simple().to_string();
    let temp = root.join(format!("incoming-{token}.git"));
    let temp_lock = root.join(format!("incoming-{token}.lock"));
    let not_initialised = || CycleError::NotInitialised {
        url: url.to_string(),
    };
    let utf8 = |path: &Path| -> Result<String> {
        path.to_str()
            .map(str::to_string)
            .ok_or_else(|| Error::InvalidGitInput("cache path is not UTF-8".to_string()))
    };
    let result = (|| {
        let _held = take_lock(&temp_lock)?;
        let probe = git.init_bare(&temp, ObjectFormat::Sha1)?;
        let refs = probe.ls_remote(url)?;
        let format = refs
            .iter()
            .find(|reference| reference.name == MAIN)
            .ok_or_else(not_initialised)?
            .oid
            .object_format();
        let probe = match format {
            ObjectFormat::Sha1 => probe,
            format => {
                fs::remove_dir_all(&temp)?;
                git.init_bare(&temp, format)?
            }
        };
        match &stale {
            // What a stale cache just fetched from origin is borrowed
            // (through `alternates`, absolute, then repacked into the new
            // repository, which then stands alone).
            Some(stale) => {
                let fetched = stale
                    .repo
                    .rev_parse(BOOTSTRAP_REF)?
                    .ok_or_else(not_initialised)?;
                let objects = utf8(&fs::canonicalize(stale.repo.git_dir().join("objects"))?)?;
                let alternates = temp.join("objects").join("info").join("alternates");
                fs::create_dir_all(alternates.parent().expect("a file in objects/info"))?;
                fs::write(&alternates, format!("{objects}\n"))?;
                probe.update_ref(LOCAL_MAIN, &fetched, None)?;
                probe.repack_all()?;
                fs::remove_file(&alternates)?;
            }
            None => probe.fetch(url, &[&format!("+{MAIN}:{LOCAL_MAIN}")], false)?,
        }
        let main = probe.rev_parse(LOCAL_MAIN)?.ok_or_else(not_initialised)?;
        let config = Store::new(probe.clone())?.repo_config(&main)?;
        let lock = take_lock(&lock_path(root, &config.repo_id))?;
        let path = root.join(format!("{}.git", config.repo_id));
        let existing = if path.exists() {
            Some(git.open(&path)?)
        } else {
            None
        };
        let repo = match existing {
            Some(repo) if repo.object_format()? == format => {
                repo.fetch(
                    &utf8(&temp)?,
                    &[&format!("+{LOCAL_MAIN}:{LOCAL_MAIN}")],
                    false,
                )?;
                repo
            }
            replaced => {
                if replaced.is_some() {
                    fs::remove_dir_all(&path)?;
                }
                fs::rename(&temp, &path)?;
                git.open(&path)?
            }
        };
        opened(&repo, url);
        Ok((Cache { repo, _lock: lock }, main))
    })();
    if temp.exists() {
        let _ = fs::remove_dir_all(&temp);
    }
    if let Some(stale) = stale {
        let _ = stale.repo.delete_ref(BOOTSTRAP_REF);
    }
    let _ = fs::remove_file(&temp_lock);
    result
}

/// The repo ids of the caches under `root` recorded for `url`.
fn caches_for(root: &Path, url: &str) -> Vec<RepoId> {
    fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let id = RepoId::new(name.strip_suffix(".git")?).ok()?;
                    recorded_urls(&entry.path())
                        .iter()
                        .any(|known| known == url)
                        .then_some(id)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Where a cache checked for a clone fetches origin's `main` (its own
/// `main` is only moved once the repo id matches), and where a stale one
/// keeps it for the bootstrap to borrow.
const BOOTSTRAP_REF: &str = "refs/rmu/bootstrap";

/// What [`known_cache`] found.
enum Known {
    /// A cache recorded for the URL, locked and fetched up to date.
    Found(Cache, Oid),
    /// A recorded cache whose origin now holds another repository
    /// (`repo_id`): it no longer records the URL, its `main` is as it
    /// was, and what it fetched is at [`BOOTSTRAP_REF`] (locked, for the
    /// bootstrap to borrow).
    Stale {
        cache: Cache,
        repo_id: RepoId,
    },
    None,
}

/// The caches recorded for `url`, in turn, until one is found:
/// - one of another format than origin's advertisement is skipped, never
///   re-created (it may hold the last copy of a replaced origin's
///   history);
/// - the first other one fetches origin's `main` into [`BOOTSTRAP_REF`]
///   and reads its repo id: its own means found (`main` moves there),
///   another makes it stale;
/// - once that repo id is known, a later cache with it is found (fetched
///   into from origin, incrementally), and one with another id only
///   forgets the URL, without a fetch.
fn known_cache(git: &Git, root: &Path, url: &str) -> Result<Known> {
    caches_for(root, url)
        .into_iter()
        .try_fold(Known::None, |known, repo_id| -> Result<Known> {
            let path = root.join(format!("{repo_id}.git"));
            match known {
                Known::Found(..) => Ok(known),
                Known::Stale {
                    cache: stale,
                    repo_id: fetched,
                } if fetched != repo_id => {
                    let _lock = take_lock(&lock_path(root, &repo_id))?;
                    forget_url(&path, url);
                    Ok(Known::Stale {
                        cache: stale,
                        repo_id: fetched,
                    })
                }
                Known::Stale { cache: stale, .. } => {
                    let found = recorded_and_matching(git, root, &repo_id, url)?;
                    match found {
                        Some(found) => {
                            let _ = stale.repo.delete_ref(BOOTSTRAP_REF);
                            Ok(found)
                        }
                        None => Ok(Known::Stale {
                            cache: stale,
                            repo_id,
                        }),
                    }
                }
                Known::None => checked(git, root, &repo_id, url),
            }
        })
}

/// The first check of a recorded cache: its format is origin's, and the
/// repo id of origin's `main`, fetched into [`BOOTSTRAP_REF`], is its own
/// (found) or another (stale). Another format: skipped.
fn checked(git: &Git, root: &Path, repo_id: &RepoId, url: &str) -> Result<Known> {
    let Some((cache, _)) = open_recorded(git, root, repo_id, url)? else {
        return Ok(Known::None);
    };
    cache
        .repo
        .fetch(url, &[&format!("+{MAIN}:{BOOTSTRAP_REF}")], false)?;
    let main = cache
        .repo
        .rev_parse(BOOTSTRAP_REF)?
        .ok_or_else(|| CycleError::NotInitialised {
            url: url.to_string(),
        })?;
    let fetched = Store::new(cache.repo.clone())?.repo_config(&main)?.repo_id;
    if fetched == *repo_id {
        cache.repo.update_ref(LOCAL_MAIN, &main, None)?;
        cache.repo.delete_ref(BOOTSTRAP_REF)?;
        return Ok(Known::Found(cache, main));
    }
    forget_url(cache.repo.git_dir(), url);
    Ok(Known::Stale {
        cache,
        repo_id: fetched,
    })
}

/// A recorded cache with the repo id origin's `main` was just found to
/// have: fetched into from origin (incremental), found.
fn recorded_and_matching(
    git: &Git,
    root: &Path,
    repo_id: &RepoId,
    url: &str,
) -> Result<Option<Known>> {
    let Some((cache, refs)) = open_recorded(git, root, repo_id, url)? else {
        return Ok(None);
    };
    let main = fetch(&cache.repo, url, None, Some(refs))?
        .main
        .ok_or_else(|| CycleError::NotInitialised {
            url: url.to_string(),
        })?;
    Ok(Some(Known::Found(cache, main)))
}

/// A recorded cache, locked, if it opens and has the format origin
/// advertises (with origin's advertisement); `None` otherwise.
fn open_recorded(
    git: &Git,
    root: &Path,
    repo_id: &RepoId,
    url: &str,
) -> Result<Option<(Cache, Vec<git::RemoteRef>)>> {
    let lock = take_lock(&lock_path(root, repo_id))?;
    let Ok(repo) = git.open(&root.join(format!("{repo_id}.git"))) else {
        return Ok(None);
    };
    let refs = repo.ls_remote(url)?;
    let Some(advertised) = refs.iter().find(|reference| reference.name == MAIN) else {
        return Err(CycleError::NotInitialised {
            url: url.to_string(),
        }
        .into());
    };
    if advertised.oid.object_format() != repo.object_format()? {
        return Ok(None);
    }
    opened(&repo, url);
    Ok(Some((Cache { repo, _lock: lock }, refs)))
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

/// Cache bounds: commits by count, trees by entries, JSON by an estimate
/// of its parsed size ([`PARSED_JSON_FACTOR`] times its source bytes).
const COMMIT_CACHE: usize = 16 * 1024;
const TREE_CACHE: usize = 256 * 1024;
const JSON_CACHE: usize = 64 * 1024 * 1024;

/// A parsed `serde_json::Value` takes about this many times its source
/// bytes (measured: a 700-page PDF's 168 KB `.content` parses into about
/// 2.7 MB, a node per small register).
const PARSED_JSON_FACTOR: usize = 16;

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

    /// A commit object as stored (headers included), uncached: `log`
    /// reads each commit's author line once.
    pub fn raw_commit(&self, oid: &Oid) -> Result<Vec<u8>> {
        self.object(oid, ObjectKind::Commit)
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
        // Each commit's `docs` tree, then the `docs/<uuid>` entries that
        // differ from its first parent's: one entry per changed document,
        // however many files changed in it.
        let names: Vec<String> = commits
            .iter()
            .map(|entry| format!("{}:docs", entry.commit))
            .collect();
        let docs: HashMap<&Oid, Oid> = commits
            .iter()
            .zip(self.repo.resolve(&names)?)
            .filter_map(|(entry, found)| match found {
                Some((tree, ObjectKind::Tree)) => Some((&entry.commit, tree)),
                _ => None,
            })
            .collect();
        let empty = self.format.empty_tree();
        let docs_of = |commit: Option<&Oid>| {
            commit
                .and_then(|commit| docs.get(commit))
                .unwrap_or(&empty)
                .clone()
        };
        let pairs: Vec<(usize, (Oid, Oid))> = commits
            .iter()
            .enumerate()
            .map(|(at, entry)| {
                (
                    at,
                    (docs_of(entry.parents.first()), docs_of(Some(&entry.commit))),
                )
            })
            .filter(|(_, (old, new))| old != new)
            .collect();
        let diffs = self.repo.tree_diffs(
            &pairs
                .iter()
                .map(|(_, pair)| pair.clone())
                .collect::<Vec<_>>(),
        )?;
        let changes: Vec<CommitChanges> = pairs
            .iter()
            .zip(diffs)
            .map(|((at, _), changes)| CommitChanges {
                commit: commits[*at].commit.clone(),
                changes: changes
                    .into_iter()
                    .map(|change| RawChange {
                        path: [&b"docs/"[..], &change.path].concat(),
                        ..change
                    })
                    .collect(),
            })
            .collect();
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
        self.json.borrow_mut().insert(
            id.clone(),
            parsed.clone(),
            bytes.len() * PARSED_JSON_FACTOR + 1,
        );
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
        let git = hermetic(&scratch);
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
        // The side's version of d1 is on no first-parent commit.
        let side = commit(
            &repo,
            &[&root],
            &[meta(&d1, "1x"), meta(&d2, "2"), meta(&d3, "3")],
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
            // The merge's history: those stretches' present versions,
            // never a version only a merged side line has.
            let rules = MirrorRules::latest();
            let history = StoreHistory {
                store: &store,
                rules,
                tip: &tip,
            };
            let present: Vec<DocVersion> = stretches
                .iter()
                .map(|subtree| store.doc_version(rules, d, subtree).unwrap())
                .filter(DocVersion::is_present)
                .collect();
            assert_eq!(history.present_versions(d).unwrap(), present, "{d}");
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

    /// This process's peak resident set size in KiB (Linux), after
    /// `reset` cleared it.
    fn peak_rss_kib(reset: bool) -> Option<u64> {
        if reset {
            let _ = fs::write("/proc/self/clear_refs", "5");
        }
        fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
    }

    /// Building the index of a history whose commits change every file
    /// of a big library (2000 notebooks of 100 pages: 204,000 files):
    /// time and memory must follow documents, not files.
    /// `cargo test --release -p libremarkable-utils --lib -- --ignored
    /// --nocapture index_of_a_history_with_many_files`.
    #[test]
    #[ignore = "measurement; run in release with --ignored --nocapture"]
    fn index_of_a_history_with_many_files() {
        let scratch = Scratch::new("history-files");
        let git = hermetic(&scratch);
        let repo = git
            .init_bare(&scratch.path().join("r.git"), ObjectFormat::Sha1)
            .unwrap();
        let who = Signature {
            name: "Test".to_string(),
            email: "test@example.invalid".to_string(),
            time: 1_800_000_000,
            offset_minutes: 0,
        };
        let library = |parent: Option<&Oid>, ink: &str| -> Oid {
            let mut writer = repo
                .commit_writer(&crate::progress::NoProgress, None)
                .unwrap();
            let page = writer
                .write_blob("page", ink.len() as u64, &mut ink.as_bytes())
                .unwrap();
            let json = writer.write_blob("json", 2, &mut &b"{}"[..]).unwrap();
            (0..2000u32).for_each(|n| {
                let d = doc(n);
                [("metadata", &json), ("content", &json)]
                    .into_iter()
                    .for_each(|(name, blob)| {
                        writer
                            .add_existing(format!("docs/{d}/{name}"), FileMode::Regular, blob)
                            .unwrap();
                    });
                (0..100).for_each(|p| {
                    writer
                        .add_existing(format!("docs/{d}/pages/p{p}.rm"), FileMode::Regular, &page)
                        .unwrap();
                });
            });
            writer
                .commit(&CommitMeta {
                    parents: parent.into_iter().cloned().collect(),
                    author: who.clone(),
                    committer: who.clone(),
                    message: "library\n".to_string(),
                })
                .unwrap()
        };
        let first = library(None, "ink 1");
        let tip = library(Some(&first), "ink 2");
        let store = Store::new(repo).unwrap();
        let before = peak_rss_kib(true);
        let started = std::time::Instant::now();
        let index = store.history(&tip).unwrap();
        let took = started.elapsed();
        let grew = peak_rss_kib(false)
            .zip(before)
            .map(|(after, before)| after.saturating_sub(before));
        eprintln!(
            "index of 2 commits x 204,000 files: {took:.2?}, peak RSS +{} KiB",
            grew.map_or("?".to_string(), |kib| kib.to_string())
        );
        assert_eq!(index.versions(&doc(7)).len(), 2);
    }

    fn hermetic(scratch: &Scratch) -> Git {
        Git::new()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_COUNT", "0")
            .env("GIT_DEFAULT_HASH", "sha1")
            .env("HOME", scratch.path())
    }

    /// A cache that holds refs is never deleted: one of another format
    /// than origin's is refused (its `main` kept), and one that does not
    /// open is an error, its directory kept.
    #[test]
    fn a_cache_with_refs_is_never_deleted() {
        let scratch = Scratch::new("cache-kept");
        let git = hermetic(&scratch);
        let root = scratch.path().join("caches");
        let id = RepoId::random();
        let origin = scratch.path().join("origin.git");
        let url = origin_of(&git, &origin, ObjectFormat::Sha1, &id);
        let (cache, refs) = open_cache(&git, &root, &id, &url).unwrap();
        let main = fetch(&cache.repo, &url, None, Some(refs)).unwrap().main;
        assert!(main.is_some());
        drop(cache);
        fs::remove_dir_all(&origin).unwrap();
        origin_of(&git, &origin, ObjectFormat::Sha256, &RepoId::random());
        let refused = open_cache(&git, &root, &id, &url);
        assert!(
            matches!(&refused, Err(Error::HubCycle(CycleError::CacheFormatMismatch { cache, .. }))
                if *cache == root.join(format!("{id}.git"))),
            "{:?}",
            refused.as_ref().err()
        );
        let kept = git.open(&root.join(format!("{id}.git"))).unwrap();
        assert_eq!(kept.rev_parse(LOCAL_MAIN).unwrap(), main);
        // Not a repository (or git failing to open it): an error, kept.
        let broken = RepoId::random();
        let dir = root.join(format!("{broken}.git"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("precious"), b"x").unwrap();
        assert!(open_cache(&git, &root, &broken, &url).is_err());
        assert!(dir.join("precious").exists());
    }

    /// A bootstrap ref an interrupted clone left (it pins a whole
    /// library) is dropped the next time the cache is opened.
    #[test]
    fn a_leftover_bootstrap_ref_is_dropped_on_open() {
        let scratch = Scratch::new("bootstrap-ref");
        let git = hermetic(&scratch);
        let root = scratch.path().join("caches");
        let id = RepoId::random();
        let url = origin_of(&git, &scratch.path().join("o.git"), ObjectFormat::Sha1, &id);
        let (cache, refs) = open_cache(&git, &root, &id, &url).unwrap();
        let main = fetch(&cache.repo, &url, None, Some(refs))
            .unwrap()
            .main
            .unwrap();
        cache.repo.update_ref(BOOTSTRAP_REF, &main, None).unwrap();
        drop(cache);
        let (cache, _) = open_cache(&git, &root, &id, &url).unwrap();
        assert_eq!(cache.repo.rev_parse(BOOTSTRAP_REF).unwrap(), None);
    }

    #[test]
    fn origin_url_records_are_atomic_and_never_empty() {
        let scratch = Scratch::new("url-records");
        let dir = scratch.path();
        write_urls(dir, &["a".to_string(), "b".to_string()]);
        assert_eq!(recorded_urls(dir), ["a", "b"]);
        assert!(!dir.join(format!("{ORIGIN_FILE}.tmp")).exists());
        forget_url(dir, "a");
        forget_url(dir, "b");
        assert!(!dir.join(ORIGIN_FILE).exists());
        // Blank lines (an older record) are not URLs.
        fs::write(dir.join(ORIGIN_FILE), "\nx\n\n").unwrap();
        assert_eq!(recorded_urls(dir), ["x"]);
    }

    /// An origin of `format` whose `main` holds `.rmu/hub.json` for
    /// `repo_id`.
    fn origin_of(git: &Git, path: &Path, format: ObjectFormat, repo_id: &RepoId) -> String {
        let repo = git.init_bare(path, format).unwrap();
        let config = model::RepoConfig::new(repo_id.clone()).to_json();
        let main = commit(
            &repo,
            &[],
            &[(REPO_CONFIG_PATH.to_string(), config.as_str())],
        );
        repo.update_ref(MAIN, &main, None).unwrap();
        path.to_str().unwrap().to_string()
    }

    /// A cache keeps its object format: a SHA-256 cache is neither deleted
    /// nor re-created by opening it, by a clone from its origin, or by a
    /// clone from a URL that now serves a SHA-1 repository (skipped, not
    /// re-created, as is a SHA-1 cache whose URL serves SHA-256 now).
    #[test]
    fn caches_keep_their_object_format() {
        let scratch = Scratch::new("cache-formats");
        let git = hermetic(&scratch);
        let root = scratch.path().join("caches");
        let (id256, id1) = (RepoId::random(), RepoId::random());
        let url = origin_of(
            &git,
            &scratch.path().join("o256.git"),
            ObjectFormat::Sha256,
            &id256,
        );
        let marker = |id: &RepoId| root.join(format!("{id}.git/marker"));
        let (cache, _) = open_cache(&git, &root, &id256, &url).unwrap();
        assert_eq!(cache.repo.object_format().unwrap(), ObjectFormat::Sha256);
        drop(cache);
        fs::write(marker(&id256), b"kept").unwrap();
        let (cache, _) = open_cache(&git, &root, &id256, &url).unwrap();
        drop(cache);
        assert!(
            marker(&id256).exists(),
            "an open re-created the SHA-256 cache"
        );
        let (cache, _) = bootstrap_cache(&git, &root, &url).unwrap();
        drop(cache);
        assert!(
            marker(&id256).exists(),
            "a clone re-created the SHA-256 cache"
        );
        // The URL now serves a SHA-1 repository: the SHA-256 cache recorded
        // for it is skipped, and kept.
        fs::remove_dir_all(scratch.path().join("o256.git")).unwrap();
        origin_of(
            &git,
            &scratch.path().join("o256.git"),
            ObjectFormat::Sha1,
            &id1,
        );
        let (cache, _) = bootstrap_cache(&git, &root, &url).unwrap();
        assert_eq!(cache.repo.git_dir(), root.join(format!("{id1}.git")));
        drop(cache);
        assert!(marker(&id256).exists());
        // And back: the SHA-1 cache, recorded for the URL, is skipped and
        // kept when it serves SHA-256 again.
        fs::write(marker(&id1), b"kept").unwrap();
        fs::remove_dir_all(scratch.path().join("o256.git")).unwrap();
        origin_of(
            &git,
            &scratch.path().join("o256.git"),
            ObjectFormat::Sha256,
            &id256,
        );
        let (cache, _) = bootstrap_cache(&git, &root, &url).unwrap();
        assert_eq!(cache.repo.git_dir(), root.join(format!("{id256}.git")));
        drop(cache);
        assert!(marker(&id1).exists());
        assert!(marker(&id256).exists());
    }

    #[test]
    fn a_cache_records_a_bounded_set_of_urls() {
        let urls = (0..10).fold(Vec::new(), |urls, n| with_url(urls, &format!("u{n}")));
        let newest: Vec<String> = (2..10).rev().map(|n| format!("u{n}")).collect();
        assert_eq!(urls, newest);
        // A known URL moves to the front, without a repeat.
        let urls = with_url(urls, "u5");
        assert_eq!(urls[0], "u5");
        assert_eq!(urls.iter().filter(|url| *url == "u5").count(), 1);
        assert_eq!(urls.len(), ORIGIN_URLS);
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
