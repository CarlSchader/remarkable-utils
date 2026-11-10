//! Tests of the master and the timeout defaults: pure ones on fabricated
//! `ssh -G` output, and sessions over a mock `ssh`.

use super::*;
use crate::ssh::SshSession;

/// `ssh -G` output (an excerpt) with the given values.
fn effective(connect_timeout: &str, alive_interval: &str) -> String {
    format!(
        "user root\nhostname 10.11.99.1\nport 22\nserveralivecountmax 3\n\
         serveraliveinterval {alive_interval}\ntcpkeepalive yes\n\
         connecttimeout {connect_timeout}\ncontrolmaster false\n"
    )
}

#[test]
fn timeout_defaults_fill_only_what_ssh_left_at_its_builtin() {
    let none: &[String] = &[];
    // Nothing configured: both defaults (older OpenSSH prints -1).
    let both = ["ConnectTimeout=15", "ServerAliveInterval=10"];
    assert_eq!(timeout_defaults(&effective("none", "0"), none), both);
    assert_eq!(timeout_defaults(&effective("-1", "0"), none), both);
    // Set in the user's ssh config: theirs stays.
    assert_eq!(
        timeout_defaults(&effective("5", "0"), none),
        ["ServerAliveInterval=10"]
    );
    assert_eq!(
        timeout_defaults(&effective("none", "60"), none),
        ["ConnectTimeout=15"]
    );
    assert!(timeout_defaults(&effective("30", "120"), none).is_empty());
    // An option ssh -G did not report, or no output at all (it
    // failed): nothing added rather than a guess.
    assert_eq!(
        timeout_defaults("connecttimeout none\n", none),
        ["ConnectTimeout=15"]
    );
    assert!(timeout_defaults("", none).is_empty());
}

/// A known limit, pinned: `ssh -G` prints a value the user's config sets
/// to ssh's built-in default exactly as it prints no value, so rmu's
/// default is added; the user keeps ssh's behaviour with `-o` on the rmu
/// command line (or by turning the defaults off).
#[test]
fn a_config_value_equal_to_the_builtin_looks_unset() {
    // `ServerAliveInterval 0` and `ConnectTimeout none` in ~/.ssh/config:
    let configured = effective("none", "0");
    assert_eq!(
        timeout_defaults(&configured, &[]),
        ["ConnectTimeout=15", "ServerAliveInterval=10"]
    );
    let keep = [
        "ServerAliveInterval=0".to_string(),
        "ConnectTimeout=none".to_string(),
    ];
    assert!(timeout_defaults(&configured, &keep).is_empty());
}

#[test]
fn timeout_defaults_yield_to_the_users_own_options() {
    let effective = effective("none", "0");
    let options = |opts: &[&str]| opts.iter().map(|o| o.to_string()).collect::<Vec<_>>();
    // Any spelling ssh accepts, any case, even ssh's own default value.
    [
        "ConnectTimeout=0",
        "connecttimeout 60",
        "CONNECTTIMEOUT = 5",
    ]
    .iter()
    .for_each(|opt| {
        assert_eq!(
            timeout_defaults(&effective, &options(&[opt])),
            ["ServerAliveInterval=10"],
            "{opt}"
        );
    });
    assert!(
        timeout_defaults(
            &effective,
            &options(&["ServerAliveInterval=0", "ConnectTimeout=3"])
        )
        .is_empty()
    );
    // Similar names are other options.
    assert_eq!(
        timeout_defaults(&effective, &options(&["ServerAliveCountMax=1"])).len(),
        2
    );
}

#[cfg(unix)]
mod mock_ssh {
    use super::*;
    use crate::ssh::{SshOptions, shell_quote};

    /// A scratch directory with a mock `ssh` in it.
    struct Mock(test_support::Scratch);

    impl Mock {
        fn new() -> Self {
            Self(test_support::Scratch::new("ssh-defaults"))
        }

        /// A session whose ssh answers `-G` with `effective`, and
        /// otherwise records its arguments and runs `script`.
        fn session(&self, effective: &str, script: &str, extra: &[&str]) -> SshSession {
            self.session_with(effective, script, extra, false)
        }

        /// [`Self::session`], multiplexed: `-N` opens the master (its
        /// socket, a plain file here) unless `open-fails` exists,
        /// whose text is then ssh's message; `-O` closes it. Scripts
        /// see the socket as `$sock` and the master's log as `$log`.
        fn session_with(
            &self,
            effective: &str,
            script: &str,
            extra: &[&str],
            multiplex: bool,
        ) -> SshSession {
            self.session_opts(
                effective,
                script,
                SshOptions {
                    destination: "tablet.invalid".into(),
                    extra_options: extra.iter().map(|o| o.to_string()).collect(),
                    multiplex,
                    ..SshOptions::default()
                },
            )
        }

        /// [`Self::session_with`] with any options. `-G` calls are
        /// counted in `g-calls`, master opens tried in `attempts`.
        fn session_opts(&self, effective: &str, script: &str, opts: SshOptions) -> SshSession {
            let exe = self.0.join("ssh");
            let effective_file = self.0.join("effective");
            fs::write(&effective_file, effective).unwrap();
            let file = |name: &str| shell_quote(&self.0.join(name).to_string_lossy());
            test_support::write_script(
                &exe,
                &format!(
                    "for arg in \"$@\"; do [ \"$arg\" = -G ] && {{ echo G >> {g_calls}; exec cat {effective}; }}; done\n\
                     sock=\n\
                     for arg in \"$@\"; do case \"$arg\" in ControlPath=*) sock=${{arg#ControlPath=}} ;; esac; done\n\
                     log=${{sock%/*}}/master.log\n\
                     case \" $* \" in\n\
                     *' -N '*)\n\
                     echo attempt >> {attempts}\n\
                     [ ! -e {fails} ] || {{ cat {fails} >&2; exit 255; }}\n\
                     : > \"$sock\"; echo opened >> {opened}; exit 0 ;;\n\
                     *' -O '*) rm -f \"$sock\"; exit 0 ;;\n\
                     esac\n\
                     printf '%s\\n' \"$@\" >> {args}\n\
                     {script}\n",
                    effective = shell_quote(&effective_file.to_string_lossy()),
                    args = shell_quote(&self.args_file().to_string_lossy()),
                    fails = file("open-fails"),
                    opened = file("opened"),
                    attempts = file("attempts"),
                    g_calls = file("g-calls"),
                ),
            );
            let mut session = SshSession::new(opts).unwrap();
            session.set_ssh_exe(exe);
            session
        }

        fn opened(&self) -> usize {
            self.count("opened")
        }

        /// Lines in the mock's file `name`.
        fn count(&self, name: &str) -> usize {
            fs::read_to_string(self.0.join(name))
                .unwrap_or_default()
                .lines()
                .count()
        }

        fn args_file(&self) -> PathBuf {
            self.0.join("args")
        }

        fn args(&self) -> Vec<String> {
            fs::read_to_string(self.args_file())
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    #[test]
    fn defaults_reach_the_command_line_after_the_users_options() {
        let scratch = Mock::new();
        let session = scratch.session(&effective("none", "0"), "echo ok", &["ConnectTimeout=4"]);
        assert_eq!(session.run_checked("true").unwrap(), "ok\n");
        assert_eq!(session.run_checked("true").unwrap(), "ok\n");
        let args = scratch.args();
        let position = |value: &str| args.iter().position(|arg| arg == value);
        assert!(
            position("ConnectTimeout=4").unwrap() < position("ServerAliveInterval=10").unwrap()
        );
        assert_eq!(position("ConnectTimeout=15"), None, "{args:?}");
        // Both commands carry the defaults decided once.
        assert_eq!(
            args.iter()
                .filter(|arg| *arg == "ServerAliveInterval=10")
                .count(),
            2
        );
    }

    #[test]
    fn a_dead_master_explains_its_silent_clients() {
        let scratch = Mock::new();
        let dead = shell_quote(&scratch.0.join("dead").to_string_lossy());
        // The master dies with the tablet silent: its log says why,
        // its client exits 255 without a word.
        let session = scratch.session_with(
            &effective("none", "0"),
            &format!(
                "if [ -e {dead} ]; then\n\
                 echo 'Timeout, server tablet.invalid not responding.' >> \"$log\"\n\
                 rm -f \"$sock\"; exit 255\n\
                 fi\n\
                 echo ok"
            ),
            &[],
            true,
        );
        assert_eq!(session.run_checked("true").unwrap(), "ok\n");
        assert_eq!(session.run_checked("true").unwrap(), "ok\n");
        assert_eq!(scratch.opened(), 1, "one master for both");
        fs::write(scratch.0.join("dead"), "").unwrap();
        match session.run_checked("true") {
            Err(Error::Unreachable(detail)) => assert!(
                detail.contains("Timeout, server tablet.invalid not responding."),
                "{detail}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(matches!(session.run("true"), Err(Error::Unreachable(_))));
        assert!(matches!(
            session
                .run_script("true")
                .map(|output| is_unreachable(&output)),
            Ok(true)
        ));
        // Awake again: a new master, whose log does not carry the old
        // one's words.
        fs::remove_file(scratch.0.join("dead")).unwrap();
        assert_eq!(session.run_checked("true").unwrap(), "ok\n");
        assert_eq!(scratch.opened(), 4);
        let log = fs::read_to_string(session.master_log().unwrap()).unwrap();
        assert!(!log.contains("not responding"), "{log}");
        // Without the master's word, a silent 255 is the remote's.
        let silent = Mock::new();
        let session = silent.session_with(&effective("none", "0"), "exit 255", &[], true);
        assert!(matches!(
            session.run_checked("true"),
            Err(Error::Remote { status: 255, .. })
        ));
    }

    #[test]
    fn the_users_multiplexing_options_leave_connections_to_ssh() {
        ["ControlPath=none", "controlmaster no", "ControlPersist = 5"]
            .iter()
            .for_each(|option| {
                let scratch = Mock::new();
                let session =
                    scratch.session_with(&effective("none", "0"), "echo ok", &[option], true);
                assert_eq!(session.run_checked("true").unwrap(), "ok\n");
                assert_eq!(scratch.opened(), 0, "{option}");
            });
    }

    #[test]
    fn a_master_that_cannot_connect_is_the_only_try() {
        let scratch = Mock::new();
        fs::write(
            scratch.0.join("open-fails"),
            "ssh: connect to host tablet.invalid port 22: Connection timed out\n",
        )
        .unwrap();
        let session = scratch.session_with(&effective("none", "0"), "echo ok", &[], true);
        assert!(matches!(
            session.run_checked("true"),
            Err(Error::Unreachable(detail)) if detail.contains("Connection timed out")
        ));
        assert!(scratch.args().is_empty(), "the command never ran");
        // Other failures are left to the command, which reports them.
        fs::write(
            scratch.0.join("open-fails"),
            "root@tablet.invalid: Permission denied (publickey).\n",
        )
        .unwrap();
        assert_eq!(session.run_checked("true").unwrap(), "ok\n");
        // Remembered: later commands do not try again (one
        // authentication each).
        assert_eq!(session.run_checked("true").unwrap(), "ok\n");
        assert_eq!(
            scratch.count("attempts"),
            2,
            "the refused open is not tried again"
        );
    }

    #[test]
    fn the_defaults_can_be_turned_off() {
        let scratch = Mock::new();
        let session = scratch.session_opts(
            &effective("none", "0"),
            "echo ok",
            SshOptions {
                destination: "tablet.invalid".into(),
                multiplex: false,
                default_timeouts: false,
                ..SshOptions::default()
            },
        );
        session.run_checked("true").unwrap();
        session.run_checked("true").unwrap();
        let args = scratch.args();
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("ConnectTimeout") || arg.starts_with("ServerAlive")),
            "{args:?}"
        );
        assert_eq!(scratch.count("g-calls"), 0, "ssh -G is not even asked");
        // On: asked once per session.
        let scratch = Mock::new();
        let session = scratch.session(&effective("none", "0"), "echo ok", &[]);
        session.run_checked("true").unwrap();
        session.run_checked("true").unwrap();
        assert_eq!(scratch.count("g-calls"), 1);
    }

    #[test]
    fn a_configured_value_is_left_to_ssh() {
        let scratch = Mock::new();
        let session = scratch.session(&effective("7", "30"), "echo ok", &[]);
        session.run_checked("true").unwrap();
        let args = scratch.args();
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("ConnectTimeout") || arg.starts_with("ServerAlive")),
            "{args:?}"
        );
    }

    #[test]
    fn a_tablet_that_does_not_answer_is_the_typed_error() {
        let scratch = Mock::new();
        let timeout = scratch.session(
            &effective("none", "0"),
            "echo 'ssh: connect to host tablet.invalid port 22: Connection timed out' >&2\nexit 255",
            &[],
        );
        let unreachable = |result: Result<String>| match result {
            Err(error @ Error::Unreachable(_)) => error.to_string(),
            other => panic!("{other:?}"),
        };
        let message = unreachable(timeout.run_checked("true"));
        assert!(message.contains("Connection timed out"), "{message}");
        assert!(message.contains("asleep"), "{message}");
        // Raw runs too, and the keepalive giving up mid-command.
        assert!(matches!(timeout.run("true"), Err(Error::Unreachable(_))));
        let silent = scratch.session(
            &effective("none", "0"),
            "echo 'Timeout, server tablet.invalid not responding.' >&2\nexit 255",
            &[],
        );
        unreachable(silent.run_checked("true"));
        // Other ssh failures, and the same words from a remote
        // command, stay remote errors.
        let refused = scratch.session(
            &effective("none", "0"),
            "echo 'ssh: connect to host tablet.invalid port 22: Connection refused' >&2\nexit 255",
            &[],
        );
        assert!(matches!(
            refused.run_checked("true"),
            Err(Error::Remote { status: 255, .. })
        ));
        assert!(refused.run("true").is_ok());
        let remote = scratch.session(
            &effective("none", "0"),
            "echo 'curl: Connection timed out' >&2\nexit 28",
            &[],
        );
        assert!(matches!(
            remote.run_checked("true"),
            Err(Error::Remote { status: 28, .. })
        ));
    }
}

#[test]
fn explain_appends_the_masters_reason_to_a_silent_failure() {
    use std::os::unix::process::ExitStatusExt;
    let output = |code: i32, stderr: &str| Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    };
    let log = "Warning: Permanently added 'tablet' to the list of known hosts.\n\
               Timeout, server tablet not responding.\n";
    let mut silent = output(255, "");
    explain(&mut silent, log);
    assert_eq!(silent.stderr, b"Timeout, server tablet not responding.");
    assert!(is_unreachable(&silent));
    let mut said = output(255, "partial");
    explain(&mut said, log);
    assert_eq!(
        said.stderr,
        b"partial\nTimeout, server tablet not responding."
    );
    // A log without such a line changes nothing.
    let mut other = output(255, "");
    explain(&mut other, "Warning: Permanently added 'tablet'.\n");
    assert!(other.stderr.is_empty());
}
