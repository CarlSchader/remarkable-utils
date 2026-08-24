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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::error::{Error, Result};

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
    pub host: String,
    pub user: String,
    pub port: u16,
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
            host: DEFAULT_USB_HOST.to_string(),
            user: DEFAULT_SSH_USER.to_string(),
            port: DEFAULT_SSH_PORT,
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

    /// `user@host` destination string.
    pub fn target(&self) -> String {
        format!("{}@{}", self.opts.user, self.opts.host)
    }

    /// ssh invocation with all options and env set, but no destination.
    fn base_command(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-p").arg(self.opts.port.to_string());
        if let Some(identity) = &self.opts.identity_file {
            cmd.arg("-i").arg(identity);
        }
        for opt in &self.opts.extra_options {
            cmd.arg("-o").arg(opt);
        }
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
        let stdout = self.run_checked_bytes(remote_cmd)?;
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    /// Like [`Self::run_checked`], but for binary stdout (e.g. tar streams).
    pub fn run_checked_bytes(&self, remote_cmd: &str) -> Result<Vec<u8>> {
        let output = checked(self.run(remote_cmd)?)?;
        Ok(output.stdout)
    }

    /// Write bytes to a remote file (via `cat > path`).
    pub fn write_remote_file(&self, remote_path: &str, data: &[u8]) -> Result<()> {
        let mut child = self
            .command(&format!("cat > {}", shell_quote(remote_path)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            stdin.write_all(data)?;
        }
        checked(child.wait_with_output()?)?;
        Ok(())
    }

    /// Stream a local file to a remote path.
    pub fn upload_local_file(&self, local: &Path, remote_path: &str) -> Result<()> {
        let file = fs::File::open(local)?;
        let output = self
            .command(&format!("cat > {}", shell_quote(remote_path)))
            .stdin(Stdio::from(file))
            .output()?;
        checked(output)?;
        Ok(())
    }

    /// Stream a remote file to a local path.
    pub fn download_remote_file(&self, remote_path: &str, local: &Path) -> Result<()> {
        let file = fs::File::create(local)?;
        let output = self
            .command(&format!("cat {}", shell_quote(remote_path)))
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .output()?;
        checked(output)?;
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

    #[test]
    fn quote_passthrough_for_safe_strings() {
        assert_eq!(
            shell_quote("/home/root/.local/share"),
            "/home/root/.local/share"
        );
        assert_eq!(shell_quote("abc-123_x.y"), "abc-123_x.y");
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
