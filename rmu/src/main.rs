//! `rmu` — reMarkable utilities CLI.
//!
//! Manages documents and folders on a reMarkable tablet over SSH. All
//! configuration is passed as flags; there is no config file.
//!
//! Output discipline: stdout carries only machine-usable results (the
//! `ls` tree/JSON, downloaded paths, created UUIDs). Everything else —
//! progress bars, human status lines — goes to stderr.

mod hub;

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use libremarkable_utils::client::Client;
use libremarkable_utils::progress::{NoProgress, Progress};
use libremarkable_utils::ssh::{
    Auth, DEFAULT_SSH_USER, DEFAULT_USB_HOST, SshOptions, SshSession, maybe_run_askpass,
};
use libremarkable_utils::sync::{
    self, ConflictPolicy, DocSide, Endpoint, FsEndpoint, LocalFs, Mode, SshFs, SyncAction,
    SyncOptions,
};
use libremarkable_utils::xochitl::{self, Item, XOCHITL_DATA_DIR};

/// Password fallback for scripting, used when neither `--password` nor
/// `--password-file` is given.
const PASSWORD_ENV: &str = "RMU_SSH_PASSWORD";

#[derive(Parser)]
#[command(
    name = "rmu",
    about = "Utilities for the reMarkable tablet",
    version,
    after_help = "Targets are logical paths (e.g. Books/Math) or item UUIDs; '/' is the root.\n\
                  Authentication uses your ssh keys/config by default; see --password."
)]
struct Cli {
    /// Hostname or IP of the tablet
    #[arg(long, global = true, default_value = DEFAULT_USB_HOST)]
    host: String,

    /// SSH user on the tablet
    #[arg(long, global = true, default_value = DEFAULT_SSH_USER)]
    user: String,

    /// SSH port (default: ssh config, or 22)
    #[arg(long, global = true)]
    port: Option<u16>,

    /// SSH identity (private key) file
    #[arg(short = 'i', long, global = true)]
    identity: Option<PathBuf>,

    /// Extra ssh -o option (repeatable)
    #[arg(short = 'o', long = "ssh-option", global = true)]
    ssh_option: Vec<String>,

    /// Prompt for an SSH password (default: key-based auth)
    #[arg(long, global = true)]
    password: bool,

    /// Read the SSH password from the first line of a file
    #[arg(long, global = true, conflicts_with = "password")]
    password_file: Option<PathBuf>,

    /// xochitl data directory on the device
    #[arg(long, global = true, default_value = XOCHITL_DATA_DIR)]
    xochitl_dir: String,

    /// Do not restart xochitl after write operations
    #[arg(long, global = true)]
    no_restart: bool,

    /// Disable SSH connection multiplexing
    #[arg(long, global = true)]
    no_multiplex: bool,

    /// Suppress progress bars and status messages on stderr
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Whether `--host` was given rather than defaulted (`rmu hub log`,
    /// `restore` and `add` contact the tablet then, even with `--origin`).
    #[arg(skip)]
    host_given: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the logical tree of folders and documents
    Ls {
        /// Show folders only
        #[arg(long)]
        folders_only: bool,
        /// Include UUIDs in the output
        #[arg(long)]
        show_uuid: bool,
        /// Emit the flat item list as JSON
        #[arg(long)]
        json: bool,
    },
    /// Create a folder or nested folder path
    Mkdir {
        /// Folder path to create, e.g. Books/Math
        path: String,
        /// Parent folder (UUID or logical path); default: root
        #[arg(long, default_value = "")]
        parent: String,
    },
    /// Upload a document (.pdf, .epub, or .rmdoc)
    Upload {
        /// Local .pdf, .epub, or .rmdoc file
        file: PathBuf,
        /// Visible name on the device (default: bundle name or file stem)
        #[arg(short, long)]
        name: Option<String>,
        /// Destination folder (UUID or logical path); default: root
        #[arg(long, default_value = "")]
        parent: String,
    },
    /// Download documents (notebooks download as .rmdoc bundles)
    Download {
        /// Document UUID, logical path, or glob pattern (quote it so
        /// your shell doesn't expand it), e.g. 'Books/vol-*'
        target: String,
        /// Local file path or directory (default: current directory)
        output: Option<PathBuf>,
        /// Force an .rmdoc bundle (raw file set incl. annotations)
        /// even for PDFs/EPUBs
        #[arg(long)]
        bundle: bool,
    },
    /// Delete documents or folders
    Rm {
        /// Item UUIDs, logical paths, or glob patterns (all validated
        /// before anything is deleted), e.g. 'Books/vol-*'
        #[arg(required = true)]
        targets: Vec<String>,
        /// Delete non-empty folders recursively
        #[arg(short, long)]
        recursive: bool,
    },
    /// Move items into another folder
    Mv {
        /// Item UUID, logical path, or glob pattern
        target: String,
        /// Destination folder (UUID or logical path); '/' for root
        destination: String,
    },
    /// Rename a document or folder
    Rename {
        /// Item UUID or logical path
        target: String,
        /// New visible name
        new_name: String,
    },
    /// Show the tablet's system state (model, firmware, CPU/RAM/disk,
    /// battery, document counts)
    Status {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Permanently delete everything in the device's trash
    EmptyTrash,
    /// Restart the xochitl UI service on the device
    Restart,
    /// Sync a folder with the tablet, SRC -> DST or --two-way
    /// (scp-style endpoints: `[user@]host:path` is remote, resolved
    /// via ssh config)
    Sync {
        /// Source endpoint, e.g. `./books` or `remarkable:/Books`
        src: String,
        /// Destination endpoint
        dst: String,
        /// Print the plan without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Sync in both directions (argument order only matters for
        /// --conflict src/dst)
        #[arg(long)]
        two_way: bool,
        /// Propagate deletions of previously synced files (never
        /// touches files that were never synced)
        #[arg(long)]
        delete: bool,
        /// What to do when both sides changed (or unmapped files
        /// collide): report, newest timestamp wins, or a fixed side
        #[arg(long, value_enum, default_value_t = ConflictArg::Skip)]
        conflict: ConflictArg,
        /// Override remote endpoint auto-detection (applies to every
        /// remote endpoint)
        #[arg(long, value_enum)]
        remote_kind: Option<RemoteKindArg>,
    },
    /// Git-backed library shared by several tablets: clone, sync,
    /// backup, status (see the README)
    #[command(after_help = hub::AFTER_HELP)]
    Hub(hub::HubArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ConflictArg {
    /// Report conflicts and change nothing (default)
    Skip,
    /// The side with the newer timestamp wins (beware clock skew)
    Newest,
    /// The first argument's side wins
    Src,
    /// The second argument's side wins
    Dst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RemoteKindArg {
    /// A reMarkable tablet (skip the probe)
    Remarkable,
    /// A generic ssh filesystem host (plain file sync)
    Fs,
}

fn main() -> ExitCode {
    // Must run before argument parsing: ssh re-executes this binary as
    // an askpass helper when password auth is in use.
    maybe_run_askpass();
    let cli = parse_cli(std::env::args_os()).unwrap_or_else(|error| error.exit());

    // Progress bars only when stderr is a terminal and not --quiet.
    let progress: Arc<dyn Progress> = if cli.quiet || !std::io::stderr().is_terminal() {
        Arc::new(NoProgress)
    } else {
        Arc::new(CliProgress::default())
    };

    let result = match &cli.command {
        Command::Sync { .. } => run_sync(&cli, progress.clone()),
        Command::Hub(args) => hub::run(
            args,
            &hub::Env {
                quiet: cli.quiet,
                xochitl_dir: &cli.xochitl_dir,
                progress: progress.clone(),
                host_given: cli.host_given,
            },
            || make_session(&cli, &format!("{}@{}", cli.user, cli.host)),
        ),
        _ => run_regular(&cli, progress.clone()),
    };
    // Clear any progress UI before errors are printed.
    progress.finished();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // As `main` returning the error would print it.
            eprintln!("Error: {error:?}");
            ExitCode::from(match cli.command {
                Command::Hub(_) => hub::exit_code(&error),
                _ => 1,
            })
        }
    }
}

/// Parse the command line, with the checks clap cannot express: global
/// flags that do not apply to a subcommand are refused, not ignored.
fn parse_cli<I, T>(args: I) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    use clap::{CommandFactory, FromArgMatches};
    let matches = Cli::command().try_get_matches_from(args)?;
    let mut cli = Cli::from_arg_matches(&matches)?;
    cli.host_given = matches.value_source("host") == Some(clap::parser::ValueSource::CommandLine);
    if cli.no_restart && matches!(cli.command, Command::Hub(_)) {
        return Err(Cli::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            hub::NO_RESTART_REFUSED,
        ));
    }
    Ok(cli)
}

/// Build a session for a destination string (`host` or `user@host`).
fn make_session(cli: &Cli, destination: &str) -> Result<SshSession> {
    let auth = resolve_auth(cli, destination)?;
    Ok(SshSession::new(SshOptions {
        destination: destination.to_string(),
        port: cli.port,
        identity_file: cli.identity.clone(),
        extra_options: cli.ssh_option.clone(),
        auth,
        multiplex: !cli.no_multiplex,
    })?)
}

/// All commands except `sync`: one device, addressed by --host/--user.
fn run_regular(cli: &Cli, progress: Arc<dyn Progress>) -> Result<()> {
    let destination = format!("{}@{}", cli.user, cli.host);
    let session = make_session(cli, &destination)?;
    let client = Client::new(session, cli.xochitl_dir.clone())
        .with_progress(progress.clone())
        .with_listing_cache(sync::listing_cache_path(&destination, &cli.xochitl_dir));

    let modified = run_command(&client, cli)?;
    if modified {
        if cli.no_restart {
            info(
                cli,
                "note: xochitl not restarted (--no-restart); changes appear after restart",
            );
        } else {
            restart_after_writes(&client, "changes were written")?;
        }
    }
    Ok(())
}

/// One built side of a sync: a file tree (local or generic ssh host)
/// or a reMarkable tablet.
enum BuiltSide {
    Fs {
        endpoint: Box<dyn FsEndpoint>,
        is_local: bool,
        /// Stable identity for archive keying (canonical local path,
        /// or `destination:path` for ssh hosts).
        identity: String,
    },
    Tablet {
        client: Box<Client>,
        path: String,
        destination: String,
    },
}

/// Build a sync side from a parsed endpoint: local paths become
/// `LocalFs`; remote endpoints are probed (or `--remote-kind`-forced)
/// into a tablet or a generic ssh filesystem host.
fn build_side(
    cli: &Cli,
    progress: &Arc<dyn Progress>,
    endpoint: Endpoint,
    remote_kind: Option<RemoteKindArg>,
) -> Result<BuiltSide> {
    match endpoint {
        Endpoint::Local(path) => {
            let identity = canonical_identity(&path);
            Ok(BuiltSide::Fs {
                endpoint: Box::new(LocalFs::new(path)),
                is_local: true,
                identity,
            })
        }
        Endpoint::Remote { destination, path } => {
            let session = make_session(cli, &destination)?;
            let is_tablet = match remote_kind {
                Some(RemoteKindArg::Remarkable) => true,
                Some(RemoteKindArg::Fs) => false,
                None => {
                    progress.step(&format!("Probing {destination}"));
                    sync::probe_remarkable(&session, &cli.xochitl_dir)?
                }
            };
            if is_tablet {
                Ok(BuiltSide::Tablet {
                    client: Box::new(
                        Client::new(session, cli.xochitl_dir.clone())
                            .with_progress(progress.clone())
                            .with_listing_cache(sync::listing_cache_path(
                                &destination,
                                &cli.xochitl_dir,
                            )),
                    ),
                    path,
                    destination,
                })
            } else {
                // An empty SSH path is the login directory, not the filesystem
                // root. Keep `/` distinct when removing trailing separators so
                // same-host home/root pairs have stable, different identities.
                let trimmed = path.trim_end_matches('/');
                let identity_path = if trimmed.is_empty() && !path.is_empty() {
                    "/"
                } else {
                    trimmed
                };
                Ok(BuiltSide::Fs {
                    identity: format!("{destination}:{identity_path}"),
                    endpoint: Box::new(SshFs::new(session, path, progress.clone())),
                    is_local: false,
                })
            }
        }
    }
}

/// `rmu sync SRC DST` — see `docs/sync-design.md`.
fn run_sync(cli: &Cli, progress: Arc<dyn Progress>) -> Result<()> {
    let Command::Sync {
        src,
        dst,
        dry_run,
        two_way,
        delete,
        conflict,
        remote_kind,
    } = &cli.command
    else {
        unreachable!("run_sync is only called for the sync command");
    };

    let src_side = build_side(cli, &progress, sync::parse_endpoint(src), *remote_kind)?;
    let dst_side = build_side(cli, &progress, sync::parse_endpoint(dst), *remote_kind)?;

    match (src_side, dst_side) {
        (
            BuiltSide::Fs {
                endpoint, identity, ..
            },
            BuiltSide::Tablet {
                client,
                path,
                destination,
            },
        ) => {
            let state_path =
                sync::sync_state_path(&identity, &tablet_identity(&destination, &path));
            run_device_sync(
                cli,
                &progress,
                &*endpoint,
                &client,
                &tablet_identity(&destination, &path),
                &path,
                &state_path,
                true,
                *two_way,
                *delete,
                *conflict,
                *dry_run,
            )
        }
        (
            BuiltSide::Tablet {
                client,
                path,
                destination,
            },
            BuiltSide::Fs {
                endpoint, identity, ..
            },
        ) => {
            let state_path =
                sync::sync_state_path(&identity, &tablet_identity(&destination, &path));
            run_device_sync(
                cli,
                &progress,
                &*endpoint,
                &client,
                &tablet_identity(&destination, &path),
                &path,
                &state_path,
                false,
                *two_way,
                *delete,
                *conflict,
                *dry_run,
            )
        }
        (
            BuiltSide::Fs {
                endpoint: src_ep,
                is_local: src_local,
                identity: src_id,
            },
            BuiltSide::Fs {
                endpoint: dst_ep,
                is_local: dst_local,
                identity: dst_id,
            },
        ) => run_files_sync(
            cli,
            &progress,
            (src_ep, src_local, src_id),
            (dst_ep, dst_local, dst_id),
            *two_way,
            *delete,
            *conflict,
            *dry_run,
        ),
        (
            BuiltSide::Tablet {
                client: src_client,
                path: src_path,
                destination: src_dest,
            },
            BuiltSide::Tablet {
                client: dst_client,
                path: dst_path,
                destination: dst_dest,
            },
        ) => run_docs_sync(
            cli,
            &progress,
            (src_client, src_path, src_dest),
            (dst_client, dst_path, dst_dest),
            *two_way,
            *delete,
            *conflict,
            *dry_run,
        ),
    }
}

/// Canonical identity of a local path for archive keying, so
/// `./books`, `books`, and the absolute path all find the same
/// archive — **including before the directory exists** (pull into a
/// fresh dir): the deepest existing ancestor is canonicalized (which
/// also resolves symlinks like macOS `/var` → `/private/var`) and the
/// missing tail is appended.
fn canonical_identity(path: &str) -> String {
    fn canon(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
                canon(parent).join(name)
            }
            _ => path.to_path_buf(),
        })
    }
    let joined;
    let absolute = if Path::new(path).is_absolute() {
        Path::new(path)
    } else {
        joined = std::env::current_dir().unwrap_or_default().join(path);
        &joined
    };
    canon(absolute).display().to_string()
}

/// Stable archive-keying identity for a tablet endpoint.
fn tablet_identity(destination: &str, path: &str) -> String {
    format!("{destination}:{}", path.trim_end_matches('/'))
}

/// Snapshot a filesystem root without creating it. Only writable sides
/// may be absent; source errors and errors walking existing trees propagate.
fn sync_fs_snapshot(
    endpoint: &dyn FsEndpoint,
    writable: bool,
) -> Result<(Vec<sync::LocalEntry>, usize, bool)> {
    if writable && !endpoint.root_exists()? {
        return Ok((Vec::new(), 0, true));
    }
    let (entries, ignored) = endpoint
        .snapshot()
        .with_context(|| format!("reading {}", endpoint.label()))?;
    Ok((entries, ignored, false))
}

/// A missing writable tablet root is an empty, virtual subtree. Its
/// root mapping is replaced with the created UUID before execution.
fn sync_tablet_snapshot(
    client: &Client,
    path: &str,
    writable: bool,
) -> Result<(sync::RemoteSnapshot, bool)> {
    // Both source and destination trees can drive destructive actions by UUID.
    // Root resolution and planning must therefore bypass cached stat-fastcheck.
    let items = client.list_items_authoritative()?;
    match xochitl::resolve_folder_ref(&items, path) {
        Ok(uuid) => Ok((sync::remote_snapshot(&items, &uuid), false)),
        Err(libremarkable_utils::Error::PathNotFound(_)) if writable => {
            // Check the existing prefix just as mkdir_path does: a document
            // or ambiguous folder must not be treated as a missing directory.
            path.split('/')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .try_fold(Some(String::new()), |parent, part| {
                    let Some(parent) = parent else {
                        return Ok::<_, libremarkable_utils::Error>(None);
                    };
                    if let Some(folder) = xochitl::find_child(&items, &parent, part, true)? {
                        Ok(Some(folder.uuid.clone()))
                    } else {
                        xochitl::ensure_no_conflict(&items, &parent, part, None)?;
                        Ok(None)
                    }
                })?;
            Ok((sync::remote_snapshot(&[], ""), true))
        }
        Err(err) => Err(err.into()),
    }
}

/// Sync between two tablets via `.rmdoc` bundle streaming.
#[allow(clippy::too_many_arguments)]
fn run_docs_sync(
    cli: &Cli,
    progress: &Arc<dyn Progress>,
    src: (Box<Client>, String, String),
    dst: (Box<Client>, String, String),
    two_way: bool,
    delete: bool,
    conflict: ConflictArg,
    dry_run: bool,
) -> Result<()> {
    let (src_client, src_path, src_dest) = src;
    let (dst_client, dst_path, dst_dest) = dst;

    // Side A is chosen order-independently (lexicographically smaller
    // endpoint identity) so the pair-state file is found regardless of
    // argument order.
    let src_id = format!("{src_dest}:{}", src_path.trim_end_matches('/'));
    let dst_id = format!("{dst_dest}:{}", dst_path.trim_end_matches('/'));
    let a_is_src = src_id <= dst_id;
    let (client_a, path_a, id_a, client_b, path_b, id_b) = if a_is_src {
        (src_client, src_path, src_id, dst_client, dst_path, dst_id)
    } else {
        (dst_client, dst_path, dst_id, src_client, src_path, src_id)
    };

    let mode = match (two_way, a_is_src) {
        (true, _) => Mode::TwoWay,
        (false, true) => Mode::Push,  // A -> B
        (false, false) => Mode::Pull, // B -> A
    };
    let options = SyncOptions {
        mode,
        delete,
        conflict: map_conflict(conflict, a_is_src),
    };

    let (snapshot_a, missing_a) = sync_tablet_snapshot(&client_a, &path_a, mode != Mode::Push)?;
    let (snapshot_b, missing_b) = sync_tablet_snapshot(&client_b, &path_b, mode != Mode::Pull)?;
    let state_path = sync::pair_state_path(&id_a, &id_b);
    let mut state = sync::PairState::load(&state_path)?;
    let plan = sync::plan_docs(options, &snapshot_a, &snapshot_b, &state);
    [
        (DocSide::A, &client_a, &id_a, missing_a),
        (DocSide::B, &client_b, &id_b, missing_b),
    ]
    .into_iter()
    .filter(|(side, .., missing)| *missing || plan.iter().any(|action| action.writes(*side)))
    .for_each(|(side, client, id, _)| {
        let deletes = plan.iter().any(|action| action.deletes(side));
        warn_if_hub_attached(cli, client, id, deletes);
    });

    info(cli, format!("A = {id_a}, B = {id_b}"));
    if dry_run {
        if missing_a {
            println!("mkdir root A   {path_a}");
        }
        if missing_b {
            println!("mkdir root B   {path_b}");
        }
        plan.iter()
            .for_each(|action| println!("{}", sync::describe_doc(action)));
        if plan.is_empty() && !missing_a && !missing_b {
            info(cli, "Already in sync; nothing to do.");
        }
        return Ok(());
    }

    let mut folders_a = snapshot_a.folders.clone();
    let mut folders_b = snapshot_b.folders.clone();
    if missing_a {
        folders_a.insert(String::new(), client_a.mkdir_path(&path_a, "")?.uuid);
    }
    if missing_b {
        folders_b.insert(String::new(), client_b.mkdir_path(&path_b, "")?.uuid);
    }
    let mut outcome = sync::execute_docs(
        &client_a,
        &client_b,
        &**progress,
        &plan,
        &mut folders_a,
        &mut folders_b,
        &mut state,
        &state_path,
    )?;

    outcome.modified_a |= missing_a;
    outcome.modified_b |= missing_b;
    outcome
        .conflicts
        .iter()
        .for_each(|(key, reason)| info(cli, format!("conflict {key}: {reason}")));
    info(
        cli,
        format!(
            "Sync complete: {} copied to A, {} copied to B, {} deleted on A, \
             {} deleted on B, {} folder(s) created, {} conflict(s).",
            outcome.copied_to_a,
            outcome.copied_to_b,
            outcome.deleted_a,
            outcome.deleted_b,
            outcome.folders_created,
            outcome.conflicts.len(),
        ),
    );

    if !cli.no_restart {
        if outcome.modified_a {
            restart_after_writes(&client_a, "sync wrote changes to side A")?;
        }
        if outcome.modified_b {
            restart_after_writes(&client_b, "sync wrote changes to side B")?;
        }
    } else if outcome.modified_a || outcome.modified_b {
        info(
            cli,
            "note: xochitl not restarted (--no-restart); changes appear after restart",
        );
    }
    Ok(())
}

/// Sync between a file tree and a tablet.
#[allow(clippy::too_many_arguments)]
fn run_device_sync(
    cli: &Cli,
    progress: &Arc<dyn Progress>,
    fs_side: &dyn FsEndpoint,
    client: &Client,
    tablet: &str,
    remote_path: &str,
    state_path: &Path,
    fs_is_src: bool,
    two_way: bool,
    delete: bool,
    conflict: ConflictArg,
    dry_run: bool,
) -> Result<()> {
    let mode = match (two_way, fs_is_src) {
        (true, _) => Mode::TwoWay,
        (false, true) => Mode::Push,
        (false, false) => Mode::Pull,
    };
    let options = SyncOptions {
        mode,
        delete,
        conflict: map_conflict(conflict, fs_is_src),
    };

    let (mut fs_entries, ignored, missing_fs) = sync_fs_snapshot(fs_side, mode != Mode::Push)?;
    let (mut snapshot, missing_remote) =
        sync_tablet_snapshot(client, remote_path, mode != Mode::Pull)?;
    let mut state = sync::SyncState::load(state_path)?;
    warn_legacy_state(cli, fs_side);

    // Content hashes: local files stamp-gated against the archive
    // (unchanged files are never re-read), device payloads lazily,
    // in one batched round trip, only where a decision needs one.
    sync::attach_hashes(fs_side, &mut fs_entries, &state)
        .with_context(|| format!("hashing files in {}", fs_side.label()))?;
    let wanted = sync::device_hash_candidates(&fs_entries, &snapshot, &state);
    let payload_hashes = client.payload_hashes(&wanted)?;
    sync::attach_payload_hashes(&mut snapshot, &payload_hashes);

    let plan = sync::plan(options, &fs_entries, &snapshot, &state);
    if missing_remote || plan.actions.iter().any(SyncAction::writes_remote) {
        let deletes = plan.actions.iter().any(SyncAction::deletes_remote);
        warn_if_hub_attached(cli, client, tablet, deletes);
    }

    if dry_run {
        // The plan *is* the output in dry-run mode.
        if missing_fs {
            println!("mkdir root fs   {}", fs_side.label());
        }
        if missing_remote {
            println!("mkdir root tablet   {remote_path}");
        }
        plan.actions
            .iter()
            .for_each(|action| println!("{}", sync::describe(action)));
        if plan.actions.is_empty() && !missing_fs && !missing_remote {
            info(cli, "Already in sync; nothing to do.");
        }
        return Ok(());
    }

    if missing_fs {
        fs_side.ensure_root()?;
    }
    let mut folders = snapshot.folders.clone();
    if missing_remote {
        folders.insert(String::new(), client.mkdir_path(remote_path, "")?.uuid);
    }
    let mut outcome = sync::execute(
        client,
        &**progress,
        fs_side,
        &plan,
        &mut folders,
        &mut state,
        state_path,
    )?;

    outcome.modified_remote |= missing_remote;
    outcome
        .skipped
        .iter()
        .for_each(|(path, reason)| info(cli, format!("skipped {path}: {reason}")));
    outcome
        .conflicts
        .iter()
        .for_each(|(path, reason)| info(cli, format!("conflict {path}: {reason}")));
    info(
        cli,
        format!(
            "Sync complete: {} uploaded, {} updated, {} downloaded, {} moved \
             ({} local / {} device), {} copied without transfer ({} local / {} device), \
             {} deleted ({} local / {} device), {} folder(s) created, {} emptied \
             folder(s) removed, {} conflict(s), {} skipped, {} unsupported file(s) \
             ignored.",
            outcome.uploaded,
            outcome.updated,
            outcome.downloaded,
            outcome.moved_local + outcome.moved_remote,
            outcome.moved_local,
            outcome.moved_remote,
            outcome.copied_local + outcome.copied_remote,
            outcome.copied_local,
            outcome.copied_remote,
            outcome.deleted_local + outcome.deleted_remote,
            outcome.deleted_local,
            outcome.deleted_remote,
            outcome.folders_created,
            outcome.deleted_local_dirs + outcome.deleted_remote_folders,
            outcome.conflicts.len(),
            outcome.skipped.len(),
            ignored,
        ),
    );

    if outcome.modified_remote {
        if cli.no_restart {
            info(
                cli,
                "note: xochitl not restarted (--no-restart); changes appear after restart",
            );
        } else {
            restart_after_writes(client, "sync wrote changes")?;
        }
    }
    Ok(())
}

/// Plain file sync between two file trees (local↔local, local↔ssh
/// host, ssh↔ssh). No conversions; same supported file types.
#[allow(clippy::too_many_arguments)]
fn run_files_sync(
    cli: &Cli,
    progress: &Arc<dyn Progress>,
    src: (Box<dyn FsEndpoint>, bool, String),
    dst: (Box<dyn FsEndpoint>, bool, String),
    two_way: bool,
    delete: bool,
    conflict: ConflictArg,
    dry_run: bool,
) -> Result<()> {
    let (src_ep, src_local, src_id) = src;
    let (dst_ep, dst_local, dst_id) = dst;

    // Keep the existing local-A convention for mixed pairs. Same-kind
    // pairs use identity order, never argument order: their archive key
    // is also order-independent, but its stamps belong to specific sides.
    let same_kind = src_local == dst_local;
    let a_is_src = if same_kind {
        src_id <= dst_id
    } else {
        src_local
    };
    let old_state_path = sync::sync_state_path(&src_id, &dst_id);
    // Older same-kind archives did not record which endpoint was A.
    // Isolate the stable-role format rather than guessing that orientation
    // (or letting older binaries interpret our newly ordered stamps).
    // Mixed pairs already had stable roles and keep their existing archive.
    let state_path = if same_kind {
        old_state_path.with_extension("files-v1.json")
    } else {
        old_state_path.clone()
    };
    if same_kind && !state_path.exists() && old_state_path.exists() {
        info(
            cli,
            format!(
                "note: ignoring {}: filesystem archive has unknown side order; \
                 starting fresh. Review --dry-run before choosing --conflict; \
                 previous deletions cannot be inferred.",
                old_state_path.display(),
            ),
        );
    }
    let (a, b) = if a_is_src {
        (src_ep, dst_ep)
    } else {
        (dst_ep, src_ep)
    };

    let mode = match (two_way, a_is_src) {
        (true, _) => Mode::TwoWay,
        (false, true) => Mode::Push,  // A -> B
        (false, false) => Mode::Pull, // B -> A
    };
    let options = SyncOptions {
        mode,
        delete,
        conflict: map_conflict(conflict, a_is_src),
    };

    let (a_entries, a_ignored, missing_a) = sync_fs_snapshot(&*a, mode != Mode::Push)?;
    let (b_entries, b_ignored, missing_b) = sync_fs_snapshot(&*b, mode != Mode::Pull)?;
    let mut state = sync::SyncState::load(&state_path)?;
    warn_legacy_state(cli, &*a);
    let plan = sync::plan_files(options, &a_entries, &b_entries, &state);

    info(cli, format!("A = {}, B = {}", a.label(), b.label()));
    if dry_run {
        if missing_a {
            println!("mkdir root A   {}", a.label());
        }
        if missing_b {
            println!("mkdir root B   {}", b.label());
        }
        plan.iter()
            .for_each(|action| println!("{}", sync::describe_file(action)));
        if plan.is_empty() && !missing_a && !missing_b {
            info(cli, "Already in sync; nothing to do.");
        }
        return Ok(());
    }

    if missing_a {
        a.ensure_root()?;
    }
    if missing_b {
        b.ensure_root()?;
    }
    let outcome = sync::execute_files(&*a, &*b, &**progress, &plan, &mut state, &state_path)?;
    outcome
        .conflicts
        .iter()
        .for_each(|(path, reason)| info(cli, format!("conflict {path}: {reason}")));
    info(
        cli,
        format!(
            "Sync complete: {} copied to A, {} copied to B, {} deleted on A, \
             {} deleted on B, {} conflict(s), {} unsupported file(s) ignored.",
            outcome.copied_to_a,
            outcome.copied_to_b,
            outcome.deleted_a,
            outcome.deleted_b,
            outcome.conflicts.len(),
            a_ignored + b_ignored,
        ),
    );
    Ok(())
}

/// Map `--conflict src|dst` onto the planner's local/remote (A/B)
/// policy sides based on which argument plays the "local"/"A" role.
fn map_conflict(conflict: ConflictArg, src_is_local_side: bool) -> ConflictPolicy {
    match conflict {
        ConflictArg::Skip => ConflictPolicy::Skip,
        ConflictArg::Newest => ConflictPolicy::Newest,
        ConflictArg::Src if src_is_local_side => ConflictPolicy::PreferLocal,
        ConflictArg::Src => ConflictPolicy::PreferRemote,
        ConflictArg::Dst if src_is_local_side => ConflictPolicy::PreferRemote,
        ConflictArg::Dst => ConflictPolicy::PreferLocal,
    }
}

/// One-time nudge: sync state moved out of the synced tree; an old
/// in-root `.rmu-sync.json` is ignored and can be deleted.
fn warn_legacy_state(cli: &Cli, fs_side: &dyn FsEndpoint) {
    if let Some(root) = fs_side.as_local_path("")
        && root.join(sync::LEGACY_STATE_FILE_NAME).exists()
    {
        info(
            cli,
            format!(
                "note: sync state now lives under ~/.local/state/rmu; the legacy {} in {} \
                 is ignored and can be deleted",
                sync::LEGACY_STATE_FILE_NAME,
                fs_side.label(),
            ),
        );
    }
}

/// `rmu sync` is about to write to a tablet attached to an `rmu hub`
/// (`deletes`: and to delete there).
fn hub_attached_warning(tablet: &str, deletes: bool) -> String {
    let deletions = match deletes {
        true => {
            ", and the documents it deletes there are deleted from the library, on every hub \
             tablet"
        }
        false => "",
    };
    format!(
        "warning: {tablet} is attached to an rmu hub: documents rmu sync copies onto it get new \
         ids, so its next `rmu hub sync` adds them to the library as new documents (duplicates \
         of any copies already there){deletions}; `rmu hub` is the way to share documents \
         between hub tablets"
    )
}

/// Warn (stderr) when `rmu sync`'s plan writes to a hub-attached
/// tablet. The answer comes from the look at the hub that rode along in
/// the snapshot's listing; a failed look only skips the warning (a write
/// then checks again, and fails loudly).
fn warn_if_hub_attached(cli: &Cli, client: &Client, tablet: &str, deletes: bool) {
    if client.hub_attached().unwrap_or(false) {
        info(cli, hub_attached_warning(tablet, deletes));
    }
}

/// Restart xochitl after writes (`written` says which: "changes were
/// written", "sync wrote changes to side A", ...). A hub run that took
/// the tablet since keeps it an error (xochitl must reload what was
/// written), with what to do about it.
fn restart_after_writes(client: &Client, written: &str) -> Result<()> {
    match client.restart_xochitl() {
        Err(libremarkable_utils::Error::HubLocked(owner)) => {
            let owner = owner.map_or_else(
                || "its owner record is missing".to_string(),
                |owner| owner.to_string(),
            );
            Err(anyhow::anyhow!(
                "{written}, but xochitl was not restarted: an rmu hub run now holds the tablet \
                 ({owner}). A hub sync restarts xochitl when it finishes; otherwise run `rmu \
                 restart` once the lock is free"
            ))
        }
        other => other.with_context(|| format!("{written}, but restarting xochitl failed")),
    }
}

/// Human status line: stderr, suppressed by --quiet.
fn info(cli: &Cli, message: impl AsRef<str>) {
    if !cli.quiet {
        eprintln!("{}", message.as_ref());
    }
}

/// Execute a subcommand; returns whether device storage was modified.
///
/// stdout gets machine-usable results only; human status goes to
/// stderr via [`info`].
fn run_command(client: &Client, cli: &Cli) -> Result<bool> {
    match &cli.command {
        Command::Ls {
            folders_only,
            show_uuid,
            json,
        } => {
            let items = client.list_items()?;
            if *json {
                let out: Vec<serde_json::Value> = items
                    .iter()
                    .map(|item| {
                        serde_json::json!({
                            "uuid": item.uuid,
                            "name": item.visible_name,
                            "parent_uuid": item.parent,
                            "type": if item.is_folder() { "folder" } else { "document" },
                            "path": xochitl::build_path(&items, item),
                            "file_type": item.file_type,
                            "created_time": item.created_time,
                            "last_modified": item.last_modified,
                            "size_bytes": item.size_bytes,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for line in xochitl::render_tree(&items, *show_uuid, *folders_only) {
                    println!("{line}");
                }
            }
            Ok(false)
        }
        Command::Mkdir { path, parent } => {
            let item = client.mkdir_path(path, parent)?;
            info(cli, format!("Created folder: {}/", item.visible_name));
            println!("{}", item.uuid);
            Ok(true)
        }
        Command::Upload { file, name, parent } => {
            let item = client.upload(file, parent, name.as_deref())?;
            info(
                cli,
                format!(
                    "Uploaded {} as '{}' ({})",
                    file.display(),
                    item.visible_name,
                    item.file_type.as_deref().unwrap_or("?"),
                ),
            );
            println!("{}", item.uuid);
            Ok(true)
        }
        Command::Download {
            target,
            output,
            bundle,
        } => {
            let destinations = client.download_matching(target, output.as_deref(), *bundle)?;
            info(cli, "Downloaded to:");
            destinations
                .iter()
                .for_each(|path| println!("{}", path.display()));
            Ok(false)
        }
        Command::Rm { targets, recursive } => {
            let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
            let deleted = client.delete_many(&refs, *recursive)?;
            info(cli, format!("Deleted {} item(s):", deleted.len()));
            deleted.iter().for_each(|item| {
                info(
                    cli,
                    format!(
                        "  {}{} [{}]",
                        item.visible_name,
                        item_suffix(item),
                        item.uuid
                    ),
                );
            });
            Ok(true)
        }
        Command::Mv {
            target,
            destination,
        } => {
            let moved = client.move_items(target, destination)?;
            moved.iter().for_each(|item| {
                info(
                    cli,
                    format!(
                        "Moved '{}' to {}",
                        item.visible_name,
                        if item.parent.is_empty() {
                            "(root)"
                        } else {
                            &item.parent
                        }
                    ),
                );
            });
            Ok(true)
        }
        Command::Rename { target, new_name } => {
            let item = client.rename(target, new_name)?;
            info(cli, format!("Renamed to: {}", item.visible_name));
            Ok(true)
        }
        Command::Status { json } => {
            let status = client.system_status()?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                libremarkable_utils::status::render(&status)
                    .iter()
                    .for_each(|line| println!("{line}"));
            }
            Ok(false)
        }
        Command::EmptyTrash => {
            let deleted = client.empty_trash()?;
            if deleted.is_empty() {
                info(cli, "Trash is already empty.");
                return Ok(false);
            }
            info(cli, format!("Emptied trash: {} item(s):", deleted.len()));
            deleted.iter().for_each(|item| {
                info(
                    cli,
                    format!(
                        "  {}{} [{}]",
                        item.visible_name,
                        item_suffix(item),
                        item.uuid
                    ),
                );
            });
            Ok(true)
        }
        Command::Restart => {
            client.restart_xochitl()?;
            info(cli, "xochitl restarted");
            Ok(false)
        }
        // Dispatched in main, never reach run_command.
        Command::Sync { .. } | Command::Hub(_) => {
            unreachable!("sync and hub are dispatched in main")
        }
    }
}

/// Renders [`Progress`] events as indicatif bars on stderr: a spinner
/// per step, upgraded in place to a byte bar during transfers.
#[derive(Default)]
struct CliProgress {
    bar: Mutex<Option<ProgressBar>>,
}

impl CliProgress {
    fn new_bar() -> ProgressBar {
        let bar = ProgressBar::new_spinner();
        bar.enable_steady_tick(Duration::from_millis(80));
        bar
    }

    fn spinner_style() -> ProgressStyle {
        ProgressStyle::with_template("{spinner} {msg}").expect("static template")
    }

    fn bytes_style() -> ProgressStyle {
        ProgressStyle::with_template(
            "{msg} [{bar:25}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta})",
        )
        .expect("static template")
        .progress_chars("=> ")
    }

    fn bytes_unknown_style() -> ProgressStyle {
        ProgressStyle::with_template("{spinner} {msg} {bytes} ({bytes_per_sec})")
            .expect("static template")
    }
}

impl Progress for CliProgress {
    fn step(&self, message: &str) {
        let mut state = self.bar.lock().expect("progress mutex");
        let bar = state.get_or_insert_with(Self::new_bar);
        bar.set_style(Self::spinner_style());
        bar.set_message(message.to_string());
    }

    fn bytes(&self, transferred: u64, total: Option<u64>) {
        let mut state = self.bar.lock().expect("progress mutex");
        let bar = state.get_or_insert_with(Self::new_bar);
        match total {
            Some(total) => {
                if bar.length() != Some(total) {
                    bar.set_style(Self::bytes_style());
                    bar.set_length(total);
                }
            }
            None => {
                if bar.length().is_some() {
                    bar.unset_length();
                }
                bar.set_style(Self::bytes_unknown_style());
            }
        }
        bar.set_position(transferred);
    }

    fn bytes_done(&self) {
        // Drop back to a plain spinner; the next step restyles it.
        let state = self.bar.lock().expect("progress mutex");
        if let Some(bar) = state.as_ref() {
            bar.set_style(Self::spinner_style());
            bar.unset_length();
        }
    }

    fn finished(&self) {
        if let Some(bar) = self.bar.lock().expect("progress mutex").take() {
            bar.finish_and_clear();
        }
    }
}

fn resolve_auth(cli: &Cli, destination: &str) -> Result<Auth> {
    if let Some(path) = &cli.password_file {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading password file {}", path.display()))?;
        return Ok(Auth::Password(
            text.lines().next().unwrap_or("").to_string(),
        ));
    }
    if cli.password {
        let prompt = format!("Password for {destination}: ");
        return Ok(Auth::Password(rpassword::prompt_password(prompt)?));
    }
    if let Ok(password) = std::env::var(PASSWORD_ENV) {
        return Ok(Auth::Password(password));
    }
    Ok(Auth::Default)
}

fn item_suffix(item: &Item) -> String {
    if item.is_folder() {
        "/".to_string()
    } else {
        item.file_type
            .as_deref()
            .map(|t| format!(" ({t})"))
            .unwrap_or_default()
    }
}
