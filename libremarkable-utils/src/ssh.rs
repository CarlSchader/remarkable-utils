//! SSH transport for talking to a reMarkable tablet.
//!
//! Shells out to the system `ssh` binary rather than linking an SSH
//! library: users get their ssh config, keys, agent, and known-hosts
//! behavior for free, and the crate stays free of native dependencies.
//!
//! Authentication defaults to whatever the user's ssh setup does (keys,
//! agent, interactive prompts). Password authentication is supported by
//! re-executing the current binary as an `SSH_ASKPASS` helper with
//! `SSH_ASKPASS_REQUIRE=force` (requires OpenSSH >= 8.4). The password
//! is handed to the helper via an environment variable — never argv —
//! so it is not visible to other users in `ps`.
//!
//! Connections are multiplexed via `ControlMaster` by default so one
//! logical operation (which may issue several ssh commands) only
//! authenticates once.

use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::{self, JoinHandle};

#[cfg(unix)]
use std::{os::fd::AsFd, os::unix::net::UnixStream, sync::Arc};

use crate::error::{Error, Result};
use crate::progress::{NoProgress, Progress};

/// Default hostname of a reMarkable tablet when connected over USB.
pub const DEFAULT_USB_HOST: &str = "10.11.99.1";

/// Default SSH user on the tablet.
pub const DEFAULT_SSH_USER: &str = "root";

/// Default SSH port.
pub const DEFAULT_SSH_PORT: u16 = 22;

/// Set to `1` on child ssh processes; when the current binary is
/// re-executed with this set, it must behave as an askpass helper.
pub const ASKPASS_MODE_ENV: &str = "RMU_ASKPASS_MODE";

/// Carries the password to the askpass helper invocation.
pub const ASKPASS_PASSWORD_ENV: &str = "RMU_ASKPASS_PASSWORD";

/// How ssh should authenticate.
#[derive(Clone, Default)]
pub enum Auth {
    /// Whatever the user's ssh config/agent/keys provide.
    #[default]
    Default,
    /// Password auth via the self-askpass mechanism.
    Password(String),
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::Default => f.write_str("Default"),
            Auth::Password(_) => f.write_str("Password(<redacted>)"),
        }
    }
}

/// Connection parameters for a device.
#[derive(Debug, Clone)]
pub struct SshOptions {
    /// ssh destination, passed to the binary verbatim: `host` or
    /// `user@host`. Plain hostnames get full ssh-config resolution
    /// (`HostName`, `User`, `Port`, `IdentityFile`, ...).
    pub destination: String,
    /// Explicit port; `None` lets ssh config / defaults decide.
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
    /// Extra `-o` options passed verbatim to ssh.
    pub extra_options: Vec<String>,
    pub auth: Auth,
    /// Multiplex connections via ControlMaster (recommended).
    pub multiplex: bool,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            destination: format!("{DEFAULT_SSH_USER}@{DEFAULT_USB_HOST}"),
            port: None,
            identity_file: None,
            extra_options: Vec::new(),
            auth: Auth::Default,
            multiplex: true,
        }
    }
}

/// A (possibly multiplexed) ssh session to one device.
///
/// Dropping the session tears down the ControlMaster socket.
pub struct SshSession {
    opts: SshOptions,
    askpass_exe: Option<PathBuf>,
    control_dir: Option<PathBuf>,
    #[cfg(test)]
    ssh_exe: Option<PathBuf>,
}

impl SshSession {
    pub fn new(opts: SshOptions) -> Result<Self> {
        let askpass_exe = match opts.auth {
            Auth::Password(_) => Some(std::env::current_exe()?),
            Auth::Default => None,
        };
        let control_dir = if opts.multiplex {
            make_control_dir()
        } else {
            None
        };
        Ok(Self {
            opts,
            askpass_exe,
            control_dir,
            #[cfg(test)]
            ssh_exe: None,
        })
    }

    /// The ssh destination (`host` or `user@host`).
    pub fn target(&self) -> String {
        self.opts.destination.clone()
    }

    /// ssh invocation with all options and env set, but no destination.
    fn base_command(&self) -> Command {
        let mut cmd = Command::new("ssh");
        #[cfg(test)]
        if let Some(exe) = &self.ssh_exe {
            cmd = Command::new(exe);
        }
        if let Some(port) = self.opts.port {
            cmd.arg("-p").arg(port.to_string());
        }
        if let Some(identity) = &self.opts.identity_file {
            cmd.arg("-i").arg(identity);
        }
        self.opts.extra_options.iter().for_each(|opt| {
            cmd.arg("-o").arg(opt);
        });
        if let Some(dir) = &self.control_dir {
            cmd.arg("-o").arg("ControlMaster=auto");
            cmd.arg("-o")
                .arg(format!("ControlPath={}/cm-%C", dir.display()));
            cmd.arg("-o").arg("ControlPersist=30");
        }
        if let (Auth::Password(password), Some(exe)) = (&self.opts.auth, &self.askpass_exe) {
            cmd.arg("-o")
                .arg("PreferredAuthentications=password,keyboard-interactive");
            cmd.arg("-o").arg("NumberOfPasswordPrompts=1");
            // Host-key confirmation prompts would otherwise also be
            // routed to the askpass helper; accept-new avoids them for
            // first connections while still rejecting changed keys.
            cmd.arg("-o").arg("StrictHostKeyChecking=accept-new");
            cmd.env("SSH_ASKPASS", exe);
            cmd.env("SSH_ASKPASS_REQUIRE", "force");
            cmd.env(ASKPASS_MODE_ENV, "1");
            cmd.env(ASKPASS_PASSWORD_ENV, password);
            if std::env::var_os("DISPLAY").is_none() {
                cmd.env("DISPLAY", ":0");
            }
        }
        cmd
    }

    fn command(&self, remote_cmd: &str) -> Command {
        let mut cmd = self.base_command();
        cmd.arg("--").arg(self.target()).arg(remote_cmd);
        cmd
    }

    /// Run a remote command, capturing output. Does not fail on
    /// non-zero exit; callers inspect the status.
    pub fn run(&self, remote_cmd: &str) -> Result<Output> {
        Ok(self.command(remote_cmd).stdin(Stdio::null()).output()?)
    }

    /// Run a remote command and return stdout; non-zero exit is an error.
    pub fn run_checked(&self, remote_cmd: &str) -> Result<String> {
        let stdout = self.run_checked_bytes(remote_cmd, &NoProgress)?;
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    /// Like [`Self::run_checked`], but for binary stdout (e.g. tar
    /// streams), reporting transfer progress (total unknown).
    pub fn run_checked_bytes(&self, remote_cmd: &str, progress: &dyn Progress) -> Result<Vec<u8>> {
        self.run_checked_bytes_hint(remote_cmd, None, progress)
    }

    /// [`Self::run_checked_bytes`] with a size hint for the progress
    /// bar (the transfer itself does not depend on it).
    pub fn run_checked_bytes_hint(
        &self,
        remote_cmd: &str,
        total: Option<u64>,
        progress: &dyn Progress,
    ) -> Result<Vec<u8>> {
        let mut child = StreamingChild::spawn(&mut self.command(remote_cmd), false)?;
        let mut data = Vec::new();
        let transfer = {
            let mut stdout = child.child.stdout.take().expect("stdout was piped");
            copy_with_progress(&mut stdout, &mut data, total, progress)
        };
        child.finish(transfer)?;
        progress.bytes_done();
        Ok(data)
    }

    /// Run a remote command with bytes piped to its stdin; non-zero
    /// exit is an error. Reports transfer progress (total known).
    pub fn run_checked_with_stdin(
        &self,
        remote_cmd: &str,
        data: &[u8],
        progress: &dyn Progress,
    ) -> Result<()> {
        let mut child = StreamingChild::spawn(&mut self.command(remote_cmd), true)?;
        let transfer = {
            let mut stdin = child.child.stdin.take().expect("stdin was piped");
            let mut reader = data;
            copy_with_progress(&mut reader, &mut stdin, Some(data.len() as u64), progress)
        };
        child.finish(transfer)?;
        progress.bytes_done();
        Ok(())
    }

    /// Write bytes to a remote file (via `cat > path`). Small control
    /// writes; no progress reporting.
    pub fn write_remote_file(&self, remote_path: &str, data: &[u8]) -> Result<()> {
        self.run_checked_with_stdin(
            &format!("cat > {}", shell_quote(remote_path)),
            data,
            &NoProgress,
        )
    }

    /// Stream a local file to a remote path, reporting progress.
    pub fn upload_local_file(
        &self,
        local: &Path,
        remote_path: &str,
        progress: &dyn Progress,
    ) -> Result<()> {
        let mut file = fs::File::open(local)?;
        let total = file.metadata()?.len();
        let mut child = StreamingChild::spawn(
            &mut self.command(&format!("cat > {}", shell_quote(remote_path))),
            true,
        )?;
        let transfer = {
            let mut stdin = child.child.stdin.take().expect("stdin was piped");
            copy_with_progress(&mut file, &mut stdin, Some(total), progress)
        };
        child.finish(transfer)?;
        progress.bytes_done();
        Ok(())
    }

    /// Stream a remote file to a local path, reporting progress.
    /// `total` is a size hint for the progress bar (e.g. from a prior
    /// listing); the transfer itself does not depend on it.
    pub fn download_remote_file(
        &self,
        remote_path: &str,
        local: &Path,
        total: Option<u64>,
        progress: &dyn Progress,
    ) -> Result<()> {
        let mut child = StreamingChild::spawn(
            &mut self.command(&format!("cat {}", shell_quote(remote_path))),
            false,
        )?;
        let transfer = {
            let mut stdout = child.child.stdout.take().expect("stdout was piped");
            let mut file = fs::File::create(local)?;
            copy_with_progress(&mut stdout, &mut file, total, progress)
        };
        child.finish(transfer)?;
        progress.bytes_done();
        Ok(())
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        if self.control_dir.is_some() {
            // Best-effort teardown of the multiplexing master.
            let _ = self
                .base_command()
                .arg("-O")
                .arg("exit")
                .arg("--")
                .arg(self.target())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if let Some(dir) = self.control_dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// If invoked as an ssh askpass helper, print the password and exit.
///
/// Binaries using [`Auth::Password`] must call this at the very start
/// of `main`, before any argument parsing.
pub fn maybe_run_askpass() {
    if !std::env::var_os(ASKPASS_MODE_ENV).is_some_and(|v| v == "1") {
        return;
    }
    // ssh sets SSH_ASKPASS_PROMPT=confirm for yes/no questions (e.g. a
    // changed host key); never answer those with the password.
    if std::env::var("SSH_ASKPASS_PROMPT").as_deref() == Ok("confirm") {
        std::process::exit(1);
    }
    match std::env::var(ASKPASS_PASSWORD_ENV) {
        Ok(password) => {
            println!("{password}");
            std::process::exit(0);
        }
        Err(_) => std::process::exit(1),
    }
}

/// Quote a string for a POSIX shell (busybox `ash` on the device).
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'.' | b'_' | b'-' | b'/' | b'@' | b':' | b'+' | b',' | b'%'
                )
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Owns a streaming subprocess and its concurrent output readers. Transfer
/// pipes must be closed before `finish`, so a successful upload sends EOF.
/// On an early error (or unwind), cancel readers and kill/reap the direct child
/// before joining. On non-Unix systems, readers still require pipe EOF.
struct StreamingChild {
    child: Child,
    stderr: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    stdout: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    reaped: bool,
    cancellation: DrainCancellation,
}

impl StreamingChild {
    fn spawn(command: &mut Command, with_stdin: bool) -> std::io::Result<Self> {
        let cancellation = DrainCancellation::new()?;
        let child = command
            .stdin(if with_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stream = Self {
            child,
            stderr: None,
            stdout: None,
            reaped: false,
            cancellation,
        };
        // Fallible thread creation leaves cleanup to the guard too.
        stream.stderr = Some(drain_pipe(
            stream.child.stderr.take().expect("stderr was piped"),
            &stream.cancellation,
        )?);
        if with_stdin {
            stream.stdout = Some(drain_pipe(
                stream.child.stdout.take().expect("stdout was piped"),
                &stream.cancellation,
            )?);
        }
        Ok(stream)
    }

    fn finish(mut self, transfer: std::io::Result<u64>) -> Result<()> {
        transfer?;
        let status = self.child.wait()?;
        self.reaped = true;
        let stderr = join_pipe(self.stderr.take())?;
        let stdout = join_pipe(self.stdout.take())?;
        checked(Output {
            status,
            stdout,
            stderr,
        })?;
        Ok(())
    }
}

impl Drop for StreamingChild {
    fn drop(&mut self) {
        // A ProxyCommand (or another local descendant) may keep output pipes
        // open after SSH exits. Error cleanup must not depend on their EOF.
        self.cancellation.cancel();
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = join_pipe(self.stderr.take());
        let _ = join_pipe(self.stdout.take());
    }
}

/// Closing the sole writer wakes every reader, without changing SSH process
/// groups or killing unrelated helpers. UnixStream descriptors are close-on-exec.
struct DrainCancellation {
    #[cfg(unix)]
    reader: Arc<UnixStream>,
    #[cfg(unix)]
    writer: Option<UnixStream>,
}

impl DrainCancellation {
    fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let (reader, writer) = UnixStream::pair()?;
            Ok(Self {
                reader: Arc::new(reader),
                writer: Some(writer),
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    fn cancel(&mut self) {
        #[cfg(unix)]
        self.writer.take();
    }
}

#[cfg(unix)]
fn drain_pipe(
    mut pipe: impl Read + AsFd + Send + 'static,
    cancellation: &DrainCancellation,
) -> std::io::Result<JoinHandle<std::io::Result<Vec<u8>>>> {
    use rustix::event::{PollFd, PollFlags, poll};

    let cancelled = Arc::clone(&cancellation.reader);
    thread::Builder::new().spawn(move || {
        let mut data = Vec::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let mut fds = [
                PollFd::new(&pipe, PollFlags::IN),
                PollFd::new(&*cancelled, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
            // Cancellation takes precedence even if output is continuously ready.
            if !fds[1].revents().is_empty() {
                return Ok(data);
            }
            if fds[0].revents().contains(PollFlags::NVAL) {
                return Err(std::io::Error::other("invalid SSH output descriptor"));
            }
            // These are single-reader pipes: a ready read returns available
            // bytes without waiting to fill the buffer. HUP can include unread
            // data, so only a zero-byte read means the diagnostics are complete.
            match pipe.read(&mut buffer) {
                Ok(0) => return Ok(data),
                Ok(n) => data.extend_from_slice(&buffer[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    })
}

#[cfg(not(unix))]
fn drain_pipe(
    mut pipe: impl Read + Send + 'static,
    _cancellation: &DrainCancellation,
) -> std::io::Result<JoinHandle<std::io::Result<Vec<u8>>>> {
    thread::Builder::new().spawn(move || {
        let mut data = Vec::new();
        pipe.read_to_end(&mut data)?;
        Ok(data)
    })
}

fn join_pipe(reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>) -> std::io::Result<Vec<u8>> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_| std::io::Error::other("SSH output reader panicked"))?,
        None => Ok(Vec::new()),
    }
}

/// `io::copy` with per-chunk progress callbacks.
fn copy_with_progress(
    reader: &mut impl Read,
    writer: &mut impl Write,
    total: Option<u64>,
    progress: &dyn Progress,
) -> std::io::Result<u64> {
    let mut buffer = [0u8; 64 * 1024];
    let mut transferred = 0u64;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            return Ok(transferred);
        }
        writer.write_all(&buffer[..n])?;
        transferred += n as u64;
        progress.bytes(transferred, total);
    }
}

fn checked(output: Output) -> Result<Output> {
    if output.status.success() {
        return Ok(output);
    }
    Err(Error::Remote {
        status: output.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// Short-pathed private directory for ControlMaster sockets.
///
/// `/tmp` rather than `std::env::temp_dir()`: socket paths are limited
/// to ~104 bytes on macOS and `%C` alone expands to 40 characters.
#[cfg(unix)]
fn make_control_dir() -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = PathBuf::from("/tmp").join(format!("rmu-{}", &id[..8]));
    fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
    Some(dir)
}

#[cfg(not(unix))]
fn make_control_dir() -> Option<PathBuf> {
    // ControlMaster is not supported by OpenSSH on Windows.
    None
}

#[cfg(all(test, unix))]
mod streaming_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_passthrough_for_safe_strings() {
        assert_eq!(
            shell_quote("/home/root/.local/share"),
            "/home/root/.local/share"
        );
        assert_eq!(shell_quote("abc-123_x.y"), "abc-123_x.y");
    }

    #[test]
    fn copy_reports_progress() {
        use std::cell::RefCell;

        struct Recorder(RefCell<Vec<(u64, Option<u64>)>>);
        impl crate::progress::Progress for Recorder {
            fn step(&self, _: &str) {}
            fn bytes(&self, transferred: u64, total: Option<u64>) {
                self.0.borrow_mut().push((transferred, total));
            }
            fn bytes_done(&self) {}
            fn finished(&self) {}
        }

        let data = vec![7u8; 100 * 1024]; // spans two 64 KiB chunks
        let recorder = Recorder(RefCell::new(Vec::new()));
        let mut out = Vec::new();
        let copied =
            copy_with_progress(&mut &data[..], &mut out, Some(data.len() as u64), &recorder)
                .unwrap();
        assert_eq!(copied, data.len() as u64);
        assert_eq!(out, data);
        let events = recorder.0.borrow();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], (64 * 1024, Some(100 * 1024)));
        assert_eq!(events[1], (100 * 1024, Some(100 * 1024)));
    }

    #[test]
    fn quote_wraps_unsafe_strings() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("x=y"), "'x=y'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }
}
