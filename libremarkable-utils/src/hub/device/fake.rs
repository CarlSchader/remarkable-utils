//! [`FakeTablet`]: the device layer over two local directories, for the
//! end-to-end tests of the sync cycle.
//!
//! It simulates xochitl (running or stopped), the boot id and the
//! watchdog, and injects faults: a mutating operation that fails without
//! effect, one that takes effect but reports failure (a lost
//! acknowledgement), a disconnection (rmu dying: every later operation
//! fails until [`FakeTablet::reconnect`], so no error path can clean
//! up), an apply killed after K moves, and user edits run before a
//! chosen operation (deferred while xochitl is stopped, since nobody can
//! edit then). It shares the lock protocol and the apply ordering with
//! [`super::SshTablet`], reports failures with the same error kinds, and
//! checks the caller's side of the contract: xochitl is stopped only with
//! the watchdog armed, and applied to only while stopped
//! ([`DeviceError::ContractViolation`]).
//!
//! Callbacks (download visitors, staging content, edits) run without any
//! internal lock held, so they may call back into the fake.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::apply::{ApplyStep, StagedPath};
use super::lock::{self, AcquireProbe, HeldLock, LockRequest, LockState};
use super::{
    ApplyPlan, ApplyReport, DeviceError, DeviceInfo, DownloadVisitor, ExactReader, StageContent,
    StatEntry, StatListing, Tablet, TabletPaths, transfer_paths,
};
use crate::error::{Error, Result};
use crate::hub::model::{DocId, StatSignature, TabletState, XochitlPath};

/// The kinds of operation, as logged by [`FakeTablet::ops`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    AcquireLock,
    ReleaseLock,
    LockState,
    CheckGuard,
    WatchdogFired,
    ArmWatchdog,
    DisarmWatchdog,
    StopXochitl,
    StartXochitl,
    XochitlActive,
    CloudPaired,
    DeviceInfo,
    StatAll,
    Download,
    ReadFiles,
    Sha256,
    ReadState,
    WriteState,
    Stage,
    ClearStaging,
    Apply,
}

impl OpKind {
    /// Operations that change the tablet; the ones fault injection counts.
    pub fn is_mutating(self) -> bool {
        matches!(
            self,
            OpKind::AcquireLock
                | OpKind::ReleaseLock
                | OpKind::ArmWatchdog
                | OpKind::DisarmWatchdog
                | OpKind::StopXochitl
                | OpKind::StartXochitl
                | OpKind::WriteState
                | OpKind::Stage
                | OpKind::ClearStaging
                | OpKind::Apply
        )
    }
}

/// An edit of the tablet's files, given the xochitl data dir.
pub type Edit = Box<dyn FnOnce(&Path) + Send>;

/// A fault to inject. Counts are relative to when the fault is injected
/// (`1` = the next one).
pub enum Fault {
    /// The `n`th mutating operation fails without any effect.
    FailMutation(usize),
    /// The `n`th mutating operation takes effect, then reports failure.
    LoseAck(usize),
    /// The `n`th mutating operation fails without effect, and so does
    /// every operation after it until [`FakeTablet::reconnect`]: the
    /// computer running rmu died there.
    DisconnectAt(usize),
    /// The `n`th mutating operation takes effect, then the connection is
    /// gone as with [`Fault::DisconnectAt`].
    DisconnectAfter(usize),
    /// The `n`th operation of any kind (reads included) fails without
    /// effect.
    FailOp(usize),
    /// The `n`th operation of any kind fails without effect, and every
    /// operation after it until [`FakeTablet::reconnect`].
    DisconnectAtOp(usize),
    /// The next download delivers `k` files, then fails as a lost
    /// connection.
    FailDownloadAfter(usize),
    /// During the next apply, the watchdog fires right after the `k`th
    /// move (the guard before the next document must then stop it).
    FireWatchdogAfterMoves(usize),
    /// The next apply stops after `k` moves (renames, removals, document
    /// deletions), without the final `sync`, as if the connection died:
    /// at the first step after the `k`th move, so a document whose last
    /// move was the `k`th is not reported as applied.
    KillApplyAfter(usize),
    /// Run `edit` before the `n`th operation of any kind (the user
    /// editing the tablet between two steps of the cycle). If xochitl is
    /// stopped then, the edit waits for the first operation after it runs
    /// again.
    EditBefore(usize, Edit),
}

/// What happens to a mutating operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationFault {
    Fail,
    LoseAck,
    DisconnectAt,
    DisconnectAfter,
}

/// What happens to an operation of any kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpFault {
    Fail,
    Disconnect,
}

enum Pending {
    Mutation(usize, MutationFault),
    Op(usize, OpFault),
    EditBefore(usize, Edit),
}

/// The armed watchdog: its timeout and the run that armed it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Watchdog {
    timeout: Duration,
    run: String,
}

struct FakeState {
    running: bool,
    boot_id: String,
    watchdog: Option<Watchdog>,
    /// Armed since xochitl last started: what a stop requires (a timer
    /// armed before an earlier start may have fired or expired since).
    armed_since_start: bool,
    cloud_paired: bool,
    info: DeviceInfo,
    ops: usize,
    mutations: usize,
    log: Vec<OpKind>,
    pending: Vec<Pending>,
    kill_apply_after: Option<usize>,
    fail_download_after: Option<usize>,
    fire_after_moves: Option<usize>,
    disconnected: bool,
}

impl FakeState {
    fn inject(&mut self, fault: Fault) {
        let mutations = self.mutations;
        let mutation = |n: usize, fault| Pending::Mutation(mutations + n, fault);
        let pending = match fault {
            Fault::FailMutation(n) => mutation(n, MutationFault::Fail),
            Fault::LoseAck(n) => mutation(n, MutationFault::LoseAck),
            Fault::DisconnectAt(n) => mutation(n, MutationFault::DisconnectAt),
            Fault::DisconnectAfter(n) => mutation(n, MutationFault::DisconnectAfter),
            Fault::FailOp(n) => Pending::Op(self.ops + n, OpFault::Fail),
            Fault::DisconnectAtOp(n) => Pending::Op(self.ops + n, OpFault::Disconnect),
            Fault::KillApplyAfter(k) => {
                self.kill_apply_after = Some(k);
                return;
            }
            Fault::FailDownloadAfter(k) => {
                self.fail_download_after = Some(k);
                return;
            }
            Fault::FireWatchdogAfterMoves(k) => {
                self.fire_after_moves = Some(k);
                return;
            }
            Fault::EditBefore(n, edit) => Pending::EditBefore(self.ops + n, edit),
        };
        self.pending.push(pending);
    }

    /// Take the edits due at operation `op`; while xochitl is stopped they
    /// move on to the next operation.
    fn due_edits(&mut self, op: usize) -> Vec<Edit> {
        let running = self.running;
        let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .map(|pending| match pending {
                Pending::EditBefore(at, edit) if at == op && !running => {
                    Pending::EditBefore(op + 1, edit)
                }
                other => other,
            })
            .partition(|pending| matches!(pending, Pending::EditBefore(at, _) if *at == op));
        self.pending = rest;
        due.into_iter()
            .filter_map(|pending| match pending {
                Pending::EditBefore(_, edit) => Some(edit),
                Pending::Mutation(..) | Pending::Op(..) => None,
            })
            .collect()
    }

    /// Take the fault planned for operation `op`, if any.
    fn op_fault(&mut self, op: usize) -> Option<OpFault> {
        let position = self
            .pending
            .iter()
            .position(|pending| matches!(pending, Pending::Op(at, _) if *at == op))?;
        match self.pending.remove(position) {
            Pending::Op(_, fault) => Some(fault),
            Pending::Mutation(..) | Pending::EditBefore(..) => None,
        }
    }

    /// Take the fault planned for mutation `n`, if any.
    fn mutation_fault(&mut self, n: usize) -> Option<MutationFault> {
        let position = self
            .pending
            .iter()
            .position(|pending| matches!(pending, Pending::Mutation(at, _) if *at == n))?;
        match self.pending.remove(position) {
            Pending::Mutation(_, fault) => Some(fault),
            Pending::Op(..) | Pending::EditBefore(..) => None,
        }
    }
}

/// How an operation that went ahead reports back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ack {
    Report,
    /// It took effect but the reply is lost (mutation number).
    Lose(usize),
}

/// A tablet simulated over local directories (see the module docs).
pub struct FakeTablet {
    xochitl: PathBuf,
    state: PathBuf,
    paths: TabletPaths,
    inner: Mutex<FakeState>,
}

/// Builds a [`FakeTablet`].
pub struct FakeTabletBuilder {
    root: PathBuf,
    files: Vec<(String, Vec<u8>)>,
    running: bool,
    cloud_paired: bool,
    info: DeviceInfo,
    hub_state: Option<TabletState>,
    faults: Vec<Fault>,
}

impl FakeTabletBuilder {
    /// Seed a file in the xochitl data dir.
    pub fn file(mut self, path: impl Into<String>, contents: impl Into<Vec<u8>>) -> Self {
        self.files.push((path.into(), contents.into()));
        self
    }

    /// Start with xochitl stopped (default: running).
    pub fn stopped(mut self) -> Self {
        self.running = false;
        self
    }

    pub fn cloud_paired(mut self, paired: bool) -> Self {
        self.cloud_paired = paired;
        self
    }

    pub fn device_info(mut self, model: &str, firmware: &str) -> Self {
        self.info = DeviceInfo {
            model: Some(model.to_string()),
            firmware: Some(firmware.to_string()),
        };
        self
    }

    /// Seed the hub state file.
    pub fn hub_state(mut self, state: TabletState) -> Self {
        self.hub_state = Some(state);
        self
    }

    pub fn fault(mut self, fault: Fault) -> Self {
        self.faults.push(fault);
        self
    }

    /// Create `<root>/xochitl` and `<root>/state` and seed them.
    pub fn build(self) -> Result<FakeTablet> {
        let xochitl = self.root.join("xochitl");
        let state = self.root.join("state");
        fs::create_dir_all(&xochitl)?;
        fs::create_dir_all(&state)?;
        let paths = TabletPaths {
            xochitl_dir: xochitl.to_string_lossy().into_owned(),
            state_dir: state.to_string_lossy().into_owned(),
            xochitl_conf: String::new(),
            boot_id: String::new(),
        };
        let mut inner = FakeState {
            running: self.running,
            boot_id: new_boot_id(),
            watchdog: None,
            cloud_paired: self.cloud_paired,
            info: self.info,
            ops: 0,
            mutations: 0,
            log: Vec::new(),
            pending: Vec::new(),
            kill_apply_after: None,
            fail_download_after: None,
            fire_after_moves: None,
            disconnected: false,
            armed_since_start: false,
        };
        self.faults
            .into_iter()
            .for_each(|fault| inner.inject(fault));
        let tablet = FakeTablet {
            xochitl,
            state,
            paths,
            inner: Mutex::new(inner),
        };
        self.files
            .iter()
            .try_for_each(|(path, contents)| tablet.write_file(path, contents))?;
        if let Some(hub_state) = self.hub_state {
            write_atomic(
                Path::new(&tablet.paths.state_file()),
                hub_state.to_json().as_bytes(),
            )?;
        }
        Ok(tablet)
    }
}

fn new_boot_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let temp = path.with_file_name(format!(".rmu-fake-{}", uuid::Uuid::new_v4().simple()));
    fs::write(&temp, data)?;
    fs::rename(&temp, path)
}

#[cfg(unix)]
fn same_filesystem(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    matches!((fs::metadata(a), fs::metadata(b)), (Ok(a), Ok(b)) if a.dev() == b.dev())
}

#[cfg(not(unix))]
fn same_filesystem(a: &Path, b: &Path) -> bool {
    a.exists() && b.exists()
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// A failure as a real tablet reports it: ssh's exit status 255.
fn transport_error(detail: String) -> Error {
    Error::Remote {
        status: 255,
        stderr: format!("fake tablet: {detail}"),
    }
}

fn remote_failure(detail: String) -> Error {
    Error::Remote {
        status: 1,
        stderr: detail,
    }
}

fn incomplete(applied: &[DocId], reason: String) -> Error {
    DeviceError::ApplyIncomplete {
        applied: applied.to_vec(),
        reason,
    }
    .into()
}

#[cfg(unix)]
fn signature(meta: &fs::Metadata) -> (u32, StatSignature) {
    use std::os::unix::fs::MetadataExt;
    let ns = |secs: i64, nanos: i64| secs * 1_000_000_000 + nanos;
    (
        meta.mode(),
        StatSignature {
            size: meta.size(),
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
            inode: meta.ino(),
        },
    )
}

#[cfg(not(unix))]
fn signature(meta: &fs::Metadata) -> (u32, StatSignature) {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64);
    let mode = if meta.is_file() {
        0o100_644
    } else if meta.file_type().is_symlink() {
        0o120_777
    } else {
        0
    };
    (
        mode,
        StatSignature {
            size: meta.len(),
            mtime_ns,
            ctime_ns: mtime_ns,
            inode: 0,
        },
    )
}

/// Every non-directory entry below `dir`; names with a newline are
/// counted and pruned, as on a real tablet.
fn walk(root: &Path, dir: &Path, listing: &mut StatListing) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    entries.iter().try_for_each(|entry| {
        let path = entry.path();
        if entry.file_name().to_string_lossy().contains(['\n', '\r']) {
            listing.unlisted += 1;
            return Ok(());
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            return walk(root, &path, listing);
        }
        let relative = path
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let (mode, stat) = signature(&meta);
        listing.entries.push(StatEntry {
            path: relative,
            mode,
            stat,
        });
        Ok(())
    })
}

impl FakeTablet {
    /// A builder for a fake tablet living under `root` (which the caller
    /// owns and removes).
    pub fn builder(root: impl Into<PathBuf>) -> FakeTabletBuilder {
        FakeTabletBuilder {
            root: root.into(),
            files: Vec::new(),
            running: true,
            cloud_paired: false,
            info: DeviceInfo {
                model: Some("reMarkable Fake".to_string()),
                firmware: Some("0.0.0".to_string()),
            },
            hub_state: None,
            faults: Vec::new(),
        }
    }

    pub fn xochitl_dir(&self) -> &Path {
        &self.xochitl
    }

    pub fn state_dir(&self) -> &Path {
        &self.state
    }

    /// The layout of the state dir is the real one ([`TabletPaths`]).
    pub fn paths(&self) -> &TabletPaths {
        &self.paths
    }

    fn state(&self) -> MutexGuard<'_, FakeState> {
        // A panicking test callback must not hide the tablet from the rest
        // of the test.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn data(&self, path: &str) -> PathBuf {
        self.xochitl.join(path)
    }

    /// Write a file in the data dir as the user (or xochitl) would.
    pub fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        let target = self.data(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, contents)?;
        Ok(())
    }

    pub fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.data(path)).ok()
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        Ok(remove_if_present(&self.data(path))?)
    }

    pub fn is_running(&self) -> bool {
        self.state().running
    }

    pub fn boot_id(&self) -> String {
        self.state().boot_id.clone()
    }

    /// Reboot: a new boot id, xochitl running, no watchdog. Files, the
    /// lock and staging survive (they live on `/home`).
    pub fn reboot(&self) {
        let mut state = self.state();
        state.boot_id = new_boot_id();
        state.running = true;
        state.watchdog = None;
    }

    /// Fire the armed watchdog now: try to leave the marker, and start
    /// xochitl even if that fails (e.g. the state dir is gone). Returns
    /// whether one was armed.
    pub fn fire_watchdog(&self) -> bool {
        let mut state = self.state();
        if state.watchdog.take().is_none() {
            return false;
        }
        let _ = fs::write(self.paths.watchdog_marker(), b"");
        state.running = true;
        true
    }

    /// The armed watchdog's timeout.
    pub fn watchdog_armed(&self) -> Option<Duration> {
        self.state()
            .watchdog
            .as_ref()
            .map(|watchdog| watchdog.timeout)
    }

    pub fn inject(&self, fault: Fault) {
        self.state().inject(fault);
    }

    /// Every operation so far, in order.
    pub fn ops(&self) -> Vec<OpKind> {
        self.state().log.clone()
    }

    /// Mutating operations so far (what the mutation faults count).
    pub fn mutations(&self) -> usize {
        self.state().mutations
    }

    /// Whether a [`Fault::DisconnectAt`]/[`Fault::DisconnectAfter`] fired.
    pub fn is_disconnected(&self) -> bool {
        self.state().disconnected
    }

    /// Accept operations again after a disconnection (a new rmu run).
    pub fn reconnect(&self) {
        self.state().disconnected = false;
    }

    /// Count the operation, run due edits, apply injected faults.
    fn begin(&self, kind: OpKind) -> Result<Ack> {
        let (edits, outcome) = {
            let mut state = self.state();
            state.ops += 1;
            state.log.push(kind);
            let op = state.ops;
            let edits = state.due_edits(op);
            let outcome = if state.disconnected {
                Err(transport_error("disconnected".to_string()))
            } else if let Some(fault) = state.op_fault(op) {
                if fault == OpFault::Disconnect {
                    state.disconnected = true;
                }
                Err(transport_error(format!(
                    "injected failure of operation {op}"
                )))
            } else if kind.is_mutating() {
                state.mutations += 1;
                let n = state.mutations;
                match state.mutation_fault(n) {
                    Some(MutationFault::Fail) => {
                        Err(transport_error(format!("injected failure of mutation {n}")))
                    }
                    Some(MutationFault::DisconnectAt) => {
                        state.disconnected = true;
                        Err(transport_error(format!("disconnected at mutation {n}")))
                    }
                    Some(MutationFault::DisconnectAfter) => {
                        state.disconnected = true;
                        Ok(Ack::Lose(n))
                    }
                    Some(MutationFault::LoseAck) => Ok(Ack::Lose(n)),
                    None => Ok(Ack::Report),
                }
            } else {
                Ok(Ack::Report)
            };
            (edits, outcome)
        };
        edits.into_iter().for_each(|edit| edit(&self.xochitl));
        outcome
    }

    fn lost(n: usize) -> Error {
        transport_error(format!("connection lost after mutation {n} took effect"))
    }

    fn op<T>(&self, kind: OpKind, body: impl FnOnce() -> Result<T>) -> Result<T> {
        let ack = self.begin(kind)?;
        let value = body()?;
        match ack {
            Ack::Report => Ok(value),
            Ack::Lose(n) => Err(Self::lost(n)),
        }
    }

    fn start_reply_lost(&self, lock: &HeldLock, error: Error) -> Result<i64> {
        if self.is_disconnected() {
            return Err(error);
        }
        self.may_start(lock)?;
        if self.is_running() {
            Ok(0)
        } else {
            Err(DeviceError::XochitlStart(error.to_string()).into())
        }
    }

    fn guarded<T>(
        &self,
        kind: OpKind,
        lock: &HeldLock,
        body: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.op(kind, || {
            self.guard(lock)?;
            body()
        })
    }

    fn guard(&self, lock: &HeldLock) -> Result<()> {
        if Path::new(&self.paths.watchdog_marker()).exists() {
            return Err(DeviceError::WatchdogFired.into());
        }
        self.owns(lock)?;
        let owner = fs::read_to_string(self.paths.lock_owner()).unwrap_or_default();
        let boot = format!("boot_id={}", self.boot_id());
        if owner.lines().any(|line| line == boot) {
            Ok(())
        } else {
            Err(DeviceError::Rebooted.into())
        }
    }

    /// Starting xochitl is refused only by another run's lock.
    fn may_start(&self, lock: &HeldLock) -> Result<()> {
        if Path::new(&self.paths.lock_dir()).is_dir() {
            self.owns(lock)
        } else {
            Ok(())
        }
    }

    fn owns(&self, lock: &HeldLock) -> Result<()> {
        let owner = fs::read_to_string(self.paths.lock_owner()).unwrap_or_default();
        if owner.lines().any(|line| line == lock::run_line(lock.run())) {
            Ok(())
        } else {
            Err(DeviceError::LockLost.into())
        }
    }

    fn write_owner(&self, request: &LockRequest, boot_id: &str) -> io::Result<()> {
        write_atomic(
            Path::new(&self.paths.lock_owner()),
            lock::owner_record(request, boot_id).as_bytes(),
        )?;
        remove_if_present(Path::new(&self.paths.watchdog_marker()))
    }

    /// The owner record's version, as on a real tablet: the SHA-256 of
    /// its bytes (of nothing, if it is missing).
    fn owner_version(&self) -> String {
        let bytes = fs::read(self.paths.lock_owner()).unwrap_or_default();
        format!("{:x}", Sha256::digest(bytes))
    }

    fn staged(&self, lock: &HeldLock, path: &str) -> PathBuf {
        Path::new(&self.paths.staging_dir(lock)).join(path)
    }

    /// Execute one apply step as the real script does.
    fn apply_step(&self, lock: &HeldLock, step: &ApplyStep) -> Result<()> {
        match step {
            ApplyStep::Guard => self.guard(lock),
            ApplyStep::MakeDir(dir) => Ok(fs::create_dir_all(self.data(dir))?),
            ApplyStep::Move(path) => {
                let target = self.data(path);
                if fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_dir()) {
                    return Err(remote_failure(format!("mv: {path} is a directory")));
                }
                fs::rename(self.staged(lock, path), target)
                    .map_err(|error| remote_failure(format!("mv: {path}: {error}")))
            }
            ApplyStep::Remove(path) => match fs::symlink_metadata(self.data(path)) {
                Ok(meta) if meta.is_dir() => {
                    Err(remote_failure(format!("rm: {path} is a directory")))
                }
                Ok(_) => Ok(fs::remove_file(self.data(path))?),
                Err(_) => Ok(()),
            },
            ApplyStep::DeleteSet(doc) => {
                let prefix = format!("{doc}.");
                remove_if_present(&self.data(doc.as_str()))?;
                fs::read_dir(&self.xochitl)?
                    .collect::<io::Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
                    .try_for_each(|entry| remove_if_present(&entry.path()))?;
                Ok(())
            }
            ApplyStep::Done(_) => Ok(()),
        }
    }

    /// The checks before an apply changes anything, as the real script
    /// makes them: staging on the data dir's filesystem, every staged
    /// file a regular file of its planned size; then executables get
    /// their mode.
    fn check_staged(&self, lock: &HeldLock, staged: &[StagedPath]) -> Result<()> {
        let staging = PathBuf::from(self.paths.staging_dir(lock));
        if !same_filesystem(&staging, &self.xochitl) {
            return Err(incomplete(
                &[],
                "rmu: staging is not on the data filesystem".to_string(),
            ));
        }
        staged.iter().try_for_each(|file| {
            let path = staging.join(&file.path);
            let ok = fs::symlink_metadata(&path)
                .is_ok_and(|meta| meta.is_file() && meta.len() == file.size);
            if !ok {
                return Err(incomplete(
                    &[],
                    format!("rmu: not staged as planned: {}", file.path),
                ));
            }
            if file.executable {
                set_executable(&path)?;
            }
            Ok(())
        })
    }

    /// Run the steps; `Err` carries the documents finished so far.
    fn run_apply(
        &self,
        lock: &HeldLock,
        plan: &ApplyPlan,
        steps: &[ApplyStep],
        kill_after: Option<usize>,
    ) -> Result<ApplyReport> {
        let staged = plan.staged();
        if !staged.is_empty() {
            self.check_staged(lock, &staged)?;
        }
        let fire_after = self.state().fire_after_moves.take();
        let (applied, _) =
            steps
                .iter()
                .try_fold((Vec::new(), 0usize), |(mut applied, moves), step| {
                    let killable = step.is_mutation() || matches!(step, ApplyStep::Done(_));
                    if killable && kill_after == Some(moves) {
                        return Err(incomplete(
                            &applied,
                            format!("fake tablet: connection lost after {moves} move(s)"),
                        ));
                    }
                    match self.apply_step(lock, step) {
                        Ok(()) => {}
                        Err(Error::HubDevice(
                            guard @ (DeviceError::WatchdogFired
                            | DeviceError::LockLost
                            | DeviceError::Rebooted),
                        )) => return Err(guard.into()),
                        Err(error) => return Err(incomplete(&applied, error.to_string())),
                    }
                    if let ApplyStep::Done(doc) = step {
                        applied.push(doc.clone());
                    }
                    let moves = moves + usize::from(step.is_mutation());
                    if step.is_mutation() && fire_after == Some(moves) {
                        self.fire_watchdog();
                    }
                    Ok((applied, moves))
                })?;
        let written = staged
            .iter()
            .map(|file| {
                let (mode, stat) = signature(&fs::symlink_metadata(self.data(&file.path))?);
                Ok(StatEntry {
                    path: file.path.clone(),
                    mode,
                    stat,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ApplyReport {
            applied,
            written,
            recorded_at_ns: now_ns(),
        })
    }
}

impl Tablet for FakeTablet {
    fn acquire_lock(&self, request: &LockRequest, break_lock: bool) -> Result<HeldLock> {
        self.op(OpKind::AcquireLock, || {
            let boot_id = self.boot_id();
            let probe = || -> Result<AcquireProbe> {
                fs::create_dir_all(&self.state)?;
                match fs::create_dir(self.paths.lock_dir()) {
                    Ok(()) => {
                        self.write_owner(request, &boot_id)?;
                        return Ok(AcquireProbe::Acquired {
                            boot_id: boot_id.clone(),
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                let owner = fs::read(self.paths.lock_owner()).unwrap_or_default();
                Ok(AcquireProbe::Held {
                    boot_id: boot_id.clone(),
                    watchdog_fired: Path::new(&self.paths.watchdog_marker()).exists(),
                    owner_text: String::from_utf8_lossy(&owner).into_owned(),
                    version: self.owner_version(),
                })
            };
            let take_over = |version: &str| -> Result<Option<String>> {
                if !Path::new(&self.paths.lock_dir()).is_dir() || self.owner_version() != version {
                    return Ok(None);
                }
                self.write_owner(request, &boot_id)?;
                Ok(Some(boot_id.clone()))
            };
            lock::acquire(request, break_lock, &probe, &take_over)
        })
    }

    fn release_lock(&self, lock: &HeldLock) -> Result<()> {
        self.op(OpKind::ReleaseLock, || {
            self.owns(lock)?;
            Ok(fs::remove_dir_all(self.paths.lock_dir())?)
        })
    }

    fn lock_state(&self) -> Result<LockState> {
        self.op(OpKind::LockState, || {
            Ok(if Path::new(&self.paths.lock_dir()).is_dir() {
                let owner = fs::read_to_string(self.paths.lock_owner()).unwrap_or_default();
                LockState::Held(lock::parse_owner(&owner))
            } else {
                LockState::Free
            })
        })
    }

    fn check_guard(&self, lock: &HeldLock) -> Result<()> {
        self.op(OpKind::CheckGuard, || self.guard(lock))
    }

    fn watchdog_fired(&self) -> Result<bool> {
        self.op(OpKind::WatchdogFired, || {
            Ok(Path::new(&self.paths.watchdog_marker()).exists())
        })
    }

    fn arm_watchdog(&self, lock: &HeldLock, timeout: Duration) -> Result<()> {
        self.guarded(OpKind::ArmWatchdog, lock, || {
            let mut state = self.state();
            state.watchdog = Some(Watchdog {
                timeout,
                run: lock.run().to_string(),
            });
            state.armed_since_start = true;
            Ok(())
        })
    }

    /// A disarm while xochitl is stopped is a contract violation (the
    /// real tablet refuses it); another run's watchdog is left alone.
    fn disarm_watchdog(&self, lock: &HeldLock) -> Result<()> {
        self.guarded(OpKind::DisarmWatchdog, lock, || {
            let mut state = self.state();
            if !state.running {
                return Err(DeviceError::ContractViolation(
                    "watchdog disarmed while xochitl is stopped".to_string(),
                )
                .into());
            }
            if state
                .watchdog
                .as_ref()
                .is_some_and(|watchdog| watchdog.run == lock.run())
            {
                state.watchdog = None;
            }
            Ok(())
        })
    }

    fn stop_xochitl(&self, lock: &HeldLock) -> Result<()> {
        self.guarded(OpKind::StopXochitl, lock, || {
            let mut state = self.state();
            let armed = state
                .watchdog
                .as_ref()
                .is_some_and(|watchdog| watchdog.run == lock.run());
            if !armed || !state.armed_since_start {
                return Err(DeviceError::ContractViolation(
                    "xochitl stopped without a watchdog armed by this run since the last start"
                        .to_string(),
                )
                .into());
            }
            state.running = false;
            Ok(())
        })
    }

    /// A lost reply is handled as a real tablet handles it: over a new
    /// connection, check the lock and whether xochitl is up (then `0`).
    fn start_xochitl(&self, lock: &HeldLock) -> Result<i64> {
        match self.begin(OpKind::StartXochitl) {
            Ok(ack) => {
                self.may_start(lock)?;
                let time = now_ns();
                let mut state = self.state();
                state.running = true;
                state.armed_since_start = false;
                drop(state);
                match ack {
                    Ack::Report => Ok(time),
                    Ack::Lose(n) => self.start_reply_lost(lock, Self::lost(n)),
                }
            }
            Err(error) => self.start_reply_lost(lock, error),
        }
    }

    fn xochitl_active(&self) -> Result<bool> {
        self.op(OpKind::XochitlActive, || Ok(self.is_running()))
    }

    fn cloud_paired(&self) -> Result<bool> {
        self.op(OpKind::CloudPaired, || Ok(self.state().cloud_paired))
    }

    fn device_info(&self) -> Result<DeviceInfo> {
        self.op(OpKind::DeviceInfo, || Ok(self.state().info.clone()))
    }

    fn stat_all(&self) -> Result<StatListing> {
        self.op(OpKind::StatAll, || {
            let mut listing = StatListing::default();
            walk(&self.xochitl, &self.xochitl, &mut listing)?;
            listing.entries.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(listing)
        })
    }

    fn download(&self, files: &[XochitlPath], visit: &mut DownloadVisitor<'_>) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        transfer_paths(files)?;
        self.op(OpKind::Download, || {
            let fail_after = self.state().fail_download_after.take();
            files.iter().enumerate().try_for_each(|(delivered, file)| {
                if fail_after == Some(delivered) {
                    return Err(transport_error(format!(
                        "connection lost after {delivered} file(s)"
                    )));
                }
                let path = self.data(file.as_str());
                let meta = fs::symlink_metadata(&path)
                    .map_err(|_| Error::from(DeviceError::Download(format!("missing {file}"))))?;
                if !meta.is_file() {
                    return Err(
                        DeviceError::Download(format!("{file} is not a regular file")).into(),
                    );
                }
                let mut reader = fs::File::open(&path)?.take(meta.len());
                visit(file, meta.len(), &mut reader)
            })
        })
    }

    fn read_files(&self, files: &[XochitlPath]) -> Result<BTreeMap<XochitlPath, Option<Vec<u8>>>> {
        if files.is_empty() {
            return Ok(BTreeMap::new());
        }
        transfer_paths(files)?;
        self.op(OpKind::ReadFiles, || {
            files
                .iter()
                .map(|file| {
                    let path = self.data(file.as_str());
                    let contents = match fs::symlink_metadata(&path) {
                        Err(_) => None,
                        Ok(meta) if meta.is_file() => Some(fs::read(&path)?),
                        Ok(_) => {
                            return Err(remote_failure(format!("rmu: not a regular file: {file}")));
                        }
                    };
                    Ok((file.clone(), contents))
                })
                .collect()
        })
    }

    fn sha256(&self, files: &[XochitlPath]) -> Result<BTreeMap<XochitlPath, Option<String>>> {
        if files.is_empty() {
            return Ok(BTreeMap::new());
        }
        transfer_paths(files)?;
        self.op(OpKind::Sha256, || {
            files
                .iter()
                .map(|file| {
                    let path = self.data(file.as_str());
                    let hash = match fs::symlink_metadata(&path) {
                        Err(_) => None,
                        Ok(meta) if meta.is_file() => {
                            let mut hasher = Sha256::new();
                            io::copy(&mut fs::File::open(&path)?, &mut hasher)?;
                            Some(format!("{:x}", hasher.finalize()))
                        }
                        Ok(_) => {
                            return Err(remote_failure(format!("rmu: not a regular file: {file}")));
                        }
                    };
                    Ok((file.clone(), hash))
                })
                .collect()
        })
    }

    fn read_state(&self) -> Result<Option<TabletState>> {
        self.op(OpKind::ReadState, || {
            match fs::read(self.paths.state_file()) {
                Ok(bytes) => Ok(Some(TabletState::parse(&bytes)?)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
    }

    fn write_state(&self, lock: &HeldLock, state: &TabletState) -> Result<()> {
        self.guarded(OpKind::WriteState, lock, || {
            Ok(write_atomic(
                Path::new(&self.paths.state_file()),
                state.to_json().as_bytes(),
            )?)
        })
    }

    fn stage(
        &self,
        lock: &HeldLock,
        files: &[XochitlPath],
        content: &mut StageContent<'_>,
    ) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        transfer_paths(files)?;
        self.guarded(OpKind::Stage, lock, || {
            files.iter().try_for_each(|file| {
                let target = self.staged(lock, file.as_str());
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let stage_error = |detail: String| -> Error {
                    DeviceError::Stage {
                        path: file.to_string(),
                        detail,
                    }
                    .into()
                };
                let mut calls = 0;
                content(file, &mut |size, reader| {
                    calls += 1;
                    if calls > 1 {
                        return Err(stage_error("content supplied twice".to_string()));
                    }
                    let mut exact = ExactReader::new(reader, size);
                    let mut out = fs::File::create(&target)?;
                    match (io::copy(&mut exact, &mut out), exact.problem) {
                        (Err(_), Some(problem)) => Err(stage_error(problem)),
                        (result, _) => Ok(result.map(drop)?),
                    }
                })?;
                if calls == 1 {
                    Ok(())
                } else {
                    Err(stage_error("no content supplied".to_string()))
                }
            })
        })
    }

    fn clear_staging(&self, lock: &HeldLock) -> Result<()> {
        self.guarded(OpKind::ClearStaging, lock, || {
            Ok(remove_if_present(Path::new(&self.paths.staging_root()))?)
        })
    }

    /// Transport failures are reported as a real tablet reports them for
    /// an apply: [`DeviceError::ApplyIncomplete`] with the documents
    /// finished before the connection went.
    fn apply(&self, lock: &HeldLock, plan: &ApplyPlan) -> Result<ApplyReport> {
        let steps = plan.steps()?;
        let ack = self
            .begin(OpKind::Apply)
            .map_err(|error| incomplete(&[], error.to_string()))?;
        self.guard(lock)?;
        let kill_after = {
            let mut state = self.state();
            if state.running {
                return Err(DeviceError::ContractViolation(
                    "apply while xochitl is running".to_string(),
                )
                .into());
            }
            state.kill_apply_after.take()
        };
        let report = self.run_apply(lock, plan, &steps, kill_after)?;
        match ack {
            Ack::Report => Ok(report),
            Ack::Lose(n) => Err(incomplete(&report.applied, Self::lost(n).to_string())),
        }
    }
}
