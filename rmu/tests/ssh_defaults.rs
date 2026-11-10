//! rmu's ssh timeout defaults reach ssh, and `--no-ssh-defaults` (or
//! `RMU_NO_SSH_DEFAULTS=1`) keeps them off. The mock `ssh` answers `-G`
//! as for an empty ssh config, records every other invocation's
//! arguments, and fails it: only the arguments matter here.
#![cfg(unix)]

use std::fs;
use std::process::Command;

use test_support::Scratch;

const MOCK_SSH: &str = r#"for arg in "$@"; do
    [ "$arg" = -G ] && { printf 'connecttimeout none\nserveraliveinterval 0\n'; exit 0; }
done
printf '%s\n' "$@" >> "$MOCK_ARGS"
echo 'mock ssh: no tablet here' >&2
exit 1
"#;

/// The ssh options (the arguments before `--`) of the first command of
/// `rmu --no-multiplex <args> ls`, with `env` set.
fn ssh_args(args: &[&str], env: &[(&str, &str)]) -> Vec<String> {
    let scratch = Scratch::new("ssh-defaults-cli");
    scratch.mock_command("ssh", MOCK_SSH);
    let log = scratch.join("args");
    let output = Command::new(env!("CARGO_BIN_EXE_rmu"))
        .arg("--no-multiplex")
        .args(args)
        .arg("ls")
        .env("PATH", scratch.mock_path())
        .env("HOME", scratch.path())
        .env("XDG_CACHE_HOME", scratch.join("cache"))
        .env("XDG_STATE_HOME", scratch.join("state"))
        .env("MOCK_ARGS", &log)
        .env_remove("RMU_NO_SSH_DEFAULTS")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(!output.status.success(), "the mock fails every command");
    fs::read_to_string(&log)
        .unwrap()
        .lines()
        .take_while(|arg| *arg != "--")
        .map(str::to_string)
        .collect()
}

fn has_defaults(args: &[String]) -> bool {
    ["ConnectTimeout=15", "ServerAliveInterval=10"]
        .iter()
        .all(|default| args.iter().any(|arg| arg == default))
}

#[test]
fn defaults_reach_ssh_unless_turned_off() {
    assert!(has_defaults(&ssh_args(&[], &[])));
    let off = ssh_args(&["--no-ssh-defaults"], &[]);
    assert!(
        !off.iter()
            .any(|arg| arg.starts_with("ConnectTimeout") || arg.starts_with("ServerAlive")),
        "{off:?}"
    );
    assert_eq!(ssh_args(&[], &[("RMU_NO_SSH_DEFAULTS", "1")]), off);
    // Only `1` turns them off.
    assert!(has_defaults(&ssh_args(
        &[],
        &[("RMU_NO_SSH_DEFAULTS", "0")]
    )));
    // A -o of the user's comes first, so ssh keeps it.
    let own = ssh_args(&["-o", "ServerAliveInterval=0"], &[]);
    assert!(
        !own.iter().any(|arg| arg == "ServerAliveInterval=10"),
        "{own:?}"
    );
    assert!(own.iter().any(|arg| arg == "ConnectTimeout=15"), "{own:?}");
}
