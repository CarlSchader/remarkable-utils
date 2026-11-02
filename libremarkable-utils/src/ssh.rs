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
        })
    }

    /// The ssh destination (`host` or `user@host`).
    pub fn target(&self) -> String {
        self.opts.destination.clone()
    }

    /// ssh invocation with all options and env set, but no destination.
    fn base_command(&self) -> Command {
        let mut cmd = Command::new("ssh");
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
        let mut child = self
            .command(remote_cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut data = Vec::new();
        {
            let mut stdout = child.stdout.take().expect("stdout was piped");
            copy_with_progress(&mut stdout, &mut data, total, progress)?;
        }
        checked(child.wait_with_output()?)?;
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
        let mut child = self
            .command(remote_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            let mut reader = data;
            copy_with_progress(&mut reader, &mut stdin, Some(data.len() as u64), progress)?;
        }
        checked(child.wait_with_output()?)?;
        progress.bytes_done();
        Ok(())
    }

    /// Atomically replace a remote file after verifying the received length.
    /// Small control writes; no progress reporting.
    pub fn write_remote_file(&self, remote_path: &str, data: &[u8]) -> Result<()> {
        self.write_remote_file_with_progress(remote_path, data, &NoProgress)
    }

    /// In-memory payload writes with the same staging as local-file uploads.
    pub(crate) fn write_remote_file_with_progress(
        &self,
        remote_path: &str,
        data: &[u8],
        progress: &dyn Progress,
    ) -> Result<()> {
        self.upload_reader(&mut &data[..], remote_path, data.len() as u64, progress)
    }

    /// Stream a local file to a unique remote sibling, verify its length,
    /// then atomically replace the destination, preserving existing permissions.
    pub fn upload_local_file(
        &self,
        local: &Path,
        remote_path: &str,
        progress: &dyn Progress,
    ) -> Result<()> {
        let mut file = fs::File::open(local)?;
        let total = file.metadata()?.len();
        self.upload_reader(&mut file, remote_path, total, progress)
    }

    fn upload_reader(
        &self,
        reader: &mut impl Read,
        remote_path: &str,
        total: u64,
        progress: &dyn Progress,
    ) -> Result<()> {
        let mut child = self
            .command(&staged_write_command(remote_path, total))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let copied = {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            copy_with_progress(reader, &mut stdin, Some(total), progress)
        };
        // Close stdin and reap even on a local read/write failure, allowing
        // the remote length check and cleanup trap to run on the partial input.
        let output = child.wait_with_output()?;
        copied?;
        checked(output)?;
        progress.bytes_done();
        Ok(())
    }

    /// Stream a remote file to a local path, reporting progress.
    /// The destination is atomically replaced only after a successful transfer.
    /// Existing regular-file permissions are preserved; a destination symlink
    /// is replaced rather than followed. New files are private on Unix.
    /// `total` is a size hint for the progress bar (e.g. from a prior
    /// listing); the transfer itself does not depend on it.
    pub fn download_remote_file(
        &self,
        remote_path: &str,
        local: &Path,
        total: Option<u64>,
        progress: &dyn Progress,
    ) -> Result<()> {
        download_to_local(
            &mut self.command(&format!("cat {}", shell_quote(remote_path))),
            local,
            total,
            progress,
        )
    }
}

/// An exclusively created sibling, removed on every unsuccessful exit path.
struct DownloadTemp {
    file: fs::File,
    path: PathBuf,
}

impl DownloadTemp {
    fn new(local: &Path) -> std::io::Result<Self> {
        let parent = local.parent().filter(|p| !p.as_os_str().is_empty());
        let path = parent.unwrap_or(Path::new(".")).join(format!(
            ".rmu-download-{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok(Self { file, path })
    }
}

impl Drop for DownloadTemp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Reap the download subprocess even if copying or reading diagnostics fails.
struct DownloadChild(Child);

impl Drop for DownloadChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn download_to_local(
    command: &mut Command,
    local: &Path,
    total: Option<u64>,
    progress: &dyn Progress,
) -> Result<()> {
    let permissions = match fs::symlink_metadata(local) {
        Ok(metadata) => metadata.is_file().then(|| metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut temp = DownloadTemp::new(local)?;
    let mut child = DownloadChild(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    {
        let mut stdout = child.0.stdout.take().expect("stdout was piped");
        copy_with_progress(&mut stdout, &mut temp.file, total, progress)?;
    }
    let mut stderr = Vec::new();
    child
        .0
        .stderr
        .take()
        .expect("stderr was piped")
        .read_to_end(&mut stderr)?;
    checked(Output {
        status: child.0.wait()?,
        stdout: Vec::new(),
        stderr,
    })?;
    // Keep staging private until the transfer has succeeded. Sync before
    // rename so delayed local write errors cannot destroy the old backup.
    if let Some(permissions) = permissions {
        temp.file.set_permissions(permissions)?;
    }
    temp.file.sync_all()?;
    fs::rename(&temp.path, local)?;
    progress.bytes_done();
    Ok(())
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

/// One busybox-compatible transaction: EOF alone is not proof of a complete
/// upload (e.g. a disconnected SSH channel can give `cat` a clean short EOF).
/// The sibling is private until verified; rename stays on the same filesystem.
/// Traps clean ordinary failures and catchable disconnect signals; uncatchable
/// termination can leave an orphan sibling. This is not a crash-durability guarantee.
fn staged_write_command(remote_path: &str, total: u64) -> String {
    let parent = remote_path
        .rsplit_once('/')
        .map_or(".", |(dir, _)| if dir.is_empty() { "/" } else { dir });
    format!(
        r#"dest={dest}
 tmp=$(mktemp -- {template}) || exit 1
 trap 'rm -f -- "$tmp"' EXIT
 trap 'exit 1' HUP INT TERM
 cat > "$tmp" || exit 1
 [ "$(wc -c < "$tmp")" -eq {total} ] || {{
     echo 'incomplete remote upload: unexpected byte count' >&2
     exit 1
 }}
 if [ -e "$dest" ]; then
     attrs=$(stat -c '%a %u %g' -- "$dest") || exit 1
     set -- $attrs
     chown "$2:$3" "$tmp" || exit 1
     chmod "$1" "$tmp" || exit 1
 else
     mask=$(umask)
     chmod "$(printf '%o' "$((0666 & ~0$mask))")" "$tmp" || exit 1
 fi
 mv -fT -- "$tmp" "$dest""#,
        dest = shell_quote(remote_path),
        template = shell_quote(&format!("{parent}/.rmu-upload-XXXXXX")),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod downloads {
        use super::*;
        use std::cell::Cell;
        use std::os::unix::fs::PermissionsExt;

        struct Fixture(PathBuf);

        impl Fixture {
            fn new() -> Self {
                let path = std::env::temp_dir()
                    .join(format!("rmu-download-test-{}", uuid::Uuid::new_v4()));
                fs::create_dir(&path).unwrap();
                Self(path)
            }

            fn destination(&self) -> PathBuf {
                self.0.join("backup.pdf")
            }

            fn assert_no_temps(&self) {
                assert!(fs::read_dir(&self.0).unwrap().all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".rmu-download-")
                }));
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }

        // Inject only the subprocess command, exercising the same download
        // path as SSH without modifying PATH or contacting a real device.
        fn mock_ssh(script: &str) -> Command {
            let mut command = Command::new("sh");
            command.args(["-c", script, "mock-ssh"]);
            command
        }

        struct Observer<'a> {
            destination: &'a Path,
            before: &'a [u8],
            after: &'a [u8],
            chunks: Cell<usize>,
            done: Cell<bool>,
        }

        impl Progress for Observer<'_> {
            fn step(&self, _: &str) {}
            fn bytes(&self, _: u64, _: Option<u64>) {
                assert_eq!(fs::read(self.destination).unwrap(), self.before);
                self.chunks.set(self.chunks.get() + 1);
            }
            fn bytes_done(&self) {
                assert_eq!(fs::read(self.destination).unwrap(), self.after);
                self.done.set(true);
            }
            fn finished(&self) {}
        }

        #[test]
        fn missing_remote_source_preserves_backup_on_repeated_attempts() {
            let fixture = Fixture::new();
            let destination = fixture.destination();
            fs::write(&destination, b"existing backup").unwrap();
            let missing = fixture.0.join("missing-remote.pdf");
            (0..3).for_each(|_| {
                let result = download_to_local(
                    mock_ssh("cat \"$1\"").arg(&missing),
                    &destination,
                    None,
                    &NoProgress,
                );
                assert!(matches!(result, Err(Error::Remote { .. })));
                assert_eq!(fs::read(&destination).unwrap(), b"existing backup");
                fixture.assert_no_temps();
            });
        }

        #[test]
        fn mid_stream_ssh_failure_preserves_backup_and_permissions() {
            let fixture = Fixture::new();
            let destination = fixture.destination();
            fs::write(&destination, b"existing backup").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();
            let source = fixture.0.join("partial-remote.pdf");
            fs::write(&source, vec![7; 100 * 1024]).unwrap();
            let observer = Observer {
                destination: &destination,
                before: b"existing backup",
                after: b"must not be published",
                chunks: Cell::new(0),
                done: Cell::new(false),
            };
            (0..3).for_each(|_| {
                let result = download_to_local(
                    mock_ssh("cat \"$1\"; printf 'connection lost' >&2; exit 23").arg(&source),
                    &destination,
                    Some(200 * 1024),
                    &observer,
                );
                assert!(matches!(result, Err(Error::Remote { status: 23, stderr }) if stderr == "connection lost"));
                assert_eq!(fs::read(&destination).unwrap(), b"existing backup");
                assert_eq!(fs::metadata(&destination).unwrap().permissions().mode() & 0o777, 0o640);
                assert!(!observer.done.get());
                fixture.assert_no_temps();
            });
            assert!(observer.chunks.get() >= 6);
        }

        #[test]
        fn successful_download_replaces_only_after_completion() {
            let fixture = Fixture::new();
            let destination = fixture.destination();
            let source = fixture.0.join("remote.pdf");
            let payload = vec![9; 100 * 1024];
            fs::write(&source, &payload).unwrap();
            fs::write(&destination, b"existing backup").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();
            let observer = Observer {
                destination: &destination,
                before: b"existing backup",
                after: &payload,
                chunks: Cell::new(0),
                done: Cell::new(false),
            };
            // A stale size hint must not reject an otherwise successful copy.
            download_to_local(
                mock_ssh("cat \"$1\"").arg(&source),
                &destination,
                Some(1),
                &observer,
            )
            .unwrap();
            assert!(observer.chunks.get() >= 2);
            assert!(observer.done.get());
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                0o640
            );
            fixture.assert_no_temps();
            // Empty files are valid successful replacements too.
            download_to_local(&mut mock_ssh("exit 0"), &destination, None, &NoProgress).unwrap();
            assert!(fs::read(&destination).unwrap().is_empty());
            fixture.assert_no_temps();
        }

        #[test]
        fn spawn_failure_preserves_backup_and_removes_temp() {
            let fixture = Fixture::new();
            let destination = fixture.destination();
            fs::write(&destination, b"existing backup").unwrap();
            let result = download_to_local(
                &mut Command::new(fixture.0.join("missing-ssh")),
                &destination,
                None,
                &NoProgress,
            );
            assert!(matches!(result, Err(Error::Io(_))));
            assert_eq!(fs::read(&destination).unwrap(), b"existing backup");
            fixture.assert_no_temps();
        }

        #[test]
        fn failed_download_does_not_create_destination() {
            let fixture = Fixture::new();
            let destination = fixture.destination();
            let result = download_to_local(
                &mut mock_ssh("printf partial; exit 1"),
                &destination,
                None,
                &NoProgress,
            );
            assert!(matches!(result, Err(Error::Remote { .. })));
            assert!(!destination.exists());
            fixture.assert_no_temps();
            download_to_local(
                &mut mock_ssh("printf complete"),
                &destination,
                None,
                &NoProgress,
            )
            .unwrap();
            assert_eq!(fs::read(&destination).unwrap(), b"complete");
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fixture.assert_no_temps();
        }

        #[test]
        fn failed_rename_removes_temp_and_preserves_destination() {
            let fixture = Fixture::new();
            let destination = fixture.destination();
            fs::create_dir(&destination).unwrap();
            fs::write(destination.join("keep"), b"untouched").unwrap();
            let result = download_to_local(
                &mut mock_ssh("printf complete"),
                &destination,
                None,
                &NoProgress,
            );
            assert!(matches!(result, Err(Error::Io(_))));
            assert_eq!(fs::read(destination.join("keep")).unwrap(), b"untouched");
            fixture.assert_no_temps();
        }

        #[test]
        fn successful_download_replaces_symlink_not_its_target() {
            let fixture = Fixture::new();
            let destination = fixture.destination();
            let target = fixture.0.join("symlink-target.pdf");
            fs::write(&target, b"untouched").unwrap();
            std::os::unix::fs::symlink(&target, &destination).unwrap();
            download_to_local(
                &mut mock_ssh("printf complete"),
                &destination,
                None,
                &NoProgress,
            )
            .unwrap();
            assert!(fs::symlink_metadata(&destination).unwrap().is_file());
            assert_eq!(fs::read(&destination).unwrap(), b"complete");
            assert_eq!(fs::read(&target).unwrap(), b"untouched");
            fixture.assert_no_temps();
        }
    }

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
