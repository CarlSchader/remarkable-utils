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
//!   while a tablet is stopped; `core.hooksPath=/dev/null`: hooks from a
//!   user's template directory never run on rmu's own fetches, ref
//!   updates and pushes; `core.logAllRefUpdates=false`: reflog entries
//!   would need an auto-detected identity (a hostname). These are applied
//!   to every command — read-only commands ignore them.
//! - `GIT_TERMINAL_PROMPT=0` unless the caller says it is interactive.
//!
//! Commits are written with `git fast-import` ([`CommitWriter`]), whose
//! stream carries author and committer explicitly; identity is never
//! auto-detected, and nothing is signed. Blobs are streamed, so large
//! payloads need not be held in memory. No ref is updated unless the
//! caller asks ([`Repo::update_ref`], [`Repo::push`]).
//!
//! Output parsers are pure functions with unit tests; paths are raw
//! bytes throughout, since git allows any byte except NUL.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::str::FromStr;
use std::thread::{self, JoinHandle};

use crate::error::{Error, Result};
use crate::progress::Progress;

/// `-c` settings for every invocation (see the module docs).
const CONFIG: &[&str] = &[
    "gc.auto=0",
    "maintenance.auto=false",
    "core.hooksPath=/dev/null",
    "core.logAllRefUpdates=false",
];

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(String);

impl Oid {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Oid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let valid = matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
        valid
            .then(|| Oid(s.to_ascii_lowercase()))
            .ok_or_else(|| Error::InvalidGitInput(format!("not an object id: {s:?}")))
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

/// Mode of a file written by [`CommitWriter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileMode {
    Regular,
    Executable,
    Symlink,
}

impl FileMode {
    /// The mode as git writes it in trees.
    pub fn bits(self) -> u32 {
        match self {
            Self::Regular => 0o100644,
            Self::Executable => 0o100755,
            Self::Symlink => 0o120000,
        }
    }

    /// The writable mode for a [`TreeEntry::mode`], if it is a file.
    pub fn from_bits(mode: u32) -> Option<Self> {
        [Self::Regular, Self::Executable, Self::Symlink]
            .into_iter()
            .find(|m| m.bits() == mode)
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
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    /// The full message, written verbatim.
    pub message: String,
}

/// How to run `git`: interactivity and extra environment. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Git {
    interactive: bool,
    env: Vec<(OsString, OsString)>,
}

impl Default for Git {
    fn default() -> Self {
        Self {
            interactive: true,
            env: Vec::new(),
        }
    }
}

impl Git {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether git may prompt on the terminal for credentials (default
    /// `true`). Set `false` without a tty: prompts then fail instead of
    /// hanging (`GIT_TERMINAL_PROMPT=0`).
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

    /// Create a bare repository at `path` (re-initialising an existing
    /// one is harmless) with `HEAD` on `main` and no template files.
    pub fn init_bare(&self, path: &Path) -> Result<Repo> {
        let mut cmd = self.command(None, "init");
        cmd.args(["--bare", "--quiet", "--template=", "--initial-branch=main"])
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
        CONFIG.iter().for_each(|setting| {
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
    /// from `url`, without tags.
    pub fn fetch(&self, url: &str, refspecs: &[&str]) -> Result<()> {
        let mut cmd = self.command("fetch");
        cmd.args(["--quiet", "--no-tags", "--end-of-options"])
            .arg(url)
            .args(refspecs);
        run("fetch", &mut cmd).map(drop)
    }

    /// Push local `src` (an oid or ref) to the full remote ref `dst`.
    ///
    /// Without a lease, the remote accepts only a fast-forward (or a new
    /// ref). With one, the update is a compare-and-swap: it may rewind,
    /// but only if `dst` currently matches. Either refusal is
    /// [`Error::PushRejected`] — fetch, recompute, retry. Anything the
    /// remote itself refuses (hooks, size limits, a lost ref-lock race)
    /// is [`Error::PushFailed`] with the remote's message; failures
    /// before any ref status (auth, network) are [`Error::Git`].
    pub fn push(&self, url: &str, src: &str, dst: &str, lease: Option<&Expected>) -> Result<()> {
        if !dst.starts_with("refs/") {
            return Err(Error::InvalidGitInput(format!(
                "push destination must be a full ref name: {dst:?}"
            )));
        }
        let mut cmd = self.command("push");
        cmd.args(["--porcelain", "--no-verify"]);
        if let Some(expected) = lease {
            cmd.arg(format!("--force-with-lease={dst}:{}", expected.arg()));
        }
        cmd.arg("--end-of-options")
            .arg(url)
            .arg(format!("{src}:{dst}"));
        let output = output("push", &mut cmd)?;
        push_outcome(
            dst,
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
        .map_err(|error| match error {
            Error::Git { .. } => failure("push", &output),
            other => other,
        })
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
    /// its own ancestor). Missing objects are an error, not `false`.
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

    /// Point local ref `name` at `new`; with `old`, only if the ref
    /// currently has that value (atomic compare-and-swap).
    pub fn update_ref(&self, name: &str, new: &Oid, old: Option<&Expected>) -> Result<()> {
        let mut cmd = self.command("update-ref");
        cmd.args(["--end-of-options", name, new.as_str()]);
        if let Some(old) = old {
            cmd.arg(old.arg());
        }
        run("update-ref", &mut cmd).map(drop)
    }

    /// Every entry below `treeish` (a commit or tree), trees included,
    /// parent trees before their contents.
    pub fn ls_tree_recursive(&self, treeish: &str) -> Result<Vec<TreeEntry>> {
        let mut cmd = self.command("ls-tree");
        cmd.args(["-r", "-t", "-z", "--long", "--end-of-options", treeish]);
        parse_ls_tree(&run("ls-tree", &mut cmd)?)
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
            process,
            stdin: Some(BufWriter::with_capacity(64 * 1024, stdin)),
            stdout: BufReader::new(stdout),
            entries: Vec::new(),
            next_mark: 1,
            progress,
            transferred: 0,
            total,
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

impl CatFile {
    /// Look up `oid`; `None` if the repository does not have it.
    pub fn object(&mut self, oid: &Oid) -> Result<Option<ObjectReader<'_>>> {
        self.skip_pending()?;
        let stdin = self.stdin.as_mut().ok_or_else(|| closed("cat-file"))?;
        if let Err(error) = stdin.write_all(format!("{oid}\n").as_bytes()) {
            return Err(self.broken(error));
        }
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
                Ok(Some(ObjectReader {
                    cat: self,
                    kind,
                    size,
                }))
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
/// a size declared up front); existing blobs are referenced by id. No
/// ref is updated. Dropping the writer without committing kills
/// fast-import; objects already streamed stay as unreachable garbage.
pub struct CommitWriter<'p> {
    process: Process,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    entries: Vec<(Vec<u8>, FileMode, DataRef)>,
    next_mark: u64,
    progress: &'p dyn Progress,
    transferred: u64,
    total: Option<u64>,
}

enum DataRef {
    Mark(u64),
    Oid(Oid),
}

impl CommitWriter<'_> {
    /// Add a new file whose content is exactly `size` bytes from
    /// `reader`, returning its blob id. A reader that yields a different
    /// number of bytes is [`Error::BlobSizeMismatch`] and aborts the
    /// commit.
    pub fn add_blob(
        &mut self,
        path: impl Into<Vec<u8>>,
        mode: FileMode,
        size: u64,
        reader: &mut dyn Read,
    ) -> Result<Oid> {
        let path = path.into();
        validate_path(&path)?;
        let mark = self.next_mark;
        self.next_mark += 1;
        self.write(format!("blob\nmark :{mark}\ndata {size}\n").as_bytes())?;
        let copied = self.copy_blob(reader, size)?;
        if copied != size {
            let actual = copied + io::copy(reader, &mut io::sink()).unwrap_or(0);
            self.abort();
            return Err(Error::BlobSizeMismatch {
                path: String::from_utf8_lossy(&path).into_owned(),
                expected: size,
                actual,
            });
        }
        self.write(format!("\nget-mark :{mark}\n").as_bytes())?;
        let oid = self.read_mark()?;
        self.entries.push((path, mode, DataRef::Mark(mark)));
        Ok(oid)
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
    /// forward from a base tree). Its existence is checked at commit.
    pub fn add_existing(
        &mut self,
        path: impl Into<Vec<u8>>,
        mode: FileMode,
        oid: &Oid,
    ) -> Result<()> {
        let path = path.into();
        validate_path(&path)?;
        self.entries.push((path, mode, DataRef::Oid(oid.clone())));
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
        self.progress.bytes_done();
        let stdin = self.stdin.take().ok_or_else(|| closed("fast-import"))?;
        if let Err(error) = stdin.into_inner().map_err(|e| e.into_error()) {
            return Err(self.broken(error));
        }
        let mut line = Vec::new();
        let read = self.stdout.read_until(b'\n', &mut line);
        self.process.wait_checked()?;
        read?;
        parse_oid_line("fast-import", &line)
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

/// The fast-import stream for the commit itself: metadata, the full
/// tree, the commit id request, and a reset so no ref is written.
fn commit_command(
    meta: &CommitMeta,
    entries: &[(Vec<u8>, FileMode, DataRef)],
    mark: u64,
) -> Result<Vec<u8>> {
    let author = format_signature(&meta.author)?;
    let committer = format_signature(&meta.committer)?;
    let header = format!(
        "commit {SCRATCH_REF}\nmark :{mark}\nauthor {author}\ncommitter {committer}\ndata {}\n",
        meta.message.len()
    );
    let parents = meta
        .parents
        .iter()
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
            format!("M {:06o} {data} {}\n", mode.bits(), quote_path(path))
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

/// Interpret a finished push of `dst`. A plain [`Error::Git`] result
/// stands for "failed without a status for `dst`"; the caller fills in
/// the real exit status and stderr.
fn push_outcome(dst: &str, success: bool, stdout: &str, stderr: &str) -> Result<()> {
    let status = parse_push_porcelain(stdout)
        .into_iter()
        .find(|s| s.to == dst);
    match status {
        Some(s) if s.flag == '!' => {
            let reason = s.reason.unwrap_or_else(|| s.summary.clone());
            if s.summary == "[rejected]" {
                Err(Error::PushRejected {
                    reference: dst.to_string(),
                    reason,
                })
            } else {
                Err(Error::PushFailed {
                    reference: dst.to_string(),
                    reason: format!("{} ({reason})", s.summary),
                    stderr: stderr.trim().to_string(),
                })
            }
        }
        Some(_) if success => Ok(()),
        None if success => Err(output_error(
            "push",
            format!("no status reported for {dst}"),
        )),
        _ => Err(Error::Git {
            command: "push".into(),
            status: -1,
            stderr: stderr.trim().to_string(),
        }),
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

/// A tree cannot hold the same path twice, nor a file where another
/// path needs a directory.
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
            Err("both a file and a directory")
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

    const A: &str = "0123456789abcdef0123456789abcdef01234567";
    const B: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn oid(s: &str) -> Oid {
        s.parse().unwrap()
    }

    #[test]
    fn oid_accepts_sha1_and_sha256_hex_only() {
        assert_eq!(oid(&A.to_uppercase()).as_str(), A);
        assert!(format!("{A}{}", &B[..24]).parse::<Oid>().is_ok());
        assert!(A[..39].parse::<Oid>().is_err());
        assert!(format!("{}g", &A[..39]).parse::<Oid>().is_err());
        assert!("".parse::<Oid>().is_err());
    }

    #[test]
    fn file_mode_bits_round_trip() {
        [FileMode::Regular, FileMode::Executable, FileMode::Symlink]
            .into_iter()
            .for_each(|mode| assert_eq!(FileMode::from_bits(mode.bits()), Some(mode)));
        assert_eq!(FileMode::from_bits(0o040000), None);
        assert_eq!(FileMode::from_bits(0o160000), None);
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
        assert!(push_outcome(main, true, &line("*", "[new branch]"), "").is_ok());
        assert!(push_outcome(main, true, &line(" ", "1234567..89abcde"), "").is_ok());
        assert!(push_outcome(main, true, &line("=", "[up to date]"), "").is_ok());
        ["non-fast-forward", "fetch first", "stale info"].iter().for_each(|reason| {
            let result = push_outcome(main, false, &line("!", &format!("[rejected] ({reason})")), "error: failed");
            assert!(
                matches!(&result, Err(Error::PushRejected { reference, reason: r }) if reference == main && r == reason),
                "{result:?}"
            );
        });
        let hook = push_outcome(
            main,
            false,
            &line("!", "[remote rejected] (pre-receive hook declined)"),
            "remote: file too large: docs/x/payload.pdf\nerror: failed to push some refs\n",
        );
        assert!(
            matches!(&hook, Err(Error::PushFailed { reason, stderr, .. })
                if reason == "[remote rejected] (pre-receive hook declined)" && stderr.contains("payload.pdf")),
            "{hook:?}"
        );
        assert!(matches!(
            push_outcome(
                main,
                false,
                &line("!", "[remote failure] (remote failed to report status)"),
                ""
            ),
            Err(Error::PushFailed { .. })
        ));
    }

    #[test]
    fn push_outcome_without_a_status_line() {
        let main = "refs/heads/main";
        // Auth or network failure: nothing on stdout.
        assert!(matches!(
            push_outcome(main, false, "", "fatal: Could not read"),
            Err(Error::Git { .. })
        ));
        // A status for some other ref only.
        let other = format!("To o\n*\t{A}:refs/heads/other\t[new branch]\nDone\n");
        assert!(matches!(
            push_outcome(main, true, &other, ""),
            Err(Error::GitOutput { .. })
        ));
        // A reported success that git still failed on.
        let ok = format!("To o\n*\t{A}:refs/heads/main\t[new branch]\n");
        assert!(matches!(
            push_outcome(main, false, &ok, ""),
            Err(Error::Git { .. })
        ));
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
            parents: vec![oid(A), oid(B)],
            author: sig.clone(),
            committer: sig,
            message: "merge\n".into(),
        };
        let entries = vec![
            (b"docs/x".to_vec(), FileMode::Regular, DataRef::Mark(1)),
            (b"run".to_vec(), FileMode::Executable, DataRef::Oid(oid(B))),
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
                 \nget-mark :2\nreset {SCRATCH_REF}\n\ndone\n"
            )
        );
    }
}
