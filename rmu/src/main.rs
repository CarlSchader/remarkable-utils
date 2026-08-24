//! `rmu` — reMarkable utilities CLI.

use anyhow::Result;
use clap::{Parser, Subcommand};
use libremarkable_utils::Device;
use libremarkable_utils::device::{DEFAULT_SSH_USER, DEFAULT_USB_HOST};

#[derive(Parser)]
#[command(name = "rmu", about = "Utilities for the reMarkable tablet", version)]
struct Cli {
    /// Hostname or IP of the tablet.
    #[arg(long, global = true, default_value = DEFAULT_USB_HOST)]
    host: String,

    /// SSH user on the tablet.
    #[arg(long, global = true, default_value = DEFAULT_SSH_USER)]
    user: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the resolved device connection info as JSON.
    Info,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let device = Device {
        host: cli.host,
        user: cli.user,
    };

    match cli.command {
        Command::Info => {
            println!("{}", serde_json::to_string_pretty(&device)?);
        }
    }

    Ok(())
}
