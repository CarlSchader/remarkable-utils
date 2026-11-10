//! `rmu hub`: git-backed multi-tablet sync (see `docs/hub-design.md`).
//!
//! The commands are thin wrappers around the library's [`Hub`]:
//! [`execute`] runs one against any [`Tablet`] (the real [`SshTablet`],
//! or the library's fake in tests) and returns its report; [`finish`]
//! writes the report and maps errors to exit statuses ([`Failure`]).
//!
//! Output discipline: stdout carries the result only, as a human summary
//! when it is a terminal, as JSON with `--json`, and otherwise as plain
//! ids (the summary then goes to stderr, unless `--quiet`). Progress,
//! warnings and error details go to stderr. Nothing prompts, except git
//! asking for credentials when stdin and stderr are both terminals.

mod json;
mod render;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use libremarkable_utils::Error;
use libremarkable_utils::git::Git;
use libremarkable_utils::hub::cycle::{
    self, BackupReport, CycleError, DetachReport, DryRunReport, Hub, HubOptions, InitReport,
    RemoteReport, SetUrlReport, StatusReport, SyncReport,
};
use libremarkable_utils::hub::device::{DeviceError, LockRequest, SshTablet, Tablet, TabletPaths};
use libremarkable_utils::hub::model::{DeviceName, DocId, XochitlPath};
use libremarkable_utils::progress::Progress;
use libremarkable_utils::ssh::SshSession;
use serde::Serialize;
use serde_json::Value;

/// Appended to the help of `rmu hub` and its subcommands.
pub const AFTER_HELP: &str = "\
Exit status:
  0  success
  1  error; worth retrying as is when origin kept moving, or the tablet's
     state or origin's configuration changed during the run
  2  invalid arguments
  3  the deletion guard refused: the sync would delete many documents;
     origin's main and the tablet were left unchanged (the tablet's backup
     was pushed); rerun with --allow-deletions if that is intended
  4  the tablet is locked or busy with another rmu hub run, or the local
     cache with another run on this computer (retry later; if a tablet
     run is gone, --break-lock takes its lock over)
  5  origin does not match the tablet: rewound or replaced, or another
     tablet uses this device id (`rmu hub clone --reattach` attaches the
     tablet to it deliberately); or origin lost its main branch (check the
     URL, then restore origin from a backup or a computer's cache: the
     error says how)
  6  the tablet is paired with the reMarkable cloud (unpair it first)
  7  an interrupted sync must be finished first: run `rmu hub sync`

Output: stdout carries the result only: a summary on a terminal, JSON
with --json, otherwise plain ids (the summary then goes to stderr).
Progress, warnings and error details go to stderr.

JSON (\"schema\": 1) is unstable until the hub is validated on devices.
Errors are {\"schema\": 1, \"error\": {\"kind\", \"exit_code\", \"message\"}};
kind is error, deletions_need_consent (adds from_origin, from_tablet and
empty_snapshot), lock_held, origin_mismatch, cloud_paired or
journal_pending.";

/// Why `--no-restart` is refused for `rmu hub`.
pub const NO_RESTART_REFUSED: &str = "--no-restart does not apply to rmu hub: it stops and \
     restarts xochitl itself, with a watchdog on the tablet";

/// `rmu hub …`.
#[derive(Args, Debug)]
pub struct HubArgs {
    /// Print the result (or the error) as JSON on stdout, for scripts
    /// (unstable until device validation; see below)
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: HubCommand,
}

#[derive(Subcommand, Debug)]
pub enum HubCommand {
    /// Create an origin: a new bare repository at a local path, or the
    /// first commit of an existing empty repository at a URL
    #[command(after_help = AFTER_HELP)]
    Init {
        /// Local path or git URL of the origin (keep it private)
        origin: String,
    },
    /// Attach the tablet to an origin and run its first sync
    #[command(after_help = AFTER_HELP)]
    Clone {
        /// Local path or git URL of the origin
        origin: String,
        /// This tablet's name in the library's history (default: its
        /// model and a short id, or its current name with --reattach)
        #[arg(long, value_parser = parse_device_name)]
        name: Option<DeviceName>,
        /// Attach a tablet that already has hub state again (its state
        /// was lost, or origin was replaced), keeping its device id
        #[arg(long)]
        reattach: bool,
        /// Allow a first sync that deletes many documents (see `sync`)
        #[arg(long)]
        allow_deletions: bool,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Sync the tablet with origin: back it up, merge, push, apply
    #[command(after_help = AFTER_HELP)]
    Sync {
        /// Show what a sync would do, without stopping xochitl, taking
        /// the lock or pushing anything (best effort)
        #[arg(long, conflicts_with_all = ["break_lock", "watchdog"])]
        dry_run: bool,
        /// Allow a sync that deletes many documents (from origin or from
        /// the tablet); the consent is recorded in the merge commit
        #[arg(long)]
        allow_deletions: bool,
        #[command(flatten)]
        run: RunArgs,
        #[command(flatten)]
        origin: OriginArg,
    },
    /// Back the tablet up to its device ref on origin; nothing is merged
    /// and the tablet is not written
    #[command(after_help = AFTER_HELP)]
    Backup {
        #[command(flatten)]
        run: RunArgs,
        #[command(flatten)]
        origin: OriginArg,
    },
    /// Show the tablet's changes since its last sync, how far origin is
    /// ahead, open conflict copies and quarantined documents (read-only,
    /// best effort)
    #[command(after_help = AFTER_HELP)]
    Status {
        #[command(flatten)]
        origin: OriginArg,
    },
    /// Show the origin URL stored on the tablet, or change it
    #[command(after_help = AFTER_HELP, args_conflicts_with_subcommands = true)]
    Remote {
        #[command(subcommand)]
        action: Option<RemoteAction>,
        #[command(flatten)]
        origin: OriginArg,
    },
    /// Forget the hub state on the tablet; its documents are untouched
    #[command(after_help = AFTER_HELP)]
    Detach {
        /// Also delete the tablet's device ref (its backups) from origin
        #[arg(long)]
        prune_ref: bool,
        #[command(flatten)]
        lock: LockArg,
        #[command(flatten)]
        origin: OriginArg,
    },
}

#[derive(Subcommand, Debug)]
pub enum RemoteAction {
    /// Store another origin URL on the tablet (it must hold the tablet's
    /// repository, with every commit the tablet has synced)
    #[command(after_help = AFTER_HELP)]
    SetUrl {
        /// The new local path or git URL
        url: String,
        #[command(flatten)]
        lock: LockArg,
    },
}

/// `--break-lock` alone, for the commands that lock the tablet without
/// stopping xochitl.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct LockArg {
    /// Take over the tablet's hub lock even if it does not look stale;
    /// only when you are sure no other rmu hub run is still working on it
    #[arg(long)]
    pub break_lock: bool,
}

/// Flags of the commands that lock the tablet and stop xochitl.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    /// Take over the tablet's hub lock even if it does not look stale;
    /// only when you are sure no other rmu hub run is still working on it
    #[arg(long)]
    pub break_lock: bool,

    /// Watchdog timeout: if rmu disappears while xochitl is stopped, the
    /// tablet starts it again after this many seconds
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = default_watchdog_secs(),
        value_parser = clap::value_parser!(u64).range(10..)
    )]
    pub watchdog: u64,
}

impl Default for RunArgs {
    fn default() -> Self {
        Self {
            break_lock: false,
            watchdog: default_watchdog_secs(),
        }
    }
}

#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct OriginArg {
    /// Origin URL for this run, instead of the one stored on the tablet
    /// (ssh aliases differ between computers)
    #[arg(long, value_name = "URL")]
    pub origin: Option<String>,
}

fn default_watchdog_secs() -> u64 {
    HubOptions::default().watchdog.as_secs()
}

fn parse_device_name(name: &str) -> std::result::Result<DeviceName, String> {
    DeviceName::new(name).map_err(|_| {
        "a device name has 1 to 64 characters, without control characters, '<' or '>'".to_string()
    })
}

/// What a command produced.
#[derive(Debug)]
pub enum Report {
    Init { origin: String, report: InitReport },
    Clone(SyncReport),
    Sync(SyncReport),
    DryRun(DryRunReport),
    Backup(BackupReport),
    Status(StatusReport),
    Remote(RemoteReport),
    SetUrl(SetUrlReport),
    Detach(DetachReport),
}

/// Why a command failed, as an exit status (see [`AFTER_HELP`]); it
/// serialises as the `kind` of a `--json` error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Error,
    DeletionsNeedConsent,
    LockHeld,
    OriginMismatch,
    CloudPaired,
    JournalPending,
}

impl Failure {
    pub fn of(error: &Error) -> Self {
        match error {
            Error::HubCycle(CycleError::DeletionsNeedConsent { .. }) => Self::DeletionsNeedConsent,
            Error::HubDevice(
                DeviceError::Locked(_) | DeviceError::LockContended | DeviceError::Busy,
            )
            | Error::HubCycle(CycleError::CacheBusy(_)) => Self::LockHeld,
            Error::HubCycle(
                CycleError::OriginRewound { .. }
                | CycleError::RepoMismatch { .. }
                | CycleError::OriginEmpty { .. }
                | CycleError::DeviceRefConflict { .. },
            ) => Self::OriginMismatch,
            Error::HubCycle(CycleError::CloudPaired) => Self::CloudPaired,
            Error::HubCycle(CycleError::JournalPending) => Self::JournalPending,
            _ => Self::Error,
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Self::Error => 1,
            Self::DeletionsNeedConsent => 3,
            Self::LockHeld => 4,
            Self::OriginMismatch => 5,
            Self::CloudPaired => 6,
            Self::JournalPending => 7,
        }
    }
}

/// The exit status for an error of `rmu hub`.
pub fn exit_code(error: &anyhow::Error) -> u8 {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Error>())
        .map_or(Failure::Error, Failure::of)
        .code()
}

/// Where the result goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The summary on stdout (a terminal).
    Human,
    /// Plain ids on stdout, the summary on stderr.
    Plain,
    Json,
}

impl Output {
    fn detect(json: bool) -> Self {
        match (json, std::io::stdout().is_terminal()) {
            (true, _) => Self::Json,
            (false, true) => Self::Human,
            (false, false) => Self::Plain,
        }
    }
}

/// What `rmu hub` takes from the global flags.
pub struct Env<'a> {
    pub quiet: bool,
    pub xochitl_dir: &'a str,
    pub progress: Arc<dyn Progress>,
}

/// The library options for a command.
pub fn options(command: &HubCommand) -> HubOptions {
    let run = |run: &RunArgs, options: HubOptions| HubOptions {
        watchdog: Duration::from_secs(run.watchdog),
        break_lock: run.break_lock,
        ..options
    };
    let origin = |origin: &OriginArg| HubOptions {
        origin: origin.origin.as_deref().map(absolute_origin),
        ..HubOptions::default()
    };
    match command {
        HubCommand::Init { .. } => HubOptions::default(),
        HubCommand::Clone {
            allow_deletions,
            run: args,
            ..
        } => HubOptions {
            allow_deletions: *allow_deletions,
            ..run(args, HubOptions::default())
        },
        HubCommand::Sync {
            allow_deletions,
            run: args,
            origin: url,
            ..
        } => HubOptions {
            allow_deletions: *allow_deletions,
            ..run(args, origin(url))
        },
        HubCommand::Backup {
            run: args,
            origin: url,
        } => run(args, origin(url)),
        HubCommand::Status { origin: url }
        | HubCommand::Remote {
            action: None,
            origin: url,
        } => origin(url),
        HubCommand::Remote {
            action: Some(RemoteAction::SetUrl { lock, .. }),
            ..
        } => HubOptions {
            break_lock: lock.break_lock,
            ..HubOptions::default()
        },
        HubCommand::Detach {
            lock, origin: url, ..
        } => HubOptions {
            break_lock: lock.break_lock,
            ..origin(url)
        },
    }
}

/// A local origin path made absolute (it is stored on the tablet, and
/// later runs start elsewhere); URLs and scp-style `host:path` are kept
/// ([`cycle::is_local_path`]).
pub fn absolute_origin(origin: &str) -> String {
    if !cycle::is_local_path(origin) {
        return origin.to_string();
    }
    std::path::absolute(origin)
        .map_or_else(|_| origin.to_string(), |path| path.display().to_string())
}

/// This computer's name for the lock record on the tablet (never
/// committed anywhere).
fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| {
            std::process::Command::new("uname")
                .arg("-n")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
        })
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `rmu hub …`: `connect` opens the session to the tablet (`init` never
/// calls it).
pub fn run(
    args: &HubArgs,
    env: &Env<'_>,
    connect: impl FnOnce() -> Result<SshSession>,
) -> Result<()> {
    let output = Output::detect(args.json);
    // Setup failures (no cache directory, no ssh session) are errors too.
    let setup_error = |error: anyhow::Error| {
        if output == Output::Json {
            let value = json::error(Failure::Error, &format!("{error:#}"), None);
            let _ = writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::to_string_pretty(&value).expect("errors serialise")
            );
        }
        error
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let cache = cycle::default_cache_dir()
        .context("no cache directory for rmu hub: set XDG_CACHE_HOME or HOME")
        .map_err(setup_error)?;
    let hub = Hub::new(
        Git::new().interactive(interactive),
        cache.clone(),
        LockRequest::for_this_process(host_name()),
        &*env.progress,
    )
    .with_options(options(&args.command));
    let tablet = match &args.command {
        HubCommand::Init { .. } => None,
        _ => Some(
            SshTablet::new(connect().map_err(setup_error)?)
                .with_paths(TabletPaths {
                    xochitl_dir: env.xochitl_dir.to_string(),
                    ..TabletPaths::default()
                })
                .with_progress(env.progress.clone()),
        ),
    };
    let tablet = tablet.as_ref().map(|tablet| tablet as &dyn Tablet);
    let result = execute(&hub, tablet, &args.command);
    // Clear the progress UI before anything is printed.
    env.progress.finished();
    finish(
        result,
        &OutputContext {
            tablet,
            cache_dir: Some(&cache),
            output,
            quiet: env.quiet,
        },
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// Run a command. Every command but `init` needs `tablet`.
pub fn execute(
    hub: &Hub<'_>,
    tablet: Option<&dyn Tablet>,
    command: &HubCommand,
) -> libremarkable_utils::Result<Report> {
    let tablet = || tablet.expect("every command but init has a tablet");
    match command {
        HubCommand::Init { origin } => {
            let origin = absolute_origin(origin);
            hub.init(&origin)
                .map(|report| Report::Init { origin, report })
        }
        HubCommand::Clone {
            origin,
            name,
            reattach,
            ..
        } => hub
            .clone_tablet(tablet(), &absolute_origin(origin), name.clone(), *reattach)
            .map(Report::Clone),
        HubCommand::Sync { dry_run: true, .. } => hub.dry_run(tablet()).map(Report::DryRun),
        HubCommand::Sync { .. } => hub.sync(tablet()).map(Report::Sync),
        HubCommand::Backup { .. } => hub.backup(tablet()).map(Report::Backup),
        HubCommand::Status { .. } => hub.status(tablet()).map(Report::Status),
        HubCommand::Remote { action: None, .. } => hub.remote(tablet()).map(Report::Remote),
        HubCommand::Remote {
            action: Some(RemoteAction::SetUrl { url, .. }),
            ..
        } => hub
            .set_remote_url(tablet(), &absolute_origin(url))
            .map(Report::SetUrl),
        HubCommand::Detach { prune_ref, .. } => {
            hub.detach(tablet(), *prune_ref).map(Report::Detach)
        }
    }
}

/// Visible names of documents on the tablet, best effort (one batched
/// read); documents without a readable `.metadata` are left out.
pub fn lookup_names(tablet: &dyn Tablet, docs: &[DocId]) -> render::Names {
    let paths: Vec<XochitlPath> = docs
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|doc| XochitlPath::new(format!("{doc}.metadata")).ok())
        .collect();
    if paths.is_empty() {
        return BTreeMap::new();
    }
    tablet
        .read_files(&paths)
        .map(|files| {
            files
                .into_iter()
                .filter_map(|(path, bytes)| {
                    let doc = DocId::new(path.as_str().strip_suffix(".metadata")?).ok()?;
                    let metadata: Value = serde_json::from_slice(&bytes?).ok()?;
                    let name = metadata.get("visibleName")?.as_str()?;
                    Some((doc, name.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What [`finish`] needs besides the result.
pub struct OutputContext<'a> {
    /// For names (every command but `init`).
    pub tablet: Option<&'a dyn Tablet>,
    /// Where local caches live (for recovery instructions).
    pub cache_dir: Option<&'a Path>,
    pub output: Output,
    pub quiet: bool,
}

/// Write a command's result, or its error's details, and turn the error
/// into one `main` reports (its exit status from [`exit_code`]). Once
/// the command succeeded, a closed stdout (`| head`) is not an error.
pub fn finish(
    result: libremarkable_utils::Result<Report>,
    context: &OutputContext<'_>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    let names = |docs: Vec<DocId>| {
        context
            .tablet
            .map(|tablet| lookup_names(tablet, &docs))
            .unwrap_or_default()
    };
    match result {
        Ok(report) => {
            let written = write_report(&report, context, &names, stdout, stderr);
            match written {
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                other => Ok(other?),
            }
        }
        Err(error) => {
            // Best effort: a failed write must not replace the error (and
            // its exit status).
            let failure = Failure::of(&error);
            let details = render::error_details(
                &error,
                &render::ErrorContext {
                    names: names(render::error_docs(&error)),
                    repo_id: context
                        .tablet
                        .and_then(|tablet| tablet.read_state().ok().flatten())
                        .map(|state| state.repo_id),
                    cache_dir: context.cache_dir.map(Path::to_path_buf),
                },
            );
            let _ = details
                .iter()
                .try_for_each(|line| writeln!(stderr, "{line}"));
            if context.output == Output::Json {
                let value = json::error(failure, &error.to_string(), Some(&error));
                let _ = writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string_pretty(&value).expect("errors serialise")
                );
            }
            Err(error.into())
        }
    }
}

fn write_report(
    report: &Report,
    context: &OutputContext<'_>,
    names: &dyn Fn(Vec<DocId>) -> render::Names,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    let print = |out: &mut dyn Write, lines: &[String]| {
        lines.iter().try_for_each(|line| writeln!(out, "{line}"))
    };
    match context.output {
        Output::Json => {
            let value =
                serde_json::to_string_pretty(&json::report(report)).expect("reports serialise");
            writeln!(stdout, "{value}")?;
            if !context.quiet {
                print(stderr, &render::warnings(report))?;
            }
        }
        Output::Human => {
            let names = names(render::named_docs(report));
            print(stdout, &render::human(report, &names))?;
        }
        Output::Plain => {
            print(stdout, &render::plain(report))?;
            if !context.quiet {
                let names = names(render::named_docs(report));
                print(stderr, &render::human(report, &names))?;
            }
        }
    }
    Ok(())
}
