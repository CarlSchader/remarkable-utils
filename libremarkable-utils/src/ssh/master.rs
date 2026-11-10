//! The multiplexing master rmu opens itself, rmu's ssh timeout defaults,
//! and telling a tablet that does not answer from other failures.
//!
//! The defaults are decided once per session from `ssh -G`, which prints
//! the effective value of every option for the destination, ssh's
//! built-in defaults included. A value the user's ssh config sets *to*
//! the built-in default (`ConnectTimeout none`, `ServerAliveInterval 0`)
//! therefore looks the same as no value at all, and rmu adds its own; a
//! command-line `-o` beats the config file. To keep ssh's built-in
//! behaviour, pass the option with `-o` on the rmu command line (it comes
//! first, so it wins), or turn the defaults off
//! ([`SshOptions::default_timeouts`](super::SshOptions::default_timeouts)).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::PoisonError;

use super::SshSession;
use crate::error::{Error, Result};

/// rmu's value for each ssh option the user left at ssh's built-in
/// default: the option, its built-in values as `ssh -G` prints them, and
/// rmu's value.
/// `ServerAliveCountMax` keeps its own value (3 by default): three
/// unanswered probes 10 s apart end a command within about 40 s.
pub const TIMEOUT_DEFAULTS: &[(&str, &[&str], &str)] = &[
    // Older OpenSSH prints an unset timeout as -1.
    ("ConnectTimeout", &["none", "-1"], "15"),
    ("ServerAliveInterval", &["0"], "10"),
];

/// What ssh prints (exit 255) when the tablet does not answer: a connect
/// or banner timeout (Linux, then macOS wording), the keepalive giving up,
/// no route to it (asleep on the local network).
const UNREACHABLE: &[&str] = &[
    "Connection timed out",
    "Operation timed out",
    "not responding",
    "No route to host",
    "Host is down",
];

/// The master rmu opens itself (`ssh -f -N`), with its log in the
/// session's control directory: a backgrounded master's stderr is
/// /dev/null, and when it dies its clients exit 255 without a word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Master {
    /// Opened before a command whenever its socket is missing: the first
    /// command, and again after it ended (`ControlPersist` expiring
    /// between commands, a lost connection).
    OnDemand,
    /// Not rmu's to open: the user's own `Control*` options win over
    /// rmu's, so the master could be a plain connection backgrounded for
    /// good; or one opened without a socket. Commands connect themselves.
    Unavailable,
    /// An open failed for another reason than an unreachable tablet (a
    /// refused key, a wrong password): commands connect themselves rather
    /// than try again, so each authenticates once.
    Failed,
}

impl SshSession {
    /// rmu's timeout defaults for this destination (see the module
    /// docs), decided on first use; none if the user turned them off. If
    /// `ssh -G` cannot tell (it fails, or leaves an option out), rmu adds
    /// nothing rather than guess.
    pub(super) fn timeout_options(&self) -> &[String] {
        self.defaults.get_or_init(|| {
            if !self.opts.default_timeouts {
                return Vec::new();
            }
            self.user_command()
                .arg("-G")
                .arg("--")
                .arg(self.target())
                .stdin(Stdio::null())
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| {
                    timeout_defaults(
                        &String::from_utf8_lossy(&output.stdout),
                        &self.opts.extra_options,
                    )
                })
                .unwrap_or_default()
        })
    }

    /// The master's control socket and log, when multiplexing.
    pub(super) fn master_paths(&self) -> Option<(PathBuf, PathBuf)> {
        self.control_dir
            .as_ref()
            .map(|dir| (dir.join("cm"), dir.join("master.log")))
    }

    pub(super) fn master_log(&self) -> Option<PathBuf> {
        self.master_paths().map(|(_, log)| log)
    }

    /// Open the master if it is due (see [`Master`]). A tablet that does
    /// not answer is the error; any other failure is left to the command,
    /// which then connects, and reports it, itself.
    pub(super) fn ensure_master(&self) -> Result<()> {
        let Some((socket, log)) = self.master_paths() else {
            return Ok(());
        };
        let mut master = self.master.lock().unwrap_or_else(PoisonError::into_inner);
        if *master != Master::OnDemand || socket.exists() {
            return Ok(());
        }
        if ["ControlMaster", "ControlPath", "ControlPersist"]
            .iter()
            .any(|option| sets_option(&self.opts.extra_options, option))
        {
            *master = Master::Unavailable;
            return Ok(());
        }
        // Truncated: only this master's own words explain its clients.
        fs::write(&log, b"")?;
        let stderr = fs::OpenOptions::new().append(true).open(&log)?;
        let status = self
            .base_command()
            .arg("-E")
            .arg(&log)
            .args(["-f", "-N", "--"])
            .arg(self.target())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .status()?;
        let output = Output {
            status,
            stdout: Vec::new(),
            stderr: fs::read(&log).unwrap_or_default(),
        };
        if is_unreachable(&output) {
            return Err(failure(&output));
        }
        *master = match (status.success(), socket.exists()) {
            (true, true) => Master::OnDemand,
            // ssh connected but could not bind the socket (OpenSSH goes
            // on without one, e.g. on `EADDRINUSE`): the backgrounded
            // connection then stays until the tablet drops it, out of
            // reach of `-O exit`. Rare in a private directory.
            (true, false) => Master::Unavailable,
            (false, _) => Master::Failed,
        };
        Ok(())
    }
}

/// The error for a failed command: [`Error::Unreachable`] when ssh gave
/// up on the tablet (see [`is_unreachable`]), else [`Error::Remote`].
pub fn failure(output: &Output) -> Error {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if is_unreachable(output) {
        return Error::Unreachable(stderr);
    }
    Error::Remote {
        status: output.status.code().unwrap_or(-1),
        stderr,
    }
}

/// A multiplexed command whose master connection died exits 255 without
/// a word; the master's log at `log` says why. Appends the log's last
/// line about an unreachable tablet to the command's stderr.
pub(super) fn explain_from_master_log(output: &mut Output, log: Option<&Path>) {
    if output.status.code() != Some(255) || is_unreachable(output) {
        return;
    }
    let Some(log) = log.and_then(|log| fs::read_to_string(log).ok()) else {
        return;
    };
    explain(output, &log);
}

/// [`explain_from_master_log`] given the log's text. Pure.
fn explain(output: &mut Output, log: &str) {
    if let Some(line) = log
        .lines()
        .rev()
        .find(|line| UNREACHABLE.iter().any(|text| line.contains(text)))
    {
        if !output.stderr.is_empty() && !output.stderr.ends_with(b"\n") {
            output.stderr.push(b'\n');
        }
        output.stderr.extend_from_slice(line.trim().as_bytes());
    }
}

/// Whether ssh itself failed (255) because the tablet did not answer:
/// asleep, off the network, or gone silent mid-command.
pub fn is_unreachable(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    output.status.code() == Some(255) && UNREACHABLE.iter().any(|text| stderr.contains(text))
}

/// Whether the `-o` options `extra` set `option` (`Option=value`,
/// `Option value` and `Option = value`, any case). Pure.
fn sets_option(extra: &[String], option: &str) -> bool {
    extra.iter().any(|opt| {
        opt.trim()
            .split(|c: char| c == '=' || c.is_whitespace())
            .next()
            .is_some_and(|key| key.eq_ignore_ascii_case(option))
    })
}

/// `-o Option=value` arguments for the [`TIMEOUT_DEFAULTS`] that neither
/// the user's `-o` options (`extra`) nor their ssh config set, given
/// `effective`, the output of `ssh -G` (lowercase `option value` lines,
/// built-in defaults included). An option missing from `effective` is
/// left alone. Pure.
pub fn timeout_defaults(effective: &str, extra: &[String]) -> Vec<String> {
    let effective_value = |option: &str| {
        effective.lines().find_map(|line| {
            let (key, value) = line.trim().split_once(' ')?;
            key.eq_ignore_ascii_case(option).then(|| value.trim())
        })
    };
    TIMEOUT_DEFAULTS
        .iter()
        .filter(|(option, _, _)| !sets_option(extra, option))
        .filter(|(option, builtin, _)| {
            effective_value(option).is_some_and(|value| builtin.contains(&value))
        })
        .map(|(option, _, value)| format!("{option}={value}"))
        .collect()
}

#[cfg(test)]
mod tests;
