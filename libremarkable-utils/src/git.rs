//! Plumbing over the system `git` binary, for git-backed storage,
//! history and transport (`rmu hub`, see `docs/hub-design.md`).
//!
//! As with [`crate::ssh`], git runs as a subprocess rather than through a
//! linked library: no native dependencies, and the user's git
//! configuration, SSH setup, `url.*.insteadOf` rules and credential
//! helpers apply unchanged. Every invocation uses explicit arguments
//! (never a shell string), an explicit `--git-dir` (bare repositories
//! only, no working tree), and an explicit environment:
//!
//! - Variables that would redirect git to another repository (`GIT_DIR`,
//!   `GIT_INDEX_FILE`, ... as set inside git hooks) are removed.
//! - `gc.auto=0` and `maintenance.auto=false`: no background repacking
//!   while a tablet is stopped (only [`Repo::maintenance_auto`] runs
//!   without them); `core.hooksPath=/dev/null`: hooks from a user's
//!   template directory never run on rmu's own fetches, ref updates and
//!   pushes; `core.logAllRefUpdates=false`: reflog entries would need an
//!   auto-detected identity (a hostname); `push.gpgSign=false`: nothing
//!   rmu writes is signed. These are applied to every command —
//!   read-only commands ignore them.
//! - `GIT_TERMINAL_PROMPT=0` unless the caller opts in to interactivity.
//!
//! Commits are written with `git fast-import` ([`CommitWriter`]), whose
//! stream carries author and committer explicitly; identity is never
//! auto-detected, and nothing is signed. Blobs are streamed, so large
//! payloads need not be held in memory. No ref is updated unless the
//! caller asks ([`Repo::update_ref`], [`Repo::push`]).
//!
//! Output parsers are pure functions with unit tests; paths are raw
//! bytes throughout, since git allows any byte except NUL.
//!
//! Requires git 2.30 or newer (`rev-parse --end-of-options`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::str::FromStr;
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::progress::Progress;

/// `-c` settings for every invocation (see the module docs).
const CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.logAllRefUpdates=false",
    "push.gpgSign=false",
];

/// `-c` settings for every invocation but [`Repo::maintenance_auto`].
const NO_AUTO_MAINTENANCE: &[&str] = &["gc.auto=0", "maintenance.auto=false"];

/// Keeps [`Repo::maintenance_auto`] in the foreground.
const NO_DETACH: &[&str] = &["gc.autoDetach=false", "maintenance.autoDetach=false"];

/// Inherited variables that would point git at a different repository
/// or object store than the one rmu names (e.g. when run from a hook).
const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_QUARANTINE_PATH",
    "GIT_SHALLOW_FILE",
    "GIT_REPLACE_REF_BASE",
];

/// The ref fast-import builds commits on. It is reset before the stream
/// ends, so fast-import never writes it.
const SCRATCH_REF: &str = "refs/rmu/fast-import/scratch";

/// An object id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
///
/// Uppercase is rejected rather than normalised: git always prints
/// lowercase, and so does every state file rmu writes, so uppercase
/// input means a hand-edited or corrupt file. Shared by this module and
/// [`crate::hub::model`]; (de)serialised as its string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Oid(String);

impl Oid {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The hash function that produced this id, from its length.
    pub fn object_format(&self) -> ObjectFormat {
        if self.0.len() == 64 {
            ObjectFormat::Sha256
        } else {
            ObjectFormat::Sha1
        }
    }
}

/// A repository's hash function. Fetching requires both sides to agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    /// The id of the empty tree, which git treats as always present.
    pub fn empty_tree(self) -> Oid {
        Oid(match self {
            Self::Sha1 => "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string(),
            Self::Sha256 => {
                "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321".to_string()
            }
        })
    }
}

impl FromStr for Oid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let valid =
            matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        valid
            .then(|| Oid(s.to_string()))
            .ok_or_else(|| Error::InvalidObjectId(s.to_string()))
    }
}

impl TryFrom<String> for Oid {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Oid> for String {
    fn from(oid: Oid) -> Self {
        oid.0
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Oid {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Object type, as printed by `ls-tree` and `cat-file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl FromStr for ObjectKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "blob" => Ok(Self::Blob),
            "tree" => Ok(Self::Tree),
            "commit" => Ok(Self::Commit),
            "tag" => Ok(Self::Tag),
            _ => Err(Error::InvalidGitInput(format!("unknown object type {s:?}"))),
        }
    }
}

/// Mode of a non-tree entry: a file written by [`CommitWriter`], or a
/// gitlink (a submodule commit), which can only be carried forward
/// ([`CommitWriter::add_existing`]). Shared with [`crate::hub::model`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileMode {
    Regular,
    Executable,
    Symlink,
    Gitlink,
}

impl FileMode {
    /// Every mode git writes for a non-tree entry.
    pub const ALL: [FileMode; 4] = [
        Self::Regular,
        Self::Executable,
        Self::Symlink,
        Self::Gitlink,
    ];

    /// The mode as git writes it in trees.
    pub fn bits(self) -> u32 {
        match self {
            Self::Regular => 0o100644,
            Self::Executable => 0o100755,
            Self::Symlink => 0o120000,
            Self::Gitlink => 0o160000,
        }
    }

    /// The mode for a [`TreeEntry::mode`], unless it is a tree (or a
    /// mode git does not write).
    pub fn from_bits(mode: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.bits() == mode)
    }

    /// Regular or executable: content that can be written to a file.
    pub fn is_file(self) -> bool {
        matches!(self, Self::Regular | Self::Executable)
    }
}

/// One entry of [`Repo::ls_tree_recursive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Mode bits as stored in the tree (e.g. `0o100644`, `0o040000`).
    pub mode: u32,
    pub kind: ObjectKind,
    pub oid: Oid,
    /// Blob size; `None` for trees and submodule commits.
    pub size: Option<u64>,
    /// Full path from the root of the listed tree; any byte but NUL.
    pub path: Vec<u8>,
}

/// A commit and its parents, in order ([`Repo::rev_list_parents`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitParents {
    pub commit: Oid,
    pub parents: Vec<Oid>,
}

/// One entry of a raw diff ([`Repo::tree_diffs`]). An absent side has
/// mode 0 and the all-zero id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChange {
    pub old_mode: u32,
    pub new_mode: u32,
    pub old: Oid,
    pub new: Oid,
    /// `A`, `D`, `M`, `T`, ... (any score dropped).
    pub status: u8,
    pub path: Vec<u8>,
}

/// One pair of trees and the entries that differ between them
/// ([`parse_tree_diffs`]).
pub type TreeDiff = ((Oid, Oid), Vec<RawChange>);

/// What one commit changed against its first parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitChanges {
    pub commit: Oid,
    pub changes: Vec<RawChange>,
}

/// One ref advertised by [`Repo::ls_remote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    /// Full ref name (`HEAD`, `refs/heads/main`, `refs/tags/v1^{}`, ...).
    pub name: String,
    pub oid: Oid,
}

/// Expected current value of a ref for a compare-and-swap update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// The ref must not exist.
    Absent,
    /// The ref must point at exactly this object.
    At(Oid),
}

impl Expected {
    /// The value as `update-ref` and `--force-with-lease` take it: the
    /// empty string means "must not exist".
    fn arg(&self) -> &str {
        match self {
            Self::Absent => "",
            Self::At(oid) => oid.as_str(),
        }
    }
}

/// Author or committer of a commit. Always given explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
    /// Offset from UTC in minutes (e.g. `60` for `+0100`).
    pub offset_minutes: i16,
}

/// Commit metadata for [`CommitWriter::commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    /// Parent commits, first parent first; empty for a root commit.
    /// Repeats are dropped (first occurrence kept), so a snapshot whose
    /// base equals the previous device ref gets one parent.
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    /// The full message, written verbatim.
    pub message: String,
}

/// How to run `git`: interactivity and extra environment. Cheap to clone.
///
/// Non-interactive by default, so a forgotten setting fails instead of
/// hanging on a prompt.
#[derive(Debug, Clone, Default)]
pub struct Git {
    interactive: bool,
    env: Vec<(OsString, OsString)>,
}

impl Git {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether git may prompt on the terminal for credentials (default
    /// `false`: prompts fail instead of hanging, `GIT_TERMINAL_PROMPT=0`).
    /// Opt in only when there is a tty to prompt on.
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// Set an environment variable on every git process (e.g.
    /// `GIT_SSH_COMMAND`; tests use it for a hermetic configuration).
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    /// Create a bare repository at `path` with `HEAD` on `main`, no
    /// template files, and the given object format (never the user's
    /// `init.defaultObjectFormat` or `GIT_DEFAULT_HASH`). Re-initialising
    /// an existing repository of the same format is harmless.
    ///
    /// A cache must have its origin's format to fetch from it.
    /// `ls_remote` works across formats, so compare
    /// [`Oid::object_format`] of origin's refs with
    /// [`Repo::object_format`] and re-create the cache on a mismatch. An
    /// empty origin advertises nothing; create new origins as
    /// [`ObjectFormat::Sha1`], which every forge accepts.
    pub fn init_bare(&self, path: &Path, format: ObjectFormat) -> Result<Repo> {
        let mut cmd = self.command(None, "init");
        cmd.args(["--bare", "--quiet", "--template=", "--initial-branch=main"])
            .arg(format!("--object-format={}", format.name()))
            .arg("--end-of-options")
            .arg(path);
        run("init", &mut cmd)?;
        Ok(self.repo(path))
    }

    /// Open an existing bare repository.
    pub fn open(&self, path: &Path) -> Result<Repo> {
        let repo = self.repo(path);
        let mut cmd = repo.command("rev-parse");
        cmd.arg("--is-bare-repository");
        match run("rev-parse", &mut cmd)?.as_slice() {
            b"true\n" => Ok(repo),
            _ => Err(Error::NotABareRepository(path.to_path_buf())),
        }
    }

    fn repo(&self, path: &Path) -> Repo {
        Repo {
            git: self.clone(),
            git_dir: path.to_path_buf(),
        }
    }

    fn command(&self, git_dir: Option<&Path>, subcommand: &str) -> Command {
        self.command_with(git_dir, subcommand, &[NO_AUTO_MAINTENANCE, CONFIG])
    }

    fn command_with(
        &self,
        git_dir: Option<&Path>,
        subcommand: &str,
        config: &[&[&str]],
    ) -> Command {
        let mut cmd = Command::new("git");
        SCRUBBED_ENV.iter().for_each(|key| {
            cmd.env_remove(key);
        });
        if !self.interactive {
            cmd.env("GIT_TERMINAL_PROMPT", "0");
        }
        self.env.iter().for_each(|(key, value)| {
            cmd.env(key, value);
        });
        if let Some(dir) = git_dir {
            cmd.arg("--git-dir").arg(dir);
        }
        config.iter().flat_map(|c| c.iter()).for_each(|setting| {
            cmd.arg("-c").arg(setting);
        });
        cmd.arg(subcommand);
        cmd
    }
}

/// A bare repository (e.g. the hub's local cache).
#[derive(Debug, Clone)]
pub struct Repo {
    git: Git,
    git_dir: PathBuf,
}

impl Repo {
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    fn command(&self, subcommand: &str) -> Command {
        self.git.command(Some(&self.git_dir), subcommand)
    }

    /// Refs advertised by `url` (also checks reachability and auth).
    pub fn ls_remote(&self, url: &str) -> Result<Vec<RemoteRef>> {
        let mut cmd = self.command("ls-remote");
        cmd.arg("--end-of-options").arg(url);
        parse_ls_remote(&run("ls-remote", &mut cmd)?)
    }

    /// Fetch `refspecs` (e.g. `+refs/heads/main:refs/remotes/origin/main`)
    /// from `url`, without tags. Local refs under a refspec's destination
    /// that the remote no longer has are deleted only with `prune`
    /// (a user's `fetch.prune` never applies).
    pub fn fetch(&self, url: &str, refspecs: &[&str], prune: bool) -> Result<()> {
        let mut cmd = self.command("fetch");
        cmd.args(["--quiet", "--no-tags"])
            .arg(if prune { "--prune" } else { "--no-prune" })
            .arg("--end-of-options")
            .arg(url)
            .args(refspecs);
        run("fetch", &mut cmd).map(drop)
    }

    /// Remove the `.keep` files `git fast-import` leaves on the packs its
    /// checkpoints wrote when it is killed (their content is exactly
    /// `fast-import`): git never repacks or prunes a kept pack. Call only
    /// while no fast-import runs on this repository. Returns how many.
    pub fn remove_fast_import_keeps(&self) -> Result<usize> {
        let removed = keep_files(&self.git_dir.join("objects").join("pack"))
            .into_iter()
            .filter(|keep| {
                fs::read(keep).is_ok_and(|content| content.trim_ascii() == b"fast-import")
            })
            .map(|keep| fs::remove_file(keep).map_err(Error::from))
            .collect::<Result<Vec<()>>>()?;
        Ok(removed.len())
    }

    /// Push commit `src` to the full remote ref `dst`.
    ///
    /// Without a lease, the remote accepts only a fast-forward (or a new
    /// ref). With one, the update is a compare-and-swap: it may rewind,
    /// but only if `dst` currently matches. Every way of losing to a
    /// concurrent writer is [`Error::PushRejected`] — fetch, recompute,
    /// retry: the client-side check (non-fast-forward, stale lease) and
    /// the remote's own compare-and-swap (its ref moved between the
    /// advertisement and the update). Anything else the remote refuses
    /// (hooks, size limits) is [`Error::PushFailed`] with the remote's
    /// message; failures before any ref status (auth, network) are
    /// [`Error::Git`]. A status the request cannot have asked for (a
    /// deletion, or a forced update without a lease) is an error too.
    pub fn push(&self, url: &str, src: &Oid, dst: &str, lease: Option<&Expected>) -> Result<()> {
        validate_ref_name(dst)?;
        let mut cmd = self.command("push");
        cmd.args(["--porcelain", "--no-verify"]);
        if let Some(expected) = lease {
            cmd.arg(format!("--force-with-lease={dst}:{}", expected.arg()));
        }
        cmd.arg("--end-of-options")
            .arg(url)
            .arg(format!("{src}:{dst}"));
        let output = output("push", &mut cmd)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        match push_outcome(
            dst,
            lease.is_some(),
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
            &stderr,
        ) {
            PushOutcome::Updated => Ok(()),
            PushOutcome::Rejected(reason) => Err(Error::PushRejected {
                reference: dst.to_string(),
                reason,
            }),
            PushOutcome::Refused(reason) => Err(Error::PushFailed {
                reference: dst.to_string(),
                reason,
                stderr: stderr.trim().to_string(),
            }),
            PushOutcome::Unexpected(detail) => Err(output_error("push", detail)),
            PushOutcome::Failed => Err(failure("push", &output)),
        }
    }

    /// Delete the full remote ref `dst`, only if it is at `expected`
    /// (`--force-with-lease`): a compare-and-swap like [`Repo::push`]'s,
    /// with the same errors ([`Error::PushRejected`] when it moved).
    pub fn delete_remote_ref(&self, url: &str, dst: &str, expected: &Oid) -> Result<()> {
        validate_ref_name(dst)?;
        let mut cmd = self.command("push");
        cmd.args(["--porcelain", "--no-verify"])
            .arg(format!("--force-with-lease={dst}:{expected}"))
            .arg("--end-of-options")
            .arg(url)
            .arg(format!(":{dst}"));
        let output = output("push", &mut cmd)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let status = parse_push_porcelain(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .find(|status| status.to == dst);
        match status {
            Some(status) if status.flag == '-' && output.status.success() => Ok(()),
            Some(status) if status.flag == '!' => {
                let reason = status.reason.unwrap_or_else(|| status.summary.clone());
                match status.summary.as_str() {
                    "[rejected]" => Err(Error::PushRejected {
                        reference: dst.to_string(),
                        reason,
                    }),
                    summary => Err(Error::PushFailed {
                        reference: dst.to_string(),
                        reason: format!("{summary} ({reason})"),
                        stderr: stderr.trim().to_string(),
                    }),
                }
            }
            Some(status) => Err(output_error(
                "push",
                format!("unexpected status {:?} deleting {dst}", status.flag),
            )),
            None => Err(failure("push", &output)),
        }
    }

    /// Resolve `rev` like `git rev-parse --verify`; `None` if it does not
    /// resolve. A full hex id resolves even if the object is absent:
    /// append `^{commit}` (or `^{object}`) to require that it exists.
    pub fn rev_parse(&self, rev: &str) -> Result<Option<Oid>> {
        let mut cmd = self.command("rev-parse");
        cmd.args(["--verify", "--quiet", "--end-of-options", rev]);
        let output = output("rev-parse", &mut cmd)?;
        match output.status.code() {
            Some(0) => parse_oid_line("rev-parse", &output.stdout).map(Some),
            Some(1) if output.stdout.is_empty() => Ok(None),
            _ => Err(failure("rev-parse", &output)),
        }
    }

    /// Whether `ancestor` is reachable from `descendant` (a commit is
    /// its own ancestor).
    ///
    /// A missing object — including a missing `ancestor` — is an error,
    /// not `false`. A commit that may never have reached this repository
    /// (e.g. a journaled merge whose push was lost, checked from another
    /// computer or a fresh cache) must be checked with
    /// `rev_parse("<oid>^{commit}")` first; `None` there means it is not
    /// in the fetched history.
    pub fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool> {
        let mut cmd = self.command("merge-base");
        cmd.args([
            "--is-ancestor",
            "--end-of-options",
            ancestor.as_str(),
            descendant.as_str(),
        ]);
        let output = output("merge-base", &mut cmd)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failure("merge-base", &output)),
        }
    }

    /// Pack every object reachable from the repository's refs into one
    /// pack, objects borrowed from its alternates included (`repack -a
    /// -d`, without `-l`): afterwards it no longer needs them.
    pub fn repack_all(&self) -> Result<()> {
        let mut cmd = self.command("repack");
        cmd.args(["-a", "-d", "-q"]);
        run("repack", &mut cmd).map(drop)
    }

    /// Delete the local ref `name` (absent: nothing to do).
    pub fn delete_ref(&self, name: &str) -> Result<()> {
        validate_ref_name(name)?;
        let mut cmd = self.command("update-ref");
        cmd.args(["-d", "--end-of-options", name]);
        run("update-ref", &mut cmd).map(drop)
    }

    /// Whether the repository has any ref at all.
    pub fn has_refs(&self) -> Result<bool> {
        let mut cmd = self.command("for-each-ref");
        cmd.args(["--count=1", "--format=%(refname)"]);
        Ok(!run("for-each-ref", &mut cmd)?.is_empty())
    }

    /// Point local ref `name` (a full `refs/...` name) at `new`; with
    /// `old`, only if the ref currently has that value (atomic
    /// compare-and-swap).
    pub fn update_ref(&self, name: &str, new: &Oid, old: Option<&Expected>) -> Result<()> {
        validate_ref_name(name)?;
        let mut cmd = self.command("update-ref");
        cmd.args(["--end-of-options", name, new.as_str()]);
        if let Some(old) = old {
            cmd.arg(old.arg());
        }
        run("update-ref", &mut cmd).map(drop)
    }

    /// The repository's object format.
    pub fn object_format(&self) -> Result<ObjectFormat> {
        let mut cmd = self.command("rev-parse");
        cmd.arg("--show-object-format");
        match run("rev-parse", &mut cmd)?.as_slice() {
            b"sha1\n" => Ok(ObjectFormat::Sha1),
            b"sha256\n" => Ok(ObjectFormat::Sha256),
            other => Err(output_error(
                "rev-parse",
                format!("unknown object format {:?}", String::from_utf8_lossy(other)),
            )),
        }
    }

    /// `git maintenance run --auto` in the foreground: the one command
    /// that runs without the `gc.auto=0`/`maintenance.auto=false`
    /// overrides, so the user's thresholds decide whether anything
    /// happens. Run it once the tablet is back up.
    pub fn maintenance_auto(&self) -> Result<()> {
        let mut cmd =
            self.git
                .command_with(Some(&self.git_dir), "maintenance", &[CONFIG, NO_DETACH]);
        cmd.args(["run", "--auto", "--quiet"]);
        run("maintenance", &mut cmd).map(drop)
    }

    /// Every entry below `treeish` (a commit or tree), trees included,
    /// parent trees before their contents.
    pub fn ls_tree_recursive(&self, treeish: &str) -> Result<Vec<TreeEntry>> {
        let mut cmd = self.command("ls-tree");
        cmd.args(["-r", "-t", "-z", "--long", "--end-of-options", treeish]);
        parse_ls_tree(&run("ls-tree", &mut cmd)?)
    }

    /// Commits reachable from `revisions` (e.g. `["<tip>", "^<stop>"]`;
    /// object ids or full ref names, never options), newest first in
    /// git's default order. With `first_parent`, only first parents are
    /// followed: on a branch whose every update is a merge with the
    /// previous tip as first parent, that is the branch's own history.
    pub fn rev_list(&self, revisions: &[&str], first_parent: bool) -> Result<Vec<Oid>> {
        let options: &[&str] = if first_parent {
            &["--first-parent"]
        } else {
            &[]
        };
        self.rev_list_with(revisions, options)
    }

    fn rev_list_with(&self, revisions: &[&str], options: &[&str]) -> Result<Vec<Oid>> {
        if let Some(bad) = revisions
            .iter()
            .find(|rev| rev.is_empty() || rev.starts_with('-') || rev.contains(char::is_control))
        {
            return Err(Error::InvalidGitInput(format!("invalid revision {bad:?}")));
        }
        let mut cmd = self.command("rev-list");
        cmd.args(options).arg("--end-of-options").args(revisions);
        let out = run("rev-list", &mut cmd)?;
        out.split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| parse_oid_line("rev-list", line))
            .collect()
    }

    /// Every commit reachable from `revisions`, children before their
    /// parents (`--topo-order`), so the first commit of a line of history
    /// is its newest.
    pub fn rev_list_topo(&self, revisions: &[&str]) -> Result<Vec<Oid>> {
        self.rev_list_with(revisions, &["--topo-order"])
    }

    /// Every commit reachable from `tip` with its parents, children before
    /// their parents (`rev-list --topo-order --parents`).
    pub fn rev_list_parents(&self, tip: &str) -> Result<Vec<CommitParents>> {
        if tip.is_empty() || tip.starts_with('-') || tip.contains(char::is_control) {
            return Err(Error::InvalidGitInput(format!("invalid revision {tip:?}")));
        }
        let mut cmd = self.command("rev-list");
        cmd.args(["--topo-order", "--parents", "--end-of-options", tip]);
        parse_rev_list_parents(&run("rev-list", &mut cmd)?)
    }

    /// The object each of `names` (`<commit>:<path>` and the like) names,
    /// in order, by one `cat-file --batch-check`: `None` where it names
    /// nothing.
    pub fn resolve(&self, names: &[String]) -> Result<Vec<Option<(Oid, ObjectKind)>>> {
        if let Some(bad) = names
            .iter()
            .find(|name| name.is_empty() || name.contains(char::is_control))
        {
            return Err(Error::InvalidGitInput(format!(
                "invalid object name {bad:?}"
            )));
        }
        let request: Vec<u8> = names
            .iter()
            .flat_map(|name| format!("{name}\n").into_bytes())
            .collect();
        let found: Vec<Option<(Oid, ObjectKind)>> = self
            .piped("cat-file", &["--batch-check"], request)?
            .split_inclusive(|&b| b == b'\n')
            .map(|line| {
                parse_batch_header(line).map(|header| match header {
                    BatchHeader::Found { oid, kind, .. } => Some((oid, kind)),
                    BatchHeader::Missing(_) => None,
                })
            })
            .collect::<Result<_>>()?;
        if found.len() != names.len() {
            return Err(output_error("cat-file", "not one reply per requested name"));
        }
        Ok(found)
    }

    /// The top-level entries that differ between each pair of trees, in
    /// order (no recursion: a subtree that changed is one entry), by one
    /// `diff-tree --stdin` process. Every pair must come back: one
    /// diff-tree skipped (it reports a bad input line without failing)
    /// would read as unchanged.
    pub fn tree_diffs(&self, pairs: &[(Oid, Oid)]) -> Result<Vec<Vec<RawChange>>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let request: Vec<u8> = pairs
            .iter()
            .flat_map(|(old, new)| format!("{old} {new}\n").into_bytes())
            .collect();
        let out = self.piped("diff-tree", &["--stdin", "--no-renames", "-z"], request)?;
        let parsed = parse_tree_diffs(&out)?;
        if !parsed.iter().map(|(pair, _)| pair).eq(pairs.iter()) {
            return Err(output_error(
                "diff-tree",
                "the output does not list every requested pair",
            ));
        }
        Ok(parsed.into_iter().map(|(_, changes)| changes).collect())
    }

    /// Run `git <subcommand> <args>` with `request` on its standard input
    /// (fed from a thread: the output could fill its pipe while the
    /// request is still being written) and return its output.
    fn piped(&self, subcommand: &'static str, args: &[&str], request: Vec<u8>) -> Result<Vec<u8>> {
        let mut cmd = self.command(subcommand);
        cmd.args(args);
        let mut process = Process::spawn(subcommand, &mut cmd)?;
        let mut stdin = process.child.stdin.take().expect("stdin was piped");
        let mut stdout = process.child.stdout.take().expect("stdout was piped");
        let (written, out) = thread::scope(|scope| {
            let writer = scope.spawn(move || stdin.write_all(&request));
            let mut out = Vec::new();
            let read = stdout.read_to_end(&mut out);
            // A failed read must not leave the process blocked on a full
            // pipe while the writer waits on it.
            drop(stdout);
            (
                writer
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("writer panicked"))),
                read.map(|_| out),
            )
        });
        let status = process.wait_checked();
        match (written, out, status) {
            (_, _, Err(error)) => Err(error),
            (Err(error), _, _) | (_, Err(error), _) => Err(error.into()),
            (Ok(()), Ok(out), Ok(_)) => Ok(out),
        }
    }

    /// The size of each of `oids` the repository has (absent ones are
    /// left out), in one `cat-file --batch-check`: headers only, no object
    /// content is read.
    pub fn object_sizes<'a>(
        &self,
        oids: impl IntoIterator<Item = &'a Oid>,
    ) -> Result<HashMap<Oid, u64>> {
        let request: Vec<u8> = oids
            .into_iter()
            .flat_map(|oid| format!("{oid}\n").into_bytes())
            .collect();
        if request.is_empty() {
            return Ok(HashMap::new());
        }
        let out = self.piped("cat-file", &["--batch-check"], request)?;
        out.split_inclusive(|&b| b == b'\n')
            .map(parse_batch_header)
            .filter_map(|header| match header {
                Ok(BatchHeader::Found { oid, size, .. }) => Some(Ok((oid, size))),
                Ok(BatchHeader::Missing(_)) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Start a `cat-file --batch` process for reading many objects.
    pub fn cat_file(&self) -> Result<CatFile> {
        let mut cmd = self.command("cat-file");
        cmd.arg("--batch");
        let mut process = Process::spawn("cat-file", &mut cmd)?;
        let stdin = process.child.stdin.take().expect("stdin was piped");
        let stdout = process.child.stdout.take().expect("stdout was piped");
        Ok(CatFile {
            process,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            pending: None,
        })
    }

    /// Start writing one commit with `git fast-import`. `total` is the
    /// expected number of new blob bytes, for progress reporting only.
    pub fn commit_writer<'p>(
        &self,
        progress: &'p dyn Progress,
        total: Option<u64>,
    ) -> Result<CommitWriter<'p>> {
        let mut cmd = self.command("fast-import");
        cmd.args(["--done", "--quiet", "--date-format=raw"]);
        let mut process = Process::spawn("fast-import", &mut cmd)?;
        let stdin = process.child.stdin.take().expect("stdin was piped");
        let stdout = process.child.stdout.take().expect("stdout was piped");
        Ok(CommitWriter {
            crash_report: self
                .git_dir
                .join(format!("fast_import_crash_{}", process.child.id())),
            process,
            stdin: Some(BufWriter::with_capacity(64 * 1024, stdin)),
            stdout: BufReader::new(stdout),
            entries: Vec::new(),
            next_mark: 1,
            progress,
            transferred: 0,
            total,
            pack_dir: self.git_dir.join("objects").join("pack"),
            own_keeps: BTreeSet::new(),
            finished: false,
        })
    }
}

/// A running `git cat-file --batch`: one process, any number of reads.
///
/// Dropping it kills the process; [`CatFile::finish`] ends it cleanly.
pub struct CatFile {
    process: Process,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// Unread body bytes of the last object returned; its trailing LF
    /// follows them.
    pending: Option<u64>,
}

/// One object being streamed from [`CatFile::object`]. Reading it fully
/// is optional; the next request skips whatever is left.
pub struct ObjectReader<'a> {
    cat: &'a mut CatFile,
    pub kind: ObjectKind,
    pub size: u64,
}

/// A whole object as [`CatFile::read`] returns it.
pub type RawObject = (ObjectKind, Vec<u8>);

/// Requests [`CatFile::read_many`] sends before reading answers: at most
/// about 8 KiB of ids, below any pipe's buffer, so sending never waits for
/// git while git waits for its answers to be read.
const READ_MANY_CHUNK: usize = 128;

impl CatFile {
    /// Look up `oid`; `None` if the repository does not have it.
    pub fn object(&mut self, oid: &Oid) -> Result<Option<ObjectReader<'_>>> {
        self.skip_pending()?;
        self.send(&format!("{oid}\n"))?;
        Ok(self.header(oid)?.map(|(kind, size)| ObjectReader {
            cat: self,
            kind,
            size,
        }))
    }

    /// Read whole objects, as [`Self::read`] does for each of `oids`, in
    /// order, with the requests pipelined (in chunks small enough for any
    /// pipe buffer): a round trip per chunk, not per object.
    pub fn read_many(&mut self, oids: &[Oid]) -> Result<Vec<Option<RawObject>>> {
        self.skip_pending()?;
        oids.chunks(READ_MANY_CHUNK).try_fold(
            Vec::with_capacity(oids.len()),
            |mut objects, chunk| {
                self.send(
                    &chunk
                        .iter()
                        .map(|oid| format!("{oid}\n"))
                        .collect::<String>(),
                )?;
                chunk.iter().try_for_each(|oid| {
                    self.skip_pending()?;
                    let object = self
                        .header(oid)?
                        .map(|(kind, size)| -> Result<_> {
                            let mut data = Vec::with_capacity(size.try_into().unwrap_or(0));
                            ObjectReader {
                                cat: self,
                                kind,
                                size,
                            }
                            .read_to_end(&mut data)?;
                            Ok((kind, data))
                        })
                        .transpose()?;
                    objects.push(object);
                    Ok::<_, Error>(())
                })?;
                Ok(objects)
            },
        )
    }

    fn send(&mut self, requests: &str) -> Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| closed("cat-file"))?;
        stdin
            .write_all(requests.as_bytes())
            .map_err(|error| self.broken(error))
    }

    /// The answer header for `oid` (requested already): its kind and size,
    /// with the body pending; `None` if it is missing.
    fn header(&mut self, oid: &Oid) -> Result<Option<(ObjectKind, u64)>> {
        let mut line = Vec::new();
        match self.stdout.read_until(b'\n', &mut line) {
            Ok(_) if line.ends_with(b"\n") => {}
            Ok(_) => return Err(self.broken(io::ErrorKind::UnexpectedEof.into())),
            Err(error) => return Err(self.broken(error)),
        }
        match parse_batch_header(&line)? {
            BatchHeader::Missing(_) => Ok(None),
            BatchHeader::Found {
                oid: found,
                kind,
                size,
            } if found == *oid => {
                self.pending = Some(size);
                Ok(Some((kind, size)))
            }
            BatchHeader::Found { oid: found, .. } => Err(Error::GitOutput {
                command: "cat-file".into(),
                detail: format!("asked for {oid}, got {found}"),
            }),
        }
    }

    /// Read a whole object into memory; `None` if it is missing.
    pub fn read(&mut self, oid: &Oid) -> Result<Option<(ObjectKind, Vec<u8>)>> {
        self.object(oid)?
            .map(|mut object| {
                let mut data = Vec::with_capacity(object.size.try_into().unwrap_or(0));
                object.read_to_end(&mut data)?;
                Ok((object.kind, data))
            })
            .transpose()
    }

    /// Close the process and check that it exited cleanly.
    pub fn finish(mut self) -> Result<()> {
        self.skip_pending()?;
        self.stdin.take().ok_or_else(|| closed("cat-file"))?;
        self.process.wait_checked().map(drop)
    }

    /// Discard the rest of the previous object and its trailing LF.
    fn skip_pending(&mut self) -> Result<()> {
        let Some(remaining) = self.pending.take() else {
            return Ok(());
        };
        let skipped = io::copy(&mut (&mut self.stdout).take(remaining), &mut io::sink());
        let mut lf = [0u8; 1];
        match skipped {
            Ok(n) if n == remaining && self.stdout.read_exact(&mut lf).is_ok() && lf == *b"\n" => {
                Ok(())
            }
            Ok(_) => Err(self.broken(io::ErrorKind::UnexpectedEof.into())),
            Err(error) => Err(self.broken(error)),
        }
    }

    fn broken(&mut self, error: io::Error) -> Error {
        self.pending = None;
        self.stdin.take();
        self.process.broken(&mut self.stdout, error)
    }
}

impl Read for ObjectReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.cat.pending.unwrap_or(0);
        if remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let n = self.cat.stdout.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.cat.pending = Some(remaining - n as u64);
        Ok(n)
    }
}

/// Writes one commit through `git fast-import`: add every file of the
/// full tree, then [`CommitWriter::commit`].
///
/// New blobs are streamed to git as they are added (from any reader, of
/// a size declared up front); existing blobs and whole subtrees are
/// referenced by id. No ref is updated. Dropping the writer without
/// committing kills fast-import; objects already streamed stay as
/// unreachable garbage. A crash report fast-import writes when it fails
/// (it holds recent stream lines: paths, messages) is removed on drop.
pub struct CommitWriter<'p> {
    process: Process,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    crash_report: PathBuf,
    /// Path, tree-entry mode bits, content.
    entries: Vec<(Vec<u8>, u32, DataRef)>,
    next_mark: u64,
    progress: &'p dyn Progress,
    transferred: u64,
    total: Option<u64>,
    /// `objects/pack`, and the `.keep` files this writer's checkpoints
    /// created: fast-import removes them when it ends cleanly, and this
    /// writer when it is aborted (a killed fast-import leaves them, and
    /// git never repacks or prunes a kept pack).
    pack_dir: PathBuf,
    own_keeps: BTreeSet<PathBuf>,
    finished: bool,
}

/// The `.keep` files in `dir`.
fn keep_files(dir: &Path) -> BTreeSet<PathBuf> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "keep"))
                .collect()
        })
        .unwrap_or_default()
}

enum DataRef {
    Mark(u64),
    Oid(Oid),
}

impl CommitWriter<'_> {
    /// Add a new file whose content is exactly `size` bytes from
    /// `reader`, returning its blob id. A reader that yields a different
    /// number of bytes is [`Error::BlobSizeMismatch`], and a read error
    /// is returned as is; both abort the commit. At most `size + 1`
    /// bytes are read. [`FileMode::Gitlink`] is refused before anything
    /// is written: a gitlink has no content.
    pub fn add_blob(
        &mut self,
        path: impl Into<Vec<u8>>,
        mode: FileMode,
        size: u64,
        reader: &mut dyn Read,
    ) -> Result<Oid> {
        let path = path.into();
        if mode == FileMode::Gitlink {
            return Err(Error::InvalidGitInput(format!(
                "a gitlink has no content to write: {}",
                String::from_utf8_lossy(&path)
            )));
        }
        validate_path(&path)?;
        let (mark, oid) = self.stream_blob(&String::from_utf8_lossy(&path), size, reader)?;
        self.entries.push((path, mode.bits(), DataRef::Mark(mark)));
        Ok(oid)
    }

    /// Write a blob without adding it to the commit's tree, returning
    /// its id; [`Self::add_existing`] can add it later. Used when whether
    /// a file belongs in the tree is decided after its content streamed
    /// by (a snapshot validates documents only once all files are read).
    /// `label` names the content in a [`Error::BlobSizeMismatch`]. Like
    /// every object written here, the blob is stored only once the commit
    /// is written.
    pub fn write_blob(&mut self, label: &str, size: u64, reader: &mut dyn Read) -> Result<Oid> {
        self.stream_blob(label, size, reader).map(|(_, oid)| oid)
    }

    fn stream_blob(&mut self, label: &str, size: u64, reader: &mut dyn Read) -> Result<(u64, Oid)> {
        let mark = self.next_mark;
        self.next_mark += 1;
        self.write(format!("blob\nmark :{mark}\ndata {size}\n").as_bytes())?;
        let copied = self.copy_blob(reader, size)?;
        if copied != size {
            self.abort();
            return Err(Error::BlobSizeMismatch {
                path: label.to_string(),
                expected: size,
                actual: (copied < size).then_some(copied),
            });
        }
        self.write(format!("\nget-mark :{mark}\n").as_bytes())?;
        let oid = self.read_mark()?;
        Ok((mark, oid))
    }

    /// Make every object written so far durable: fast-import closes its
    /// pack (`checkpoint`), so those objects stay in the repository even
    /// if this writer is aborted or dropped later. Returns once
    /// fast-import has done it. Fails on a writer that already failed.
    pub fn checkpoint(&mut self) -> Result<()> {
        let before = keep_files(&self.pack_dir);
        self.write(b"checkpoint\nprogress rmu-checkpoint\n")?;
        let stdin = self.stdin.as_mut().ok_or_else(|| closed("fast-import"))?;
        let mut line = Vec::new();
        match stdin
            .flush()
            .and_then(|()| self.stdout.read_until(b'\n', &mut line))
        {
            Ok(_) if line == b"progress rmu-checkpoint\n" => {
                let after = keep_files(&self.pack_dir);
                self.own_keeps.extend(after.difference(&before).cloned());
                Ok(())
            }
            Ok(_) => Err(self.broken(io::ErrorKind::UnexpectedEof.into())),
            Err(error) => Err(self.broken(error)),
        }
    }

    /// [`Self::add_blob`] for content already in memory.
    pub fn add_bytes(
        &mut self,
        path: impl Into<Vec<u8>>,
        mode: FileMode,
        data: &[u8],
    ) -> Result<Oid> {
        self.add_blob(path, mode, data.len() as u64, &mut &data[..])
    }

    /// Add a file whose blob is already in the repository (e.g. carried
    /// forward from a base tree). Its existence is checked at commit. A
    /// [`FileMode::Gitlink`] entry names a submodule commit, which git
    /// never requires to exist.
    pub fn add_existing(
        &mut self,
        path: impl Into<Vec<u8>>,
        mode: FileMode,
        oid: &Oid,
    ) -> Result<()> {
        let path = path.into();
        validate_path(&path)?;
        self.entries
            .push((path, mode.bits(), DataRef::Oid(oid.clone())));
        Ok(())
    }

    /// Add a whole existing tree at `path` (e.g. an unchanged document
    /// subtree carried forward from a base commit), whatever entries it
    /// holds. Nothing else may be added below `path`. The tree's
    /// existence is checked at commit.
    pub fn add_existing_tree(&mut self, path: impl Into<Vec<u8>>, tree: &Oid) -> Result<()> {
        let path = path.into();
        validate_path(&path)?;
        self.entries
            .push((path, 0o040000, DataRef::Oid(tree.clone())));
        Ok(())
    }

    /// Write the commit (its tree is exactly the files added) and return
    /// its id.
    pub fn commit(mut self, meta: &CommitMeta) -> Result<Oid> {
        if let Err(error) = check_tree_paths(self.entries.iter().map(|(path, ..)| path.as_slice()))
        {
            self.abort();
            return Err(error);
        }
        let stream = match commit_command(meta, &self.entries, self.next_mark) {
            Ok(stream) => stream,
            Err(error) => {
                self.abort();
                return Err(error);
            }
        };
        self.write(&stream)?;
        let stdin = self.stdin.take().ok_or_else(|| closed("fast-import"))?;
        if let Err(error) = stdin.into_inner().map_err(|e| e.into_error()) {
            return Err(self.broken(error));
        }
        let mut line = Vec::new();
        let read = self.stdout.read_until(b'\n', &mut line);
        self.process.wait_checked()?;
        read?;
        let oid = parse_oid_line("fast-import", &line)?;
        self.finished = true;
        self.progress.bytes_done();
        Ok(oid)
    }

    fn copy_blob(&mut self, reader: &mut dyn Read, size: u64) -> Result<u64> {
        let mut buffer = vec![0u8; 64 * 1024];
        let mut copied = 0u64;
        // Read one byte past `size` so an over-long reader is detected.
        while copied <= size {
            let want = buffer
                .len()
                .min(usize::try_from(size - copied + 1).unwrap_or(usize::MAX));
            let n = match reader.read(&mut buffer[..want]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.abort();
                    return Err(error.into());
                }
            };
            let to_write = n.min(usize::try_from(size - copied).unwrap_or(usize::MAX));
            self.write(&buffer[..to_write])?;
            copied += n as u64;
            self.transferred += to_write as u64;
            self.progress.bytes(self.transferred, self.total);
        }
        Ok(copied)
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| closed("fast-import"))?;
        stdin.write_all(data).map_err(|error| self.broken(error))
    }

    fn read_mark(&mut self) -> Result<Oid> {
        let stdin = self.stdin.as_mut().ok_or_else(|| closed("fast-import"))?;
        let mut line = Vec::new();
        match stdin
            .flush()
            .and_then(|()| self.stdout.read_until(b'\n', &mut line))
        {
            Ok(_) if line.ends_with(b"\n") => parse_oid_line("fast-import", &line),
            Ok(_) => Err(self.broken(io::ErrorKind::UnexpectedEof.into())),
            Err(error) => Err(self.broken(error)),
        }
    }

    fn broken(&mut self, error: io::Error) -> Error {
        self.stdin.take();
        self.process.broken(&mut self.stdout, error)
    }

    /// Kill fast-import before it can finish a pack or touch a ref.
    fn abort(&mut self) {
        self.process.kill();
        self.stdin.take();
    }
}

impl Drop for CommitWriter<'_> {
    fn drop(&mut self) {
        self.abort();
        let _ = fs::remove_file(&self.crash_report);
        if !self.finished {
            self.own_keeps.iter().for_each(|keep| {
                let _ = fs::remove_file(keep);
            });
        }
    }
}

/// The fast-import stream for the commit itself: metadata, the full
/// tree, the commit id request, and a reset so no ref is written.
fn commit_command(
    meta: &CommitMeta,
    entries: &[(Vec<u8>, u32, DataRef)],
    mark: u64,
) -> Result<Vec<u8>> {
    let author = format_signature(&meta.author)?;
    let committer = format_signature(&meta.committer)?;
    let header = format!(
        "commit {SCRATCH_REF}\nmark :{mark}\nauthor {author}\ncommitter {committer}\ndata {}\n",
        meta.message.len()
    );
    let mut seen = HashSet::new();
    let parents = meta
        .parents
        .iter()
        .filter(|parent| seen.insert(*parent))
        .enumerate()
        .map(|(i, parent)| format!("{} {parent}\n", if i == 0 { "from" } else { "merge" }))
        .collect::<String>();
    let files = entries
        .iter()
        .map(|(path, mode, data)| {
            let data = match data {
                DataRef::Mark(mark) => format!(":{mark}"),
                DataRef::Oid(oid) => oid.to_string(),
            };
            format!("M {mode:06o} {data} {}\n", quote_path(path))
        })
        .collect::<String>();
    Ok([
        header.as_bytes(),
        meta.message.as_bytes(),
        b"\n",
        parents.as_bytes(),
        b"deleteall\n",
        files.as_bytes(),
        format!("\nget-mark :{mark}\nreset {SCRATCH_REF}\n\ndone\n").as_bytes(),
    ]
    .concat())
}

/// A child process with stdin/stdout piped and stderr collected on a
/// thread (so it can never fill up and stall the protocol). Killed and
/// reaped on drop unless already waited for.
struct Process {
    name: &'static str,
    child: Child,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    reaped: bool,
}

impl Process {
    fn spawn(name: &'static str, cmd: &mut Command) -> Result<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| spawn_error(name, source))?;
        let mut stderr = child.stderr.take().expect("stderr was piped");
        let reader = thread::Builder::new().spawn(move || {
            let mut data = Vec::new();
            stderr.read_to_end(&mut data).map(|_| data)
        });
        let mut process = Self {
            name,
            child,
            stderr: None,
            reaped: false,
        };
        process.stderr = Some(reader?);
        Ok(process)
    }

    /// Wait for exit (stdin must be closed already); non-zero is an error.
    fn wait_checked(&mut self) -> Result<ExitStatus> {
        let status = self.child.wait()?;
        self.reaped = true;
        let stderr = self.join_stderr();
        if status.success() {
            Ok(status)
        } else {
            Err(Error::Git {
                command: self.name.into(),
                status: status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
            })
        }
    }

    /// The protocol broke (a pipe closed, output ended early). With stdin
    /// already closed, drain stdout so a child blocked writing it can
    /// exit, then report its exit status and stderr if it failed.
    fn broken(&mut self, stdout: &mut impl Read, error: io::Error) -> Error {
        let _ = io::copy(stdout, &mut io::sink());
        match self.wait_checked() {
            Err(git_error) => git_error,
            Ok(_) => Error::GitOutput {
                command: self.name.into(),
                detail: error.to_string(),
            },
        }
    }

    fn kill(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
        self.join_stderr();
    }

    fn join_stderr(&mut self) -> Vec<u8> {
        self.stderr
            .take()
            .and_then(|reader| reader.join().ok())
            .and_then(|result| result.ok())
            .unwrap_or_default()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A streaming process was used again after it failed.
fn closed(command: &str) -> Error {
    output_error(command, "process already failed or finished")
}

fn spawn_error(command: &str, source: io::Error) -> Error {
    Error::GitSpawn {
        command: command.into(),
        source,
    }
}

/// Run to completion with no stdin, capturing output; any exit status.
///
/// This waits for EOF on stdout and stderr, not just for git to exit: a
/// user-configured helper (an ssh wrapper, a credential daemon) that
/// inherits stderr and never closes it would stall the call. Stock
/// OpenSSH and `credential-cache` detach from it, so this is accepted
/// here; `ssh.rs` shows the heavier alternative if it ever matters.
fn output(name: &str, cmd: &mut Command) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .output()
        .map_err(|source| spawn_error(name, source))
}

/// Run to completion; non-zero exit is an error. Returns stdout.
fn run(name: &str, cmd: &mut Command) -> Result<Vec<u8>> {
    let output = output(name, cmd)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(failure(name, &output))
    }
}

fn failure(name: &str, output: &Output) -> Error {
    Error::Git {
        command: name.into(),
        status: output.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

fn output_error(command: &str, detail: impl Into<String>) -> Error {
    Error::GitOutput {
        command: command.into(),
        detail: detail.into(),
    }
}

fn parse_oid_line(command: &str, line: &[u8]) -> Result<Oid> {
    std::str::from_utf8(line)
        .ok()
        .and_then(|s| s.trim_end_matches('\n').parse().ok())
        .ok_or_else(|| {
            output_error(
                command,
                format!(
                    "expected an object id, got {:?}",
                    String::from_utf8_lossy(line)
                ),
            )
        })
}

/// Parse `ls-remote` output: `<oid> TAB <ref> LF` per ref.
fn parse_ls_remote(out: &[u8]) -> Result<Vec<RemoteRef>> {
    let text =
        std::str::from_utf8(out).map_err(|_| output_error("ls-remote", "ref name is not UTF-8"))?;
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (oid, name) = line
                .split_once('\t')
                .ok_or_else(|| output_error("ls-remote", format!("malformed line {line:?}")))?;
            Ok(RemoteRef {
                name: name.to_string(),
                oid: oid
                    .parse()
                    .map_err(|_| output_error("ls-remote", format!("malformed line {line:?}")))?,
            })
        })
        .collect()
}

/// Parse `ls-tree -z --long` output:
/// `<mode> SP <type> SP <oid> SP+ <size|-> TAB <path> NUL` per entry.
/// Parse `rev-list --parents` output: one commit per line, its parents
/// after it.
pub fn parse_rev_list_parents(out: &[u8]) -> Result<Vec<CommitParents>> {
    out.split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let malformed = || {
                output_error(
                    "rev-list",
                    format!("malformed line {:?}", String::from_utf8_lossy(line)),
                )
            };
            let text = std::str::from_utf8(line).map_err(|_| malformed())?;
            let mut ids = text.split(' ').map(|id| id.parse::<Oid>());
            let commit = ids.next().ok_or_else(malformed)?.map_err(|_| malformed())?;
            let parents = ids.collect::<Result<Vec<_>>>().map_err(|_| malformed())?;
            Ok(CommitParents { commit, parents })
        })
        .collect()
}

/// Parse `diff-tree --stdin -z` output for tree pairs: per pair, a
/// `<old> <new>` line (newline-terminated), then its entries
/// (`:<old mode> <new mode> <old id> <new id> <status>`, then the path,
/// each NUL-terminated; a path may hold any byte but NUL).
pub fn parse_tree_diffs(out: &[u8]) -> Result<Vec<TreeDiff>> {
    let malformed = |what: &[u8]| {
        output_error(
            "diff-tree",
            format!("malformed output {:?}", String::from_utf8_lossy(what)),
        )
    };
    // Split off everything up to `end`, which must be there.
    let take = |rest: &mut &[u8], end: u8| -> Result<Vec<u8>> {
        let at = rest
            .iter()
            .position(|&b| b == end)
            .ok_or_else(|| malformed(rest))?;
        let field = rest[..at].to_vec();
        *rest = &rest[at + 1..];
        Ok(field)
    };
    // One item: a pair's header line, or an entry of the pair before it.
    let next = |rest: &mut &[u8]| -> Result<std::result::Result<(Oid, Oid), RawChange>> {
        if rest.first() == Some(&b':') {
            let header = take(rest, 0)?;
            let path = take(rest, 0)?;
            let change = parse_raw_change(&header[1..], &path).ok_or_else(|| malformed(&header))?;
            Ok(Err(change))
        } else {
            let line = take(rest, b'\n')?;
            let pair = std::str::from_utf8(&line)
                .ok()
                .and_then(|line| line.split_once(' '))
                .and_then(|(old, new)| Some((old.parse().ok()?, new.parse().ok()?)))
                .ok_or_else(|| malformed(&line))?;
            Ok(Ok(pair))
        }
    };
    let mut rest = out;
    std::iter::from_fn(|| (!rest.is_empty()).then(|| next(&mut rest))).try_fold(
        Vec::new(),
        |mut pairs: Vec<TreeDiff>, item| {
            match item? {
                Ok(pair) => pairs.push((pair, Vec::new())),
                Err(change) => pairs
                    .last_mut()
                    .ok_or_else(|| malformed(&change.path))?
                    .1
                    .push(change),
            }
            Ok(pairs)
        },
    )
}

fn parse_raw_change(header: &[u8], path: &[u8]) -> Option<RawChange> {
    let header = std::str::from_utf8(header).ok()?;
    let [old_mode, new_mode, old, new, status] =
        header.split(' ').collect::<Vec<_>>().try_into().ok()?;
    let mode = |mode: &str| {
        Some(mode)
            .filter(|mode| !mode.is_empty() && mode.bytes().all(|b| (b'0'..=b'7').contains(&b)))
            .and_then(|mode| u32::from_str_radix(mode, 8).ok())
    };
    Some(RawChange {
        old_mode: mode(old_mode)?,
        new_mode: mode(new_mode)?,
        old: old.parse().ok()?,
        new: new.parse().ok()?,
        status: *status
            .as_bytes()
            .first()
            .filter(|b| b.is_ascii_uppercase())?,
        path: Some(path).filter(|path| !path.is_empty())?.to_vec(),
    })
}

fn parse_ls_tree(out: &[u8]) -> Result<Vec<TreeEntry>> {
    out.split(|&b| b == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let malformed = || {
                output_error(
                    "ls-tree",
                    format!("malformed entry {:?}", String::from_utf8_lossy(record)),
                )
            };
            // The path is everything after the first TAB and may itself
            // contain TABs; the header never does.
            let tab = record
                .iter()
                .position(|&b| b == b'\t')
                .ok_or_else(malformed)?;
            let (header, path) = (&record[..tab], &record[tab + 1..]);
            let header = std::str::from_utf8(header).map_err(|_| malformed())?;
            let [mode, kind, oid, size] = header
                .split_ascii_whitespace()
                .collect::<Vec<_>>()
                .try_into()
                .map_err(|_| malformed())?;
            Ok(TreeEntry {
                mode: u32::from_str_radix(mode, 8).map_err(|_| malformed())?,
                kind: kind.parse().map_err(|_| malformed())?,
                oid: oid.parse().map_err(|_| malformed())?,
                size: match size {
                    "-" => None,
                    n => Some(n.parse().map_err(|_| malformed())?),
                },
                path: path.to_vec(),
            })
        })
        .collect()
}

/// A `cat-file --batch` response header.
#[derive(Debug, PartialEq, Eq)]
enum BatchHeader {
    Found {
        oid: Oid,
        kind: ObjectKind,
        size: u64,
    },
    Missing(String),
}

/// Parse `<oid> SP <type> SP <size> LF` or `<name> SP missing LF`.
fn parse_batch_header(line: &[u8]) -> Result<BatchHeader> {
    let malformed = || {
        output_error(
            "cat-file",
            format!("malformed header {:?}", String::from_utf8_lossy(line)),
        )
    };
    let line = std::str::from_utf8(line)
        .ok()
        .and_then(|s| s.strip_suffix('\n'))
        .ok_or_else(malformed)?;
    match line.split(' ').collect::<Vec<_>>().as_slice() {
        [name, "missing"] => Ok(BatchHeader::Missing(name.to_string())),
        [oid, kind, size] => Ok(BatchHeader::Found {
            oid: oid.parse().map_err(|_| malformed())?,
            kind: kind.parse().map_err(|_| malformed())?,
            size: size.parse().map_err(|_| malformed())?,
        }),
        _ => Err(malformed()),
    }
}

/// The parts of a commit object rmu reads (from [`CatFile::read`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitObject {
    pub tree: Oid,
    /// First parent first.
    pub parents: Vec<Oid>,
    /// Everything after the header (lossily decoded: rmu only looks for
    /// its own ASCII trailers in it).
    pub message: String,
}

/// Parse a raw commit object: header lines (`tree`, `parent`, and others,
/// which are skipped, continuation lines included), a blank line, then
/// the message.
pub fn parse_commit(data: &[u8]) -> Result<CommitObject> {
    let malformed = |why: &str| output_error("cat-file", format!("malformed commit: {why}"));
    let (header, message) = match data.windows(2).position(|pair| pair == b"\n\n") {
        Some(end) => (&data[..end], &data[end + 2..]),
        None => (data.strip_suffix(b"\n").unwrap_or(data), &b""[..]),
    };
    let oid_of = |value: &[u8]| {
        std::str::from_utf8(value)
            .ok()
            .and_then(|s| s.parse::<Oid>().ok())
            .ok_or_else(|| malformed("bad object id"))
    };
    let (tree, parents) = header.split(|&b| b == b'\n').try_fold(
        (None, Vec::new()),
        |(tree, mut parents), line| {
            if let Some(value) = line.strip_prefix(b"tree ") {
                if tree.is_some() {
                    return Err(malformed("two trees"));
                }
                return Ok((Some(oid_of(value)?), parents));
            }
            if let Some(value) = line.strip_prefix(b"parent ") {
                parents.push(oid_of(value)?);
            }
            Ok((tree, parents))
        },
    )?;
    Ok(CommitObject {
        tree: tree.ok_or_else(|| malformed("no tree"))?,
        parents,
        message: String::from_utf8_lossy(message).into_owned(),
    })
}

/// A stored tree-entry mode as git reports it (its `canon_mode`, applied
/// by `ls-tree`, `diff-tree` and `fast-import` alike): a regular file is
/// `100644` or `100755` by its owner-execute bit (old trees hold `100664`
/// or `100775`, which `fsck` only warns about), a symlink `120000`, a
/// directory `040000`, anything else a gitlink.
fn canon_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o100000 if mode & 0o100 != 0 => 0o100755,
        0o100000 => 0o100644,
        0o120000 => 0o120000,
        0o040000 => 0o040000,
        _ => 0o160000,
    }
}

/// Parse a raw tree object (`<octal mode> SP <name> NUL <binary id>` per
/// entry) into entries named by their bare names. Modes are canonical
/// ([`canon_mode`]), as git reports them. Sizes are not part of a tree
/// object (`None`); the kind follows from the mode.
pub fn parse_tree(data: &[u8], format: ObjectFormat) -> Result<Vec<TreeEntry>> {
    let id_len = match format {
        ObjectFormat::Sha1 => 20,
        ObjectFormat::Sha256 => 32,
    };
    let malformed = || output_error("cat-file", "malformed tree object");
    let entry_at = |rest: &[u8]| -> Result<(TreeEntry, usize)> {
        let space = rest.iter().position(|&b| b == b' ').ok_or_else(malformed)?;
        let nul = rest.iter().position(|&b| b == 0).ok_or_else(malformed)?;
        // Octal digits only (`from_str_radix` alone would take a sign).
        let mode = Some(&rest[..space])
            .filter(|mode| !mode.is_empty() && mode.iter().all(|b| (b'0'..=b'7').contains(b)))
            .and_then(|mode| std::str::from_utf8(mode).ok())
            .and_then(|mode| u32::from_str_radix(mode, 8).ok())
            .filter(|_| space < nul)
            .map(canon_mode)
            .ok_or_else(malformed)?;
        let name = &rest[space + 1..nul];
        let id = rest.get(nul + 1..nul + 1 + id_len).ok_or_else(malformed)?;
        // A name is one path component, as `git fsck` requires.
        if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
            return Err(malformed());
        }
        let kind = match mode {
            0o040000 => ObjectKind::Tree,
            0o160000 => ObjectKind::Commit,
            _ => ObjectKind::Blob,
        };
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        Ok((
            TreeEntry {
                mode,
                kind,
                oid: hex.parse()?,
                size: None,
                path: name.to_vec(),
            },
            nul + 1 + id_len,
        ))
    };
    // Collecting into a `Result` stops at the first error.
    let mut rest = data;
    std::iter::from_fn(|| {
        (!rest.is_empty()).then(|| {
            entry_at(rest).map(|(entry, used)| {
                rest = &rest[used..];
                entry
            })
        })
    })
    .collect()
}

/// One ref's line of `push --porcelain`:
/// `<flag> TAB <from>:<to> TAB <summary> [(<reason>)]`.
#[derive(Debug, PartialEq, Eq)]
struct PushStatus {
    flag: char,
    to: String,
    summary: String,
    reason: Option<String>,
}

fn parse_push_porcelain(stdout: &str) -> Vec<PushStatus> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let flag = match fields.next()?.as_bytes() {
                [f] if b" +-*!=".contains(f) => char::from(*f),
                _ => return None,
            };
            let (_, to) = fields.next()?.rsplit_once(':')?;
            let rest = fields.next()?;
            let (summary, reason) = match rest.split_once(" (") {
                Some((summary, reason)) => (summary, reason.strip_suffix(')').map(str::to_string)),
                None => (rest, None),
            };
            Some(PushStatus {
                flag,
                to: to.to_string(),
                summary: summary.to_string(),
                reason,
            })
        })
        .collect()
}

/// What a finished push of one ref amounted to.
#[derive(Debug, PartialEq, Eq)]
enum PushOutcome {
    Updated,
    /// Lost to a concurrent writer; fetch and retry.
    Rejected(String),
    /// The remote refused for another reason.
    Refused(String),
    /// A status the request cannot have asked for.
    Unexpected(String),
    /// Git failed without a usable status for the ref.
    Failed,
}

/// `[remote rejected]` reasons (and the stderr line) with which a remote
/// reports that its own compare-and-swap failed: the ref moved between
/// the advertisement and the update. Older `receive-pack` versions use
/// the vaguer reasons; misreading a genuine lock failure as a race costs
/// one bounded retry.
const REMOTE_RACE_REASONS: &[&str] = &[
    "incorrect old value provided",
    "failed to update ref",
    "failed to lock",
];

/// Interpret a finished push of `dst`.
fn push_outcome(dst: &str, leased: bool, success: bool, stdout: &str, stderr: &str) -> PushOutcome {
    let status = parse_push_porcelain(stdout)
        .into_iter()
        .find(|s| s.to == dst);
    let remote_race = |reason: &str| {
        REMOTE_RACE_REASONS.contains(&reason)
            || stderr
                .lines()
                .any(|line| line.contains("cannot lock ref") && line.contains("but expected"))
    };
    match status {
        Some(s) if s.flag == '!' => {
            let reason = s.reason.unwrap_or_else(|| s.summary.clone());
            match s.summary.as_str() {
                "[rejected]" => PushOutcome::Rejected(reason),
                "[remote rejected]" if remote_race(&reason) => PushOutcome::Rejected(reason),
                summary => PushOutcome::Refused(format!("{summary} ({reason})")),
            }
        }
        Some(s) if s.flag == '-' => PushOutcome::Unexpected(format!("{dst} was deleted")),
        Some(s) if s.flag == '+' && !leased => {
            PushOutcome::Unexpected(format!("{dst} was force-updated without a lease"))
        }
        Some(_) if success => PushOutcome::Updated,
        None if success => PushOutcome::Unexpected(format!("no status reported for {dst}")),
        _ => PushOutcome::Failed,
    }
}

/// The ref names rmu updates: full `refs/...` names, never refspec
/// syntax (a leading `+` forces, a `:` splits source from destination).
fn validate_ref_name(name: &str) -> Result<()> {
    if name.starts_with("refs/") && !name.contains(':') && !name.contains(char::is_control) {
        Ok(())
    } else {
        Err(Error::InvalidGitInput(format!(
            "not a full ref name (refs/...): {name:?}"
        )))
    }
}

/// Reject paths git cannot store or that would be unsafe to check out.
fn validate_path(path: &[u8]) -> Result<()> {
    let invalid = |why: &str| {
        Err(Error::InvalidGitInput(format!(
            "invalid tree path {:?}: {why}",
            String::from_utf8_lossy(path)
        )))
    };
    if path.is_empty() {
        return invalid("empty");
    }
    if path.contains(&0) {
        return invalid("contains NUL");
    }
    match path
        .split(|&b| b == b'/')
        .find(|c| c.is_empty() || *c == b"." || *c == b".." || c.eq_ignore_ascii_case(b".git"))
    {
        Some(_) => invalid("empty, '.', '..' or '.git' component"),
        None => Ok(()),
    }
}

/// A tree cannot hold the same path twice, nor an entry (file or whole
/// tree) where another path needs a directory.
fn check_tree_paths<'a>(mut paths: impl Iterator<Item = &'a [u8]> + Clone) -> Result<()> {
    let directories = paths
        .clone()
        .flat_map(|path| {
            path.iter()
                .enumerate()
                .filter(|&(_, &b)| b == b'/')
                .map(move |(i, _)| &path[..i])
        })
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    paths.try_for_each(|path| {
        if !seen.insert(path) {
            Err("listed twice")
        } else if directories.contains(path) {
            Err("both an entry and a directory of other entries")
        } else {
            Ok(())
        }
        .map_err(|why| {
            Error::InvalidGitInput(format!(
                "tree path {:?} is {why}",
                String::from_utf8_lossy(path)
            ))
        })
    })
}

/// C-style quote a path for fast-import: always quoted, so any byte but
/// NUL round-trips (LF, quotes, leading `"`, non-UTF-8).
fn quote_path(path: &[u8]) -> String {
    let body = path
        .iter()
        .map(|&b| match b {
            b'"' => "\\\"".to_string(),
            b'\\' => "\\\\".to_string(),
            b'\n' => "\\n".to_string(),
            0x20..=0x7e => char::from(b).to_string(),
            _ => format!("\\{b:03o}"),
        })
        .collect::<String>();
    format!("\"{body}\"")
}

/// `Name <email> <seconds> <+hhmm>` (fast-import's raw date format).
fn format_signature(sig: &Signature) -> Result<String> {
    let bad = |field: &str, value: &str, forbidden: &[char]| {
        value.contains(forbidden).then(|| {
            Error::InvalidGitInput(format!("{field} {value:?} contains '<', '>' or a newline"))
        })
    };
    let forbidden = ['<', '>', '\n', '\0'];
    if let Some(error) =
        bad("name", &sig.name, &forbidden).or_else(|| bad("email", &sig.email, &forbidden))
    {
        return Err(error);
    }
    let offset = sig.offset_minutes.unsigned_abs();
    if offset >= 24 * 60 {
        return Err(Error::InvalidGitInput(format!(
            "time zone offset out of range: {} minutes",
            sig.offset_minutes
        )));
    }
    let sign = if sig.offset_minutes < 0 { '-' } else { '+' };
    Ok(format!(
        "{} <{}> {} {sign}{:02}{:02}",
        sig.name,
        sig.email,
        sig.time,
        offset / 60,
        offset % 60
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;

    const A: &str = "0123456789abcdef0123456789abcdef01234567";
    const B: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn oid(s: &str) -> Oid {
        s.parse().unwrap()
    }

    #[test]
    fn oid_accepts_lowercase_sha1_and_sha256_hex_only() {
        assert_eq!(oid(A).as_str(), A);
        // Uppercase is refused, not normalised (see `Oid`).
        assert!(matches!(
            A.to_uppercase().parse::<Oid>(),
            Err(Error::InvalidObjectId(_))
        ));
        assert!(format!("{A}{}", &B[..24]).parse::<Oid>().is_ok());
        assert!(A[..39].parse::<Oid>().is_err());
        assert!(format!("{}g", &A[..39]).parse::<Oid>().is_err());
        assert!("".parse::<Oid>().is_err());
    }

    #[test]
    fn file_mode_bits_round_trip() {
        [
            FileMode::Regular,
            FileMode::Executable,
            FileMode::Symlink,
            FileMode::Gitlink,
        ]
        .into_iter()
        .for_each(|mode| assert_eq!(FileMode::from_bits(mode.bits()), Some(mode)));
        assert_eq!(
            FileMode::ALL,
            [
                FileMode::Regular,
                FileMode::Executable,
                FileMode::Symlink,
                FileMode::Gitlink
            ]
        );
        assert_eq!(FileMode::from_bits(0o160000), Some(FileMode::Gitlink));
        assert_eq!(FileMode::from_bits(0o040000), None);
        assert_eq!(FileMode::from_bits(0o100664), None);
        assert!(FileMode::Regular.is_file() && FileMode::Executable.is_file());
        assert!(!FileMode::Symlink.is_file() && !FileMode::Gitlink.is_file());
    }

    #[test]
    fn oid_serde_is_its_string_and_validates() {
        let json = serde_json::to_string(&oid(A)).unwrap();
        assert_eq!(json, format!("\"{A}\""));
        assert_eq!(serde_json::from_str::<Oid>(&json).unwrap(), oid(A));
        [A.to_uppercase(), A[..39].to_string(), "HEAD".to_string()]
            .iter()
            .for_each(|bad| assert!(serde_json::from_str::<Oid>(&format!("\"{bad}\"")).is_err()));
    }

    #[test]
    fn ls_remote_parses_refs_and_peeled_tags() {
        let out = format!("{A}\tHEAD\n{A}\trefs/heads/main\n{B}\trefs/tags/v1^{{}}\n");
        let refs = parse_ls_remote(out.as_bytes()).unwrap();
        assert_eq!(
            refs,
            vec![
                RemoteRef {
                    name: "HEAD".into(),
                    oid: oid(A)
                },
                RemoteRef {
                    name: "refs/heads/main".into(),
                    oid: oid(A)
                },
                RemoteRef {
                    name: "refs/tags/v1^{}".into(),
                    oid: oid(B)
                },
            ]
        );
        assert_eq!(parse_ls_remote(b"").unwrap(), vec![]);
    }

    #[test]
    fn ls_remote_rejects_malformed_lines() {
        assert!(matches!(
            parse_ls_remote(b"nonsense\n"),
            Err(Error::GitOutput { .. })
        ));
        assert!(parse_ls_remote(b"abc\trefs/heads/main\n").is_err());
        assert!(
            parse_ls_remote(&[format!("{A}\trefs/heads/").as_bytes(), b"\xff\n"].concat()).is_err()
        );
    }

    #[test]
    fn ls_tree_parses_blobs_trees_and_odd_paths() {
        let mut out = Vec::new();
        out.extend_from_slice(format!("040000 tree {A}       -\tdocs").as_bytes());
        out.push(0);
        out.extend_from_slice(
            format!("100644 blob {B}     123\tdocs/a b\tc\nd \"q\" \\").as_bytes(),
        );
        out.push(0);
        out.extend_from_slice(format!("100755 blob {B} 4294967296\tbin/").as_bytes());
        out.extend_from_slice(b"\xff\xfe\0");
        out.extend_from_slice(format!("160000 commit {A}       -\tsub").as_bytes());
        out.push(0);
        let entries = parse_ls_tree(&out).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries[0],
            TreeEntry {
                mode: 0o040000,
                kind: ObjectKind::Tree,
                oid: oid(A),
                size: None,
                path: b"docs".to_vec(),
            }
        );
        assert_eq!(entries[1].path, b"docs/a b\tc\nd \"q\" \\");
        assert_eq!(entries[1].size, Some(123));
        assert_eq!(entries[1].kind, ObjectKind::Blob);
        assert_eq!(entries[2].mode, 0o100755);
        assert_eq!(entries[2].size, Some(4_294_967_296));
        assert_eq!(entries[2].path, b"bin/\xff\xfe");
        assert_eq!(entries[3].kind, ObjectKind::Commit);
        assert_eq!(parse_ls_tree(b"").unwrap(), vec![]);
    }

    #[test]
    fn ls_tree_rejects_malformed_records() {
        [
            format!("100644 blob {A} 1 path"),   // no TAB
            format!("100644 blob {A}\tpath"),    // no size
            format!("100648 blob {A} 1\tpath"),  // not octal
            format!("100644 blub {A} 1\tpath"),  // unknown type
            "100644 blob nothex 1\tpath".into(), // bad oid
            format!("100644 blob {A} x\tpath"),  // bad size
        ]
        .iter()
        .for_each(|record| {
            assert!(
                matches!(
                    parse_ls_tree(record.as_bytes()),
                    Err(Error::GitOutput { .. })
                ),
                "{record}"
            );
        });
    }

    #[test]
    fn batch_header_found_missing_and_malformed() {
        assert_eq!(
            parse_batch_header(format!("{A} blob 42\n").as_bytes()).unwrap(),
            BatchHeader::Found {
                oid: oid(A),
                kind: ObjectKind::Blob,
                size: 42
            }
        );
        assert_eq!(
            parse_batch_header(format!("{B} missing\n").as_bytes()).unwrap(),
            BatchHeader::Missing(B.into())
        );
        [
            format!("{A} blob 42"),
            format!("{A} blob\n"),
            format!("{A} blob -1\n"),
            format!("{A} ambiguous\n"),
            format!("{A} widget 1\n"),
            "\n".into(),
        ]
        .iter()
        .for_each(|line| assert!(parse_batch_header(line.as_bytes()).is_err(), "{line:?}"));
    }

    #[test]
    fn porcelain_parses_every_flag_shape() {
        let out = format!(
            "To ../origin.git\n\
             *\t{A}:refs/heads/main\t[new branch]\n\
             =\trefs/heads/x:refs/heads/x\t[up to date]\n\
             +\t{A}:refs/rmu/devices/d\t1234567...89abcde (forced update)\n\
             !\t{B}:refs/heads/y\t[remote rejected] (hook said (no))\n\
             -\t:refs/heads/gone\t[deleted]\n\
             Done\n"
        );
        let statuses = parse_push_porcelain(&out);
        assert_eq!(
            statuses,
            vec![
                PushStatus {
                    flag: '*',
                    to: "refs/heads/main".into(),
                    summary: "[new branch]".into(),
                    reason: None
                },
                PushStatus {
                    flag: '=',
                    to: "refs/heads/x".into(),
                    summary: "[up to date]".into(),
                    reason: None
                },
                PushStatus {
                    flag: '+',
                    to: "refs/rmu/devices/d".into(),
                    summary: "1234567...89abcde".into(),
                    reason: Some("forced update".into()),
                },
                PushStatus {
                    flag: '!',
                    to: "refs/heads/y".into(),
                    summary: "[remote rejected]".into(),
                    reason: Some("hook said (no)".into()),
                },
                PushStatus {
                    flag: '-',
                    to: "refs/heads/gone".into(),
                    summary: "[deleted]".into(),
                    reason: None
                },
            ]
        );
    }

    #[test]
    fn push_outcome_classifies_rejections() {
        let line =
            |flag: &str, rest: &str| format!("To o\n{flag}\t{A}:refs/heads/main\t{rest}\nDone\n");
        let main = "refs/heads/main";
        let outcome = |leased, success, stdout: &str, stderr| {
            push_outcome(main, leased, success, stdout, stderr)
        };
        assert_eq!(
            outcome(false, true, &line("*", "[new branch]"), ""),
            PushOutcome::Updated
        );
        assert_eq!(
            outcome(false, true, &line(" ", "1234567..89abcde"), ""),
            PushOutcome::Updated
        );
        assert_eq!(
            outcome(false, true, &line("=", "[up to date]"), ""),
            PushOutcome::Updated
        );
        ["non-fast-forward", "fetch first", "stale info"]
            .iter()
            .for_each(|reason| {
                assert_eq!(
                    outcome(
                        true,
                        false,
                        &line("!", &format!("[rejected] ({reason})")),
                        "error: failed"
                    ),
                    PushOutcome::Rejected(reason.to_string())
                );
            });
        // The remote's own compare-and-swap lost a race: retry too.
        REMOTE_RACE_REASONS.iter().for_each(|reason| {
            assert_eq!(
                outcome(
                    false,
                    false,
                    &line("!", &format!("[remote rejected] ({reason})")),
                    ""
                ),
                PushOutcome::Rejected(reason.to_string())
            );
        });
        assert_eq!(
            outcome(
                false,
                false,
                &line("!", "[remote rejected] (some future wording)"),
                "remote: error: cannot lock ref 'refs/heads/main': is at 1 but expected 2\n",
            ),
            PushOutcome::Rejected("some future wording".into())
        );
        assert_eq!(
            outcome(
                false,
                false,
                &line("!", "[remote rejected] (pre-receive hook declined)"),
                "remote: file too large: docs/x/payload.pdf\nerror: failed to push some refs\n",
            ),
            PushOutcome::Refused("[remote rejected] (pre-receive hook declined)".into())
        );
        assert!(matches!(
            outcome(
                false,
                false,
                &line("!", "[remote failure] (remote failed to report status)"),
                ""
            ),
            PushOutcome::Refused(_)
        ));
    }

    #[test]
    fn push_outcome_rejects_statuses_the_request_cannot_ask_for() {
        let main = "refs/heads/main";
        let deleted = format!("To o\n-\t:{main}\t[deleted]\nDone\n");
        let forced = format!("To o\n+\t{A}:{main}\t1234567...89abcde (forced update)\nDone\n");
        [true, false].iter().for_each(|&leased| {
            assert!(matches!(
                push_outcome(main, leased, true, &deleted, ""),
                PushOutcome::Unexpected(_)
            ));
        });
        assert!(matches!(
            push_outcome(main, false, true, &forced, ""),
            PushOutcome::Unexpected(_)
        ));
        assert_eq!(
            push_outcome(main, true, true, &forced, ""),
            PushOutcome::Updated
        );
    }

    #[test]
    fn push_outcome_without_a_status_line() {
        let main = "refs/heads/main";
        // Auth or network failure: nothing on stdout.
        assert_eq!(
            push_outcome(main, false, false, "", "fatal: Could not read"),
            PushOutcome::Failed
        );
        // A status for some other ref only.
        let other = format!("To o\n*\t{A}:refs/heads/other\t[new branch]\nDone\n");
        assert!(matches!(
            push_outcome(main, false, true, &other, ""),
            PushOutcome::Unexpected(_)
        ));
        // A reported success that git still failed on.
        let ok = format!("To o\n*\t{A}:refs/heads/main\t[new branch]\n");
        assert_eq!(
            push_outcome(main, false, false, &ok, ""),
            PushOutcome::Failed
        );
    }

    #[test]
    fn ref_names_must_be_full_and_not_refspecs() {
        ["refs/heads/main", "refs/rmu/devices/0000aaaa"]
            .iter()
            .for_each(|name| assert!(validate_ref_name(name).is_ok(), "{name}"));
        [
            "",
            "main",
            "HEAD",
            "objects/x",
            "--stdin",
            "-d",
            "+refs/heads/main",
            "refs/heads/a:refs/heads/b",
            ":refs/heads/main",
            "refs/heads/a\nb",
        ]
        .iter()
        .for_each(|name| {
            assert!(
                matches!(validate_ref_name(name), Err(Error::InvalidGitInput(_))),
                "{name:?}"
            );
        });
    }

    #[test]
    fn paths_are_validated() {
        ["docs/a/metadata", "a b", "x\ny", ".rmu/hub.json", "\u{e9}"]
            .iter()
            .for_each(|p| assert!(validate_path(p.as_bytes()).is_ok(), "{p:?}"));
        assert!(validate_path(b"\xff/\x01").is_ok());
        [
            "",
            "/a",
            "a/",
            "a//b",
            "./a",
            "a/../b",
            ".git/config",
            "a/.GIT",
            "a\0b",
        ]
        .iter()
        .for_each(|p| assert!(validate_path(p.as_bytes()).is_err(), "{p:?}"));
    }

    #[test]
    fn tree_paths_reject_duplicates_and_file_directory_clashes() {
        let check = |paths: &[&str]| check_tree_paths(paths.iter().map(|p| p.as_bytes()));
        assert!(check(&[]).is_ok());
        assert!(check(&["a", "a b", "b/c", "b/d", "ab/c"]).is_ok());
        assert!(check(&["a", "a"]).is_err());
        assert!(check(&["a/b/c", "a/b"]).is_err());
        assert!(check(&["a", "a/b"]).is_err());
    }

    #[test]
    fn quote_path_escapes_everything_but_printable_ascii() {
        assert_eq!(quote_path(b"docs/a b"), "\"docs/a b\"");
        assert_eq!(quote_path(b"\"q\"\\"), r#""\"q\"\\""#);
        assert_eq!(quote_path(b"a\nb\tc"), r#""a\nb\011c""#);
        assert_eq!(quote_path(b"\xff\x7f\xc3\xa9"), r#""\377\177\303\251""#);
    }

    #[test]
    fn signatures_format_raw_dates_and_reject_bad_identities() {
        let sig = |name: &str, email: &str, offset| Signature {
            name: name.into(),
            email: email.into(),
            time: 1_700_000_000,
            offset_minutes: offset,
        };
        assert_eq!(
            format_signature(&sig("Paper Pro", "tablet@example.invalid", 0)).unwrap(),
            "Paper Pro <tablet@example.invalid> 1700000000 +0000"
        );
        assert!(
            format_signature(&sig("a", "b", 330))
                .unwrap()
                .ends_with(" +0530")
        );
        assert!(
            format_signature(&sig("a", "b", -480))
                .unwrap()
                .ends_with(" -0800")
        );
        assert!(format_signature(&sig("a <x>", "b", 0)).is_err());
        assert!(format_signature(&sig("a", "b>", 0)).is_err());
        assert!(format_signature(&sig("a\n", "b", 0)).is_err());
        assert!(format_signature(&sig("a", "b", 24 * 60)).is_err());
    }

    #[test]
    fn commit_command_lists_parents_and_full_tree() {
        let sig = Signature {
            name: "rmu".into(),
            email: "rmu@example.invalid".into(),
            time: 1,
            offset_minutes: 0,
        };
        let meta = CommitMeta {
            parents: vec![oid(A), oid(B), oid(A), oid(B)],
            author: sig.clone(),
            committer: sig,
            message: "merge\n".into(),
        };
        let entries = vec![
            (
                b"docs/x".to_vec(),
                FileMode::Regular.bits(),
                DataRef::Mark(1),
            ),
            (
                b"run".to_vec(),
                FileMode::Executable.bits(),
                DataRef::Oid(oid(B)),
            ),
            (b"docs/y".to_vec(), 0o040000, DataRef::Oid(oid(A))),
        ];
        let stream = String::from_utf8(commit_command(&meta, &entries, 2).unwrap()).unwrap();
        assert_eq!(
            stream,
            format!(
                "commit {SCRATCH_REF}\nmark :2\n\
                 author rmu <rmu@example.invalid> 1 +0000\n\
                 committer rmu <rmu@example.invalid> 1 +0000\n\
                 data 6\nmerge\n\n\
                 from {A}\nmerge {B}\n\
                 deleteall\n\
                 M 100644 :1 \"docs/x\"\n\
                 M 100755 {B} \"run\"\n\
                 M 040000 {A} \"docs/y\"\n\
                 \nget-mark :2\nreset {SCRATCH_REF}\n\ndone\n"
            )
        );
    }

    /// A temporary directory with a hermetic [`Git`] for it.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("rmu-git-unit-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(path.join("home")).unwrap();
            Self(path)
        }

        fn git(&self) -> Git {
            Git::new()
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_COUNT", "0")
                .env("GIT_CONFIG_PARAMETERS", "")
                .env("HOME", self.0.join("home"))
                .env("XDG_CONFIG_HOME", self.0.join("home"))
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn truncated_stream_fails_without_writing_a_ref_or_crash_report() {
        let sandbox = Sandbox::new();
        let repo = sandbox
            .git()
            .init_bare(&sandbox.0.join("cache.git"), ObjectFormat::Sha1)
            .unwrap();
        let mut writer = repo.commit_writer(&NoProgress, None).unwrap();
        writer.add_bytes("a", FileMode::Regular, b"x").unwrap();
        let sig = Signature {
            name: "rmu".into(),
            email: "rmu@example.invalid".into(),
            time: 1,
            offset_minutes: 0,
        };
        let meta = CommitMeta {
            parents: vec![],
            author: sig.clone(),
            committer: sig,
            message: "cut\n".into(),
        };
        // As if rmu died after sending the whole commit but before the
        // reset and `done`: without `--done`, fast-import would accept
        // this and write the scratch ref.
        let stream = commit_command(&meta, &writer.entries, 2).unwrap();
        let cut = stream.windows(9).position(|w| w == b"\nget-mark").unwrap();
        writer.write(&stream[..=cut]).unwrap();
        drop(writer.stdin.take());
        let result = writer.process.wait_checked();
        assert!(
            matches!(&result, Err(Error::Git { stderr, .. }) if stderr.contains("stream ends early")),
            "{result:?}"
        );
        let crash_report = writer.crash_report.clone();
        assert!(crash_report.exists());
        drop(writer);
        assert!(!crash_report.exists());
        assert_eq!(repo.rev_parse(SCRATCH_REF).unwrap(), None);
    }

    #[test]
    fn commit_objects_parse_tree_parents_and_message() {
        let raw = format!(
            "tree {A}\nparent {B}\nparent {A}\nauthor X <x@example.invalid> 1 +0000\n\
             committer rmu <rmu@example.invalid> 2 +0000\ngpgsig -----BEGIN-----\n \n \
             parent {B}\n -----END-----\n\nsubject\n\nRmu-Device: Tablet\n"
        );
        let commit = parse_commit(raw.as_bytes()).unwrap();
        assert_eq!(commit.tree, oid(A));
        // A continuation line of another header is never a parent.
        assert_eq!(commit.parents, vec![oid(B), oid(A)]);
        assert_eq!(commit.message, "subject\n\nRmu-Device: Tablet\n");
        let root = parse_commit(format!("tree {A}\nauthor X <x> 1 +0000\n\n").as_bytes()).unwrap();
        assert!(root.parents.is_empty() && root.message.is_empty());
        [
            format!("parent {A}\n\nno tree\n"),
            format!("tree {A}\ntree {B}\n\ntwo trees\n"),
            "tree nothex\n\nx\n".to_string(),
        ]
        .iter()
        .for_each(|bad| {
            assert!(
                matches!(parse_commit(bad.as_bytes()), Err(Error::GitOutput { .. })),
                "{bad}"
            );
        });
    }

    #[test]
    fn tree_objects_parse_modes_names_and_binary_ids() {
        let binary = |hex: &str| -> Vec<u8> {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect()
        };
        let entry = |mode: &str, name: &[u8], hex: &str| {
            [mode.as_bytes(), b" ", name, b"\0", &binary(hex)].concat()
        };
        let raw = [
            entry("100644", b"a b\n\xff", A),
            entry("40000", b"docs", B),
            entry("160000", b"sub", A),
            entry("100755", b"x", B),
        ]
        .concat();
        let entries = parse_tree(&raw, ObjectFormat::Sha1).unwrap();
        let summary: Vec<_> = entries
            .iter()
            .map(|e| (e.mode, e.kind, e.oid.clone(), e.path.clone(), e.size))
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    0o100644,
                    ObjectKind::Blob,
                    oid(A),
                    b"a b\n\xff".to_vec(),
                    None
                ),
                (0o040000, ObjectKind::Tree, oid(B), b"docs".to_vec(), None),
                (0o160000, ObjectKind::Commit, oid(A), b"sub".to_vec(), None),
                (0o100755, ObjectKind::Blob, oid(B), b"x".to_vec(), None),
            ]
        );
        assert_eq!(parse_tree(b"", ObjectFormat::Sha1).unwrap(), vec![]);
        let long = format!("{A}{}", &B[..24]);
        let sha256 = parse_tree(&entry("100644", b"f", &long), ObjectFormat::Sha256).unwrap();
        assert_eq!(sha256[0].oid, oid(&long));
        let truncated = &raw[..raw.len() - 1];
        [
            truncated.to_vec(),
            entry("10064x", b"f", A),
            entry("100644", b"", A),
            entry("+100644", b"f", A),
            entry("100644", b"a/b", A),
            entry("40000", b".", A),
            entry("40000", b"..", A),
            b"100644 f".to_vec(),
            entry("100644", b"f", A)[..10].to_vec(),
        ]
        .iter()
        .for_each(|bad| {
            assert!(
                matches!(
                    parse_tree(bad, ObjectFormat::Sha1),
                    Err(Error::GitOutput { .. })
                ),
                "{bad:?}"
            );
        });
        // A SHA-1 tree read as SHA-256 runs out of id bytes.
        assert!(parse_tree(&entry("100644", b"f", A), ObjectFormat::Sha256).is_err());
    }

    #[test]
    fn tree_modes_are_canonical_as_git_reports_them() {
        let binary: Vec<u8> = (0..A.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&A[i..i + 2], 16).unwrap())
            .collect();
        [
            ("100664", 0o100644, ObjectKind::Blob),
            ("100600", 0o100644, ObjectKind::Blob),
            ("100775", 0o100755, ObjectKind::Blob),
            ("100700", 0o100755, ObjectKind::Blob),
            ("120777", 0o120000, ObjectKind::Blob),
            ("40755", 0o040000, ObjectKind::Tree),
            ("160755", 0o160000, ObjectKind::Commit),
            // No file type at all: git takes it for a gitlink.
            ("644", 0o160000, ObjectKind::Commit),
        ]
        .into_iter()
        .for_each(|(stored, mode, kind)| {
            let raw = [stored.as_bytes(), b" f\0", &binary].concat();
            let entries = parse_tree(&raw, ObjectFormat::Sha1).unwrap();
            assert_eq!((entries[0].mode, entries[0].kind), (mode, kind), "{stored}");
        });
    }

    #[test]
    fn rev_list_parents_lines() {
        let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let out = format!("{a} {b} {c}\n{b} {c}\n{c}\n");
        let parsed = parse_rev_list_parents(out.as_bytes()).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].commit.as_str(), a);
        assert_eq!(
            parsed[0].parents,
            vec![b.parse().unwrap(), c.parse::<Oid>().unwrap()]
        );
        assert!(parsed[2].parents.is_empty());
        assert!(parse_rev_list_parents(b"nothex\n").is_err());
        assert!(parse_rev_list_parents(format!("{a}  {b}\n").as_bytes()).is_err());
    }

    #[test]
    fn tree_diff_output() {
        let (t1, t2, t3) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
        let (zero, x, y) = ("0".repeat(40), "a".repeat(40), "b".repeat(40));
        let out = [
            format!("{t1} {t2}\n:000000 040000 {zero} {x} A\0d1\0"),
            format!(":040000 040000 {x} {y} M\0odd\nname\0"),
            format!("{t2} {t2}\n"),
            format!("{t2} {t3}\n:040000 000000 {x} {zero} D\0d1\0"),
        ]
        .concat();
        let parsed = parse_tree_diffs(out.as_bytes()).unwrap();
        assert_eq!(parsed.len(), 3);
        let ((old, new), changes) = &parsed[0];
        assert_eq!((old.as_str(), new.as_str()), (t1.as_str(), t2.as_str()));
        assert_eq!(changes.len(), 2);
        assert_eq!(
            (changes[0].old_mode, changes[0].new_mode, changes[0].status),
            (0, 0o040000, b'A')
        );
        // A path is taken verbatim, newlines included.
        assert_eq!(changes[1].path, b"odd\nname");
        assert!(parsed[1].1.is_empty());
        assert_eq!(parsed[2].1[0].status, b'D');
        assert!(parse_tree_diffs(b"").unwrap().is_empty());
        // An entry before any pair, a header without its path, a bad mode
        // or status, a header line that is not a pair.
        [
            format!(":000000 040000 {zero} {x} A\0d1\0"),
            format!("{t1} {t2}\n:000000 040000 {zero} {x} A\0"),
            format!("{t1} {t2}\n:+00000 040000 {zero} {x} A\0p\0"),
            format!("{t1} {t2}\n:000000 040000 {zero} {x} a\0p\0"),
            format!("{t1}\n"),
        ]
        .iter()
        .for_each(|bad| assert!(parse_tree_diffs(bad.as_bytes()).is_err(), "{bad:?}"));
    }
}
