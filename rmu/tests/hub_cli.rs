//! `rmu hub` end to end: the real binary against a local bare origin and
//! a "tablet" that is a scratch directory behind a mock `ssh`. The mock
//! runs every remote command locally with a curated `PATH` (the host's
//! tools plus stub `systemctl`/`systemd-run`, so nothing reaches the
//! host's systemd) and rewrites the tablet's fixed paths (`/home/root/…`,
//! the boot id, model and firmware files) into the scratch directory.
//! Scripts arrive on stdin (`sh -s`) and are rewritten there; other
//! commands are rewritten on the command line, leaving their stdin
//! (tar streams, state files) untouched. With `RMU_TEST_BUSYBOX` set (as
//! in the dev shell and `nix flake check`), every scenario also runs with
//! BusyBox applets and `ash`, as on a tablet. Git is hermetic. Every name
//! and id is fabricated.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use test_support::{Scratch, assert_success};

/// Tools the tablet scripts may run (as in the library's device tests).
const TOOLS: &[&str] = &[
    "sh",
    "cat",
    "mkdir",
    "mv",
    "rm",
    "stat",
    "find",
    "tar",
    "sha256sum",
    "grep",
    "date",
    "wc",
    "sync",
    "touch",
    "sleep",
    "sed",
    "mktemp",
    "printf",
    "chmod",
    "chown",
    "head",
    "flock",
    "setsid",
    "kill",
    "true",
    "false",
    "uname",
    "df",
];

const MOCK_SSH: &str = r#"while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
[ "$#" -ge 3 ] || { echo "mock ssh: unexpected arguments" >&2; exit 90; }
shift 2
PATH=$MOCK_TOOLS
export PATH
# The scratch directory, escaped for a sed replacement.
dev=$(printf '%s\n' "$MOCK_DEVICE" | sed 's/[\\&#]/\\&/g')
rewrite() {
    sed -e "s#/home/root/#$dev/home/root/#g" \
        -e "s#/proc/sys/kernel/random/boot_id#$dev/boot_id#g" \
        -e "s#/sys/devices/soc0/machine#$dev/machine#g" \
        -e "s#/proc/device-tree/#$dev/device-tree/#g" \
        -e "s#/etc/os-release#$dev/os-release#g" \
        -e "s#/etc/version#$dev/version#g" \
        -e "s#/usr/share/remarkable/#$dev/usr-share/#g"
}
command=$(printf '%s\n' "$1" | rewrite)
printf '%s\n' "$command" | head -n 1 >> "$MOCK_DEVICE/ssh.log"
if [ "$command" = "sh -s" ]; then
    rewrite | sh -s
    exit $?
fi
exec sh -c "$command"
"#;

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

/// Where the tablet's tools come from: the host, or BusyBox.
fn tool_sets() -> Vec<Option<PathBuf>> {
    std::iter::once(None)
        .chain(std::env::var_os("RMU_TEST_BUSYBOX").map(|busybox| Some(PathBuf::from(busybox))))
        .collect()
}

struct World {
    scratch: Scratch,
}

impl World {
    fn new(busybox: Option<&Path>) -> Self {
        let scratch = Scratch::new("hub-cli");
        let device = scratch.join("device");
        let tools = scratch.join("tools");
        [
            device.join("home/root/.local/share/remarkable/xochitl"),
            device.join("home/root/.local/share/rmu"),
            device.join("home/root/.config/remarkable"),
            tools.clone(),
            scratch.join("home"),
        ]
        .iter()
        .for_each(|dir| fs::create_dir_all(dir).unwrap());
        match busybox {
            None => TOOLS.iter().for_each(|tool| {
                let found = which(tool).unwrap_or_else(|| panic!("{tool} is not on PATH"));
                std::os::unix::fs::symlink(found, tools.join(tool)).unwrap();
            }),
            Some(busybox) => {
                let applets = Command::new(busybox).arg("--list").output().unwrap();
                let applets = String::from_utf8(applets.stdout).unwrap();
                TOOLS
                    .iter()
                    .filter(|tool| applets.lines().any(|applet| applet == **tool))
                    .for_each(|tool| {
                        std::os::unix::fs::symlink(busybox, tools.join(tool)).unwrap()
                    });
            }
        }
        let sys = device.join("sys");
        fs::create_dir_all(&sys).unwrap();
        let sys = sys.display().to_string();
        test_support::write_script(
            &tools.join("systemctl"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{sys}/systemctl.log'\n\
                 case \"$*\" in\n\
                 'stop xochitl.service') rm -f '{sys}/running' ;;\n\
                 'start xochitl.service') : > '{sys}/running' ;;\n\
                 'is-active xochitl.service'|'is-active xochitl') if [ -e '{sys}/running' ]; then echo active; else echo inactive; exit 3; fi ;;\n\
                 'is-active --quiet xochitl.service') [ -e '{sys}/running' ] || exit 3 ;;\n\
                 'is-active --quiet '*'.timer') u=${{3%.timer}}; [ -e '{sys}'/watchdog-\"$u\" ] || exit 3 ;;\n\
                 'stop '*'.timer') u=${{2%.timer}}; rm -f '{sys}'/watchdog-\"$u\" ;;\n\
                 esac\n\
                 exit 0\n"
            ),
        );
        test_support::write_script(
            &tools.join("systemd-run"),
            &format!(
                "unit=\n\
                 while [ $# -gt 0 ]; do case \"$1\" in --unit=*) unit=${{1#--unit=}}; shift ;; --*) shift ;; *) break ;; esac; done\n\
                 : > '{sys}'/watchdog-\"$unit\"\n"
            ),
        );
        fs::write(device.join("sys/running"), b"").unwrap();
        fs::write(
            device.join("boot_id"),
            "0f0e0d0c-0000-4000-8000-0000000000b1\n",
        )
        .unwrap();
        fs::write(device.join("machine"), "reMarkable Fake\n").unwrap();
        fs::write(device.join("os-release"), "IMG_VERSION=\"3.28.0\"\n").unwrap();
        fs::write(
            device.join("home/root/.config/remarkable/xochitl.conf"),
            "[General]\ndevicetoken=\n",
        )
        .unwrap();
        scratch.mock_command("ssh", MOCK_SSH);
        Self { scratch }
    }

    fn device(&self) -> PathBuf {
        self.scratch.join("device")
    }

    fn xochitl(&self) -> PathBuf {
        self.device()
            .join("home/root/.local/share/remarkable/xochitl")
    }

    fn origin(&self) -> String {
        self.scratch.join("origin.git").display().to_string()
    }

    /// `rmu --host tablet --no-multiplex hub <args>`, stdout not a terminal.
    fn rmu(&self, args: &[&str]) -> Output {
        self.rmu_in(&self.scratch.join("home"), args)
    }

    fn rmu_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.rmu_full(dir, &[], args)
    }

    /// `rmu --host tablet --no-multiplex <globals> hub <args>` in `dir`.
    fn rmu_full(&self, dir: &Path, globals: &[&str], args: &[&str]) -> Output {
        self.command(dir)
            .args(["--host", "tablet", "--no-multiplex"])
            .args(globals)
            .arg("hub")
            .args(args)
            .output()
            .unwrap()
    }

    /// `rmu --no-multiplex hub <args>`: the default host, not given.
    fn rmu_default_host(&self, args: &[&str]) -> Output {
        self.command(&self.scratch.join("home"))
            .args(["--no-multiplex", "hub"])
            .args(args)
            .output()
            .unwrap()
    }

    /// The binary in `dir`, with the mock `ssh` and hermetic git.
    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rmu"));
        command
            .current_dir(dir)
            .env("PATH", self.scratch.mock_path())
            .env("MOCK_TOOLS", self.scratch.join("tools"))
            .env("MOCK_DEVICE", self.device())
            .env("HOME", self.scratch.join("home"))
            .env("XDG_CONFIG_HOME", self.scratch.join("home"))
            .env("XDG_CACHE_HOME", self.scratch.join("cache"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_COUNT", "0")
            .env("GIT_DEFAULT_HASH", "sha1")
            .env_remove("GIT_TRACE")
            .env_remove("GIT_TRACE_PACKET")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("RMU_SSH_PASSWORD");
        command
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.rmu(&[args, &["--json"]].concat());
        assert_success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn put_pdf(&self, n: u64, name: &str) -> String {
        self.put_pdf_in(&self.xochitl(), n, name)
    }

    fn put_pdf_in(&self, dir: &Path, n: u64, name: &str) -> String {
        let id = format!("8c1f0b9e-0000-4000-8000-{n:012x}");
        fs::write(
            dir.join(format!("{id}.metadata")),
            json!({"parent": "", "type": "DocumentType", "visibleName": name}).to_string(),
        )
        .unwrap();
        fs::write(
            dir.join(format!("{id}.content")),
            libremarkable_utils_content(),
        )
        .unwrap();
        fs::write(
            dir.join(format!("{id}.pdf")),
            format!("%PDF-1.4 fabricated {n}\n"),
        )
        .unwrap();
        id
    }

    fn ssh_log(&self) -> String {
        fs::read_to_string(self.device().join("ssh.log")).unwrap_or_default()
    }

    fn running(&self) -> bool {
        self.device().join("sys/running").exists()
    }
}

/// A PDF's `.content`, as `rmu upload` writes it.
fn libremarkable_utils_content() -> String {
    json!({
        "cPages": {"original": {"timestamp": "1:0", "value": -1}, "pages": []},
        "coverPageNumber": 0,
        "documentMetadata": {},
        "extraMetadata": {},
        "fileType": "pdf",
        "formatVersion": 2,
        "pageCount": 0,
        "tags": [],
    })
    .to_string()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn every_command_over_ssh() {
    tool_sets()
        .iter()
        .for_each(|busybox| every_command(busybox.as_deref()));
}

fn every_command(busybox: Option<&Path>) {
    let world = World::new(busybox);
    let origin = world.origin();
    let first = world.put_pdf(1, "Paper one");

    // A relative origin is stored absolute: later runs start elsewhere.
    let output = world.rmu_in(world.scratch.path(), &["init", "origin.git", "--json"]);
    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "init");
    assert_eq!(
        fs::canonicalize(value["origin"].as_str().unwrap()).unwrap(),
        fs::canonicalize(&origin).unwrap()
    );
    assert!(Path::new(&origin).join("HEAD").exists());
    assert!(world.ssh_log().is_empty(), "init never contacts the tablet");

    let output = world.rmu_in(
        world.scratch.path(),
        &["clone", "origin.git", "--name", "Tablet A", "--json"],
    );
    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "clone");
    assert_eq!(value["summary"]["added"], 1);
    assert!(
        world
            .device()
            .join("home/root/.local/share/rmu/hub.json")
            .exists()
    );
    assert!(
        !world
            .device()
            .join("home/root/.local/share/rmu/lock")
            .exists()
    );
    assert!(world.running());
    let systemctl = fs::read_to_string(world.device().join("sys/systemctl.log")).unwrap();
    assert!(
        systemctl.contains("reset-failed xochitl.service\nstart xochitl.service"),
        "{systemctl}"
    );

    let second = world.put_pdf(2, "Paper two");
    let output = world.rmu(&["status"]);
    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("A {second}\n")
    );
    assert!(stderr(&output).contains(&format!("added    \"Paper two\" ({second})")));
    let output = world.rmu(&["--quiet", "status"]);
    assert!(output.stderr.is_empty(), "{}", stderr(&output));

    let value = world.json(&["sync", "--dry-run"]);
    assert_eq!(value["command"], "sync --dry-run");
    assert_eq!(value["would_push"], true);

    let output = world.rmu(&["sync"]);
    assert_success(&output);
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(commit.len(), 40, "{commit}");
    assert!(
        stderr(&output).contains("Synced: pushed the tablet's changes"),
        "{}",
        stderr(&output)
    );
    let log = Command::new("git")
        .args(["--git-dir", &origin, "log", "--format=%an", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("Tablet A"));

    let value = world.json(&["backup"]);
    assert_eq!(value["command"], "backup");
    let value = world.json(&["status"]);
    assert_eq!(value["changes"], json!({}));
    assert_eq!(value["device_name"], "Tablet A");

    // Remote and detach: the attachment only, documents untouched.
    let output = world.rmu(&["remote"]);
    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{origin}\n")
    );
    let moved = world.scratch.join("moved.git").display().to_string();
    assert!(
        Command::new("cp")
            .args(["-R", &origin, &moved])
            .status()
            .unwrap()
            .success()
    );
    let value = world.json(&["remote", "set-url", &moved]);
    assert_eq!(value["previous"], origin.as_str());
    assert_eq!(value["remote"]["origin"], moved.as_str());
    world.json(&["sync"]);
    let value = world.json(&["detach", "--prune-ref"]);
    assert!(
        value["pruned"].as_str().unwrap().contains("rmu/devices/"),
        "{value}"
    );
    assert!(
        !world
            .device()
            .join("home/root/.local/share/rmu/hub.json")
            .exists()
    );
    let output = world.rmu(&["status"]);
    assert_eq!(code(&output), 1);
    assert!(world.running());
    assert!(world.xochitl().join(format!("{first}.pdf")).exists());
    assert!(world.xochitl().join(format!("{second}.pdf")).exists());
}

#[test]
fn history_commands_over_ssh() {
    tool_sets()
        .iter()
        .for_each(|busybox| history_commands(busybox.as_deref()));
}

/// `log`, `restore` and `add`: with `--origin` alone the tablet is never
/// contacted; without it, it supplies its stored origin; with `--host`
/// as well, it is the commit's author.
fn history_commands(busybox: Option<&Path>) {
    let world = World::new(busybox);
    let origin = world.origin();
    let paper = world.put_pdf(1, "Paper one");
    assert_success(&world.rmu(&["init", &origin]));
    assert_success(&world.rmu(&["clone", &origin, "--name", "Tablet A"]));
    let contacts = world.ssh_log().lines().count();

    let file = world.scratch.join("Scan.pdf");
    fs::write(&file, test_support::pdf(4)).unwrap();
    let output = world.rmu_default_host(&["add", &file.display().to_string(), "--origin", &origin]);
    assert_success(&output);
    let added = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        stderr(&output).contains("4 pages (page list written)"),
        "{}",
        stderr(&output)
    );
    let output = world.rmu_default_host(&["log", "--origin", &origin, "--json"]);
    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["entries"][0]["added"], json!([added]));
    assert_eq!(value["tablet"], Value::Null);
    assert_eq!(
        world.ssh_log().lines().count(),
        contacts,
        "--origin alone never contacts the tablet"
    );

    let value = world.json(&["log", "Paper one"]);
    assert_eq!(value["doc"], paper.as_str());
    assert_eq!(value["tablet"]["device_name"], "Tablet A");
    let value = world.json(&["restore", &paper, "--at", "main", "--origin", &origin]);
    assert_eq!(value["command"], "restore");
    let author = Command::new("git")
        .args(["--git-dir", &origin, "log", "-1", "--format=%an", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&author.stdout).trim(), "Tablet A");

    let value = world.json(&["sync"]);
    assert_eq!(value["applied"].as_array().unwrap().len(), 2, "{value}");
    assert!(world.xochitl().join(format!("{added}.pdf")).exists());
    let content: Value = serde_json::from_slice(
        &fs::read(world.xochitl().join(format!("{added}.content"))).unwrap(),
    )
    .unwrap();
    assert_eq!(content["cPages"]["pages"].as_array().unwrap().len(), 4);
}

#[test]
fn exit_codes_over_ssh() {
    tool_sets()
        .iter()
        .for_each(|busybox| exit_codes(busybox.as_deref()));
}

fn exit_codes(busybox: Option<&Path>) {
    let world = World::new(busybox);
    let origin = world.origin();
    world.put_pdf(1, "Paper one");
    assert_eq!(code(&world.rmu(&["sync", "--dry-run", "--break-lock"])), 2);
    let output = world.rmu_full(&world.scratch.join("home"), &["--no-restart"], &["sync"]);
    assert_eq!(code(&output), 2);
    assert!(
        stderr(&output).contains("--no-restart does not apply to rmu hub"),
        "{}",
        stderr(&output)
    );
    assert!(world.ssh_log().is_empty(), "refused before connecting");

    let output = world.rmu(&["status"]);
    assert_eq!(code(&output), 1, "not attached");
    assert!(
        stderr(&output).contains("rmu hub clone"),
        "{}",
        stderr(&output)
    );

    world.json(&["init", &origin]);
    world.json(&["clone", &origin]);

    // Another run's lock, from this boot: not stale.
    let lock = world.device().join("home/root/.local/share/rmu/lock");
    fs::create_dir(&lock).unwrap();
    fs::write(
        lock.join("owner"),
        "run=0f0e0d0c00004000800000000000d001\nhost=other-computer\npid=7\nstarted=1790000000\nboot_id=0f0e0d0c-0000-4000-8000-0000000000b1\n",
    )
    .unwrap();
    let output = world.rmu(&["sync", "--json"]);
    assert_eq!(code(&output), 4, "{}", stderr(&output));
    assert!(stderr(&output).contains("--break-lock"));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["kind"], "lock_held");
    assert!(world.running());
    assert_success(&world.rmu(&["sync", "--break-lock"]));

    // A different origin: identity check.
    let other = world.scratch.join("other.git").display().to_string();
    world.json(&["init", &other]);
    let output = world.rmu(&["sync", "--origin", &other]);
    assert_eq!(code(&output), 5, "{}", stderr(&output));
    assert!(stderr(&output).contains("--reattach"));

    // Cloud pairing.
    fs::write(
        world
            .device()
            .join("home/root/.config/remarkable/xochitl.conf"),
        "[General]\ndevicetoken=fabricated-token\n",
    )
    .unwrap();
    let output = world.rmu(&["sync"]);
    assert_eq!(code(&output), 6, "{}", stderr(&output));
    assert!(world.running());
}

#[test]
fn a_custom_xochitl_dir_over_ssh() {
    tool_sets()
        .iter()
        .for_each(|busybox| custom_xochitl_dir(busybox.as_deref()));
}

/// `--xochitl-dir` reaches every tablet script (the default stays empty).
fn custom_xochitl_dir(busybox: Option<&Path>) {
    let world = World::new(busybox);
    let origin = world.origin();
    let custom = world.device().join("custom xochitl");
    fs::create_dir_all(&custom).unwrap();
    let id = world.put_pdf_in(&custom, 1, "Paper one");
    let custom_arg = custom.display().to_string();
    let globals = ["--xochitl-dir", custom_arg.as_str()];
    let home = world.scratch.join("home");
    world.json(&["init", &origin]);
    let output = world.rmu_full(&home, &globals, &["clone", &origin, "--json"]);
    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["summary"]["added"], 1, "{value}");
    assert_eq!(fs::read_dir(world.xochitl()).unwrap().count(), 0);
    let output = world.rmu_full(&home, &globals, &["status"]);
    assert_success(&output);
    assert!(output.stdout.is_empty(), "no changes");
    world.put_pdf_in(&custom, 2, "Paper two");
    let output = world.rmu_full(&home, &globals, &["status"]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("A 8c1f0b9e-0000-4000-8000-{:012x}\n", 2)
    );
    assert!(custom.join(format!("{id}.pdf")).exists());
}
