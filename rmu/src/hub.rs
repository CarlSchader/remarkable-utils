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
//!
//! `log`, `restore` and `add` work on origin alone: with `--origin` they
//! never contact the tablet unless a flag naming it (`--host`, `--user`,
//! ...) is given too; without it the
//! tablet supplies its stored origin URL ([`wants_tablet`]).

mod json;
mod render;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use libremarkable_utils::Error;
use libremarkable_utils::git::Git;
use libremarkable_utils::hub::cycle::{
    self, AddReport, BackupReport, CycleError, DetachOptions, DetachReport, DocLogReport,
    DryRunReport, Hub, HubOptions, InitReport, LogReport, RemoteReport, RestoreReport,
    SetUrlReport, StatusReport, SyncReport,
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
  5  origin does not match the tablet: rewound or replaced, or the
     tablet's backups on it were rewritten or are missing (`rmu hub clone
     --reattach` attaches the tablet to it deliberately); or origin lost
     its main branch (check the URL, then restore origin from a backup or
     a computer's cache: the error says how)
  6  the tablet is paired with the reMarkable cloud (unpair it first)
  7  an interrupted sync must be finished first: run `rmu hub sync`
     against the current origin (for `detach`, --force discards it)
  8  the tablet did not answer (asleep or off the network): wake it, keep
     it awake, and run the command again

Output: stdout carries the result only: a summary on a terminal, JSON
with --json, otherwise plain ids (the summary then goes to stderr).
Progress, warnings and error details go to stderr.

JSON (\"schema\": 1) is unstable until the hub is validated on devices.
Errors are {\"schema\": 1, \"error\": {\"kind\", \"exit_code\", \"message\"}};
kind is error, deletions_need_consent (adds from_origin, from_tablet,
empty_snapshot and the documents' names), lock_held, origin_mismatch,
cloud_paired or journal_pending.";

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
    /// Show the library's history (one entry per sync, restore and add),
    /// or one document's (by logical path or UUID). No tablet is needed
    /// with --origin (unless --host or another SSH flag names one)
    #[command(after_help = AFTER_HELP)]
    Log {
        /// A document's logical path (as it is now, or was) or UUID
        doc: Option<String>,
        /// Show at most this many entries of the library's history
        #[arg(short = 'n', long, value_name = "N", conflicts_with = "doc")]
        max_count: Option<usize>,
        #[command(flatten)]
        origin: OriginArg,
    },
    /// Bring back an old version of a document, deleted ones included, as
    /// a new document (the original is never touched); every tablet gets
    /// it at its next sync. No tablet is needed with --origin (unless --host or another SSH flag names one)
    #[command(after_help = AFTER_HELP)]
    Restore {
        /// The document's logical path (at that commit) or UUID
        doc: String,
        /// The commit to restore from, as `rmu hub log` shows it, or main,
        /// main~N (a document deleted by then comes back as it was just
        /// before its deletion)
        #[arg(long, value_name = "REV")]
        at: String,
        #[command(flatten)]
        origin: OriginArg,
    },
    /// Add a PDF or EPUB to the library on origin; every tablet gets it at
    /// its next sync. No tablet is needed with --origin (unless --host or another SSH flag names one)
    #[command(after_help = AFTER_HELP)]
    Add {
        /// The .pdf or .epub file
        file: PathBuf,
        /// The folder (logical path or UUID; default: the root)
        folder: Option<String>,
        /// The document's name (default: the file name without extension)
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        origin: OriginArg,
    },
    /// Forget the hub state on the tablet; its documents are untouched
    #[command(after_help = AFTER_HELP)]
    Detach {
        /// Also delete the tablet's device ref (its backups) from origin
        #[arg(long)]
        prune_ref: bool,
        /// Detach even if an interrupted sync left a journal (documents
        /// its apply was writing may stay half-updated on the tablet;
        /// both versions stay in origin's history). Prefer `rmu hub sync`
        #[arg(long)]
        force: bool,
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
    Log(LogReport),
    DocLog(DocLogReport),
    Restore(RestoreReport),
    Add(AddReport),
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
    Unreachable,
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
                | CycleError::DeviceRefConflict { .. }
                | CycleError::DeviceRefMissing { .. },
            ) => Self::OriginMismatch,
            Error::HubCycle(CycleError::CloudPaired) => Self::CloudPaired,
            Error::HubCycle(
                CycleError::JournalPending
                | CycleError::DetachJournalPending
                | CycleError::JournalMergeMissing { .. },
            ) => Self::JournalPending,
            Error::Unreachable(_) => Self::Unreachable,
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
            Self::Unreachable => 8,
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
    /// A flag naming the tablet (`--host`, `--user`, `--port`, `-i`, `-o`,
    /// `--password`, `--password-file`) was given explicitly.
    pub tablet_given: bool,
}

/// Whether a command contacts the tablet. `init` never does; `log`,
/// `restore` and `add` only without `--origin` (the tablet supplies its
/// stored URL) or with an explicit flag naming the tablet (for its base,
/// or as the commit's author); every other command always does.
pub fn wants_tablet(command: &HubCommand, tablet_given: bool) -> bool {
    match command {
        HubCommand::Init { .. } => false,
        HubCommand::Log { origin, .. }
        | HubCommand::Restore { origin, .. }
        | HubCommand::Add { origin, .. } => origin.origin.is_none() || tablet_given,
        _ => true,
    }
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
        }
        | HubCommand::Log { origin: url, .. }
        | HubCommand::Restore { origin: url, .. }
        | HubCommand::Add { origin: url, .. } => origin(url),
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
    let tablet = match wants_tablet(&args.command, env.tablet_given) {
        false => None,
        true => Some(
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

/// Run a command. Every command but `init`, `log`, `restore` and `add`
/// needs `tablet` (see [`wants_tablet`]).
pub fn execute(
    hub: &Hub<'_>,
    tablet: Option<&dyn Tablet>,
    command: &HubCommand,
) -> libremarkable_utils::Result<Report> {
    let optional = tablet;
    let tablet = || tablet.expect("the command has a tablet");
    match command {
        HubCommand::Log {
            doc: None,
            max_count,
            ..
        } => hub.log(optional, *max_count).map(Report::Log),
        HubCommand::Log { doc: Some(doc), .. } => hub.doc_log(optional, doc).map(Report::DocLog),
        HubCommand::Restore { doc, at, .. } => hub.restore(optional, doc, at).map(Report::Restore),
        HubCommand::Add {
            file, folder, name, ..
        } => hub
            .add(optional, file, folder.as_deref(), name.as_deref())
            .map(Report::Add),
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
        HubCommand::Detach {
            prune_ref, force, ..
        } => hub
            .detach(
                tablet(),
                DetachOptions {
                    prune_device_ref: *prune_ref,
                    force: *force,
                },
            )
            .map(Report::Detach),
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
    // The tablet's `.metadata`, only for the documents the report or
    // error carries no name for (those come from origin's or the
    // tablet's history: the tablet may no longer have them).
    let names = |known: render::Names, docs: Vec<DocId>| {
        let missing: Vec<DocId> = docs
            .into_iter()
            .filter(|doc| !known.contains_key(doc))
            .collect();
        context
            .tablet
            .map(|tablet| lookup_names(tablet, &missing))
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
            // Only the recovery from an empty origin names the tablet's
            // repository: any other error, a tablet that stopped
            // answering above all (another timeout), is not worth a read.
            let repo_id = matches!(error, Error::HubCycle(CycleError::OriginEmpty { .. }))
                .then(|| context.tablet?.read_state().ok().flatten())
                .flatten()
                .map(|state| state.repo_id);
            let details = render::error_details(
                &error,
                &render::ErrorContext {
                    names: names(render::error_names(&error), render::error_docs(&error)),
                    repo_id,
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
    names: &dyn Fn(render::Names, Vec<DocId>) -> render::Names,
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
            let names = names(render::report_names(report), render::named_docs(report));
            print(stdout, &render::human(report, &names))?;
        }
        Output::Plain => {
            print(stdout, &render::plain(report))?;
            if !context.quiet {
                let names = names(render::report_names(report), render::named_docs(report));
                print(stderr, &render::human(report, &names))?;
            }
        }
    }
    Ok(())
}
