//! `rmu` — reMarkable utilities CLI.
//!
//! Manages documents and folders on a reMarkable tablet over SSH. All
//! configuration is passed as flags; there is no config file.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use libremarkable_utils::client::Client;
use libremarkable_utils::ssh::{
    Auth, DEFAULT_SSH_PORT, DEFAULT_SSH_USER, DEFAULT_USB_HOST, SshOptions, SshSession,
    maybe_run_askpass,
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

    /// SSH port
    #[arg(long, global = true, default_value_t = DEFAULT_SSH_PORT)]
    port: u16,

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
    /// Upload a PDF or EPUB document
    Upload {
        /// Local .pdf or .epub file
        file: PathBuf,
        /// Visible name on the device (default: file stem)
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
}

fn main() -> Result<()> {
    // Must run before argument parsing: ssh re-executes this binary as
    // an askpass helper when password auth is in use.
    maybe_run_askpass();
    let cli = Cli::parse();

    let auth = resolve_auth(&cli)?;
    let session = SshSession::new(SshOptions {
        host: cli.host.clone(),
        user: cli.user.clone(),
        port: cli.port,
        identity_file: cli.identity.clone(),
        extra_options: cli.ssh_option.clone(),
        auth,
        multiplex: !cli.no_multiplex,
    })?;
    let client = Client::new(session, cli.xochitl_dir.clone());

    let modified = run_command(&client, &cli.command)?;
    if modified {
        if cli.no_restart {
            eprintln!("note: xochitl not restarted (--no-restart); changes appear after restart");
        } else {
            client
                .restart_xochitl()
                .context("changes were written, but restarting xochitl failed")?;
        }
    }
    Ok(())
}

/// Execute a subcommand; returns whether device storage was modified.
fn run_command(client: &Client, command: &Command) -> Result<bool> {
    match command {
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
            println!("Created folder: {}/", item.visible_name);
            println!("UUID: {}", item.uuid);
            Ok(true)
        }
        Command::Upload { file, name, parent } => {
            let item = client.upload(file, parent, name.as_deref())?;
            println!("Uploaded: {}", file.display());
            println!("Visible name: {}", item.visible_name);
            println!("UUID: {}", item.uuid);
            println!("Type: {}", item.file_type.as_deref().unwrap_or("?"));
            Ok(true)
        }
        Command::Download {
            target,
            output,
            bundle,
        } => {
            let destination = client.download(target, output.as_deref(), *bundle)?;
            println!("Downloaded to: {}", destination.display());
            Ok(false)
        }
        Command::Rm { target, recursive } => {
            let deleted = client.delete(target, *recursive)?;
            println!("Deleted {} item(s):", deleted.len());
            for item in &deleted {
                println!(
                    "  {}{} [{}]",
                    item.visible_name,
                    item_suffix(item),
                    item.uuid
                );
            }
            Ok(true)
        }
        Command::Mv {
            target,
            destination,
        } => {
            let item = client.move_item(target, destination)?;
            println!("Moved: {}", item.visible_name);
            println!(
                "New parent: {}",
                if item.parent.is_empty() {
                    "(root)"
                } else {
                    &item.parent
                }
            );
            Ok(true)
        }
        Command::Rename { target, new_name } => {
            let item = client.rename(target, new_name)?;
            println!("Renamed to: {}", item.visible_name);
            Ok(true)
        }
        Command::Restart => {
            client.restart_xochitl()?;
            println!("xochitl restarted");
            Ok(false)
        }
    }
}

fn resolve_auth(cli: &Cli) -> Result<Auth> {
    if let Some(path) = &cli.password_file {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading password file {}", path.display()))?;
        return Ok(Auth::Password(
            text.lines().next().unwrap_or("").to_string(),
        ));
    }
    if cli.password {
        let prompt = format!("Password for {}@{}: ", cli.user, cli.host);
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
