//! `codypendent` — CLI entry point.
//!
//! Phase 0 surface:
//!
//! ```text
//! codypendent daemon start
//! codypendent daemon status [--json]
//! codypendent daemon stop
//! ```

mod client;
mod commands;

use clap::{Parser, Subcommand};
use codypendent_protocol::discovery::RuntimePaths;

#[derive(Parser)]
#[command(
    name = "codypendent",
    version,
    about = "Codypendent — the local-first agentic developer environment"
)]
struct Cli {
    #[command(subcommand)]
    command: TopCommand,
}

#[derive(Subcommand)]
enum TopCommand {
    /// Manage the codypendentd daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Start the daemon if it is not already running.
    Start,
    /// Ask a running daemon to shut down gracefully.
    Stop,
    /// Show daemon status. Exit code 0 when running, 1 when not.
    Status {
        /// Print machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let paths = RuntimePaths::resolve()?;
    match cli.command {
        TopCommand::Daemon { command } => match command {
            DaemonCommand::Start => commands::start(&paths).await,
            DaemonCommand::Stop => commands::stop(&paths).await,
            DaemonCommand::Status { json } => commands::status(&paths, json).await,
        },
    }
}
