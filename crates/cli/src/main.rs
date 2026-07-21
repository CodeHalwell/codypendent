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
//!
//! Phase 5 STEP 5.1 adds workflow-manifest validation:
//!
//! ```text
//! codypendent workflow validate path/to/workflow.yaml
//! ```
//!
//! Phase 6 STEP 6.1 adds plugin inspection and permission-diffing:
//!
//! ```text
//! codypendent plugin inspect path/to/plugin.toml
//! codypendent plugin diff installed.toml update.toml
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
    /// Force a theme for the interactive TUI, overriding automatic terminal
    /// detection (`NO_COLOR`/`COLORTERM`/`TERM`) and any `CODYPENDENT_THEME`
    /// env var — a manual override always wins (STEP 6.6). Accepts a
    /// built-in variant (`dark`, `light`, `high-contrast`, `color-blind-safe`,
    /// `ansi256`, `ansi16`, `monochrome`) or the id of a theme pack loaded
    /// from `<data-dir>/themes/<id>.toml`. Only meaningful for the bare
    /// `codypendent` invocation, which is the only one that renders a
    /// themed UI.
    #[arg(long)]
    theme: Option<String>,
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
        /// The last sequence already seen: replay resumes at the *next* event
        /// (an exclusive cursor). Omit to replay the full retained history —
        /// or a snapshot — from the beginning of what the daemon still holds.
        #[arg(long = "from-sequence")]
        from_sequence: Option<u64>,
        /// Output format for the event stream. `jsonl` is the only format
        /// today; the flag exists so future formats are additive.
        #[arg(long, value_enum, default_value = "jsonl")]
        events: EventsFormat,
    },
    /// Maintain the knowledge fabric's derived indexes (Phase 2).
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
    /// Work with declarative workflow manifests (Phase 5).
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
    },
    /// Inspect plugin manifests and their permissions (Phase 6).
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Expose the daemon as a Zed ACP agent over stdio (STEP 3.6). Zed's
    /// `agent_servers` config points at this; it is not meant to be run by hand.
    Acp {
        /// Repository the ACP-driven runs operate in. Defaults to the current
        /// directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Hand a session off to an IDE (STEP 3.7): print how to attach, and launch
    /// the editor if it is on `PATH`. The IDE attaches as a contributor to the
    /// same session — the run keeps going, it never restarts.
    Open {
        /// The session to open in the IDE.
        session_id: SessionId,
        /// Which IDE to open the session in.
        #[arg(long = "in", value_enum, default_value = "vscode")]
        ide: IdeArg,
        /// Repository path to open. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

/// The IDEs `codypendent open --in <IDE>` knows how to launch.
#[derive(Clone, Copy, ValueEnum)]
enum IdeArg {
    Vscode,
    Cursor,
    Zed,
}

impl IdeArg {
    /// The launcher binary and human name for this IDE.
    fn binary_and_name(self) -> (&'static str, &'static str) {
        match self {
            IdeArg::Vscode => ("code", "VS Code"),
            IdeArg::Cursor => ("cursor", "Cursor"),
            IdeArg::Zed => ("zed", "Zed"),
        }
    }
}

#[derive(Subcommand)]
enum IndexCommand {
    /// Delete the derived indexes and rebuild them from the authoritative rows.
    Rebuild,
}

#[derive(Subcommand)]
enum WorkflowCommand {
    /// Parse and compile a `workflow.yaml`, reporting the validated graph or the
    /// precise error. Structural validation only; it does not run the workflow.
    /// With `--agents`, it additionally cross-checks that every agent step's role
    /// resolves to a profile in that directory.
    Validate {
        /// Path to the workflow manifest to validate.
        file: PathBuf,
        /// Optional directory of `agent.toml` profiles to resolve step roles
        /// against (e.g. `.codypendent/agents`). When given, an agent step naming
        /// a role no profile fulfils is reported as an error.
        #[arg(long)]
        agents: Option<PathBuf>,
    },
    /// Compile a `workflow.yaml` and print its full graph (nodes, actions, edges,
    /// approvals, outputs) as a human tree, or the JSON projection with `--json`.
    Show {
        /// Path to the workflow manifest to show.
        file: PathBuf,
        /// Emit the compiled graph as JSON instead of a human tree.
        #[arg(long)]
        json: bool,
    },
    /// Start a durable workflow run from a manifest (Phase 5 STEP 5.2). Ensures a
    /// daemon, sends the manifest, and prints the new run id the daemon drives to a
    /// terminal state in the background.
    Run {
        /// Path to the workflow manifest to run.
        file: PathBuf,
        /// The typed inputs the manifest declares, as a JSON value (e.g.
        /// '{"pull_request": 7}'). Defaults to null.
        #[arg(long)]
        inputs: Option<String>,
    },
    /// Pause a running workflow run so its driver stops launching new nodes; resume
    /// it later with `workflow resume` (Phase 5 STEP 5.2).
    Pause {
        /// The durable workflow-run id (as printed by `workflow run`).
        workflow_run_id: String,
    },
    /// Resume a paused workflow run, driving it onward from where it stopped.
    Resume {
        /// The durable workflow-run id.
        workflow_run_id: String,
    },
    /// Re-drive a workflow run from a chosen node (its transitive dependents reset
    /// with it) — e.g. after fixing what made the node fail.
    Retry {
        /// The durable workflow-run id.
        workflow_run_id: String,
        /// The node id to re-drive from.
        #[arg(long)]
        node: String,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Parse a `plugin.toml` and render its identity, the capability list it
    /// requests, its resource caps, and its trust posture (signed? sandbox
    /// profile) — the "evaluate permissions" step a user sees before enabling a
    /// plugin (Phase 6 STEP 6.1). Manifest parsing only; it does not run anything.
    Inspect {
        /// Path to the plugin manifest to inspect.
        file: PathBuf,
    },
    /// Compare an installed `plugin.toml` against an update and print the
    /// permission diff, reporting whether the update expands permissions and so
    /// requires re-approval (Phase 6 STEP 6.1, exit criterion 2).
    Diff {
        /// The currently-installed manifest.
        installed: PathBuf,
        /// The candidate update manifest.
        update: PathBuf,
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
    // `--theme` wins over `CODYPENDENT_THEME`; an empty value (either source)
    // is treated as unset, matching `RuntimePaths`' own `non_empty_env` rule
    // for env overrides.
    let theme_override = cli
        .theme
        .or_else(|| std::env::var("CODYPENDENT_THEME").ok())
        .filter(|v| !v.trim().is_empty());
    let Some(command) = cli.command else {
        // Bare `codypendent`: open the TUI for the current directory's repo.
        return tui::run(&paths, std::env::current_dir()?, theme_override).await;
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
        TopCommand::Index {
            command: IndexCommand::Rebuild,
        } => commands::index_rebuild(&paths).await,
        TopCommand::Workflow { command } => match command {
            WorkflowCommand::Validate { file, agents } => {
                commands::workflow_validate(&file, agents.as_deref())
            }
            WorkflowCommand::Show { file, json } => commands::workflow_show(&file, json),
            WorkflowCommand::Run { file, inputs } => {
                commands::workflow_run(&paths, &file, inputs).await
            }
            WorkflowCommand::Pause { workflow_run_id } => {
                commands::workflow_pause(&paths, workflow_run_id).await
            }
            WorkflowCommand::Resume { workflow_run_id } => {
                commands::workflow_resume(&paths, workflow_run_id).await
            }
            WorkflowCommand::Retry {
                workflow_run_id,
                node,
            } => commands::workflow_retry(&paths, workflow_run_id, node).await,
        },
        TopCommand::Plugin { command } => match command {
            PluginCommand::Inspect { file } => commands::plugin_inspect(&file),
            PluginCommand::Diff { installed, update } => commands::plugin_diff(&installed, &update),
        },
        TopCommand::Acp { repo } => {
            let repo = match repo {
                Some(repo) => repo,
                None => std::env::current_dir()?,
            };
            codypendent_cli::acp::serve(&paths, repo).await
        }
        TopCommand::Open {
            session_id,
            ide,
            repo,
        } => {
            let repo = match repo {
                Some(repo) => repo,
                None => std::env::current_dir()?,
            };
            let (binary, name) = ide.binary_and_name();
            commands::open(&paths, session_id, binary, name, repo).await
        }
    }
}
