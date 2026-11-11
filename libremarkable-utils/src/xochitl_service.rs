//! The xochitl systemd unit, as `rmu`'s maintenance restart
//! ([`Client::restart_xochitl`](crate::Client::restart_xochitl)) and
//! `rmu hub` (its starts and its watchdog) drive it, and the parts of
//! bringing it back that both share.
//!
//! xochitl has a strict start limit: repeated plain starts or restarts
//! can hit `start-limit-hit` and reboot the whole tablet, so every start
//! or restart is preceded by [`RESET_FAILED`].
//!
//! **`restart` or `start`.** The maintenance restart uses [`RESTART`]:
//! xochitl is running, rmu changed files behind its back, and only a
//! restart makes it read them again (`start` does nothing to a running
//! unit). The hub uses [`START`]: it stopped xochitl itself, and by the
//! time it brings it back xochitl may already be up again (the watchdog
//! fired, or an earlier start whose reply was lost did run). A `restart`
//! would then kill a xochitl someone may be writing in; `start` leaves it
//! alone, so sending it again is always safe.
//!
//! **After a failure** both flows are the same ([`came_back`]): a tablet
//! that did not answer is not asked again; otherwise wait
//! [`RECHECK_DELAY`] and look whether xochitl came back anyway (the
//! command channel can drop while it does), reading the answer with
//! [`active`]. ssh's own failure (exit 255) is never read as "not
//! active". What each flow checks besides stays its own: the hub's lock
//! check before its start (and before the look), the client's write guard
//! around its restart.

use std::process::Output;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::ssh;

/// Clears the unit's failure count; its exit status does not matter.
pub(crate) const RESET_FAILED: &str = "systemctl reset-failed xochitl.service";

/// The hub's way back (see the module docs).
pub(crate) const START: &str = "systemctl start xochitl.service";

/// The maintenance restart's (see the module docs).
pub(crate) const RESTART: &str = "systemctl restart xochitl.service";

/// Prints the unit's state (see [`state`]); exits non-zero for every
/// state but `active`.
pub(crate) const IS_ACTIVE: &str = "systemctl is-active xochitl.service";

/// How long [`came_back`] waits before it looks: xochitl needs a moment
/// to come up after its command channel dropped.
pub(crate) const RECHECK_DELAY: Duration = Duration::from_secs(1);

/// The state [`IS_ACTIVE`] printed (`active`, `inactive`, `failed`, ...),
/// trimmed.
pub(crate) fn state(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout).trim().to_string()
}

/// Whether xochitl is active, from the output of [`IS_ACTIVE`]. Exit 255
/// is ssh's own (`systemctl` never exits 255): the tablet did not answer,
/// [`Error::Unreachable`], never "not active". An empty answer is no
/// answer either: [`Error::Remote`].
pub(crate) fn active(output: &Output) -> Result<bool> {
    if output.status.code() == Some(255) {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::Unreachable(if stderr.is_empty() {
            "ssh exited with status 255".to_string()
        } else {
            stderr
        }));
    }
    match state(&output.stdout).as_str() {
        "" => Err(ssh::remote_error(output)),
        state => Ok(state == "active"),
    }
}

/// Whether xochitl is up after all, following a start or restart that
/// failed with `output` (callers map their own refusals, guard statuses,
/// first). A tablet that did not answer it ([`ssh::is_unreachable`]) is
/// not asked again, since every further try only waits for another
/// timeout: [`Error::Unreachable`]. Any other failure, a dropped channel
/// included, gets one look after `delay` ([`RECHECK_DELAY`]; tests pass
/// less): `look` runs [`IS_ACTIVE`], after whatever the caller must
/// check first, and reads it with [`active`].
pub(crate) fn came_back(
    output: &Output,
    delay: Duration,
    look: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    if ssh::is_unreachable(output) {
        return Err(ssh::failure(output));
    }
    std::thread::sleep(delay);
    look()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn state_is_the_trimmed_answer() {
        assert_eq!(state(b"active\n"), "active");
        assert_eq!(state(b" inactive \n"), "inactive");
        assert_eq!(state(b""), "");
    }

    #[test]
    fn active_reads_systemctl_and_never_takes_ssh_failure_for_inactive() {
        assert!(active(&output(0, "active\n", "")).unwrap());
        assert!(!active(&output(3, "inactive\n", "")).unwrap());
        assert!(!active(&output(3, "failed\n", "")).unwrap());
        assert!(!active(&output(3, "activating\n", "")).unwrap());
        // ssh's own failure: the tablet did not answer, with or without
        // a message, and whatever it printed before.
        assert!(matches!(
            active(&output(255, "", "")),
            Err(Error::Unreachable(detail)) if detail == "ssh exited with status 255"
        ));
        assert!(matches!(
            active(&output(255, "inactive\n", "Connection to tablet closed by remote host.\n")),
            Err(Error::Unreachable(detail)) if detail == "Connection to tablet closed by remote host."
        ));
        // No answer at all is not "not active" either.
        assert!(matches!(
            active(&output(1, "", "systemctl: not found")),
            Err(Error::Remote { status: 1, .. })
        ));
    }

    #[test]
    fn came_back_looks_once_unless_the_tablet_did_not_answer() {
        use std::cell::Cell;
        let looks = Cell::new(0);
        let look = |answer: Result<bool>| {
            looks.set(looks.get() + 1);
            answer
        };
        // A dropped channel (255 without a timeout): one look.
        let dropped = output(255, "", "Connection to tablet closed by remote host.");
        assert!(came_back(&dropped, Duration::ZERO, || look(Ok(true))).unwrap());
        assert!(
            !came_back(&output(1, "", "Job failed"), Duration::ZERO, || look(Ok(
                false
            )))
            .unwrap()
        );
        assert!(matches!(
            came_back(&output(1, "", ""), Duration::ZERO, || look(Err(
                Error::Unreachable("x".into())
            ))),
            Err(Error::Unreachable(_))
        ));
        assert_eq!(looks.get(), 3);
        // ssh gave up on the tablet: no look, its typed error.
        let asleep = output(
            255,
            "",
            "ssh: connect to host tablet port 22: Connection timed out",
        );
        assert!(matches!(
            came_back(&asleep, Duration::ZERO, || look(Ok(true))),
            Err(Error::Unreachable(_))
        ));
        assert_eq!(looks.get(), 3);
    }
}
