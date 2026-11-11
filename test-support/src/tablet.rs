//! A simulated reMarkable tablet host for tests that run rmu's real device
//! scripts locally: a curated tool directory (the host's GNU tools, or
//! BusyBox applets and `ash`, as on a tablet), stub `systemctl` and
//! `systemd-run` that simulate xochitl and the watchdog timers in a state
//! directory (nothing ever reaches the host's systemd), and a mock `ssh`
//! that runs each remote command against a device directory laid out like
//! the tablet's filesystem.
//!
//! - [`TabletTools`]: the tools and stubs, for tests that pass their own
//!   paths to the scripts (the library's device harness).
//! - [`SimulatedTablet`]: a device directory with the tablet's fixed paths
//!   (`home/root/…`, the boot id, model and firmware files), reached
//!   through [`REWRITING_SSH`], for tests of the `rmu` binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::write_script;

/// The tools the tablet scripts may run (the applets a tablet's BusyBox
/// provides that rmu relies on); nothing else is on the simulated `PATH`.
pub const TOOLS: &[&str] = &[
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

/// The fabricated boot id a [`SimulatedTablet`] starts with.
pub const BOOT_ID: &str = "0f0e0d0c-0000-4000-8000-0000000000b1";

/// The xochitl data dir, relative to a [`SimulatedTablet`]'s device dir.
pub const XOCHITL: &str = "home/root/.local/share/remarkable/xochitl";

/// rmu's hub state dir, relative to a [`SimulatedTablet`]'s device dir.
pub const STATE: &str = "home/root/.local/share/rmu";

/// Where the simulated tablet's tools come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toolset {
    /// The host's (GNU) tools.
    Host,
    /// The applets of this BusyBox binary, `sh` included.
    BusyBox(PathBuf),
}

impl Toolset {
    /// BusyBox from `RMU_TEST_BUSYBOX`, if set (the dev shell and the nix
    /// checks set it).
    pub fn busybox() -> Option<Toolset> {
        std::env::var_os("RMU_TEST_BUSYBOX").map(|path| Toolset::BusyBox(PathBuf::from(path)))
    }

    /// The host's tools, then BusyBox if available.
    pub fn all() -> Vec<Toolset> {
        std::iter::once(Toolset::Host)
            .chain(Self::busybox())
            .collect()
    }
}

/// `text` as one single-quoted shell word.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn q(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

/// The first `tool` on the `PATH` of this process.
pub fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

/// The simulated host's tools (`bin`) and systemd state (`sys`).
pub struct TabletTools {
    bin: PathBuf,
    sys: PathBuf,
    toolset: Toolset,
}

impl TabletTools {
    /// Link [`TOOLS`] from `toolset` into `bin` (tools the toolset lacks
    /// are left out: see [`Self::has_tool`]), install the stub `systemctl`
    /// and `systemd-run`, and start with xochitl running. Both directories
    /// are created.
    pub fn install(bin: &Path, sys: &Path, toolset: &Toolset) -> Self {
        fs::create_dir_all(bin).unwrap();
        fs::create_dir_all(sys).unwrap();
        match toolset {
            Toolset::Host => TOOLS.iter().for_each(|tool| {
                if let Some(found) = which(tool) {
                    std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
                }
            }),
            Toolset::BusyBox(busybox) => {
                let output = Command::new(busybox).arg("--list").output().unwrap();
                let applets = String::from_utf8(output.stdout).unwrap();
                TOOLS
                    .iter()
                    .filter(|tool| applets.lines().any(|applet| applet == **tool))
                    .for_each(|tool| std::os::unix::fs::symlink(busybox, bin.join(tool)).unwrap());
            }
        }
        let tools = Self {
            bin: bin.to_path_buf(),
            sys: sys.to_path_buf(),
            toolset: toolset.clone(),
        };
        write_script(&bin.join("systemctl"), &tools.systemctl_stub());
        write_script(&bin.join("systemd-run"), &tools.systemd_run_stub());
        fs::write(sys.join("running"), b"").unwrap();
        tools
    }

    /// Logs every call; `stop`/`start` of xochitl remove/create
    /// `sys/running`, which `is-active` reports; a timer is active while
    /// its `sys/watchdog-<unit>` exists, and `stop` removes it. Anything
    /// else succeeds.
    fn systemctl_stub(&self) -> String {
        format!(
            "[ -n \"$RMU_SYSTEMCTL_LOGGED\" ] || printf '%s\\n' \"$*\" >> {log}\n\
             case \"$*\" in\n\
             'stop xochitl.service') rm -f {running} ;;\n\
             'start xochitl.service') : > {running} ;;\n\
             'is-active xochitl.service'|'is-active xochitl') if [ -e {running} ]; then echo active; else echo inactive; exit 3; fi ;;\n\
             'is-active --quiet xochitl.service') [ -e {running} ] || exit 3 ;;\n\
             'is-active --quiet '*'.timer') u=${{3%.timer}}; [ -e {sys}/watchdog-\"$u\" ] || exit 3 ;;\n\
             'stop '*'.timer') u=${{2%.timer}}; rm -f {sys}/watchdog-\"$u\" ;;\n\
             esac\n\
             exit 0\n",
            log = q(&self.sys.join("systemctl.log")),
            running = q(&self.sys.join("running")),
            sys = q(&self.sys),
        )
    }

    /// Logs every call and records the command of `--unit=<unit>` (its
    /// arguments, NUL-separated) in `sys/watchdog-<unit>`: an armed timer,
    /// which [`Self::fire_watchdog`] fires.
    fn systemd_run_stub(&self) -> String {
        format!(
            "printf '%s\\n' \"$*\" >> {log}\n\
             unit=\n\
             while [ $# -gt 0 ]; do case \"$1\" in --unit=*) unit=${{1#--unit=}}; shift ;; --*) shift ;; *) break ;; esac; done\n\
             for a do printf '%s\\0' \"$a\"; done > {sys}/watchdog-\"$unit\"\n",
            log = q(&self.sys.join("systemd-run.log")),
            sys = q(&self.sys),
        )
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    pub fn sys(&self) -> &Path {
        &self.sys
    }

    pub fn toolset(&self) -> &Toolset {
        &self.toolset
    }

    /// Whether `tool` is on the simulated `PATH`.
    pub fn has_tool(&self, tool: &str) -> bool {
        self.bin.join(tool).exists()
    }

    /// A shell command running the real `tool` (for wrapping it).
    pub fn real_command(&self, tool: &str) -> String {
        match &self.toolset {
            Toolset::Host => q(&which(tool).unwrap_or_else(|| panic!("{tool} is not on PATH"))),
            Toolset::BusyBox(busybox) => format!("{} {tool}", q(busybox)),
        }
    }

    /// A mock `ssh` that runs its last argument with the simulated `PATH`
    /// and nothing rewritten (for scripts given the simulated paths).
    pub fn direct_ssh(&self) -> String {
        format!(
            "for cmd do :; done\nPATH={}; export PATH\nexec sh -c \"$cmd\"\n",
            q(&self.bin)
        )
    }

    /// Make `systemctl` run `arms` first: `case "$*"` arms such as
    /// `'start xochitl.service') exit 1 ;;`, which can run the stub (to
    /// simulate the command) as `"$stub" "$@"`. Anything no arm ends goes
    /// to the stub. Every call is logged as before. Each call replaces the
    /// previous arms; `""` restores plain stub behaviour. Also for
    /// failures only ssh could cause (exit 255): the mock `ssh` passes the
    /// status through.
    pub fn systemctl_arms(&self, arms: &str) {
        let systemctl = self.bin.join("systemctl");
        let stub = self.sys.join("systemctl-stub");
        if !stub.exists() {
            fs::rename(&systemctl, &stub).unwrap();
        }
        write_script(
            &systemctl,
            &format!(
                "printf '%s\\n' \"$*\" >> {log}\n\
                 RMU_SYSTEMCTL_LOGGED=1; export RMU_SYSTEMCTL_LOGGED\n\
                 stub={stub}\ncase \"$*\" in\n{arms}\nesac\nexec \"$stub\" \"$@\"\n",
                log = q(&self.sys.join("systemctl.log")),
                stub = q(&stub),
            ),
        );
    }

    pub fn systemctl_log(&self) -> String {
        fs::read_to_string(self.sys.join("systemctl.log")).unwrap_or_default()
    }

    pub fn systemd_run_log(&self) -> String {
        fs::read_to_string(self.sys.join("systemd-run.log")).unwrap_or_default()
    }

    /// Whether the simulated xochitl runs.
    pub fn running(&self) -> bool {
        self.sys.join("running").exists()
    }

    /// The timers `systemd-run` armed that are not stopped, sorted.
    pub fn armed_units(&self) -> Vec<PathBuf> {
        let mut units: Vec<PathBuf> = fs::read_dir(&self.sys)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("watchdog-")
            })
            .collect();
        units.sort();
        units
    }

    /// Fire the one armed timer: run its recorded command with the
    /// simulated `PATH`.
    pub fn fire_watchdog(&self) {
        let units = self.armed_units();
        assert_eq!(units.len(), 1, "armed watchdogs: {units:?}");
        let recorded = fs::read(&units[0]).unwrap();
        fs::remove_file(&units[0]).unwrap();
        let args: Vec<String> = recorded
            .split(|&b| b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect();
        let status = Command::new(&args[0])
            .args(&args[1..])
            .env("PATH", &self.bin)
            .status()
            .unwrap();
        assert!(status.success(), "the watchdog's command failed");
    }
}

/// The mock `ssh` for [`SimulatedTablet`]s. It runs the remote command
/// locally with the device's `tools/` as `PATH`, after rewriting the
/// tablet's fixed paths into the device dir: `$MOCK_DEVICE`, or with
/// `$MOCK_ROOT` set, `$MOCK_ROOT/<host>` (one device per ssh host).
/// Scripts arriving on stdin (`sh -s`) are rewritten there; other commands
/// on the command line, leaving their stdin (tar streams, state files)
/// untouched. The device dir is substituted as it is: commands must quote
/// the paths (as rmu's do) if it has spaces. Each command is logged in the device dir: its first line in
/// `ssh.log` (one line per contact), all of it in `ssh-commands.log`
/// (commands separated by `----` lines). `MOCK_LOCK_WHEN` (a substring of
/// a command) and `MOCK_LOCK_RECORD` simulate a hub run taking the lock
/// right before that command runs (once).
pub const REWRITING_SSH: &str = r#"while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
[ "$#" -ge 3 ] || { echo "mock ssh: unexpected arguments" >&2; exit 90; }
host=${2#*@}
shift 2
if [ -n "$MOCK_ROOT" ]; then MOCK_DEVICE=$MOCK_ROOT/$host; fi
export MOCK_DEVICE
PATH=$MOCK_DEVICE/tools
export PATH
# The device dir, escaped for a sed replacement.
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
printf '%s\n----\n' "$command" >> "$MOCK_DEVICE/ssh-commands.log"
lock=$MOCK_DEVICE/home/root/.local/share/rmu/lock
case "$command" in
*"${MOCK_LOCK_WHEN:-no command contains this}"*)
    if [ ! -e "$lock" ]; then
        mkdir -p "$lock"
        printf '%s' "$MOCK_LOCK_RECORD" > "$lock/owner"
    fi ;;
esac
if [ "$command" = "sh -s" ]; then
    rewrite | sh -s
    exit $?
fi
exec sh -c "$command"
"#;

/// A tablet's filesystem in a device dir, served by [`REWRITING_SSH`]:
/// the xochitl data and hub state dirs, an unpaired `xochitl.conf`, the
/// boot id ([`BOOT_ID`]), a model and a firmware version, the tools in
/// `tools/` and the systemd state in `sys/`.
pub struct SimulatedTablet {
    device: PathBuf,
    tools: TabletTools,
}

impl SimulatedTablet {
    pub fn new(device: &Path, toolset: &Toolset) -> Self {
        [XOCHITL, STATE, "home/root/.config/remarkable"]
            .iter()
            .for_each(|dir| fs::create_dir_all(device.join(dir)).unwrap());
        let tools = TabletTools::install(&device.join("tools"), &device.join("sys"), toolset);
        fs::write(device.join("boot_id"), format!("{BOOT_ID}\n")).unwrap();
        fs::write(device.join("machine"), "reMarkable Fake\n").unwrap();
        fs::write(device.join("os-release"), "IMG_VERSION=\"3.28.0\"\n").unwrap();
        fs::write(
            device.join("home/root/.config/remarkable/xochitl.conf"),
            "[General]\ndevicetoken=\n",
        )
        .unwrap();
        Self {
            device: device.to_path_buf(),
            tools,
        }
    }

    /// The device dir (the tablet's `/`).
    pub fn device(&self) -> &Path {
        &self.device
    }

    pub fn xochitl(&self) -> PathBuf {
        self.device.join(XOCHITL)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.device.join(STATE)
    }

    pub fn tools(&self) -> &TabletTools {
        &self.tools
    }

    /// The first line of every command run on this tablet, one per line.
    pub fn ssh_log(&self) -> String {
        fs::read_to_string(self.device.join("ssh.log")).unwrap_or_default()
    }

    /// Every command run on this tablet, in full.
    pub fn commands(&self) -> Vec<String> {
        fs::read_to_string(self.device.join("ssh-commands.log"))
            .unwrap_or_default()
            .split("\n----\n")
            .filter(|command| !command.is_empty())
            .map(str::to_string)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::process::Output;

    use super::*;
    use crate::{Scratch, assert_success};

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn shell_quote_survives_the_shell() {
        ["plain", "it's", "$(x) `y` \"z\"", "a\nb", "", "'"]
            .into_iter()
            .for_each(|text| {
                let output = Command::new("sh")
                    .args(["-c", &format!("printf %s {}", shell_quote(text))])
                    .output()
                    .unwrap();
                assert_eq!(stdout(&output), text);
            });
    }

    fn tools(scratch: &Scratch, toolset: &Toolset) -> TabletTools {
        TabletTools::install(&scratch.join("bin"), &scratch.join("sys"), toolset)
    }

    /// `script` with only the simulated tools on `PATH`.
    fn run(tools: &TabletTools, script: &str) -> Output {
        Command::new(tools.bin().join("sh"))
            .args(["-c", script])
            .env("PATH", tools.bin())
            .output()
            .unwrap()
    }

    #[test]
    fn only_the_curated_tools_are_installed() {
        let scratch = Scratch::new("tablet-tools");
        let tools = tools(&scratch, &Toolset::Host);
        let installed: Vec<String> = fs::read_dir(tools.bin())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(installed.iter().all(|tool| {
            TOOLS.contains(&tool.as_str()) || tool == "systemctl" || tool == "systemd-run"
        }));
        assert!(tools.has_tool("sh") && tools.has_tool("tar"));
        assert!(!tools.has_tool("python3") && !tools.has_tool("bash"));
        assert_eq!(stdout(&run(&tools, "command -v ls || echo none")), "none\n");
    }

    #[test]
    fn busybox_provides_the_tools_when_available() {
        let Some(busybox) = Toolset::busybox() else {
            eprintln!("RMU_TEST_BUSYBOX is not set; skipping");
            return;
        };
        let Toolset::BusyBox(binary) = &busybox else {
            unreachable!()
        };
        let scratch = Scratch::new("tablet-busybox");
        let tools = tools(&scratch, &busybox);
        assert_eq!(&fs::read_link(tools.bin().join("sh")).unwrap(), binary);
        assert_eq!(Toolset::all(), vec![Toolset::Host, busybox]);
        assert!(tools.real_command("mv").ends_with(" mv"));
    }

    #[test]
    fn systemctl_simulates_xochitl_and_the_timers() {
        let scratch = Scratch::new("tablet-systemd");
        let tools = tools(&scratch, &Toolset::Host);
        let code = |script: &str| run(&tools, script).status.code();
        assert!(tools.running());
        assert_eq!(
            stdout(&run(&tools, "systemctl is-active xochitl.service")),
            "active\n"
        );
        assert_eq!(code("systemctl stop xochitl.service"), Some(0));
        assert!(!tools.running());
        let inactive = run(&tools, "systemctl is-active xochitl");
        assert_eq!(
            (stdout(&inactive).as_str(), inactive.status.code()),
            ("inactive\n", Some(3))
        );
        assert_eq!(code("systemctl is-active --quiet xochitl.service"), Some(3));
        assert_eq!(code("systemctl reset-failed xochitl.service"), Some(0));
        assert_eq!(code("systemctl start xochitl.service"), Some(0));
        assert!(tools.running());
        // A timer: armed by systemd-run, active until stopped.
        let fired = scratch.join("fired");
        assert_eq!(
            code(&format!(
                "systemd-run --quiet --unit=w1 --on-active=5s /bin/sh -c {}",
                shell_quote(&format!(": > {}", q(&fired)))
            )),
            Some(0)
        );
        assert_eq!(tools.armed_units(), vec![tools.sys().join("watchdog-w1")]);
        assert_eq!(code("systemctl is-active --quiet w1.timer"), Some(0));
        tools.fire_watchdog();
        assert!(fired.exists() && tools.armed_units().is_empty());
        run(
            &tools,
            "systemd-run --unit=w2 --on-active=5s /bin/sh -c true",
        );
        assert_eq!(code("systemctl stop w2.timer"), Some(0));
        assert_eq!(code("systemctl is-active --quiet w2.timer"), Some(3));
        assert!(tools.armed_units().is_empty());
        assert_eq!(
            tools.systemctl_log().lines().collect::<Vec<_>>(),
            [
                "is-active xochitl.service",
                "stop xochitl.service",
                "is-active xochitl",
                "is-active --quiet xochitl.service",
                "reset-failed xochitl.service",
                "start xochitl.service",
                "is-active --quiet w1.timer",
                "stop w2.timer",
                "is-active --quiet w2.timer",
            ]
        );
        assert_eq!(tools.systemd_run_log().lines().count(), 2);
    }

    #[test]
    fn the_direct_ssh_runs_the_last_argument_with_the_tools() {
        let scratch = Scratch::new("tablet-direct");
        let tools = tools(&scratch, &Toolset::Host);
        scratch.mock_command("ssh", &tools.direct_ssh());
        let output = Command::new(scratch.join("bin/ssh"))
            .args([
                "-o",
                "BatchMode=yes",
                "--",
                "mock.invalid",
                "printf '%s' \"$PATH\"",
            ])
            .output()
            .unwrap();
        assert_eq!(stdout(&output), tools.bin().to_string_lossy());
    }

    /// Run `command` (and `stdin`) through [`REWRITING_SSH`] on `host`.
    fn ssh(
        scratch: &Scratch,
        env: &[(&str, &Path)],
        host: &str,
        command: &str,
        stdin: &str,
    ) -> Output {
        use std::io::Write;
        let mut child = Command::new(scratch.join("bin/ssh"))
            .args([
                "-o",
                "BatchMode=yes",
                "--",
                &format!("root@{host}"),
                command,
            ])
            .envs(env.iter().copied())
            .env_remove("MOCK_LOCK_WHEN")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn the_rewriting_ssh_maps_the_tablets_paths_into_the_device() {
        let scratch = Scratch::new("tablet-rewrite");
        // A device dir whose name needs escaping in a sed replacement.
        let tablet = SimulatedTablet::new(&scratch.join("dev #1 & \\x"), &Toolset::Host);
        scratch.mock_command("ssh", REWRITING_SSH);
        let env = [("MOCK_DEVICE", tablet.device())];
        let read = ssh(
            &scratch,
            &env,
            "10.11.99.1",
            "cat '/proc/sys/kernel/random/boot_id' '/sys/devices/soc0/machine'\nls /home 2>&1 || true",
            "",
        );
        assert_success(&read);
        let read = stdout(&read);
        let lines: Vec<&str> = read.lines().collect();
        assert_eq!(lines[..2], [BOOT_ID, "reMarkable Fake"], "{read}");
        // `ls` is not a tablet tool here (the shell's wording varies).
        assert!(
            lines[2].contains("ls") && lines[2].contains("not found"),
            "{read}"
        );
        // Scripts on stdin are rewritten too, and run with the tools.
        let script = ssh(
            &scratch,
            &env,
            "10.11.99.1",
            "sh -s",
            "printf '%s' \"$PATH\" > '/home/root/.local/share/rmu/path'\n",
        );
        assert_success(&script);
        assert_eq!(
            fs::read_to_string(tablet.state_dir().join("path")).unwrap(),
            tablet.device().join("tools").to_string_lossy()
        );
        assert_eq!(
            tablet.ssh_log().lines().collect::<Vec<_>>(),
            [
                format!(
                    "cat '{d}/boot_id' '{d}/machine'",
                    d = tablet.device().display()
                ),
                "sh -s".to_string(),
            ]
        );
        let commands = tablet.commands();
        assert_eq!(commands.len(), 2);
        assert!(
            commands[0].ends_with("ls /home 2>&1 || true"),
            "{commands:?}"
        );
    }

    #[test]
    fn one_device_per_host_and_a_simulated_lock() {
        let scratch = Scratch::new("tablet-hosts");
        let root = scratch.join("hosts");
        let a = SimulatedTablet::new(&root.join("a"), &Toolset::Host);
        let b = SimulatedTablet::new(&root.join("b"), &Toolset::Host);
        scratch.mock_command("ssh", REWRITING_SSH);
        let env = [("MOCK_ROOT", root.as_path())];
        assert_success(&ssh(
            &scratch,
            &env,
            "b",
            ": > /home/root/.local/share/rmu/here",
            "",
        ));
        assert!(b.state_dir().join("here").exists() && !a.state_dir().join("here").exists());
        assert!(a.ssh_log().is_empty());
        // A hub run "takes the lock" right before the matching command.
        let output = Command::new(scratch.join("bin/ssh"))
            .args(["--", "root@a", "cat /home/root/.local/share/rmu/lock/owner"])
            .env("MOCK_ROOT", &root)
            .env("MOCK_LOCK_WHEN", "lock/owner")
            .env("MOCK_LOCK_RECORD", "run=x\n")
            .output()
            .unwrap();
        assert_eq!(stdout(&output), "run=x\n");
    }
}
