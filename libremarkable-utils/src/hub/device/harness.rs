//! Test harnesses for [`Tablet`] implementations: [`FakeHarness`] around
//! the fake, and (Linux) [`local_ssh::SshHarness`] around the real
//! [`SshTablet`] with a mock `ssh` that runs the generated scripts
//! locally on scratch directories with hostile names, a curated `PATH`
//! (the host's GNU tools, or BusyBox applets and `ash` from
//! `RMU_TEST_BUSYBOX`) and stub `systemctl`/`systemd-run`, so nothing
//! ever reaches the host's systemd. Shared by the contract tests and the
//! end-to-end tests of the sync cycle.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;

/// What a scenario needs besides the [`Tablet`] trait: what only the
/// environment can do.
pub(crate) trait Harness {
    fn tablet(&self) -> &dyn Tablet;
    /// Write a file in the data dir behind rmu's back.
    fn put(&self, path: &str, contents: &[u8]);
    /// Make `path` in the data dir a symlink to `target`.
    fn put_symlink(&self, path: &str, target: &str);
    fn get(&self, path: &str) -> Option<Vec<u8>>;
    /// Make the armed watchdog fire now.
    fn fire_watchdog(&self);
    fn reboot(&self);
    /// Remove rmu's state directory (lock, state file, staging).
    fn wipe_state(&self);
    /// The data dir on the local filesystem.
    fn data_dir(&self) -> PathBuf;
}

/// Whether the tests run as root (permission tests are meaningless then).
pub(crate) fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let probe = Scratch::new("uid");
        let file = probe.0.join("probe");
        fs::write(&file, b"").unwrap();
        fs::metadata(&file).unwrap().uid() == 0
    }
    #[cfg(not(unix))]
    false
}

/// A scratch directory, removed on drop (permissions restored first).
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("rmu-device-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // A test may have left an unreadable directory behind.
            let _ = std::process::Command::new("chmod")
                .args(["-R", "u+rwx"])
                .arg(&self.0)
                .status();
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The fake tablet in a scratch directory.
pub(crate) struct FakeHarness {
    pub(crate) tablet: FakeTablet,
    _scratch: Scratch,
}

impl FakeHarness {
    pub(crate) fn new() -> Self {
        Self::with(|builder| builder)
    }

    pub(crate) fn with(configure: impl FnOnce(FakeTabletBuilder) -> FakeTabletBuilder) -> Self {
        let scratch = Scratch::new("fake");
        let tablet = configure(FakeTablet::builder(&scratch.0)).build().unwrap();
        Self {
            tablet,
            _scratch: scratch,
        }
    }
}

impl Harness for FakeHarness {
    fn tablet(&self) -> &dyn Tablet {
        &self.tablet
    }

    fn put(&self, path: &str, contents: &[u8]) {
        self.tablet.write_file(path, contents).unwrap();
    }

    fn put_symlink(&self, path: &str, target: &str) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, self.tablet.xochitl_dir().join(path)).unwrap();
        #[cfg(not(unix))]
        let _ = (path, target);
    }

    fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.tablet.read_file(path)
    }

    fn fire_watchdog(&self) {
        assert!(self.tablet.fire_watchdog(), "no watchdog armed");
    }

    fn reboot(&self) {
        self.tablet.reboot();
    }

    fn wipe_state(&self) {
        fs::remove_dir_all(self.tablet.state_dir()).unwrap();
    }

    fn data_dir(&self) -> PathBuf {
        self.tablet.xochitl_dir().to_path_buf()
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod local_ssh {
    use super::*;
    use crate::ssh::{SshOptions, SshSession, shell_quote};
    use std::process::{Child, Command};

    /// Tools the scripts may run; everything else is absent from `PATH`.
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

    fn which(tool: &str) -> Option<PathBuf> {
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|dir| dir.join(tool))
            .find(|candidate| candidate.is_file())
    }

    /// Where the tools come from.
    #[derive(Clone, Copy)]
    pub(crate) enum Tools<'a> {
        /// The host's (GNU) tools.
        Host,
        /// BusyBox applets, `sh` included.
        BusyBox(&'a Path),
    }

    /// The BusyBox binary from `RMU_TEST_BUSYBOX`, if set.
    pub(crate) fn busybox() -> Option<PathBuf> {
        std::env::var_os("RMU_TEST_BUSYBOX").map(PathBuf::from)
    }

    fn q(path: &Path) -> String {
        shell_quote(&path.to_string_lossy())
    }

    /// Run `check` with the host's tools and, if available, BusyBox.
    pub(crate) fn on_both(systemd_run: bool, check: impl Fn(&SshHarness)) {
        check(&SshHarness::new(Tools::Host, systemd_run));
        match busybox() {
            Some(busybox) => check(&SshHarness::new(Tools::BusyBox(&busybox), systemd_run)),
            None => eprintln!("RMU_TEST_BUSYBOX is not set; skipping the BusyBox run"),
        }
    }

    /// The real tablet over the mock transport.
    pub(crate) struct SshHarness {
        tablet: SshTablet,
        root: PathBuf,
        bin: PathBuf,
        sys: PathBuf,
        boot_file: PathBuf,
        /// The real `mv` command (for wrapping it).
        real_mv: String,
        _scratch: Scratch,
    }

    impl SshHarness {
        /// `systemd_run`: install the `systemd-run` stub (else the sleep
        /// fallback runs).
        pub(crate) fn new(tools: Tools<'_>, systemd_run: bool) -> Self {
            let scratch = Scratch::new("ssh");
            // Hostile directory names: quotes, spaces, `$()`, a newline.
            let root = scratch.0.join("it's a $(dir) `x`");
            let xochitl = root.join("xochitl data");
            let state = root.join("state;dir&\nnext");
            let bin = scratch.0.join("bin");
            let sys = scratch.0.join("sys");
            [&xochitl, &state, &bin, &sys]
                .iter()
                .for_each(|dir| fs::create_dir_all(dir).unwrap());
            let real_mv = match tools {
                Tools::Host => {
                    TOOLS.iter().for_each(|tool| {
                        if let Some(found) = which(tool) {
                            std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
                        }
                    });
                    q(&which("mv").expect("mv"))
                }
                Tools::BusyBox(busybox) => {
                    let output = Command::new(busybox).arg("--list").output().unwrap();
                    let applets = String::from_utf8(output.stdout).unwrap();
                    TOOLS
                        .iter()
                        .filter(|tool| applets.lines().any(|applet| applet == **tool))
                        .for_each(|tool| {
                            std::os::unix::fs::symlink(busybox, bin.join(tool)).unwrap();
                        });
                    format!("{} mv", q(busybox))
                }
            };
            test_support::write_script(
                &bin.join("systemctl"),
                &format!(
                    "printf '%s\\n' \"$*\" >> {log}\n\
                     case \"$*\" in\n\
                     'stop xochitl.service') rm -f {running} ;;\n\
                     'start xochitl.service') : > {running} ;;\n\
                     'is-active xochitl.service') if [ -e {running} ]; then echo active; else echo inactive; exit 3; fi ;;\n\
                     'is-active --quiet xochitl.service') [ -e {running} ] || exit 3 ;;\n\
                     'is-active --quiet '*'.timer') u=${{3%.timer}}; [ -e {sys}/watchdog-\"$u\" ] || exit 3 ;;\n\
                     'stop '*'.timer') u=${{2%.timer}}; rm -f {sys}/watchdog-\"$u\" ;;\n\
                     esac\n\
                     exit 0\n",
                    log = q(&sys.join("systemctl.log")),
                    running = q(&sys.join("running")),
                    sys = q(&sys),
                ),
            );
            if systemd_run {
                test_support::write_script(
                    &bin.join("systemd-run"),
                    &format!(
                        "printf '%s\\n' \"$*\" >> {log}\n\
                         unit=\n\
                         while [ $# -gt 0 ]; do case \"$1\" in --unit=*) unit=${{1#--unit=}}; shift ;; --*) shift ;; *) break ;; esac; done\n\
                         for a do printf '%s\\0' \"$a\"; done > {sys}/watchdog-\"$unit\"\n",
                        log = q(&sys.join("systemd-run.log")),
                        sys = q(&sys),
                    ),
                );
            }
            fs::write(sys.join("running"), b"").unwrap();
            let boot_file = root.join("boot id");
            fs::write(&boot_file, format!("{}\n", uuid::Uuid::new_v4())).unwrap();
            let conf = root.join("xochitl.conf");
            fs::write(&conf, "[General]\ndevicetoken=@Invalid()\n").unwrap();
            let ssh = scratch.0.join("ssh");
            test_support::write_script(
                &ssh,
                &format!(
                    "for cmd do :; done\nPATH={bin}; export PATH\nexec sh -c \"$cmd\"\n",
                    bin = q(&bin)
                ),
            );
            let mut session = SshSession::new(SshOptions {
                destination: "mock.invalid".into(),
                multiplex: false,
                ..SshOptions::default()
            })
            .unwrap();
            session.set_ssh_exe(ssh);
            let paths = TabletPaths {
                xochitl_dir: xochitl.to_string_lossy().into_owned(),
                state_dir: state.to_string_lossy().into_owned(),
                xochitl_conf: conf.to_string_lossy().into_owned(),
                boot_id: boot_file.to_string_lossy().into_owned(),
            };
            Self {
                tablet: SshTablet::new(session).with_paths(paths),
                root,
                bin,
                sys,
                boot_file,
                real_mv,
                _scratch: scratch,
            }
        }

        fn data(&self, path: &str) -> PathBuf {
            Path::new(&self.tablet.paths().xochitl_dir).join(path)
        }

        pub(crate) fn systemctl_log(&self) -> String {
            fs::read_to_string(self.sys.join("systemctl.log")).unwrap_or_default()
        }

        pub(crate) fn systemd_run_log(&self) -> String {
            fs::read_to_string(self.sys.join("systemd-run.log")).unwrap_or_default()
        }

        /// Watchdogs the `systemd-run` stub has armed and not stopped.
        pub(crate) fn armed_units(&self) -> Vec<PathBuf> {
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

        pub(crate) fn running(&self) -> bool {
            self.sys.join("running").exists()
        }

        pub(crate) fn conf(&self) -> PathBuf {
            self.root.join("xochitl.conf")
        }

        pub(crate) fn ssh_tablet(&self) -> &SshTablet {
            &self.tablet
        }

        pub(crate) fn has_tool(&self, tool: &str) -> bool {
            self.bin.join(tool).exists()
        }

        /// Make the watchdog marker appear right after the next `mv` the
        /// tablet runs (as if the watchdog fired mid-apply).
        pub(crate) fn marker_after_next_move(&self) {
            let mv = self.bin.join("mv");
            let trigger = self.sys.join("marker-after-move");
            let _ = fs::remove_file(&mv);
            test_support::write_script(
                &mv,
                &format!(
                    "{real} \"$@\" || exit $?\n\
                     if [ -e {trigger} ]; then rm -f {trigger}; true > {marker}; fi\n",
                    real = self.real_mv,
                    trigger = q(&trigger),
                    marker = shell_quote(&self.tablet.paths().watchdog_marker()),
                ),
            );
            fs::write(trigger, b"").unwrap();
        }

        /// Hold the guard `flock` from another process (with the
        /// harness's own tools) until `then` (a shell snippet) finishes.
        /// Returns once the lock is held.
        pub(crate) fn hold_guard_flock(&self, then: &str) -> Child {
            let held = self.sys.join("flock-held");
            let child = Command::new(self.bin.join("sh"))
                .args([
                    "-c",
                    &format!("exec 9>>\"$1\"; flock 9; : > \"$2\"; {then}"),
                    "hold",
                ])
                .arg(self.tablet.paths().guard_flock())
                .arg(&held)
                .env("PATH", &self.bin)
                .spawn()
                .unwrap();
            let waited = (0..200).any(|_| {
                std::thread::sleep(std::time::Duration::from_millis(25));
                held.exists()
            });
            assert!(waited, "the flock holder did not start");
            child
        }
    }

    impl Harness for SshHarness {
        fn tablet(&self) -> &dyn Tablet {
            &self.tablet
        }

        fn put(&self, path: &str, contents: &[u8]) {
            let target = self.data(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, contents).unwrap();
        }

        fn put_symlink(&self, path: &str, target: &str) {
            std::os::unix::fs::symlink(target, self.data(path)).unwrap();
        }

        fn get(&self, path: &str) -> Option<Vec<u8>> {
            fs::read(self.data(path)).ok()
        }

        /// Run the command of the one armed `systemd-run` watchdog.
        fn fire_watchdog(&self) {
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

        fn reboot(&self) {
            fs::write(&self.boot_file, format!("{}\n", uuid::Uuid::new_v4())).unwrap();
        }

        fn wipe_state(&self) {
            fs::remove_dir_all(&self.tablet.paths().state_dir).unwrap();
        }

        fn data_dir(&self) -> PathBuf {
            PathBuf::from(&self.tablet.paths().xochitl_dir)
        }
    }
}
