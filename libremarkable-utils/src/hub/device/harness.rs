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
    use test_support::tablet::TabletTools;
    pub(crate) use test_support::tablet::Toolset;

    /// Run `check` with the host's tools and, if available, BusyBox.
    pub(crate) fn on_both(systemd_run: bool, check: impl Fn(&SshHarness)) {
        check(&SshHarness::new(&Toolset::Host, systemd_run));
        match Toolset::busybox() {
            Some(busybox) => check(&SshHarness::new(&busybox, systemd_run)),
            None => eprintln!("RMU_TEST_BUSYBOX is not set; skipping the BusyBox run"),
        }
    }

    /// The real tablet over the mock transport.
    pub(crate) struct SshHarness {
        tablet: SshTablet,
        root: PathBuf,
        tools: TabletTools,
        boot_file: PathBuf,
        _scratch: Scratch,
    }

    impl SshHarness {
        /// `systemd_run`: keep the `systemd-run` stub (else the sleep
        /// fallback runs).
        pub(crate) fn new(toolset: &Toolset, systemd_run: bool) -> Self {
            let scratch = Scratch::new("ssh");
            // Hostile directory names: quotes, spaces, `$()`, a newline.
            let root = scratch.0.join("it's a $(dir) `x`");
            let xochitl = root.join("xochitl data");
            let state = root.join("state;dir&\nnext");
            [&xochitl, &state]
                .iter()
                .for_each(|dir| fs::create_dir_all(dir).unwrap());
            let tools =
                TabletTools::install(&scratch.0.join("bin"), &scratch.0.join("sys"), toolset);
            if !systemd_run {
                fs::remove_file(tools.bin().join("systemd-run")).unwrap();
            }
            let boot_file = root.join("boot id");
            fs::write(&boot_file, format!("{}\n", uuid::Uuid::new_v4())).unwrap();
            let conf = root.join("xochitl.conf");
            fs::write(&conf, "[General]\ndevicetoken=@Invalid()\n").unwrap();
            let ssh = scratch.0.join("ssh");
            test_support::write_script(&ssh, &tools.direct_ssh());
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
                tools,
                boot_file,
                _scratch: scratch,
            }
        }

        fn data(&self, path: &str) -> PathBuf {
            Path::new(&self.tablet.paths().xochitl_dir).join(path)
        }

        pub(crate) fn systemctl_log(&self) -> String {
            self.tools.systemctl_log()
        }

        pub(crate) fn systemd_run_log(&self) -> String {
            self.tools.systemd_run_log()
        }

        /// Watchdogs the `systemd-run` stub has armed and not stopped.
        pub(crate) fn armed_units(&self) -> Vec<PathBuf> {
            self.tools.armed_units()
        }

        pub(crate) fn running(&self) -> bool {
            self.tools.running()
        }

        pub(crate) fn conf(&self) -> PathBuf {
            self.root.join("xochitl.conf")
        }

        pub(crate) fn ssh_tablet(&self) -> &SshTablet {
            &self.tablet
        }

        pub(crate) fn has_tool(&self, tool: &str) -> bool {
            self.tools.has_tool(tool)
        }

        /// Make the watchdog marker appear right after the next `mv` the
        /// tablet runs (as if the watchdog fired mid-apply).
        pub(crate) fn marker_after_next_move(&self) {
            let mv = self.tools.bin().join("mv");
            let trigger = self.tools.sys().join("marker-after-move");
            let _ = fs::remove_file(&mv);
            test_support::write_script(
                &mv,
                &format!(
                    "{real} \"$@\" || exit $?\n\
                     if [ -e {trigger} ]; then rm -f {trigger}; true > {marker}; fi\n",
                    real = self.tools.real_command("mv"),
                    trigger = shell_quote(&trigger.to_string_lossy()),
                    marker = shell_quote(&self.tablet.paths().watchdog_marker()),
                ),
            );
            fs::write(trigger, b"").unwrap();
        }

        /// See [`TabletTools::systemctl_arms`].
        pub(crate) fn systemctl_arms(&self, arms: &str) {
            self.tools.systemctl_arms(arms);
        }

        /// Hold the guard `flock` from another process (with the
        /// harness's own tools) until `then` (a shell snippet) finishes.
        /// Returns once the lock is held.
        pub(crate) fn hold_guard_flock(&self, then: &str) -> Child {
            let held = self.tools.sys().join("flock-held");
            let child = Command::new(self.tools.bin().join("sh"))
                .args([
                    "-c",
                    &format!("exec 9>>\"$1\"; flock 9; : > \"$2\"; {then}"),
                    "hold",
                ])
                .arg(self.tablet.paths().guard_flock())
                .arg(&held)
                .env("PATH", self.tools.bin())
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
            self.tools.fire_watchdog();
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
