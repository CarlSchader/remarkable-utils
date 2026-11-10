//! Device-side shell scripts and the parsers for their output. Pure:
//! generation and parsing are unit-tested here, and [`super::SshTablet`]
//! only sends scripts and hands output back.
//!
//! The scripts target BusyBox `ash` and applets (the survey's list:
//! `stat -c` with `%y`/`%z`, `find -exec +`, `tar -T -`, `mv -T`,
//! `sha256sum`, `mktemp`, `flock`, `setsid`, `sync`); the contract tests
//! run them under BusyBox and GNU tools. Every interpolated value goes
//! through [`shell_quote`]. Scripts sent on stdin (`sh -s`) that change
//! anything wrap their body in a function: a truncated transfer is then
//! a syntax error and runs nothing, and no lock is taken before the
//! whole script has arrived.
//!
//! **Serialisation.** Lock operations and guarded scripts hold an
//! exclusive `flock` on [`TabletPaths::guard_flock`] from their guard
//! check to their last change, so a takeover can never fall between an
//! old run's guard and its mutation. The kernel drops the `flock` when
//! the script exits, however it ends. Only bounded, local work runs under
//! it, and waiters give up after [`FLOCK_WAIT_SECS`]; staging (a network
//! transfer into the run's own directory) checks the guard without it,
//! and the watchdog never takes it, so a stuck script cannot keep
//! xochitl down.

mod guard;
mod transfer;

#[cfg(test)]
mod fixtures;

use super::DeviceError;
use crate::ssh::shell_quote;

pub(super) use self::guard::*;
pub(super) use self::transfer::*;

/// Exit status of a guarded script whose watchdog fired.
pub(super) const EXIT_WATCHDOG_FIRED: i32 = 75;
/// Exit status of a guarded script whose lock is no longer ours.
pub(super) const EXIT_LOCK_LOST: i32 = 76;
/// Exit status of a lock takeover that found the lock changed.
pub(super) const EXIT_LOCK_CHANGED: i32 = 77;
/// Exit status of a script that could not get the guard `flock` in time.
pub(super) const EXIT_BUSY: i32 = 78;
/// Exit status of a disarm refused because xochitl is stopped.
pub(super) const EXIT_XOCHITL_STOPPED: i32 = 79;
/// Exit status of a guarded script on a tablet that rebooted since the
/// lock was taken.
pub(super) const EXIT_REBOOTED: i32 = 80;
/// Exit status of an apply refused because xochitl is running.
pub(super) const EXIT_XOCHITL_RUNNING: i32 = 81;
/// Exit status of a stop refused for want of this run's watchdog.
pub(super) const EXIT_NOT_ARMED: i32 = 82;

/// Seconds a script waits for the guard `flock`: long enough for any
/// legitimate holder (an apply, `systemctl stop xochitl`), bounded so a
/// holder stuck on a dead connection cannot block every later run.
const FLOCK_WAIT_SECS: u32 = 180;

const NANOS_PER_SEC: i64 = 1_000_000_000;

fn output_error(operation: &'static str, detail: impl Into<String>) -> DeviceError {
    DeviceError::Output {
        operation,
        detail: detail.into(),
    }
}

fn q(text: &str) -> String {
    shell_quote(text)
}
