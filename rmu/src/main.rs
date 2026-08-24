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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use libremarkable_utils::client::Client;
use libremarkable_utils::progress::{NoProgress, Progress};
use libremarkable_utils::ssh::{
    Auth, DEFAULT_SSH_USER, DEFAULT_USB_HOST, SshOptions, SshSession, maybe_run_askpass,
};
use libremarkable_utils::sync::{self, Direction, Endpoint};
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
    /// Upload a document (.pdf, .epub, .rmdoc; .md/.txt convert to EPUB)
    Upload {
        /// Local .pdf, .epub, .rmdoc, .md, or .txt file
        file: PathBuf,
        /// Visible name on the device (default: bundle name or file stem)
        #[arg(short, long)]
        name: Option<String>,
        /// Destination folder (UUID or logical path); default: root
        #[arg(long, default_value = "")]
        parent: String,
    },
    /// Download a document (notebooks download as .rmdoc bundles)
    Download {
        /// Document UUID or logical path
        target: String,
        /// Local file path or directory (default: current directory)
        output: Option<PathBuf>,
        /// Force an .rmdoc bundle (raw file set incl. annotations)
        /// even for PDFs/EPUBs
        #[arg(long)]
        bundle: bool,
    },
    /// Delete a document or folder
    Rm {
        /// Item UUID or logical path
        target: String,
        /// Delete non-empty folders recursively
        #[arg(short, long)]
        recursive: bool,
    },
    /// Move an item into another folder
    Mv {
        /// Item UUID or logical path
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
    /// Restart the xochitl UI service on the device
    Restart,
    /// Sync a folder with the tablet, one-way SRC -> DST (scp-style
    /// endpoints: `[user@]host:path` is remote, resolved via ssh config)
    Sync {
        /// Source endpoint, e.g. `./books` or `remarkable:/Books`
        src: String,
        /// Destination endpoint
        dst: String,
        /// Print the plan without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Override remote endpoint auto-detection
        #[arg(long, value_enum)]
        remote_kind: Option<RemoteKindArg>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RemoteKindArg {
    /// A reMarkable tablet (skip the probe)
    Remarkable,
    /// A generic ssh filesystem host (not yet supported)
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

/// `rmu sync SRC DST` — see `docs/sync-design.md`.
fn run_sync(cli: &Cli, progress: Arc<dyn Progress>) -> Result<()> {
    let Command::Sync {
        src,
        dst,
        dry_run,
        remote_kind,
    } = &cli.command
    else {
        unreachable!("run_sync is only called for the sync command");
    };

    // Direction from argument order, endpoint kinds from parsing.
    let (direction, local_arg, remote) =
        match (sync::parse_endpoint(src), sync::parse_endpoint(dst)) {
            (Endpoint::Local(local), Endpoint::Remote { destination, path }) => {
                (Direction::Push, local, (destination, path))
            }
            (Endpoint::Remote { destination, path }, Endpoint::Local(local)) => {
                (Direction::Pull, local, (destination, path))
            }
            (Endpoint::Local(_), Endpoint::Local(_)) => {
                bail!("both endpoints are local; local↔local sync is not supported yet")
            }
            (Endpoint::Remote { .. }, Endpoint::Remote { .. }) => {
                bail!("both endpoints are remote; tablet↔tablet sync is not supported yet")
            }
        };
    let (destination, remote_path) = remote;
    let local_root = PathBuf::from(&local_arg);

    match direction {
        Direction::Push if !local_root.is_dir() => {
            bail!("source directory not found: {}", local_root.display())
        }
        Direction::Pull => std::fs::create_dir_all(&local_root)?,
        _ => {}
    }

    let session = make_session(cli, &destination)?;
    match remote_kind {
        Some(RemoteKindArg::Fs) => {
            bail!(
                "generic ssh filesystem endpoints are not supported yet (see docs/sync-design.md)"
            )
        }
        Some(RemoteKindArg::Remarkable) => {}
        None => {
            progress.step(&format!("Probing {destination}"));
            if !sync::probe_remarkable(&session, &cli.xochitl_dir)? {
                bail!(
                    "{destination} does not look like a reMarkable tablet \
                     (generic hosts are not supported yet; --remote-kind remarkable to override)"
                );
            }
        }
    }
    let client = Client::new(session, cli.xochitl_dir.clone()).with_progress(progress.clone());

    // Resolve the device folder; on push, create it if missing.
    let mut items = client.list_items()?;
    let root_uuid = match xochitl::resolve_folder_ref(&items, &remote_path) {
        Ok(uuid) => uuid,
        Err(libremarkable_utils::Error::PathNotFound(_)) if direction == Direction::Push => {
            let created = client.mkdir_path(&remote_path, "")?;
            items = client.list_items()?;
            created.uuid
        }
        Err(err) => return Err(err.into()),
    };

    let (local_entries, ignored) = sync::local_snapshot(&local_root)?;
    let snapshot = sync::remote_snapshot(&items, &root_uuid);
    let mut state = sync::SyncState::load(&local_root)?;
    let plan = sync::plan(direction, &local_entries, &snapshot, &state);

    if *dry_run {
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
        &client,
        &*progress,
        &local_root,
        &plan,
        &mut folders,
        &mut state,
    )?;

    outcome
        .skipped
        .iter()
        .for_each(|(path, reason)| info(cli, format!("skipped {path}: {reason}")));
    info(
        cli,
        format!(
            "Sync complete: {} uploaded, {} updated, {} downloaded, {} folder(s) created, \
             {} skipped, {} unsupported file(s) ignored.",
            outcome.uploaded,
            outcome.updated,
            outcome.downloaded,
            outcome.folders_created,
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
            let destination = client.download(target, output.as_deref(), *bundle)?;
            info(cli, "Downloaded to:");
            println!("{}", destination.display());
            Ok(false)
        }
        Command::Rm { target, recursive } => {
            let deleted = client.delete(target, *recursive)?;
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
            let item = client.move_item(target, destination)?;
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
            Ok(true)
        }
        Command::Rename { target, new_name } => {
            let item = client.rename(target, new_name)?;
            info(cli, format!("Renamed to: {}", item.visible_name));
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
