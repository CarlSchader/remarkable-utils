//! The xochitl systemd unit, as `rmu`'s maintenance restart
//! ([`Client::restart_xochitl`](crate::Client::restart_xochitl)) and
//! `rmu hub` (its starts and its watchdog) drive it.
//!
//! xochitl has a strict start limit: repeated plain starts or restarts
//! can hit `start-limit-hit` and reboot the whole tablet, so every start
//! or restart is preceded by [`RESET_FAILED`].

/// Clears the unit's failure count; its exit status does not matter.
pub(crate) const RESET_FAILED: &str = "systemctl reset-failed xochitl.service";

pub(crate) const START: &str = "systemctl start xochitl.service";

pub(crate) const RESTART: &str = "systemctl restart xochitl.service";

/// Prints the unit's state (see [`state`]); exits non-zero for every
/// state but `active`.
pub(crate) const IS_ACTIVE: &str = "systemctl is-active xochitl.service";

/// The state [`IS_ACTIVE`] printed (`active`, `inactive`, `failed`, ...),
/// trimmed.
pub(crate) fn state(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_is_the_trimmed_answer() {
        assert_eq!(state(b"active\n"), "active");
        assert_eq!(state(b" inactive \n"), "inactive");
        assert_eq!(state(b""), "");
    }
}
