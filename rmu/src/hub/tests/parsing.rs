//! Argument parsing, options and exit statuses.

use super::*;

#[test]
fn parses_every_subcommand() {
    let init = hub_args(&["init", "/srv/library.git"]);
    assert!(!init.json);
    assert!(matches!(&init.command, HubCommand::Init { origin } if origin == "/srv/library.git"));

    let clone = hub_args(&[
        "clone",
        "ssh://git@example.invalid/library.git",
        "--name",
        "Desk tablet",
        "--reattach",
        "--allow-deletions",
        "--break-lock",
        "--watchdog",
        "120",
        "--json",
    ]);
    assert!(clone.json);
    match &clone.command {
        HubCommand::Clone {
            origin,
            name,
            reattach,
            allow_deletions,
            run,
        } => {
            assert_eq!(origin, "ssh://git@example.invalid/library.git");
            assert_eq!(name.as_ref().unwrap().as_str(), "Desk tablet");
            assert!(*reattach && *allow_deletions);
            assert_eq!(
                run,
                &RunArgs {
                    break_lock: true,
                    watchdog: 120
                }
            );
        }
        other => panic!("{other:?}"),
    }

    // `--json` is accepted before the subcommand too.
    let sync = hub_args(&[
        "--json",
        "sync",
        "--allow-deletions",
        "--origin",
        "laptop:lib.git",
    ]);
    assert!(sync.json);
    match &sync.command {
        HubCommand::Sync {
            dry_run,
            allow_deletions,
            run,
            origin,
        } => {
            assert!(!dry_run);
            assert!(allow_deletions);
            assert_eq!(run, &RunArgs::default());
            assert_eq!(origin.origin.as_deref(), Some("laptop:lib.git"));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        hub_args(&["sync", "--dry-run"]).command,
        HubCommand::Sync { dry_run: true, .. }
    ));
    assert!(matches!(
        hub_args(&["backup", "--break-lock"]).command,
        HubCommand::Backup {
            run: RunArgs {
                break_lock: true,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        hub_args(&["remote"]).command,
        HubCommand::Remote {
            action: None,
            origin: OriginArg { origin: None }
        }
    ));
    match hub_args(&["remote", "set-url", "laptop:lib.git", "--break-lock"]).command {
        HubCommand::Remote {
            action: Some(RemoteAction::SetUrl { url, lock }),
            ..
        } => {
            assert_eq!(url, "laptop:lib.git");
            assert!(lock.break_lock);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        hub_args(&["detach", "--prune-ref", "--origin", "/x.git"]).command,
        HubCommand::Detach {
            prune_ref: true,
            origin: OriginArg { origin: Some(_) },
            ..
        }
    ));
    assert!(matches!(
        hub_args(&["status", "--origin", "/x.git"]).command,
        HubCommand::Status {
            origin: OriginArg { origin: Some(_) }
        }
    ));
}

#[test]
fn rejects_bad_arguments() {
    use clap::error::ErrorKind;
    assert_eq!(
        parse_error(&["sync", "--dry-run", "--break-lock"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse_error(&["clone", "/x.git", "--name", "a<b"]),
        ErrorKind::ValueValidation
    );
    assert_eq!(
        parse_error(&["clone", "/x.git", "--name", ""]),
        ErrorKind::ValueValidation
    );
    assert_eq!(
        parse_error(&["sync", "--watchdog", "5"]),
        ErrorKind::ValueValidation
    );
    assert_eq!(
        parse_error(&["status", "--break-lock"]),
        ErrorKind::UnknownArgument
    );
    assert_eq!(
        parse_error(&["sync", "--dry-run", "--watchdog", "60"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse_error(&["detach", "--watchdog", "60"]),
        ErrorKind::UnknownArgument
    );
    assert_eq!(
        parse_error(&["remote", "set-url", "/x.git", "--watchdog", "60"]),
        ErrorKind::UnknownArgument
    );
    assert_eq!(parse_error(&["init"]), ErrorKind::MissingRequiredArgument);
    assert_eq!(
        parse_error(&["remote", "set-url"]),
        ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
        parse_error(&["remote", "--origin", "/a.git", "set-url", "/b.git"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        parse_error(&[]),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}

#[test]
fn local_origins_become_absolute() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        absolute_origin("library.git"),
        cwd.join("library.git").display().to_string()
    );
    assert_eq!(absolute_origin("/srv/library.git"), "/srv/library.git");
    [
        "ssh://git@example.invalid/library.git",
        "server.example:library.git",
        "https://example.invalid/library.git",
    ]
    .iter()
    .for_each(|url| assert_eq!(absolute_origin(url), *url));
    let clone = options(&hub_args(&["clone", "/o.git", "--allow-deletions"]).command);
    assert!(clone.allow_deletions);
    assert_eq!(
        options(&hub_args(&["status", "--origin", "rel.git"]).command).origin,
        Some(cwd.join("rel.git").display().to_string())
    );
}

#[test]
fn options_follow_the_flags() {
    let sync = options(
        &hub_args(&[
            "sync",
            "--allow-deletions",
            "--break-lock",
            "--watchdog",
            "90",
            "--origin",
            "/o.git",
        ])
        .command,
    );
    assert!(sync.allow_deletions && sync.break_lock);
    assert_eq!(sync.watchdog, Duration::from_secs(90));
    assert_eq!(sync.origin.as_deref(), Some("/o.git"));

    let status = options(&hub_args(&["status"]).command);
    assert_eq!(status, HubOptions::default());
    assert_eq!(
        options(&hub_args(&["clone", "/o.git"]).command).watchdog,
        HubOptions::default().watchdog
    );
}

// ---- exit codes -----------------------------------------------------------

#[test]
fn exit_codes() {
    let code = |error: Error| exit_code(&anyhow::Error::from(error));
    let consent = || {
        Error::HubCycle(CycleError::DeletionsNeedConsent {
            from_origin: vec![doc(1)],
            from_tablet: Vec::new(),
            empty_snapshot: false,
        })
    };
    assert_eq!(code(consent()), 3);
    assert_eq!(
        code(Error::HubDevice(DeviceError::Locked("pid 1 on h".into()))),
        4
    );
    assert_eq!(
        code(Error::HubCycle(CycleError::OriginRewound {
            base: oid('a')
        })),
        5
    );
    assert_eq!(
        code(Error::HubCycle(CycleError::RepoMismatch {
            tablet: RepoId::new("0f0e0d0c-0000-4000-8000-0000000000a1").unwrap(),
            origin: RepoId::new("0f0e0d0c-0000-4000-8000-0000000000a2").unwrap(),
        })),
        5
    );
    assert_eq!(
        code(Error::HubCycle(CycleError::OriginEmpty {
            url: "/o".into()
        })),
        5
    );
    assert_eq!(
        code(Error::HubCycle(CycleError::DeviceRefConflict {
            reference: "refs/rmu/devices/x".into(),
            recorded: "a".into(),
            remote: "b".into(),
        })),
        5
    );
    assert_eq!(code(Error::HubCycle(CycleError::CloudPaired)), 6);
    assert_eq!(code(Error::HubCycle(CycleError::JournalPending)), 7);
    assert!(
        CycleError::JournalPending
            .to_string()
            .contains("rmu hub sync")
    );
    assert_eq!(code(Error::HubCycle(CycleError::NotAttached)), 1);
    assert_eq!(code(Error::HubDevice(DeviceError::Busy)), 4);
    assert_eq!(code(Error::HubDevice(DeviceError::LockContended)), 4);
    assert_eq!(code(Error::HubDevice(DeviceError::LockLost)), 1);
    assert_eq!(code(Error::HubCycle(CycleError::CacheBusy("/c".into()))), 4);
    assert_eq!(
        code(Error::HubCycle(CycleError::PushRetriesExhausted(5))),
        1
    );
    assert_eq!(code(Error::PathNotFound("x".into())), 1);
    assert_eq!(exit_code(&anyhow::anyhow!("not a library error")), 1);
    // Context does not hide the cause.
    assert_eq!(
        exit_code(&anyhow::Error::from(consent()).context("while syncing")),
        3
    );
}

// ---- rendering --------------------------------------------------------------

#[test]
fn no_restart_is_refused_for_hub() {
    use clap::error::ErrorKind;
    assert_eq!(
        parse_error_with(&["--no-restart"], &["sync"]),
        ErrorKind::ArgumentConflict
    );
    let error = crate::parse_cli(["rmu", "hub", "--no-restart", "status"])
        .err()
        .expect("refused after the subcommand too");
    assert_eq!(error.exit_code(), 2);
    assert!(error.to_string().contains(NO_RESTART_REFUSED), "{error}");
    // Other commands still take it.
    assert!(crate::parse_cli(["rmu", "--no-restart", "mkdir", "Books"]).is_ok());
}
