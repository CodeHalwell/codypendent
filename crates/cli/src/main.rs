//! `codypendent` — CLI entry point.
//!
//! Phase 0 surface:
//!
//! ```text
//! codypendent daemon start
//! codypendent daemon status [--json]
//! codypendent daemon stop
//! ```
//!
//! STEP 1.13 adds the headless JSONL client:
//!
//! ```text
//! codypendent run --objective "..." [--mode build] [--repo PATH] --jsonl
//! codypendent attach <SESSION_ID> [--from-sequence N] --events jsonl
//! ```
//!
//! STEP 1.12 makes the bare invocation open the interactive TUI:
//!
//! ```text
//! codypendent            # opens the TUI for the current repository's session
//! ```

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use codypendent_cli::{commands, tui};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{AgentMode, SessionId};

#[derive(Parser)]
#[command(
    name = "codypendent",
    version,
    about = "Codypendent — the local-first agentic developer environment"
)]
struct Cli {
    /// With no subcommand, `codypendent` opens the interactive TUI attached to
    /// the current repository's session (STEP 1.12).
    #[command(subcommand)]
    command: Option<TopCommand>,
}

#[derive(Subcommand)]
enum TopCommand {
    /// Manage the codypendentd daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Start a headless run and stream its events (STEP 1.13).
    Run {
        /// What the agent should do.
        #[arg(long)]
        objective: String,
        /// The mode preset the run starts in (Chapter 20).
        #[arg(long, value_enum, default_value = "build")]
        mode: ModeArg,
        /// Repository the run operates in. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Stream every session event to stdout as JSONL until the run
        /// terminates. Currently required — interactive attach lands with
        /// the TUI (STEP 1.12).
        #[arg(long)]
        jsonl: bool,
    },
    /// Attach to an existing session and stream its events (STEP 1.13).
    Attach {
        /// The session to attach to.
        session_id: SessionId,
        /// Replay from this sequence onward. Defaults to a live tail (no
        /// catch-up replay beyond what the daemon already retains).
        #[arg(long = "from-sequence")]
        from_sequence: Option<u64>,
        /// Output format for the event stream. `jsonl` is the only format
        /// today; the flag exists so future formats are additive.
        #[arg(long, value_enum, default_value = "jsonl")]
        events: EventsFormat,
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

/// CLI-facing mirror of [`AgentMode`] so `clap` can derive `--mode`'s parser
/// and `--help` text without teaching the wire protocol crate about `clap`.
#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Ask,
    Explore,
    Plan,
    Build,
    Review,
}

impl From<ModeArg> for AgentMode {
    fn from(mode: ModeArg) -> Self {
        match mode {
            ModeArg::Ask => AgentMode::Ask,
            ModeArg::Explore => AgentMode::Explore,
            ModeArg::Plan => AgentMode::Plan,
            ModeArg::Build => AgentMode::Build,
            ModeArg::Review => AgentMode::Review,
        }
    }
}

/// `codypendent attach --events <FORMAT>`. Only `jsonl` exists today; a
/// dedicated enum keeps room for future formats without a breaking CLI change.
#[derive(Clone, Copy, ValueEnum)]
enum EventsFormat {
    Jsonl,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let paths = RuntimePaths::resolve()?;
    let Some(command) = cli.command else {
        // Bare `codypendent`: open the TUI for the current directory's repo.
        return tui::run(&paths, std::env::current_dir()?).await;
    };
    match command {
        TopCommand::Daemon { command } => match command {
            DaemonCommand::Start => commands::start(&paths).await,
            DaemonCommand::Stop => commands::stop(&paths).await,
            DaemonCommand::Status { json } => {
                // `status` returns the running-state; the exit-1-when-not-running
                // decision lives here (the only place `std::process::exit` runs).
                let running = commands::status(&paths, json).await?;
                if running {
                    Ok(())
                } else {
                    std::process::exit(1);
                }
            }
        },
        TopCommand::Run {
            objective,
            mode,
            repo,
            jsonl,
        } => {
            let repo = match repo {
                Some(repo) => repo,
                None => std::env::current_dir()?,
            };
            let exit_code = commands::run(&paths, objective, mode.into(), repo, jsonl).await?;
            std::process::exit(exit_code);
        }
        TopCommand::Attach {
            session_id,
            from_sequence,
            events: EventsFormat::Jsonl,
        } => commands::attach(&paths, session_id, from_sequence).await,
    }
}
