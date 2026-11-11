//! `rmu restart` when the restart fails, over a mock `ssh` running a
//! simulated tablet (`test_support::tablet::SimulatedTablet`). The flow is
//! the one the hub's start shares (`xochitl_service::came_back`): one look
//! after a failure, and ssh's own failure (exit 255) is the tablet not
//! answering, never "xochitl is not active". Every name is fabricated.
#![cfg(target_os = "linux")]

use std::process::{Command, Output};

use test_support::tablet::{REWRITING_SSH, SimulatedTablet, Toolset};
use test_support::{Scratch, assert_success};

const DROPPED: &str = "echo 'Connection to tablet closed by remote host.' >&2; exit 255";

struct Fixture {
    scratch: Scratch,
    tablet: SimulatedTablet,
}

impl Fixture {
    /// A tablet whose `systemctl` first runs `arms` (see
    /// `TabletTools::systemctl_arms`).
    fn new(arms: &str) -> Self {
        let scratch = Scratch::new("restart");
        scratch.mock_command("ssh", REWRITING_SSH);
        let tablet = SimulatedTablet::new(&scratch.join("hosts/tablet"), &Toolset::Host);
        tablet.tools().systemctl_arms(arms);
        Self { scratch, tablet }
    }

    fn restart(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rmu"))
            .current_dir(self.scratch.path())
            .args(["--host", "tablet", "--no-multiplex", "restart"])
            .env("PATH", self.scratch.mock_path())
            .env("MOCK_ROOT", self.scratch.join("hosts"))
            .env("XDG_STATE_HOME", self.scratch.join("state"))
            .env("HOME", self.scratch.join("home"))
            .env_remove("RMU_SSH_PASSWORD")
            .output()
            .unwrap()
    }

    /// The xochitl commands `systemctl` saw, in order.
    fn calls(&self) -> Vec<String> {
        self.tablet
            .tools()
            .systemctl_log()
            .lines()
            .filter(|line| line.ends_with("xochitl.service"))
            .map(str::to_string)
            .collect()
    }

    fn failed(&self) -> String {
        let output = self.restart();
        assert!(!output.status.success());
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

const ALL: [&str; 3] = [
    "reset-failed xochitl.service",
    "restart xochitl.service",
    "is-active xochitl.service",
];

#[test]
fn a_restart_whose_channel_dropped_succeeds_once_xochitl_is_back() {
    let fixture = Fixture::new(&format!(
        "'restart xochitl.service') \"$stub\" \"$@\"; {DROPPED} ;;"
    ));
    assert_success(&fixture.restart());
    assert_eq!(fixture.calls(), ALL);
}

#[test]
fn no_answer_to_the_look_is_an_unreachable_tablet_not_a_failed_restart() {
    let fixture = Fixture::new(&format!(
        "'restart xochitl.service') {DROPPED} ;;\n'is-active xochitl.service') exit 255 ;;"
    ));
    let stderr = fixture.failed();
    assert!(stderr.contains("the tablet did not answer"), "{stderr}");
    assert!(!stderr.contains("failed to restart xochitl"), "{stderr}");
    assert_eq!(fixture.calls(), ALL);
}

#[test]
fn a_failed_restart_with_xochitl_down_says_so() {
    let fixture = Fixture::new(
        "'restart xochitl.service') echo 'Job failed.' >&2; exit 1 ;;\n\
         'is-active xochitl.service') echo inactive; exit 3 ;;",
    );
    let stderr = fixture.failed();
    assert!(
        stderr.contains("failed to restart xochitl: restart exit=Some(1) stderr='Job failed.'; is-active: inactive"),
        "{stderr}"
    );
    assert_eq!(fixture.calls(), ALL);
}

/// For the client this is `SshSession::run` turning the timeout into
/// `Error::Unreachable` before `came_back` sees it; `came_back`'s own
/// check is exercised by the hub's tests (`xochitl_service::tests` and
/// `contract_tests::ssh_tablet`), whose start passes raw output.
#[test]
fn a_tablet_that_timed_out_is_not_asked_again() {
    let fixture = Fixture::new(
        "'restart xochitl.service') echo 'Timeout, server tablet not responding.' >&2; exit 255 ;;",
    );
    let stderr = fixture.failed();
    assert!(stderr.contains("the tablet did not answer"), "{stderr}");
    assert_eq!(fixture.calls(), ALL[..2]);
}
