//! `rmu` — reMarkable utilities CLI.
//!
//! Manages documents and folders on a reMarkable tablet over SSH. All
//! configuration is passed as flags; there is no config file.
//!
//! Output discipline: stdout carries only machine-usable results (the
//! `ls` tree/JSON, downloaded paths, created UUIDs). Everything else —
//! progress bars, human status lines — goes to stderr.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
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
    self, ConflictPolicy, Endpoint, FsEndpoint, LocalFs, Mode, SshFs, SyncOptions,
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

fn main() -> Result<()> {
    // Must run before argument parsing: ssh re-executes this binary as
    // an askpass helper when password auth is in use.
    maybe_run_askpass();
    let cli = Cli::parse();

    // Progress bars only when stderr is a terminal and not --quiet.
    let progress: Arc<dyn Progress> = if cli.quiet || !std::io::stderr().is_terminal() {
        Arc::new(NoProgress)
    } else {
        Arc::new(CliProgress::default())
    };

    let result = match &cli.command {
        Command::Sync { .. } => run_sync(&cli, progress.clone()),
        _ => run_regular(&cli, progress.clone()),
    };
    // Clear any progress UI before errors are printed.
    progress.finished();
    result
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
    let client = Client::new(session, cli.xochitl_dir.clone()).with_progress(progress.clone());

    let modified = run_command(&client, cli)?;
    if modified {
        if cli.no_restart {
            info(
                cli,
                "note: xochitl not restarted (--no-restart); changes appear after restart",
            );
        } else {
            client
                .restart_xochitl()
                .context("changes were written, but restarting xochitl failed")?;
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
            // Canonicalize so `./books`, `books`, and the absolute
            // path all key the same archive. Falls back to the raw
            // path when the directory does not exist yet (pull into
            // a fresh directory).
            let identity = fs::canonicalize(&path)
                .unwrap_or_else(|_| PathBuf::from(&path))
                .display()
                .to_string();
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
                            .with_progress(progress.clone()),
                    ),
                    path,
                    destination,
                })
            } else {
                Ok(BuiltSide::Fs {
                    identity: format!("{destination}:{}", path.trim_end_matches('/')),
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

/// Stable archive-keying identity for a tablet endpoint.
fn tablet_identity(destination: &str, path: &str) -> String {
    format!("{destination}:{}", path.trim_end_matches('/'))
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

    // Resolve each root folder; create it when that side is writable.
    let resolve_root =
        |client: &Client, path: &str, writable: bool| -> Result<(Vec<Item>, String)> {
            let mut items = client.list_items()?;
            let uuid = match xochitl::resolve_folder_ref(&items, path) {
                Ok(uuid) => uuid,
                Err(libremarkable_utils::Error::PathNotFound(_)) if writable => {
                    let created = client.mkdir_path(path, "")?;
                    items = client.list_items()?;
                    created.uuid
                }
                Err(err) => return Err(err.into()),
            };
            Ok((items, uuid))
        };
    let (items_a, root_a) = resolve_root(&client_a, &path_a, mode != Mode::Push)?;
    let (items_b, root_b) = resolve_root(&client_b, &path_b, mode != Mode::Pull)?;

    let snapshot_a = sync::remote_snapshot(&items_a, &root_a);
    let snapshot_b = sync::remote_snapshot(&items_b, &root_b);
    let state_path = sync::pair_state_path(&id_a, &id_b);
    let mut state = sync::PairState::load(&state_path)?;
    let plan = sync::plan_docs(options, &snapshot_a, &snapshot_b, &state);

    info(cli, format!("A = {id_a}, B = {id_b}"));
    if dry_run {
        plan.iter()
            .for_each(|action| println!("{}", sync::describe_doc(action)));
        if plan.is_empty() {
            info(cli, "Already in sync; nothing to do.");
        }
        return Ok(());
    }

    let mut folders_a = snapshot_a.folders.clone();
    let mut folders_b = snapshot_b.folders.clone();
    let outcome = sync::execute_docs(
        &client_a,
        &client_b,
        &**progress,
        &plan,
        &mut folders_a,
        &mut folders_b,
        &mut state,
        &state_path,
    )?;

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
            client_a
                .restart_xochitl()
                .context("sync wrote changes to side A, but restarting xochitl failed")?;
        }
        if outcome.modified_b {
            client_b
                .restart_xochitl()
                .context("sync wrote changes to side B, but restarting xochitl failed")?;
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

    // The fs side holds the state file and receives pulls: make sure
    // its root exists (except pure push, where a missing source is an
    // error surfaced by the snapshot).
    if mode != Mode::Push {
        fs_side.ensure_root()?;
    }

    // Resolve the device folder; when this side can be written to,
    // create it if missing.
    let mut items = client.list_items()?;
    let root_uuid = match xochitl::resolve_folder_ref(&items, remote_path) {
        Ok(uuid) => uuid,
        Err(libremarkable_utils::Error::PathNotFound(_)) if mode != Mode::Pull => {
            let created = client.mkdir_path(remote_path, "")?;
            items = client.list_items()?;
            created.uuid
        }
        Err(err) => return Err(err.into()),
    };

    let (mut fs_entries, ignored) = fs_side
        .snapshot()
        .with_context(|| format!("reading {}", fs_side.label()))?;
    let mut snapshot = sync::remote_snapshot(&items, &root_uuid);
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

    if dry_run {
        // The plan *is* the output in dry-run mode.
        plan.actions
            .iter()
            .for_each(|action| println!("{}", sync::describe(action)));
        if plan.actions.is_empty() {
            info(cli, "Already in sync; nothing to do.");
        }
        return Ok(());
    }

    let mut folders = snapshot.folders.clone();
    let outcome = sync::execute(
        client,
        &**progress,
        fs_side,
        &plan,
        &mut folders,
        &mut state,
        state_path,
    )?;

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
            "Sync complete: {} uploaded, {} updated, {} downloaded, {} deleted \
             ({} local / {} device), {} folder(s) created, {} emptied folder(s) \
             removed, {} conflict(s), {} skipped, {} unsupported file(s) ignored.",
            outcome.uploaded,
            outcome.updated,
            outcome.downloaded,
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
            client
                .restart_xochitl()
                .context("sync wrote changes, but restarting xochitl failed")?;
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

    // Side "A" is the local side when exactly one side is local,
    // otherwise the first argument's side. The archive itself lives
    // on this machine either way, keyed order-independently.
    let a_is_src = src_local || !dst_local;
    let state_path = sync::sync_state_path(&src_id, &dst_id);
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

    // Create write-target roots; a missing pure source stays an error.
    match mode {
        Mode::Push => b.ensure_root()?,
        Mode::Pull => a.ensure_root()?,
        Mode::TwoWay => {
            a.ensure_root()?;
            b.ensure_root()?;
        }
    }

    let (a_entries, a_ignored) = a
        .snapshot()
        .with_context(|| format!("reading {}", a.label()))?;
    let (b_entries, b_ignored) = b
        .snapshot()
        .with_context(|| format!("reading {}", b.label()))?;
    let mut state = sync::SyncState::load(&state_path)?;
    warn_legacy_state(cli, &*a);
    let plan = sync::plan_files(options, &a_entries, &b_entries, &state);

    info(cli, format!("A = {}, B = {}", a.label(), b.label()));
    if dry_run {
        plan.iter()
            .for_each(|action| println!("{}", sync::describe_file(action)));
        if plan.is_empty() {
            info(cli, "Already in sync; nothing to do.");
        }
        return Ok(());
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
        // Handled by run_sync, never reaches run_command.
        Command::Sync { .. } => unreachable!("sync is dispatched in main"),
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
