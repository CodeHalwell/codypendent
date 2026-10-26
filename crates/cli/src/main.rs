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
//! codypendent run --objective "..." [--mode build] [--repo PATH] [--jsonl]
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

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use codypendent_cli::{commands, theme_select, tui};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{AgentMode, DocumentId, SessionId};

#[derive(Parser)]
#[command(
    name = "codypendent",
    version,
    propagate_version = true,
    about = "Codypendent — the local-first agentic developer environment",
    after_help = "Examples:\n  \
        codypendent                                  open the interactive TUI in this repository\n  \
        codypendent run \"fix the failing test\" --jsonl   headless run, events as JSONL\n  \
        codypendent doctor                           diagnose the local setup\n  \
        codypendent models add openai gpt-5.4        configure a hosted model\n  \
        codypendent daemon status                    is the daemon up, and which build?"
)]
struct Cli {
    /// Start in this directory, as if launched there. Useful for IDE tasks,
    /// terminal launchers, and opening a repository from another directory.
    /// Relative command arguments are resolved from this directory.
    #[arg(
        short = 'C',
        long = "working-directory",
        global = true,
        value_name = "PATH"
    )]
    launch_directory: Option<PathBuf>,
    /// Force a theme for the interactive TUI, overriding automatic terminal
    /// detection (`NO_COLOR`/`COLORTERM`/`TERM`) and any `CODYPENDENT_THEME`
    /// env var — a manual override always wins. Accepts a
    /// built-in variant (`dark`, `light`, `high-contrast`, `color-blind-safe`,
    /// `ansi256`, `ansi16`, `monochrome`) or the id of a theme pack loaded
    /// from `<data-dir>/themes/<id>.toml`. Only meaningful for the bare
    /// `codypendent` invocation, which is the only one that renders a
    /// themed UI.
    #[arg(long, global = true)]
    theme: Option<String>,
    /// Use the cooked, line-oriented accessible client for the bare invocation.
    /// No raw mode, alternate screen, mouse capture, colour, or Unicode chrome
    /// is emitted. `--plain` is an alias for scripts and limited terminals.
    #[arg(long, visible_alias = "plain", global = true)]
    accessible: bool,
    /// With no subcommand, `codypendent` opens the interactive TUI attached to
    /// the current repository's session.
    #[command(subcommand)]
    command: Option<TopCommand>,
}

#[derive(Subcommand)]
enum TopCommand {
    /// Run the daemon in-process. `codypendent __daemon` *is* the daemon — the
    /// hidden self-spawn target `ensure_daemon` launches as `current_exe
    /// __daemon`, so an updated `codypendent` always runs a matching daemon.
    /// Hidden from `--help`; behaves exactly like the standalone `codypendentd`.
    #[command(name = "__daemon", hide = true)]
    InternalDaemon,
    #[command(name = "__desktop_tools_mcp", hide = true)]
    InternalDesktopToolsMcp {
        #[arg(long)]
        socket: PathBuf,
    },
    /// Manage the codypendentd daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Start a headless run and stream its events — the scriptable twin of
    /// the interactive TUI.
    Run {
        /// What the agent should do (positional or `--objective`).
        #[arg(value_name = "PROMPT")]
        prompt: Option<String>,
        /// What the agent should do (flag form). Mutually exclusive with the
        /// positional prompt: two objectives that disagree must be refused, not
        /// silently resolved in favour of one of them. One of the two is
        /// required — clap refuses an objective-less `run` at parse time with
        /// a usage line, instead of a bare runtime error.
        #[arg(long, conflicts_with = "prompt", required_unless_present = "prompt")]
        objective: Option<String>,
        /// The mode preset the run starts in: how much the agent may change
        /// without asking.
        #[arg(long, value_enum, default_value = "build")]
        mode: ModeArg,
        /// Repository the run operates in. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Pin the run to this configured model id (as it appears in
        /// `codypendent models list`), e.g. `openai/gpt-5.4` or `acp/cursor`.
        /// Without it, model selection falls to routing (if enabled) or the
        /// resolver's first reachable candidate in `models.toml` FILE ORDER —
        /// with several models configured, which one that is is not obvious
        /// from the command line alone. A pin overrides quality/routing
        /// choice, never the security ceiling: a pinned hosted model for
        /// classified data is still refused (fail-closed), same as the TUI's
        /// `/model` picker.
        #[arg(long)]
        model: Option<String>,
        /// Stream every session event to stdout as JSONL until the run
        /// terminates. `--json` is accepted as an alias. Without this flag,
        /// stdout streams the assistant's answer as text; progress goes to stderr.
        #[arg(long, visible_alias = "json")]
        jsonl: bool,
    },
    /// Attach to an existing session and stream its events as JSONL.
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
    /// Maintain the derived search indexes over the knowledge fabric —
    /// full-text (BM25) and vectors. This is SEARCH, not the code graph:
    /// use `codypendent graph build` for the code graph.
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
    /// Build and inspect this repository's code graph — the symbol/reference
    /// map the agent reasons over. `graph build` folds it on demand and reports
    /// why it came out the size it did; `graph status` describes what is
    /// stored; `graph show` lists it.
    Graph {
        #[command(subcommand)]
        command: GraphCommand,
    },
    /// Install skill packages into the governed registry, so retrieval can
    /// disclose them to a run.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Validate, start, and supervise declarative workflow manifests.
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
    },
    /// Investigate and repair a failed GitHub check on a pull request (`/fix-ci`).
    /// Runs the declarative `repair-github-check` workflow — the supervised
    /// investigator → implementer → independent-reviewer flow — through the
    /// daemon. Every GitHub write parks for your approval first.
    FixCi {
        /// The pull-request number whose failing check to repair.
        #[arg(long)]
        pr: u64,
        /// Repository the repair runs against (its agent nodes each get an
        /// isolated worktree). Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Write, list, and publish collaborative documents; `docs publish`
    /// commits one to Git.
    Docs {
        #[command(subcommand)]
        command: DocsCommand,
    },
    /// Score the agent against a suite of fixture cases and report the result.
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
    /// Add, list, verify, and benchmark the models this install can run.
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
    // ADR-010: promotion is operator-gated by design. Nothing the agent
    // learns can promote itself, however well it scores.
    /// Move something the agent learned — a prompt, a skill, a policy — through
    /// the evaluation gate into general use. Promotion always needs a human.
    Promote {
        #[command(subcommand)]
        command: PromoteCommand,
    },
    /// Inspect what a plugin declares and which permissions it asks for,
    /// before you install it.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Inspect the MCP servers you have declared, and the tools they offer.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Manage, inspect, and approve/reject lifecycle hooks.
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// Register the inbound webhook endpoints the listener will accept
    /// deliveries for, and rotate or retire their signing keys.
    Webhook {
        #[command(subcommand)]
        command: WebhookCommand,
    },
    /// Connect external ACP agents, or expose Codypendent itself as an ACP agent.
    Acp {
        #[command(subcommand)]
        command: Option<AcpCommand>,
        /// Backward-compatible repository for `codypendent acp` (serve mode).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Create and run durable multi-provider agent councils.
    Council {
        #[command(subcommand)]
        command: CouncilCommand,
    },
    /// Choose each task's model from measured benchmarks instead of the
    /// default order in `models.toml`. Reads and writes
    /// `<data_dir>/routing.toml`; off unless you turn it on.
    Routing {
        #[command(subcommand)]
        command: RoutingCommand,
    },
    /// Hand a session off to an IDE: print how to attach, and launch the
    /// editor if it is on `PATH`. The IDE joins the same session as a
    /// contributor — the run keeps going, it never restarts.
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
    /// Print a shell-completion script for your shell. Install e.g. with:
    /// `codypendent completion zsh > ~/.zfunc/_codypendent` (zsh),
    /// `codypendent completion bash > /etc/bash_completion.d/codypendent`, or
    /// `codypendent completion fish > ~/.config/fish/completions/codypendent.fish`.
    Completion {
        /// The shell to generate completions for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Diagnose the local setup: binary + build id, daemon health, runtime
    /// paths, model config, and provider reachability. Read-only. Exits
    /// non-zero if any check FAILS.
    Doctor {
        /// Emit a structured JSON report instead of the human checklist.
        #[arg(long)]
        json: bool,
        /// Also verify hosted providers, not just local model servers: an
        /// authenticated `GET <base_url>/models` per configured hosted model,
        /// using its resolved credentials — a real reachability + auth check,
        /// not a bare TCP connect.
        #[arg(long)]
        deep: bool,
    },
    /// Self-update: install the latest GitHub release over the running binary
    /// (via `gh`, exactly as `install.sh`), then pick up the new build through
    /// the idle-guarded daemon auto-restart — never kills an active run.
    Update {
        /// Only report whether an update is available; download nothing.
        #[arg(long)]
        check: bool,
        /// Install a specific release tag instead of the latest.
        tag: Option<String>,
    },
    /// Install a companion surface from the same GitHub release the CLI comes
    /// from: the desktop app, or the editor extension. Uses `gh` exactly as
    /// `codypendent update` does — which is what makes an UNSIGNED macOS
    /// bundle actually installable, since `gh` never sets the quarantine
    /// attribute a browser download would.
    Install {
        /// Which surface to install.
        #[command(subcommand)]
        command: InstallCommand,
    },
    /// Scaffold and check a local Unsloth QLoRA fine-tuning project.
    /// Subprocess orchestration only — no Python in this workspace; the
    /// scaffolded project is a standalone Python project you run yourself.
    Finetune {
        #[command(subcommand)]
        command: FinetuneCommand,
    },
    /// Manage approvals and learned approval rules.
    Approvals {
        #[command(subcommand)]
        command: ApprovalsCommand,
    },
    /// Browse, install, update, enable, disable, and revoke marketplace packages.
    Marketplace {
        #[command(subcommand)]
        command: MarketplaceCommand,
    },
    /// Manage brokered secrets, context-bound leases, and audit logs.
    #[command(alias = "secrets")]
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Search the Session Library and manage what it finds: rename, pin,
    /// archive, restore, delete, or export a session. Everything here is
    /// scoped daemon-side to the sessions this user owns.
    #[command(alias = "sessions")]
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Export and import versioned, redacted session/support bundles.
    ///
    /// A bundle is data interchange, not a backup: it carries no credential
    /// value and an import never restores credentials or authority.
    #[command(alias = "bundles")]
    Bundle {
        #[command(subcommand)]
        command: BundleCommand,
    },
    /// Create and manage owner-scoped spend/usage budgets.
    ///
    /// A budget is what makes the daemon's threshold evaluator do anything: it
    /// is evaluated at every run terminal against the MEASURED observations in
    /// its window, and a crossing files a `BudgetWarning` in the inbox. Every
    /// budget here belongs to the connection's own principal — there is no way
    /// to name someone else's.
    #[command(alias = "budgets")]
    Budget {
        #[command(subcommand)]
        command: BudgetCommand,
    },
}

#[derive(Subcommand)]
enum BudgetCommand {
    /// Create a budget over a measured dimension.
    Create {
        /// The measured dimension the threshold applies to. Only dimensions the
        /// daemon actually measures are expressible: a budget over an unmeasured
        /// one would have to read absent as zero to decide anything.
        #[arg(long, value_enum)]
        dimension: BudgetDimensionArg,
        /// The rolling window the threshold is measured over.
        #[arg(long, value_enum)]
        window: BudgetWindowArg,
        /// Strictly positive threshold, in the dimension's own unit (micros for
        /// cost, tokens for tokens, milliseconds for latency).
        #[arg(long)]
        threshold: u64,
        /// What the budget covers. Defaults to everything this principal runs.
        #[arg(long, value_enum, default_value = "owner")]
        scope: BudgetScopeArg,
        /// The repository/workflow/model id the scope narrows to. Required for
        /// every scope except `owner`, and rejected for `owner`.
        #[arg(long)]
        scope_value: Option<String>,
        /// Create the budget switched off. It is then stored but evaluates
        /// nothing until `budget update --enable`.
        #[arg(long)]
        disabled: bool,
        /// Emit the daemon's stored budget as JSON instead of the table row.
        #[arg(long)]
        json: bool,
    },
    /// List the budgets you own.
    List {
        /// Only enabled budgets.
        #[arg(long, conflicts_with = "disabled")]
        enabled: bool,
        /// Only disabled budgets.
        #[arg(long)]
        disabled: bool,
        /// Ask for at most this many rows. The daemon still applies its own cap.
        #[arg(long)]
        limit: Option<u32>,
        /// Emit the raw page as JSON instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Show one budget by id.
    Get {
        budget_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Change a budget's threshold and/or whether it is evaluated. Scope,
    /// dimension and window are immutable — they are the row's identity, so
    /// changing one is a delete plus a create.
    Update {
        budget_id: String,
        /// The new threshold.
        #[arg(long)]
        threshold: Option<u64>,
        /// Start evaluating this budget.
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Stop evaluating this budget without deleting it.
        #[arg(long)]
        disable: bool,
        #[arg(long)]
        json: bool,
    },
    /// Delete a budget. Its inbox warnings are unaffected: they record
    /// something that really happened.
    Delete {
        budget_id: String,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum BudgetDimensionArg {
    CostMicros,
    InputTokens,
    OutputTokens,
    LatencyMs,
}

#[derive(Clone, Copy, ValueEnum)]
enum BudgetWindowArg {
    Day,
    Week,
    Month,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum BudgetScopeArg {
    Owner,
    Repository,
    Workflow,
    Model,
}

impl BudgetDimensionArg {
    fn to_wire(self) -> codypendent_protocol::AnalyticsBudgetDimension {
        use codypendent_protocol::AnalyticsBudgetDimension as Dim;
        match self {
            Self::CostMicros => Dim::CostMicros,
            Self::InputTokens => Dim::InputTokens,
            Self::OutputTokens => Dim::OutputTokens,
            Self::LatencyMs => Dim::LatencyMs,
        }
    }
}

impl BudgetWindowArg {
    fn to_wire(self) -> codypendent_protocol::AnalyticsBudgetWindow {
        use codypendent_protocol::AnalyticsBudgetWindow as Window;
        match self {
            Self::Day => Window::Day,
            Self::Week => Window::Week,
            Self::Month => Window::Month,
        }
    }
}

impl BudgetScopeArg {
    /// Pair the scope with the value that gives it meaning, exactly as the wire
    /// type does. A narrowed scope with no value, or `owner` with one, is
    /// refused HERE rather than sent: the wire type cannot express either, so
    /// guessing would silently create a budget over the wrong thing.
    fn to_wire(
        self,
        scope_value: Option<String>,
    ) -> anyhow::Result<codypendent_protocol::AnalyticsBudgetScope> {
        use codypendent_protocol::AnalyticsBudgetScope as Scope;
        match (self, scope_value) {
            (Self::Owner, None) => Ok(Scope::Owner),
            (Self::Owner, Some(_)) => {
                anyhow::bail!("--scope owner covers everything you run and takes no --scope-value")
            }
            (Self::Repository, Some(repository_id)) => Ok(Scope::Repository { repository_id }),
            (Self::Workflow, Some(workflow_id)) => Ok(Scope::Workflow { workflow_id }),
            (Self::Model, Some(model_id)) => Ok(Scope::Model { model_id }),
            (_, None) => {
                anyhow::bail!("--scope-value is required for every --scope except owner")
            }
        }
    }
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Ranked search across every session you own — titles, transcripts, tool
    /// observations, patches, artifacts, changed paths, and symbols.
    Search {
        /// What to search for. An empty query lists the most recent sessions.
        #[arg(default_value = "")]
        query: String,
        /// Ask for at most this many hits. The daemon still applies its own cap.
        #[arg(long)]
        limit: Option<u32>,
        /// Continue from a previous page: the opaque token the last page
        /// printed (or `next_cursor` in `--json` output).
        #[arg(long, value_name = "TOKEN")]
        cursor: Option<String>,
        /// Emit the raw search page as JSON instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Give a session a new title.
    Rename {
        session_id: SessionId,
        /// The new title.
        title: String,
    },
    /// Pin a session to the top of the library.
    Pin { session_id: SessionId },
    /// Remove a session's pin.
    Unpin { session_id: SessionId },
    /// Archive a session: it leaves the default library view but is kept.
    Archive { session_id: SessionId },
    /// Restore an archived session to the default view.
    Restore { session_id: SessionId },
    /// Delete a session. The DAEMON decides whether that means a purge or a
    /// tombstone — it remains the retention authority and may refuse a mode
    /// rather than weaken retention.
    Delete {
        session_id: SessionId,
        /// Ask for a tombstone rather than the configured retention policy.
        #[arg(long)]
        tombstone_only: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Export one session's transcript to a file.
    Export {
        session_id: SessionId,
        /// Where to write the export.
        #[arg(long)]
        out: PathBuf,
        /// Export format.
        #[arg(long, value_enum, default_value = "markdown")]
        format: SessionExportFormatArg,
        /// Include artifact bodies. Off by default: an export widens what
        /// leaves the daemon, so nothing extra is included unless asked for.
        #[arg(long)]
        include_artifacts: bool,
        /// Include internal (council member, workflow node) child sessions.
        #[arg(long)]
        include_internal: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SessionExportFormatArg {
    Json,
    Markdown,
}

#[derive(Subcommand)]
enum BundleCommand {
    /// Export a bundle. Every category is opt-in: with no `--include-*` flag
    /// the archive carries only its manifest.
    Export {
        /// Sessions to include. Repeat for several; omit for a support bundle
        /// with no session material.
        #[arg(long = "session")]
        sessions: Vec<SessionId>,
        /// Where to write the archive.
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        include_transcripts: bool,
        #[arg(long)]
        include_routing: bool,
        #[arg(long)]
        include_approvals: bool,
        #[arg(long)]
        include_artifact_manifests: bool,
        #[arg(long)]
        include_patches: bool,
        #[arg(long)]
        include_diagnostics: bool,
        /// Redaction policy. `support-safe` additionally omits artifact bodies.
        #[arg(long, value_enum, default_value = "standard")]
        redaction: BundleRedactionArg,
    },
    /// Import a bundle archive.
    Import {
        /// The archive to import.
        file: PathBuf,
        /// What to do when an identity in the bundle already exists locally.
        #[arg(long, value_enum, default_value = "remap")]
        collision: BundleCollisionArg,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum BundleRedactionArg {
    Standard,
    SupportSafe,
}

#[derive(Clone, Copy, ValueEnum)]
enum BundleCollisionArg {
    Reject,
    Remap,
    Skip,
}

impl SessionExportFormatArg {
    fn to_wire(self) -> codypendent_protocol::SessionExportFormat {
        match self {
            Self::Json => codypendent_protocol::SessionExportFormat::Json,
            Self::Markdown => codypendent_protocol::SessionExportFormat::Markdown,
        }
    }
}

impl BundleRedactionArg {
    fn to_wire(self) -> codypendent_protocol::BundleRedactionPolicy {
        match self {
            Self::Standard => codypendent_protocol::BundleRedactionPolicy::Standard,
            Self::SupportSafe => codypendent_protocol::BundleRedactionPolicy::SupportSafe,
        }
    }
}

impl BundleCollisionArg {
    fn to_wire(self) -> codypendent_protocol::BundleCollisionPolicy {
        match self {
            Self::Reject => codypendent_protocol::BundleCollisionPolicy::Reject,
            Self::Remap => codypendent_protocol::BundleCollisionPolicy::Remap,
            Self::Skip => codypendent_protocol::BundleCollisionPolicy::Skip,
        }
    }
}

#[derive(Subcommand)]
enum MarketplaceCommand {
    /// Search for packages in the marketplace catalog.
    Search {
        /// Search query text (or * for all).
        query: String,
        /// Maximum number of results.
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Install a marketplace package.
    Install {
        /// The package id.
        package_id: String,
        /// Path to package manifest TOML.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Path to package artifact (.tgz).
        #[arg(long)]
        artifact: Option<PathBuf>,
        /// Allow installing unsigned package.
        #[arg(long)]
        allow_unsigned: bool,
    },
    /// Update an installed marketplace package.
    Update {
        /// The package id.
        package_id: String,
        /// Path to updated package manifest TOML.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Path to updated package artifact (.tgz).
        #[arg(long)]
        artifact: Option<PathBuf>,
        /// Allow unsigned update.
        #[arg(long)]
        allow_unsigned: bool,
    },
    /// Enable an installed marketplace package.
    Enable {
        /// The package id.
        package_id: String,
        /// Optional scope.
        #[arg(long)]
        scope: Option<String>,
        /// Optional session id to scope enablement to.
        #[arg(long)]
        session: Option<SessionId>,
    },
    /// Disable an active marketplace package.
    Disable {
        /// The package id.
        package_id: String,
    },
    /// Revoke a marketplace package.
    Revoke {
        /// The package id.
        package_id: String,
        /// Revocation reason.
        #[arg(long, default_value = "operator-revoked")]
        reason: String,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Declare an opaque secret reference.
    Declare {
        /// The secret reference name (e.g. github_token).
        name: String,
        /// The secret backend.
        #[arg(long, value_enum, default_value = "environment")]
        backend: SecretBackendArg,
        /// Backend-specific locator (e.g. env var name or vault path).
        #[arg(long)]
        locator: String,
        /// Declared capability scope required to read this secret.
        #[arg(long)]
        capability: String,
        /// Optional organization id.
        #[arg(long)]
        org: Option<String>,
        /// Optional repository id.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Issue a context-bound lease for a declared secret reference.
    Bind {
        /// The secret reference id or name.
        reference_id: String,
        /// The job or session id binding this lease.
        #[arg(long)]
        job_id: String,
        /// The required capability.
        #[arg(long)]
        capability: String,
    },
    /// List declared secret references (metadata only).
    List {
        /// Filter by capability.
        #[arg(long)]
        capability: Option<String>,
    },
    /// Revoke a declared secret reference.
    Revoke {
        /// The secret reference id.
        reference_id: String,
        /// Revocation reason.
        #[arg(long, default_value = "operator-revoked")]
        reason: String,
    },
}

#[derive(Subcommand)]
enum ApprovalsCommand {
    /// Manage persisted approval rules.
    Rules {
        #[command(subcommand)]
        command: Option<ApprovalRulesCommand>,
    },
}

#[derive(Subcommand)]
enum ApprovalRulesCommand {
    /// List active and revoked approval rules.
    List,
    /// Revoke a persisted approval rule by id.
    Revoke {
        /// The approval rule ID to revoke.
        id: String,
    },
}

/// `codypendent skill new --scope <SCOPE>`: where the authored skill is
/// registered. Typed so clap validates, enumerates in `--help`, and
/// shell-completes it — the late string check it replaces refused typos only
/// after the procedure body had already been read.
#[derive(Clone, Copy, ValueEnum)]
enum SkillScopeArg {
    /// This machine's operator, across every repository.
    User,
    /// Anchored to the checkout the command runs in.
    Repository,
}

impl SkillScopeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Repository => "repository",
        }
    }
}

/// `codypendent secret declare --backend <BACKEND>`. The doc comment used to
/// enumerate five valid values clap never checked.
#[derive(Clone, Copy, ValueEnum)]
enum SecretBackendArg {
    Environment,
    Keychain,
    Managed,
    Vault,
    /// Clap's default `ValueEnum` spelling for this variant is
    /// `workload-identity` (kebab-case); the established name everywhere
    /// else — the backend implementation, the daemon's secret gate, prior
    /// CLI docs — is `workload_identity`. Pin it so `--backend
    /// workload_identity` keeps working and `--help` advertises the name
    /// that is actually documented.
    #[value(name = "workload_identity")]
    WorkloadIdentity,
}

impl SecretBackendArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Keychain => "keychain",
            Self::Managed => "managed",
            Self::Vault => "vault",
            Self::WorkloadIdentity => "workload_identity",
        }
    }
}

/// `codypendent promote propose --kind <KIND>`: the artifact kinds the
/// promotion pipeline accepts, validated at parse time instead of by the
/// daemon after a round trip.
#[derive(Clone, Copy, ValueEnum)]
enum PromoteKindArg {
    Retrieval,
    Skill,
    Prompt,
    Router,
    Workflow,
    ModelProfile,
}

impl PromoteKindArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Retrieval => "retrieval",
            Self::Skill => "skill",
            Self::Prompt => "prompt",
            Self::Router => "router",
            Self::Workflow => "workflow",
            Self::ModelProfile => "model-profile",
        }
    }
}

/// `codypendent routing enable --data-classification <LEVEL>`.
#[derive(Clone, Copy, ValueEnum)]
enum DataClassificationArg {
    Public,
    Internal,
    Confidential,
    Secret,
}

impl DataClassificationArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Confidential => "confidential",
            Self::Secret => "secret",
        }
    }
}

#[derive(Subcommand)]
enum FinetuneCommand {
    /// Scaffold an Unsloth QLoRA project: pinned requirements, a `train.py`
    /// for the chosen base model, a JSONL chat-transcript dataset stub, and a
    /// README covering GPU requirements, training, GGUF export, and the
    /// `ollama create` step. Refuses if the target directory already exists.
    Init {
        /// The Hugging Face base model to fine-tune. Defaults to a small,
        /// popular Unsloth QLoRA-ready repo.
        #[arg(long)]
        model: Option<String>,
        /// Directory to scaffold into (must not already exist). Defaults to
        /// `./finetune`.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Verify Python and CUDA are present for running the scaffolded
    /// `train.py`. Read-only; exits non-zero only when Python itself is
    /// missing (no GPU warns instead of failing — the scaffold is still
    /// useful without one, e.g. editing the dataset).
    Check {
        /// Emit a structured JSON report instead of the human checklist.
        #[arg(long)]
        json: bool,
    },
    /// Work with the fine-tuning dataset.
    Dataset {
        #[command(subcommand)]
        command: FinetuneDatasetCommand,
    },
}

#[derive(Subcommand)]
enum FinetuneDatasetCommand {
    /// Export the repo's own session/eval history into `dataset/train.jsonl`
    /// as fine-tuning data, when a clean seam exists to do so. Today: prints
    /// exactly why that seam doesn't exist yet rather than silently no-op'ing.
    Export,
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
enum InstallCommand {
    /// Install the Codypendent desktop app from the latest release.
    ///
    /// Built for Apple Silicon macOS and x86_64 Linux only; on any other
    /// platform this REFUSES rather than installing a bundle that cannot run.
    Desktop {
        /// Install from a specific release tag instead of the latest.
        tag: Option<String>,
    },
    /// Install the VS Code-family extension (`.vsix`) into an editor, through
    /// that editor's own CLI. The `.vsix` is platform-independent, so this
    /// works on every platform — including ones with no prebuilt CLI tarball.
    #[command(alias = "extension")]
    Vscode {
        /// Which editor to install into. Defaults to VS Code.
        #[arg(long = "in", value_enum, default_value = "vscode")]
        ide: IdeArg,
        /// Install from a specific release tag instead of the latest.
        tag: Option<String>,
    },
}

#[derive(Subcommand)]
enum IndexCommand {
    /// Delete the derived SEARCH indexes (full-text BM25 + vectors) and rebuild
    /// them from the authoritative rows. This does NOT build the code graph —
    /// for that, run `codypendent graph build`.
    Rebuild,
}

#[derive(Subcommand)]
enum GraphCommand {
    /// Fold this repository's code graph now, and report what the fold saw:
    /// files walked, files folded, nodes and edges written, a per-language
    /// breakdown, and — the point of the command — every file extension that
    /// produced nothing, so an empty graph explains itself instead of being a
    /// silent zero.
    #[command(visible_alias = "rebuild")]
    Build {
        /// Repository to fold. Defaults to the current directory; either way it
        /// anchors on the enclosing checkout, never on a subdirectory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Emit the report as JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Show what the stored code graph holds for this repository — counts,
    /// per-language and per-kind breakdowns, the revision it was folded at, and
    /// whether it is stale relative to the working tree. Reads only; never
    /// re-scans.
    Status {
        /// Repository to describe. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Emit the status as JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// List the graph's nodes (and optionally their edges), filtered, so the
    /// graph is inspectable from the terminal rather than only through the TUI
    /// overlay.
    Show {
        /// Repository to read. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Only nodes whose repo-relative source path starts with this prefix.
        #[arg(long)]
        path: Option<String>,
        /// Only nodes in this language, as stored (`rust`, `python`, …).
        #[arg(long)]
        language: Option<String>,
        /// Only nodes of this kind, as stored (`function`, `type`, `file`, …).
        #[arg(long)]
        kind: Option<String>,
        /// Only nodes whose qualified name contains this text.
        #[arg(long)]
        name: Option<String>,
        /// Exactly one node, by the id `graph show` prints. Scoped to this
        /// repository like every other filter: an id belonging to another
        /// checkout is refused identically to one that does not exist.
        #[arg(long = "node")]
        node_id: Option<String>,
        /// Also list the edges incident to the selected nodes.
        #[arg(long)]
        edges: bool,
        /// Follow one recorded connected neighbourhood instead of separate pages.
        #[arg(long)]
        connected: bool,
        /// Maximum rows of each kind (server-clamped).
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Emit the page as JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Validate a skill package directory, copy it under `<data_dir>/skills/`,
    /// and register it. Idempotent: re-adding the same package keeps its
    /// registry identity, and the daemon re-verifies it on its next boot.
    Add {
        /// The package directory — the one holding `skill.toml`.
        directory: PathBuf,
    },
    /// Author a new skill package and register it as a `draft` — installed and
    /// inspectable, but never disclosed to a run until a human promotes it.
    /// Validated and installed through the same pipeline `add` runs; nothing
    /// here can register an `active` skill.
    New {
        /// The skill's manifest id (also its directory name under
        /// `<data_dir>/skills/`).
        id: String,
        /// Human-readable name.
        #[arg(long)]
        name: String,
        /// One sentence on what the skill is for — what retrieval matches on.
        #[arg(long)]
        description: String,
        /// `user` (this machine's operator) or `repository` (anchored to the
        /// checkout this command runs in).
        #[arg(long, value_enum, default_value = "user")]
        scope: SkillScopeArg,
        /// A Markdown file holding the `SKILL.md` procedure body.
        #[arg(long)]
        procedure: PathBuf,
        /// Where to author the package before installing. Defaults to a
        /// temporary directory; pass one to keep the authored source to edit.
        #[arg(long)]
        directory: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// List the MCP servers declared in `<config_dir>/mcp.toml`: launch line,
    /// env key names (never values), and the effective policy disposition.
    /// Config-level only — no server is spawned.
    List {
        /// Emit the rows as a JSON array instead of the per-server blocks.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum HookCommand {
    /// List all discovered hooks.
    List,
    /// Print a hook's hook.toml and approval status.
    Show {
        /// The hook id to show.
        id: String,
    },
    /// Approve a hook by content hash.
    Approve {
        /// The hook id to approve.
        id: String,
    },
    /// Reject a hook.
    Reject {
        /// The hook id to reject.
        id: String,
    },
}

#[derive(Subcommand)]
enum WebhookCommand {
    /// Register, list, rotate and retire inbound webhook endpoints
    /// (`automation_endpoints`), which is what makes a delivery to
    /// `POST /webhooks/<id>` verifiable at all.
    Endpoint {
        #[command(subcommand)]
        command: WebhookEndpointCommand,
    },
}

#[derive(Subcommand)]
enum WebhookEndpointCommand {
    /// Register an endpoint so `POST /webhooks/<id>` is verified against ITS
    /// signing key and body ceiling. Without a row here only `/webhook` and
    /// `/webhooks/default` are served — under the single `webhooks.toml` secret
    /// and the conservative 1 MiB unregistered body ceiling; every other path is
    /// refused with 401.
    ///
    /// The daemon picks the row up on the very next delivery: the resolver is a
    /// per-request SELECT, so no restart is needed.
    Add {
        /// The URL path segment: `POST /webhooks/<endpoint-id>`. ASCII letters,
        /// digits, `-`, `_` and `.`; validated against the listener's own path
        /// router, so an id no delivery could reach is refused.
        endpoint_id: String,
        /// The NAME of the environment variable holding the HMAC-SHA256 shared
        /// secret — in the DAEMON's environment, which is where it is read. Only
        /// the name is stored (`env:NAME`); the value never touches the database,
        /// a support bundle, or this command's output.
        #[arg(long, value_name = "NAME")]
        key_env: String,
        /// This endpoint's body ceiling in bytes (1..=8388608). Defaults to
        /// 1 MiB — the same allowance an unregistered endpoint gets, so
        /// registering can never widen the surface by accident.
        #[arg(long, value_name = "BYTES")]
        body_limit_bytes: Option<i64>,
        /// The endpoint's replay window in seconds, recorded for audit and for a
        /// future signed-timestamp scheme. NOTHING enforces it as a time window
        /// today: an HMAC GitHub delivery signs only its body, so a timestamp
        /// header would be attacker-supplied. Replays are suppressed by the
        /// permanent delivery-id + content-fingerprint reservation instead.
        #[arg(long, value_name = "SECONDS")]
        replay_window_seconds: Option<i64>,
    },
    /// List the endpoints registered by this user: id, state, scheme, key
    /// REFERENCE (never key material), body ceiling and replay window.
    List,
    /// Point an endpoint at a different environment variable — a key rotation
    /// that keeps the endpoint id, and therefore the URL, unchanged.
    Rotate {
        /// The endpoint id to rotate.
        endpoint_id: String,
        /// The NAME of the environment variable now holding the secret.
        #[arg(long, value_name = "NAME")]
        key_env: String,
    },
    /// Retire an endpoint without deleting it: deliveries are refused (401,
    /// indistinguishable from an endpoint that never existed) while the row
    /// stays for audit.
    Disable {
        /// The endpoint id to disable.
        endpoint_id: String,
    },
}

#[derive(Subcommand)]
enum AcpCommand {
    /// Refresh the curated official ACP agent registry.
    Refresh,
    /// List every agent in the official registry.
    List {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Download/install a registry agent without adding a model profile.
    Install {
        /// Registry id or alias (for example claude-code, codex, kimi-code, amp, vibe-chat).
        agent: String,
        #[arg(long)]
        refresh: bool,
        /// Permit a curated binary URL whose registry entry has no SHA-256.
        #[arg(long)]
        allow_unverified: bool,
        /// Acknowledge that Antigravity's community ACP bridge is third-party
        /// software whose OAuth use may violate Google's Terms.
        #[arg(long)]
        accept_community_risk: bool,
    },
    /// Install, handshake, and add an ACP agent to the model picker.
    Connect {
        /// Registry id or alias (for example claude-code, codex, kimi-code, amp, vibe-chat).
        agent: String,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        allow_unverified: bool,
        /// Acknowledge Antigravity community-bridge account/Terms risk.
        #[arg(long)]
        accept_community_risk: bool,
        /// Repository used for the handshake smoke test.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Send one real, tool-denied prompt without saving a model profile.
    Probe {
        /// Registry id or alias (for example claude-code, codex, kimi-code, amp, vibe-chat).
        agent: String,
        /// Harmless prompt used to verify a live vendor response.
        #[arg(
            long,
            default_value = "Reply with exactly: ACP LIVE OK. Do not inspect files or use tools."
        )]
        prompt: String,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        allow_unverified: bool,
        /// Acknowledge Antigravity community-bridge account/Terms risk.
        #[arg(long)]
        accept_community_risk: bool,
        /// Repository passed as the ACP session working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Remove an ACP model profile (downloaded agent bytes remain cached).
    Disconnect { profile: String },
    /// Show readiness of configured ACP profiles.
    Status,
    /// Expose Codypendent as an ACP agent over stdio for Zed and other clients.
    Serve {
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum CouncilCommand {
    /// Persist a named council assembled from configured model profiles.
    Create {
        /// Stable council name (`A-Za-z0-9._-`).
        name: String,
        /// Member model and role, repeated: `--member MODEL=ROLE`.
        #[arg(long, required = true)]
        member: Vec<String>,
        /// Configured model profile that synthesizes the council result.
        #[arg(long)]
        chair: String,
        /// Deliberation rounds (1-3). Later rounds critique the prior dossier.
        #[arg(long, default_value_t = 1)]
        rounds: u8,
        /// Human-readable purpose for this council.
        #[arg(long)]
        description: Option<String>,
        /// Evidence mode: members explore the repository with read-only tools
        /// and cite `file:line`, instead of reasoning with no tools at all.
        /// The chair then weighs cited evidence over unsupported assertion.
        /// Stored on the definition; `council run` may also enable it per-run.
        #[arg(long)]
        evidence: bool,
    },
    /// List configured councils.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one council's exact members, roles, chair, and rounds.
    Show {
        name: String,
        #[arg(long)]
        json: bool,
        /// Render the most recently saved run report instead of the definition.
        #[arg(long)]
        last: bool,
    },
    /// Retrieve a durable council outcome by result id, or the latest outcome
    /// for a council name. Council results are not workflow/blackboard items.
    Result {
        /// A terminal `result ID`, or a configured council name.
        selector: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove a council definition. Its prior durable sessions remain.
    Remove {
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Run member deliberation in parallel, then ask the chair to synthesize.
    /// Every run — completed, quorum-failed, or chair-failed — persists a
    /// JSON+Markdown report under the data dir; `council show <name> --last`
    /// replays it.
    Run {
        name: String,
        /// Question or decision the council should deliberate.
        #[arg(long)]
        objective: String,
        /// Repository context. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Emit the complete attributed result as JSON.
        #[arg(long)]
        json: bool,
        /// Run in evidence mode for this run even if the council was not
        /// created with `--evidence` (ORed with the stored flag; never turns
        /// evidence mode off for a council that already has it).
        #[arg(long)]
        evidence: bool,
    },
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
    /// Start a durable workflow run from a manifest. Ensures a daemon, sends
    /// the manifest, and prints the new run id the daemon drives to a terminal
    /// state in the background.
    Run {
        /// Path to the workflow manifest to run.
        file: PathBuf,
        /// The typed inputs the manifest declares, as a JSON value (e.g.
        /// '{"pull_request": 7}'). Defaults to null.
        #[arg(long)]
        inputs: Option<String>,
        /// Repository the workflow's agent nodes operate on (each writing node is
        /// carved its own isolated worktree from it). Defaults to the current
        /// directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Pause a running workflow run so its driver stops launching new nodes;
    /// resume it later with `workflow resume`.
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
    /// Cancel a workflow run: a cooperative drain — the driver stops
    /// launching new nodes, any in-flight node's agent run is interrupted, remaining
    /// pending nodes are skipped, and the run lands cancelled (terminal — no resume).
    Cancel {
        /// The durable workflow-run id.
        workflow_run_id: String,
    },
    /// Watch a workflow run's live node lifecycle: prints the run's
    /// current snapshot (each node's state, cost, and any failure/block reason), then
    /// streams each node transition and run-phase change until the run reaches a
    /// terminal state (or the stream is interrupted).
    Watch {
        /// The durable workflow-run id.
        workflow_run_id: String,
    },
}

#[derive(Subcommand)]
enum DocsCommand {
    /// Create a collaborative document (rubric #4 doc-writer). Sends
    /// `CreateDocument`; with `--from` the Markdown file is imported into typed
    /// blocks (headings / paragraphs / code / lists / tables / callouts /
    /// embeds), otherwise the document starts empty. Prints the new id.
    New {
        /// The document title.
        title: String,
        /// A Markdown file to seed the document's blocks from.
        #[arg(long = "from", value_name = "FILE")]
        from: Option<PathBuf>,
        /// The scope to create it in: `repository` (default), `system`, or
        /// `organization:<uuid>`.
        #[arg(long)]
        scope: Option<String>,
    },
    /// List the documents this checkout can see (repository + system scope),
    /// newest first, with their status and revision.
    List {
        /// Emit the rows as a JSON array instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Run the documentation staleness check (`/update-docs`): resolve every
    /// document's `{{ symbol:… }}` links against the code graph, diff them for
    /// signature changes and disappearances, and file each finding as a
    /// reviewable Maintain-mode suggestion. Prints the finding counts.
    Check,
    /// Publish a document's current revision to a Git target. Prints the
    /// computed plan (target / changed files / resulting Git action) and
    /// prompts for confirmation, then sends `PublishDocument`;
    /// ensures a daemon, parks a durable approval, and resolves it with the
    /// confirmed decision. Nothing is written until the approval resolves —
    /// on approval, the daemon executes the plan in the background and this
    /// prints the resulting commit once recorded.
    Publish {
        /// The document to publish.
        document: DocumentId,
        /// Where to publish it.
        #[arg(long, value_enum)]
        target: PublishTargetArg,
        /// The repo-relative path to write. Defaults to a slug of the
        /// document's title under `docs/`.
        #[arg(long)]
        path: Option<String>,
        /// The docs branch (`docs-branch`/`doc-pr` targets only). Defaults to
        /// `docs/publish`.
        #[arg(long)]
        branch: Option<String>,
        /// The pull-request title (`doc-pr` target only). Defaults to
        /// `Publish: <document title>`.
        #[arg(long)]
        title: Option<String>,
        /// Skip the confirmation prompt and approve immediately.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

/// `codypendent docs publish --target <TARGET>`: where a published document
/// lands. Mirrors `codypendent_knowledge::PublishTarget`'s three variants with
/// CLI-friendly names.
#[derive(Clone, Copy, ValueEnum)]
enum PublishTargetArg {
    /// Write the rendered Markdown to a repository file in the working tree.
    RepoFile,
    /// Commit the rendered Markdown to a dedicated docs branch.
    DocsBranch,
    /// Open a documentation pull request via the GitHub write path.
    DocPr,
}

impl From<PublishTargetArg> for commands::PublishTargetKind {
    fn from(arg: PublishTargetArg) -> Self {
        match arg {
            PublishTargetArg::RepoFile => commands::PublishTargetKind::RepoFile,
            PublishTargetArg::DocsBranch => commands::PublishTargetKind::DocsBranch,
            PublishTargetArg::DocPr => commands::PublishTargetKind::DocPr,
        }
    }
}

#[derive(Subcommand)]
enum EvalCommand {
    /// Execute an `evals/tasks/` suite headlessly over the JSONL client and
    /// write a `SuiteReport`. Ensures a daemon; each case starts its own run
    /// against its pinned fixture repository. Exit codes: 0 every case
    /// passed · 2 the suite ran but case(s) failed · 1 the harness itself
    /// broke (suite unloadable, daemon unreachable, policy refused).
    Run {
        /// The suite directory under `evals/tasks/` (e.g. `core` for
        /// `evals/tasks/core/`), or a path to it directly.
        #[arg(long, default_value = "core")]
        suite: String,
        /// The routing policy to select each case's model under. Resolved
        /// via `codypendent-routing`
        /// over the persisted model profiles, fail-closed: an unknown name or
        /// a case with no eligible model stops `eval run` before any case
        /// executes. The selection is recorded per case in the report
        /// (`routed_model`); it does not yet pin the daemon's own `StartRun`
        /// execution to that model (see `codypendent_cli::eval`'s module
        /// doc). Absent: every case runs under the daemon's own default model
        /// resolution, unchanged.
        #[arg(long)]
        policy: Option<String>,
        /// Bind this run's durable regression evidence to an existing promotion
        /// candidate. Without this option the report is still written, but it
        /// cannot advance any candidate's regression gate.
        #[arg(long)]
        candidate_id: Option<String>,
        /// Where to write the `SuiteReport` JSON.
        #[arg(long)]
        report: PathBuf,
    },
}

#[derive(Subcommand)]
enum ModelsCommand {
    /// List the models configured in `models.toml`, with their provider,
    /// endpoint, context window, and key status. The headless twin of the
    /// TUI's `/model` picker.
    List {
        /// Emit the rows as a JSON array instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Add a model from a catalog provider to `models.toml`, exactly as the
    /// TUI's add-model flow does (same catalog lookup, same auth header
    /// resolution, same atomic write). With no `--key-env`, a key is read
    /// from the provider's documented environment variable at call time.
    Add {
        /// The catalog provider id, as `codypendent models list-providers`
        /// spells it: `openai`, `anthropic`, `nebius`, `openrouter`, …. That
        /// listing marks the providers this command cannot serve; every
        /// example here is one it can.
        provider: String,
        /// The provider-side model id, as it must be sent on the wire.
        model: String,
        /// Store this environment variable NAME on the entry rather than
        /// relying on the provider's documented default. The VALUE is never
        /// written to disk — only the name.
        #[arg(long, value_name = "NAME")]
        key_env: Option<String>,
        /// Override the `models.toml` id (defaults to `<provider>/<model>`).
        #[arg(long, value_name = "ID")]
        id: Option<String>,
    },
    /// Verify one configured model end to end: resolve its credentials the way
    /// a run does, call the provider's `/models`, and confirm the configured
    /// model is listed. Exits non-zero when it is not.
    Check {
        /// The `models.toml` model id to check.
        id: String,
    },
    /// Benchmark a local model configured in `models.toml` and persist its
    /// measured profile: tokens/sec, time-to-first-token,
    /// warm-up, memory, context limit, structured-output reliability, tool-call
    /// accuracy, and a small coding-eval score. The router reads these MEASURED
    /// numbers (never vibes). Also caches the first-use capability probe.
    Bench {
        /// The `models.toml` model id to benchmark (its `base_url` is the
        /// endpoint the profile + probe are keyed under).
        id: String,
        /// Set (or override) this model's blended per-1M-token price for
        /// routing's cost/utility scoring, e.g. `3.5` for an average of $3.5
        /// per million tokens. Without it, a HOSTED model's price is looked
        /// up from the built-in provider catalog when the entry names a
        /// known `provider_id` (the same catalog `models add` reads); when
        /// neither is available the model is benched with an unpriced
        /// profile and stays ineligible for routing (a hosted model can
        /// never be silently treated as free).
        /// A LOCAL model needs no price to route (routing costs it at $0
        /// genuinely, not as the harness's "unmeasured" sentinel), so this
        /// flag is normally only needed for a hosted endpoint the catalog
        /// does not curate.
        #[arg(long, value_name = "USD_PER_1M")]
        price_per_1m_usd: Option<f64>,
    },
    /// List the built-in + user-extended provider catalog: id, display name,
    /// wire protocol, and whether it curates any prefilled models — the
    /// input `models add <provider> <model-id>` expects.
    ListProviders {
        /// Emit the rows as a JSON array instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Pull a GGUF model from the Unsloth catalog on Hugging Face and register
    /// it against the `ollama` provider. Resolves `<hf-repo>[:<quant>]`
    /// (defaulting a bare repo name to the `unsloth/` org and, with no
    /// `:quant`, an auto-picked default), drives `ollama pull
    /// hf.co/<org>/<repo>:<quant>` with streamed progress, then writes
    /// `models.toml` using the exact reference `ollama list` shows. Requires
    /// `ollama` on `PATH` (see https://ollama.com); this command never
    /// downloads or runs model weights itself.
    Pull {
        /// `<hf-repo>[:<quant>]`, e.g. `Qwen3-32B-GGUF`,
        /// `Qwen3-32B-GGUF:UD-Q4_K_XL`, or `some-org/Some-Model-GGUF:Q8_0`.
        spec: String,
    },
}

#[derive(Subcommand)]
enum RoutingCommand {
    /// Show whether measured routing is on, and what `routing.toml`
    /// currently declares.
    Status,
    /// Turn on measured routing: each task's model is chosen from benched
    /// profiles instead of the first reachable candidate in `models.toml`
    /// file order.
    Enable {
        /// The most sensitive data this scope is asserted to handle.
        /// Governs which models may be selected for off-device (hosted)
        /// routing — omit to keep the fail-closed default (`Unknown`,
        /// local-only).
        #[arg(long, value_enum)]
        data_classification: Option<DataClassificationArg>,
    },
    /// Turn off measured routing; `models.toml` file order decides again.
    Disable,
}

#[derive(Subcommand)]
enum PromoteCommand {
    /// Draft a candidate for the promotion pipeline. Prints the new
    /// candidate id.
    Propose {
        /// The artifact kind.
        #[arg(long, value_enum)]
        kind: PromoteKindArg,
        /// The artifact's name (e.g. `tool-selection`).
        #[arg(long)]
        name: String,
        /// The version being proposed.
        #[arg(long)]
        version: u32,
        /// Mark this candidate as synthesized (e.g. by a skill-clustering
        /// pipeline): it must pass permission review before it may enter
        /// evaluation.
        #[arg(long)]
        requires_permission_review: bool,
    },
    /// Advance a candidate through permission review, stored regression
    /// evidence, shadow traffic, and measured canary evidence. Every verdict
    /// is computed by the daemon from durable evidence it recorded itself:
    /// regression from the latest bound eval report, canary from the
    /// executions in `execution_observations`. This command supplies no
    /// numbers — it used to take `--sample-count`, `--error-rate-bps`,
    /// `--baseline-error-rate-bps`, `--p95-latency-ms` and
    /// `--baseline-p95-latency-ms`, which meant a promotion gate whose whole
    /// purpose is to catch a regression could be cleared by typing `500`.
    Advance {
        /// The candidate id (as printed by `promote propose`).
        candidate_id: String,
        /// Which transition to attempt.
        #[arg(long, value_enum)]
        step: PromoteStepArg,
    },
    // ADR-010 is enforced here, not merely documented: the `Controller` role
    // this local-first socket maps to a human operator is required, so an
    // agent- or system-initiated approval is refused structurally.
    /// **Approve and promote a candidate.** Requires a human operator; an
    /// agent cannot approve its own promotion.
    Approve { candidate_id: String },
    /// Manually roll back a promoted candidate to its predecessor version.
    Rollback { candidate_id: String },
}

/// `codypendent promote advance --step <STEP>`.
#[derive(Clone, Copy, ValueEnum)]
enum PromoteStepArg {
    ReviewPermissions,
    Regression,
    Shadow,
    Canary,
    ObserveCanary,
    FinishCanary,
}

impl PromoteStepArg {
    /// Every step is a bare transition name. No step takes a measurement from
    /// this process: `observe-canary` asks the daemon to measure the slice of
    /// its own recorded executions since the last promotion write, and the
    /// daemon refuses if that slice is not measured evidence.
    fn into_wire(self) -> codypendent_protocol::PromotionAction {
        use codypendent_protocol::PromotionAction;
        match self {
            PromoteStepArg::ReviewPermissions => PromotionAction::ReviewPermissions,
            PromoteStepArg::Regression => PromotionAction::RunRegression,
            PromoteStepArg::Shadow => PromotionAction::StartShadow,
            PromoteStepArg::Canary => PromotionAction::StartCanary,
            PromoteStepArg::ObserveCanary => PromotionAction::ObserveCanary,
            PromoteStepArg::FinishCanary => PromotionAction::FinishCanary,
        }
    }
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Verify and install a Remote UI package disabled in the daemon-owned
    /// content-addressed plugin store.
    Install {
        manifest: PathBuf,
        artifact: PathBuf,
        #[arg(long)]
        allow_unsigned: bool,
    },
    /// Start the installed worker in the real sandbox, negotiate, then stop.
    SmokeTest { id: String },
    /// Enable a smoke-tested plugin. Non-user scopes require --session.
    Enable {
        id: String,
        #[arg(long, default_value = "user")]
        scope: String,
        #[arg(long)]
        session: Option<SessionId>,
    },
    /// List durable plugin lifecycle and pending approval state.
    List {
        /// Emit the daemon's lifecycle rows as JSON instead of the report.
        #[arg(long)]
        json: bool,
    },
    /// Verify and apply/stage a package update.
    Update {
        id: String,
        manifest: PathBuf,
        artifact: PathBuf,
        #[arg(long)]
        allow_unsigned: bool,
    },
    /// Approve the exact sealed update candidate shown by `plugin update`.
    ApproveUpdate { id: String, receipt: String },
    /// Reject a sealed update candidate and restore the previous state.
    RejectUpdate { id: String, receipt: String },
    /// Revoke a plugin and stop all of its workers.
    Revoke { id: String },
    /// Parse a `plugin.toml` and render its identity, the capability list it
    /// requests, its resource caps, and its trust posture (signed? sandbox
    /// profile) — the "evaluate permissions" step a user sees before enabling a
    /// plugin. Manifest parsing only; it does not run anything.
    Inspect {
        /// Path to the plugin manifest to inspect.
        file: PathBuf,
    },
    /// Compare an installed `plugin.toml` against an update and print the
    /// permission diff, reporting whether the update expands permissions and so
    /// requires re-approval.
    Diff {
        /// The currently-installed manifest.
        installed: PathBuf,
        /// The candidate update manifest.
        update: PathBuf,
    },
    /// Verify a plugin artifact against its manifest using the trusted-publisher
    /// key store — the install gate for real keys. A signed plugin
    /// from an unknown publisher, a bad signature, or an unsigned plugin (unless
    /// `--allow-unsigned`) is refused with a non-zero exit (fails closed).
    Verify {
        /// The plugin manifest (`plugin.toml`).
        manifest: PathBuf,
        /// The plugin artifact whose checksum/signature is verified.
        artifact: PathBuf,
        /// Permit an unsigned (checksum-only) plugin. Default posture denies it.
        #[arg(long)]
        allow_unsigned: bool,
    },
    /// Manage the trusted-publisher key store: the ed25519 public keys
    /// `plugin verify` checks signatures against.
    Trust {
        #[command(subcommand)]
        command: TrustCommand,
    },
}

#[derive(Subcommand)]
enum TrustCommand {
    /// Trust a publisher: record its base64 ed25519 public key.
    Add {
        /// The publisher id (matched against a manifest's `publisher`).
        id: String,
        /// The publisher's ed25519 public key, base64-encoded (32 raw bytes).
        public_key: String,
    },
    /// List the trusted publishers and their public keys.
    List,
    /// Stop trusting a publisher, revoke/stop its signed UI plugins, and require
    /// a newly verified reinstall before the same ids can run under a new key.
    Remove {
        /// The publisher id to remove.
        id: String,
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
    /// Restart only if the daemon confirms no session or workflow run is active.
    /// If no daemon is running this simply starts one.
    Restart,
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
    #[value(alias = "json")]
    Jsonl,
}

/// Print a failed command's error the way a person reads it, and exit 1.
///
/// Returning `anyhow::Result` from `main` hands the error to Rust's
/// `Termination` impl, which prints the **Debug** form — and with
/// `RUST_BACKTRACE` set anywhere in the environment, that is the captured
/// backtrace. The 2026-08-13 review caught this as 36 frames dumped over the
/// terminal when the TUI lost its daemon, but the TUI was only where it was
/// noticed: EVERY command exits through here, so `models add azure-openai
/// gpt-5.4` printed its (correct, actionable) one-line refusal followed by a
/// stack trace naming `anyhow/error.rs` and `_start`.
///
/// `{error:#}` is anyhow's alternate Display: the whole `.context(...)` chain
/// on one line, no frames. The prefix is lowercase `error:` — the same one
/// clap prints for a parse failure — so the binary speaks with one voice
/// whether the refusal came from the parser or from a command.
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(TopCommand::InternalDesktopToolsMcp { socket }) = &cli.command {
        return codypendent_runtime::desktop_mcp::proxy_stdio(socket).await;
    }

    if let Some(directory) = &cli.launch_directory {
        std::env::set_current_dir(directory).map_err(|error| {
            anyhow::anyhow!("cannot open directory {}: {error}", directory.display())
        })?;
    }

    // `codypendent __daemon` *is* the daemon (the hidden self-spawn target of
    // `ensure_daemon`). Dispatch it before any TUI/theme setup so it behaves
    // exactly like the standalone `codypendentd` binary: init the daemon's
    // tracing, resolve paths, run the loop to shutdown.
    if matches!(cli.command, Some(TopCommand::InternalDaemon)) {
        codypendent_codypendentd::init_tracing();
        let paths = RuntimePaths::resolve()?;
        paths.ensure_directories()?;
        return codypendent_codypendentd::run_daemon(paths).await;
    }

    // Pure-text commands run BEFORE runtime paths resolve: `completion` and
    // `workflow validate`/`show` need no home directory, no data dir, and no
    // daemon, and failing them on a machine with no resolvable home was a
    // refusal with no cause.
    match &cli.command {
        Some(TopCommand::Completion { shell }) => {
            commands::completion(*shell, &mut Cli::command());
            return Ok(());
        }
        Some(TopCommand::Workflow {
            command: WorkflowCommand::Validate { file, agents },
        }) => {
            return commands::workflow_validate(file, agents.as_deref());
        }
        Some(TopCommand::Workflow {
            command: WorkflowCommand::Show { file, json },
        }) => {
            return commands::workflow_show(file, *json);
        }
        _ => {}
    }

    let paths = RuntimePaths::resolve()?;
    // `--theme` wins over `CODYPENDENT_THEME`; an empty value from either
    // source falls through to the other (see `theme_select::resolve_theme_override`
    // for why each source must be filtered before combining them).
    let theme_override =
        theme_select::resolve_theme_override(cli.theme, std::env::var("CODYPENDENT_THEME").ok());
    let Some(command) = cli.command else {
        // Bare `codypendent`: open the TUI for the current directory's repo.
        return tui::run(
            &paths,
            std::env::current_dir()?,
            theme_override,
            cli.accessible,
        )
        .await;
    };
    match command {
        // Dispatched before the match (see the early return in `main`); a
        // parsed `InternalDaemon` never reaches here.
        TopCommand::InternalDaemon => unreachable!("__daemon is dispatched before the match"),
        TopCommand::InternalDesktopToolsMcp { .. } => {
            unreachable!("desktop MCP proxy is dispatched before the match")
        }
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
            DaemonCommand::Restart => commands::restart(&paths).await,
        },
        TopCommand::Run {
            prompt,
            objective,
            mode,
            repo,
            model,
            jsonl,
        } => {
            // Clap enforces both invariants at parse time: `conflicts_with`
            // refuses two objectives that may disagree, and
            // `required_unless_present` refuses an objective-less `run` — each
            // with a usage line rather than a bare runtime error.
            let objective = match (prompt, objective) {
                (Some(p), None) => p,
                (None, Some(obj)) => obj,
                _ => unreachable!("clap enforces exactly one objective source"),
            };
            let repo = match repo {
                Some(repo) => repo,
                None => std::env::current_dir()?,
            };
            let exit_code =
                commands::run(&paths, objective, mode.into(), repo, model, jsonl).await?;
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
        TopCommand::Graph { command } => match command {
            GraphCommand::Build { repo, json } => commands::graph_build(&paths, repo, json).await,
            GraphCommand::Status { repo, json } => commands::graph_status(&paths, repo, json).await,
            GraphCommand::Show {
                repo,
                path,
                language,
                kind,
                name,
                node_id,
                edges,
                connected,
                limit,
                json,
            } => {
                let query = codypendent_protocol::CodeGraphQuery {
                    path,
                    language,
                    kind,
                    name,
                    node_id,
                    connected_view: connected,
                    include_neighbors: false,
                    include_edges: edges || connected,
                    include_nodes: true,
                    limit,
                };
                commands::graph_show(&paths, repo, query, json).await
            }
        },
        TopCommand::Skill { command } => match command {
            SkillCommand::Add { directory } => commands::skill_add(&paths, &directory).await,
            SkillCommand::New {
                id,
                name,
                description,
                scope,
                procedure,
                directory,
            } => {
                commands::skill_new(
                    &paths,
                    &id,
                    &name,
                    &description,
                    scope.as_str(),
                    &procedure,
                    directory.as_deref(),
                )
                .await
            }
        },
        TopCommand::Workflow { command } => match command {
            WorkflowCommand::Validate { file, agents } => {
                commands::workflow_validate(&file, agents.as_deref())
            }
            WorkflowCommand::Show { file, json } => commands::workflow_show(&file, json),
            WorkflowCommand::Run { file, inputs, repo } => {
                let repo = match repo {
                    Some(repo) => repo,
                    None => std::env::current_dir()?,
                };
                commands::workflow_run(&paths, &file, inputs, repo).await
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
            WorkflowCommand::Cancel { workflow_run_id } => {
                commands::workflow_cancel(&paths, workflow_run_id).await
            }
            WorkflowCommand::Watch { workflow_run_id } => {
                commands::workflow_watch(&paths, workflow_run_id).await
            }
        },
        TopCommand::FixCi { pr, repo } => {
            let repo = match repo {
                Some(repo) => repo,
                None => std::env::current_dir()?,
            };
            commands::fix_ci(&paths, pr, repo).await
        }
        TopCommand::Docs { command } => match command {
            DocsCommand::New { title, from, scope } => {
                commands::docs_new(&paths, &title, from.as_deref(), scope).await
            }
            DocsCommand::List { json } => commands::docs_list(&paths, json).await,
            DocsCommand::Check => commands::docs_check(&paths).await,
            DocsCommand::Publish {
                document,
                target,
                path,
                branch,
                title,
                yes,
            } => {
                commands::docs_publish(&paths, document, target.into(), path, branch, title, yes)
                    .await
            }
        },
        TopCommand::Eval { command } => match command {
            EvalCommand::Run {
                suite,
                policy,
                candidate_id,
                report,
            } => {
                let exit =
                    commands::eval_run(&paths, &suite, policy, candidate_id.as_deref(), &report)
                        .await?;
                if exit != 0 {
                    std::process::exit(exit);
                }
                Ok(())
            }
        },
        TopCommand::Models { command } => match command {
            ModelsCommand::List { json } => commands::models_list(&paths, json),
            ModelsCommand::Add {
                provider,
                model,
                key_env,
                id,
            } => commands::models_add(&paths, &provider, &model, key_env.as_deref(), id.as_deref()),
            ModelsCommand::Check { id } => commands::models_check(&paths, &id).await,
            ModelsCommand::Bench {
                id,
                price_per_1m_usd,
            } => commands::models_bench(&paths, &id, price_per_1m_usd).await,
            ModelsCommand::ListProviders { json } => commands::models_list_providers(&paths, json),
            ModelsCommand::Pull { spec } => {
                let hf = codypendent_integrations::unsloth::HfHubClient::hub()?;
                codypendent_cli::models_pull::run(
                    &paths,
                    &spec,
                    &hf,
                    codypendent_cli::models_pull::OLLAMA_BIN,
                )
                .await
            }
        },
        TopCommand::Promote { command } => match command {
            PromoteCommand::Propose {
                kind,
                name,
                version,
                requires_permission_review,
            } => {
                commands::promote_propose(
                    &paths,
                    kind.as_str().to_owned(),
                    name,
                    version,
                    requires_permission_review,
                )
                .await
            }
            PromoteCommand::Advance { candidate_id, step } => {
                commands::promote_advance(&paths, candidate_id, step.into_wire()).await
            }
            PromoteCommand::Approve { candidate_id } => {
                commands::promote_approve(&paths, candidate_id).await
            }
            PromoteCommand::Rollback { candidate_id } => {
                commands::promote_rollback(&paths, candidate_id).await
            }
        },
        TopCommand::Plugin { command } => match command {
            PluginCommand::Install {
                manifest,
                artifact,
                allow_unsigned,
            } => commands::plugin_install(&paths, &manifest, &artifact, allow_unsigned).await,
            PluginCommand::SmokeTest { id } => commands::plugin_smoke_test(&paths, id).await,
            PluginCommand::Enable { id, scope, session } => {
                commands::plugin_enable(&paths, id, scope, session).await
            }
            PluginCommand::List { json } => commands::plugin_list(&paths, json).await,
            PluginCommand::Update {
                id,
                manifest,
                artifact,
                allow_unsigned,
            } => commands::plugin_update(&paths, id, &manifest, &artifact, allow_unsigned).await,
            PluginCommand::ApproveUpdate { id, receipt } => {
                commands::plugin_approve_update(&paths, id, receipt).await
            }
            PluginCommand::RejectUpdate { id, receipt } => {
                commands::plugin_reject_update(&paths, id, receipt).await
            }
            PluginCommand::Revoke { id } => commands::plugin_revoke(&paths, id).await,
            PluginCommand::Inspect { file } => commands::plugin_inspect(&file),
            PluginCommand::Diff { installed, update } => commands::plugin_diff(&installed, &update),
            PluginCommand::Verify {
                manifest,
                artifact,
                allow_unsigned,
            } => commands::plugin_verify(&manifest, &artifact, allow_unsigned),
            PluginCommand::Trust { command } => match command {
                TrustCommand::Add { id, public_key } => {
                    commands::plugin_trust_add(&id, &public_key)
                }
                TrustCommand::List => commands::plugin_trust_list(),
                TrustCommand::Remove { id } => commands::plugin_trust_remove(&paths, &id).await,
            },
        },
        TopCommand::Mcp {
            command: McpCommand::List { json },
        } => commands::mcp_list(&paths, json).await,
        TopCommand::Hook { command } => match command {
            HookCommand::List => commands::hook_list(&paths).await,
            HookCommand::Show { id } => commands::hook_show(&paths, &id).await,
            HookCommand::Approve { id } => commands::hook_approve(&paths, &id).await,
            HookCommand::Reject { id } => commands::hook_reject(&paths, &id).await,
        },
        TopCommand::Webhook {
            command: WebhookCommand::Endpoint { command },
        } => match command {
            WebhookEndpointCommand::Add {
                endpoint_id,
                key_env,
                body_limit_bytes,
                replay_window_seconds,
            } => {
                codypendent_cli::webhook_endpoints::add(
                    &paths,
                    &endpoint_id,
                    &key_env,
                    body_limit_bytes,
                    replay_window_seconds,
                )
                .await
            }
            WebhookEndpointCommand::List => codypendent_cli::webhook_endpoints::list(&paths).await,
            WebhookEndpointCommand::Rotate {
                endpoint_id,
                key_env,
            } => codypendent_cli::webhook_endpoints::rotate(&paths, &endpoint_id, &key_env).await,
            WebhookEndpointCommand::Disable { endpoint_id } => {
                codypendent_cli::webhook_endpoints::disable(&paths, &endpoint_id).await
            }
        },
        TopCommand::Acp { command, repo } => match command {
            None => {
                let repo = repo.unwrap_or(std::env::current_dir()?);
                codypendent_cli::acp::serve(&paths, repo).await
            }
            Some(AcpCommand::Serve { repo: serve_repo }) => {
                let repo = serve_repo.or(repo).unwrap_or(std::env::current_dir()?);
                codypendent_cli::acp::serve(&paths, repo).await
            }
            Some(AcpCommand::Refresh) => codypendent_cli::acp_clients::refresh(&paths).await,
            Some(AcpCommand::List { refresh, json }) => {
                codypendent_cli::acp_clients::list(&paths, refresh, json).await
            }
            Some(AcpCommand::Install {
                agent,
                refresh,
                allow_unverified,
                accept_community_risk,
            }) => {
                codypendent_cli::acp_clients::install(
                    &paths,
                    &agent,
                    refresh,
                    allow_unverified,
                    accept_community_risk,
                )
                .await
            }
            Some(AcpCommand::Connect {
                agent,
                profile,
                refresh,
                allow_unverified,
                accept_community_risk,
                repo: connect_repo,
            }) => {
                let repository = connect_repo.or(repo).unwrap_or(std::env::current_dir()?);
                codypendent_cli::acp_clients::connect(
                    &paths,
                    &agent,
                    profile.as_deref(),
                    refresh,
                    allow_unverified,
                    accept_community_risk,
                    &repository,
                )
                .await
            }
            Some(AcpCommand::Probe {
                agent,
                prompt,
                refresh,
                allow_unverified,
                accept_community_risk,
                repo: probe_repo,
            }) => {
                let repository = probe_repo.or(repo).unwrap_or(std::env::current_dir()?);
                codypendent_cli::acp_clients::probe(
                    &paths,
                    &agent,
                    &prompt,
                    refresh,
                    allow_unverified,
                    accept_community_risk,
                    &repository,
                )
                .await
            }
            Some(AcpCommand::Disconnect { profile }) => {
                codypendent_cli::acp_clients::disconnect(&paths, &profile)
            }
            Some(AcpCommand::Status) => codypendent_cli::acp_clients::status(&paths).await,
        },
        TopCommand::Council { command } => match command {
            CouncilCommand::Create {
                name,
                member,
                chair,
                rounds,
                description,
                evidence,
            } => codypendent_cli::council::create(
                &paths,
                name,
                member,
                chair,
                rounds,
                description,
                evidence,
            ),
            CouncilCommand::List { json } => codypendent_cli::council::list(&paths, json),
            CouncilCommand::Show { name, json, last } => {
                codypendent_cli::council::show(&paths, &name, json, last)
            }
            CouncilCommand::Result { selector, json } => {
                codypendent_cli::council::show_result(&paths, &selector, json)
            }
            CouncilCommand::Remove { name, yes } => {
                if !commands::confirm(&format!("Remove council `{name}`?"), yes)? {
                    eprintln!("not removed");
                    return Ok(());
                }
                codypendent_cli::council::remove(&paths, &name)
            }
            CouncilCommand::Run {
                name,
                objective,
                repo,
                json,
                evidence,
            } => {
                let repository = repo.unwrap_or(std::env::current_dir()?);
                codypendent_cli::council::run(&paths, &name, objective, repository, json, evidence)
                    .await
            }
        },
        TopCommand::Routing { command } => match command {
            RoutingCommand::Status => commands::routing_status(&paths),
            RoutingCommand::Enable {
                data_classification,
            } => {
                commands::routing_enable(
                    &paths,
                    data_classification.map(DataClassificationArg::as_str),
                )
                .await
            }
            RoutingCommand::Disable => commands::routing_disable(&paths).await,
        },
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
        TopCommand::Completion { shell } => {
            // Generate from the app's own derived command so completions never
            // drift from the real CLI. No daemon, no I/O beyond stdout.
            commands::completion(shell, &mut Cli::command());
            Ok(())
        }
        TopCommand::Doctor { json, deep } => {
            // `doctor` returns whether all checks passed; the exit-1-on-failure
            // decision lives here (the library never calls `std::process::exit`).
            let healthy = codypendent_cli::doctor::run(&paths, json, deep, cli.accessible).await?;
            if healthy {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        TopCommand::Update { check, tag } => {
            // `--check` exits 2 when an update is available (scriptable); the
            // exit decision lives here (the library never calls `process::exit`).
            let available = codypendent_cli::update::run(&paths, check, tag).await?;
            if check && available {
                std::process::exit(2);
            }
            Ok(())
        }
        TopCommand::Install { command } => match command {
            InstallCommand::Desktop { tag } => codypendent_cli::update::install(
                codypendent_cli::update::InstallTarget::Desktop,
                tag,
            ),
            InstallCommand::Vscode { ide, tag } => {
                let (binary, name) = ide.binary_and_name();
                if matches!(ide, IdeArg::Zed) {
                    // Zed has its own extension format and no `--install-extension`
                    // for a `.vsix`. Refusing beats shelling out to a flag that
                    // does not exist and reporting whatever it prints as success.
                    anyhow::bail!(
                        "Zed cannot install a VS Code `.vsix` — it uses its own extension \
                         format. Use `--in vscode` or `--in cursor`."
                    );
                }
                codypendent_cli::update::install(
                    codypendent_cli::update::InstallTarget::Editor { binary, name },
                    tag,
                )
            }
        },
        TopCommand::Finetune { command } => match command {
            FinetuneCommand::Init { model, out } => {
                let options = codypendent_cli::finetune::InitOptions {
                    base_model: model.unwrap_or_else(|| {
                        codypendent_cli::finetune::DEFAULT_BASE_MODEL.to_string()
                    }),
                    out_dir: out.unwrap_or_else(|| {
                        PathBuf::from(codypendent_cli::finetune::DEFAULT_OUT_DIR)
                    }),
                };
                let report = codypendent_cli::finetune::init(&options)?;
                println!(
                    "codypendent finetune init: scaffolded {} file(s) in {}",
                    report.files_written.len(),
                    report.out_dir.display()
                );
                println!(
                    "next: cd {} && pip install -r requirements.txt",
                    report.out_dir.display()
                );
                println!("      codypendent finetune check   # verify python/CUDA first");
                Ok(())
            }
            FinetuneCommand::Check { json } => {
                // Same exit-decision-lives-in-main convention as `doctor`
                // above: the library never calls `std::process::exit`. Only a
                // missing Python fails the process; a missing GPU warns.
                let report = codypendent_cli::finetune::check("python3", "nvidia-smi");
                if json {
                    // `render_json` existed from day one and was unreachable
                    // from the CLI; `doctor --json` scripts expect the twin.
                    println!("{}", report.render_json());
                } else {
                    print!(
                        "{}",
                        report.render_text_with(codypendent_cli::doctor::ascii_output(
                            cli.accessible
                        ))
                    );
                }
                if report.worst() == codypendent_cli::doctor::Status::Fail {
                    std::process::exit(1);
                }
                Ok(())
            }
            FinetuneCommand::Dataset { command } => match command {
                FinetuneDatasetCommand::Export => {
                    // The explanation is a REFUSAL, not a result: it goes to
                    // stderr and exits non-zero, so a script consuming the
                    // dataset never reads exit-0 plus an essay as success.
                    eprintln!("{}", codypendent_cli::finetune::dataset_export());
                    std::process::exit(2);
                }
            },
        },
        TopCommand::Approvals { command } => match command {
            ApprovalsCommand::Rules { command } => {
                match command.unwrap_or(ApprovalRulesCommand::List) {
                    ApprovalRulesCommand::List => commands::approvals_rules_list(&paths).await,
                    ApprovalRulesCommand::Revoke { id } => {
                        commands::approvals_rules_revoke(&paths, &id).await
                    }
                }
            }
        },
        TopCommand::Marketplace { command } => match command {
            MarketplaceCommand::Search { query, limit } => {
                commands::marketplace_search(&paths, &query, limit).await
            }
            MarketplaceCommand::Install {
                package_id,
                manifest,
                artifact,
                allow_unsigned,
            } => {
                commands::marketplace_install(
                    &paths,
                    &package_id,
                    manifest.as_deref(),
                    artifact.as_deref(),
                    allow_unsigned,
                )
                .await
            }
            MarketplaceCommand::Update {
                package_id,
                manifest,
                artifact,
                allow_unsigned,
            } => {
                commands::marketplace_update(
                    &paths,
                    &package_id,
                    manifest.as_deref(),
                    artifact.as_deref(),
                    allow_unsigned,
                )
                .await
            }
            MarketplaceCommand::Enable {
                package_id,
                scope,
                session,
            } => commands::marketplace_enable(&paths, &package_id, scope.as_deref(), session).await,
            MarketplaceCommand::Disable { package_id } => {
                commands::marketplace_disable(&paths, &package_id).await
            }
            MarketplaceCommand::Revoke { package_id, reason } => {
                commands::marketplace_revoke(&paths, &package_id, &reason).await
            }
        },
        TopCommand::Secret { command } => match command {
            SecretCommand::Declare {
                name,
                backend,
                locator,
                capability,
                org,
                repo,
            } => {
                commands::secret_declare(
                    &paths,
                    &name,
                    backend.as_str(),
                    &locator,
                    &capability,
                    org.as_deref(),
                    repo.as_deref(),
                )
                .await
            }
            SecretCommand::Bind {
                reference_id,
                job_id,
                capability,
            } => commands::secret_bind(&paths, &reference_id, &job_id, &capability).await,
            SecretCommand::List { capability } => {
                commands::secret_list(&paths, capability.as_deref()).await
            }
            SecretCommand::Revoke {
                reference_id,
                reason,
            } => commands::secret_revoke(&paths, &reference_id, &reason).await,
        },
        TopCommand::Session { command } => match command {
            SessionCommand::Search {
                query,
                limit,
                cursor,
                json,
            } => commands::session_search(&paths, &query, limit, cursor, json).await,
            SessionCommand::Rename { session_id, title } => {
                commands::session_lifecycle(
                    &paths,
                    session_id,
                    codypendent_protocol::SessionLifecycleAction::Rename { title },
                    "renamed",
                )
                .await
            }
            SessionCommand::Pin { session_id } => {
                commands::session_lifecycle(
                    &paths,
                    session_id,
                    codypendent_protocol::SessionLifecycleAction::Pin,
                    "pinned",
                )
                .await
            }
            SessionCommand::Unpin { session_id } => {
                commands::session_lifecycle(
                    &paths,
                    session_id,
                    codypendent_protocol::SessionLifecycleAction::Unpin,
                    "unpinned",
                )
                .await
            }
            SessionCommand::Archive { session_id } => {
                commands::session_lifecycle(
                    &paths,
                    session_id,
                    codypendent_protocol::SessionLifecycleAction::Archive,
                    "archived",
                )
                .await
            }
            SessionCommand::Restore { session_id } => {
                commands::session_lifecycle(
                    &paths,
                    session_id,
                    codypendent_protocol::SessionLifecycleAction::Restore,
                    "restored",
                )
                .await
            }
            SessionCommand::Delete {
                session_id,
                tombstone_only,
                yes,
            } => {
                // The client has no undo, so deletion earns the shared local
                // gate every destructive verb now goes through. The daemon
                // remains the retention authority either way.
                if !commands::confirm(&format!("Delete session {session_id}?"), yes)? {
                    eprintln!("not deleted");
                    return Ok(());
                }
                commands::session_lifecycle(
                    &paths,
                    session_id,
                    codypendent_protocol::SessionLifecycleAction::Delete {
                        mode: if tombstone_only {
                            codypendent_protocol::SessionDeletionMode::TombstoneOnly
                        } else {
                            codypendent_protocol::SessionDeletionMode::RetentionPolicy
                        },
                    },
                    "deleted",
                )
                .await
            }
            SessionCommand::Export {
                session_id,
                out,
                format,
                include_artifacts,
                include_internal,
            } => {
                commands::session_export(
                    &paths,
                    session_id,
                    format.to_wire(),
                    include_artifacts,
                    include_internal,
                    &out,
                )
                .await
            }
        },
        TopCommand::Bundle { command } => match command {
            BundleCommand::Export {
                sessions,
                out,
                include_transcripts,
                include_routing,
                include_approvals,
                include_artifact_manifests,
                include_patches,
                include_diagnostics,
                redaction,
            } => {
                commands::bundle_export(
                    &paths,
                    sessions,
                    codypendent_protocol::BundleInclusionPolicy {
                        transcript_events: include_transcripts,
                        routing_metadata: include_routing,
                        approvals: include_approvals,
                        artifact_manifests: include_artifact_manifests,
                        patches: include_patches,
                        environment_diagnostics: include_diagnostics,
                    },
                    redaction.to_wire(),
                    &out,
                )
                .await
            }
            BundleCommand::Import { file, collision } => {
                commands::bundle_import(&paths, &file, collision.to_wire()).await
            }
        },
        TopCommand::Budget { command } => match command {
            BudgetCommand::Create {
                dimension,
                window,
                threshold,
                scope,
                scope_value,
                disabled,
                json,
            } => {
                commands::budget_manage(
                    &paths,
                    codypendent_protocol::AnalyticsBudgetRequest::Create {
                        budget: codypendent_protocol::AnalyticsBudgetDraft {
                            scope: scope.to_wire(scope_value)?,
                            dimension: dimension.to_wire(),
                            window: window.to_wire(),
                            threshold,
                            enabled: !disabled,
                        },
                    },
                    json,
                )
                .await
            }
            BudgetCommand::List {
                enabled,
                disabled,
                limit,
                json,
            } => {
                // Absent means "no filter" — not `false`. Coercing an omitted
                // flag to a value would silently hide every disabled budget.
                let enabled_filter = match (enabled, disabled) {
                    (true, false) => Some(true),
                    (false, true) => Some(false),
                    _ => None,
                };
                commands::budget_manage(
                    &paths,
                    codypendent_protocol::AnalyticsBudgetRequest::List {
                        query: codypendent_protocol::AnalyticsBudgetQuery {
                            enabled: enabled_filter,
                            // 0 asks for the daemon's own page size; a
                            // client-chosen limit is a request, not a grant.
                            limit: limit.unwrap_or(0),
                        },
                    },
                    json,
                )
                .await
            }
            BudgetCommand::Get { budget_id, json } => {
                commands::budget_manage(
                    &paths,
                    codypendent_protocol::AnalyticsBudgetRequest::Get { id: budget_id },
                    json,
                )
                .await
            }
            BudgetCommand::Update {
                budget_id,
                threshold,
                enable,
                disable,
                json,
            } => {
                let patch = codypendent_protocol::AnalyticsBudgetPatch {
                    threshold,
                    enabled: match (enable, disable) {
                        (true, false) => Some(true),
                        (false, true) => Some(false),
                        _ => None,
                    },
                };
                // The empty-patch refusal lives in
                // `commands::budget_manage_over_connection`, so every client of
                // that core gets it rather than just this one.
                commands::budget_manage(
                    &paths,
                    codypendent_protocol::AnalyticsBudgetRequest::Update {
                        id: budget_id,
                        patch,
                    },
                    json,
                )
                .await
            }
            BudgetCommand::Delete { budget_id, yes } => {
                if !commands::confirm(&format!("Delete budget {budget_id}?"), yes)? {
                    eprintln!("not deleted");
                    return Ok(());
                }
                commands::budget_manage(
                    &paths,
                    codypendent_protocol::AnalyticsBudgetRequest::Delete { id: budget_id },
                    false,
                )
                .await
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_can_be_selected_by_a_launcher_or_after_a_subcommand() {
        let cli = Cli::try_parse_from(["codypendent", "-C", "/tmp/My Project"]).unwrap();
        assert_eq!(cli.launch_directory, Some(PathBuf::from("/tmp/My Project")));
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from([
            "codypendent",
            "workflow",
            "validate",
            "workflow.yaml",
            "--working-directory",
            "/tmp/project",
        ])
        .unwrap();
        assert_eq!(cli.launch_directory, Some(PathBuf::from("/tmp/project")));
        let skill = Cli::try_parse_from(["codypendent", "skill", "add", "package"]).unwrap();
        assert!(skill.launch_directory.is_none());
        assert!(matches!(skill.command, Some(TopCommand::Skill {
            command: SkillCommand::Add { directory }
        }) if directory == PathBuf::from("package")));
        let skill = Cli::try_parse_from([
            "codypendent",
            "-C",
            "/tmp/project",
            "skill",
            "new",
            "example",
            "--name",
            "Example",
            "--description",
            "Example package",
            "--procedure",
            "body.md",
            "--directory",
            "output",
        ])
        .unwrap();
        assert_eq!(skill.launch_directory, Some(PathBuf::from("/tmp/project")));
        assert!(matches!(skill.command, Some(TopCommand::Skill {
            command: SkillCommand::New { directory: Some(directory), .. }
        }) if directory == PathBuf::from("output")));
    }
    use clap::CommandFactory;

    #[test]
    fn daemon_subcommand_parses_to_internal_daemon() {
        // `codypendent __daemon` is the hidden self-spawn target; it must parse.
        let cli = Cli::try_parse_from(["codypendent", "__daemon"]).expect("__daemon must parse");
        assert!(matches!(cli.command, Some(TopCommand::InternalDaemon)));
    }

    #[test]
    fn internal_daemon_is_hidden_from_help() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            !help.contains("__daemon"),
            "the __daemon subcommand must be hidden from --help, got:\n{help}"
        );
    }

    /// The release now attaches a desktop bundle and a `.vsix`, and
    /// `codypendent install` is the only thing that puts either on a machine.
    /// A handler nothing can reach is the failure mode this asserts against:
    /// these are the exact argv strings a user types, parsed by the real `Cli`.
    #[test]
    fn install_parses_from_the_argv_a_user_actually_types() {
        let desktop = Cli::try_parse_from(["codypendent", "install", "desktop"])
            .expect("`codypendent install desktop` must parse");
        assert!(matches!(
            desktop.command,
            Some(TopCommand::Install {
                command: InstallCommand::Desktop { tag: None }
            })
        ));

        // A pinned tag is positional, exactly as `codypendent update <tag>` is.
        let pinned = Cli::try_parse_from(["codypendent", "install", "desktop", "v0.11.0"])
            .expect("`codypendent install desktop <tag>` must parse");
        let Some(TopCommand::Install {
            command: InstallCommand::Desktop { tag: Some(tag) },
        }) = pinned.command
        else {
            panic!("a positional tag must reach the desktop install handler");
        };
        assert_eq!(tag, "v0.11.0");

        // Defaults to VS Code, and `--in cursor` reaches Cursor's own launcher
        // rather than hard-coding `code` for every editor.
        let vscode = Cli::try_parse_from(["codypendent", "install", "vscode"])
            .expect("`codypendent install vscode` must parse");
        let Some(TopCommand::Install {
            command: InstallCommand::Vscode { ide, tag: None },
        }) = vscode.command
        else {
            panic!("`install vscode` must default to an editor without a tag");
        };
        assert_eq!(ide.binary_and_name(), ("code", "VS Code"));

        let cursor = Cli::try_parse_from(["codypendent", "install", "extension", "--in", "cursor"])
            .expect("the `extension` alias and `--in cursor` must parse");
        let Some(TopCommand::Install {
            command: InstallCommand::Vscode { ide, .. },
        }) = cursor.command
        else {
            panic!("`--in cursor` must reach the editor install handler");
        };
        assert_eq!(ide.binary_and_name(), ("cursor", "Cursor"));
    }

    /// `install` must be discoverable, unlike `__daemon`: a user who does not
    /// already know the command exists finds it only through `--help`.
    #[test]
    fn install_is_advertised_in_help() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains("install"),
            "`codypendent install` must appear in --help, got:\n{help}"
        );
    }

    /// A positional prompt and `--objective` are two answers to the same
    /// question. Preferring the positional silently discarded whatever the
    /// operator typed after `--objective`; the conflict is now explicit.
    #[test]
    fn run_refuses_a_positional_prompt_and_objective_together() {
        // `Cli` is not `Debug`, so `expect_err` is unavailable here.
        let Err(both) = Cli::try_parse_from([
            "codypendent",
            "run",
            "positional objective",
            "--objective",
            "flag objective",
        ]) else {
            panic!("a positional prompt and --objective must conflict");
        };
        assert_eq!(both.kind(), clap::error::ErrorKind::ArgumentConflict);

        let positional =
            Cli::try_parse_from(["codypendent", "run", "just the positional", "--jsonl"])
                .expect("a lone positional prompt still parses");
        assert!(matches!(
            positional.command,
            Some(TopCommand::Run {
                prompt: Some(_),
                objective: None,
                ..
            })
        ));

        let flag = Cli::try_parse_from([
            "codypendent",
            "run",
            "--objective",
            "just the flag",
            "--jsonl",
        ])
        .expect("a lone --objective still parses");
        assert!(matches!(
            flag.command,
            Some(TopCommand::Run {
                prompt: None,
                objective: Some(_),
                ..
            })
        ));
    }

    /// A prompt remains required; humans no longer need to opt into JSONL.
    #[test]
    fn run_requires_an_objective_and_defaults_to_text() {
        let Err(no_objective) = Cli::try_parse_from(["codypendent", "run", "--jsonl"]) else {
            panic!("an objective-less run must be refused at parse time");
        };
        assert_eq!(
            no_objective.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let cli = Cli::try_parse_from(["codypendent", "run", "fix the tests"])
            .expect("plain runs must parse");
        assert!(matches!(
            cli.command,
            Some(TopCommand::Run { jsonl: false, .. })
        ));
    }

    #[test]
    fn accessible_and_plain_flags_select_the_cooked_client() {
        let accessible =
            Cli::try_parse_from(["codypendent", "--accessible"]).expect("accessible must parse");
        assert!(accessible.accessible);
        assert!(accessible.command.is_none());

        let plain = Cli::try_parse_from(["codypendent", "--plain"]).expect("plain must parse");
        assert!(plain.accessible);
        assert!(plain.command.is_none());

        let mut command = Cli::command();
        let help = command.render_long_help().to_string();
        assert!(help.contains("--accessible"));
        assert!(help.contains("--plain"));
    }

    /// `automation_endpoints` had a reader on the ingest path and no writer
    /// anywhere, so its per-endpoint signing key, body ceiling and replay window
    /// governed nothing. These are the exact argv strings an operator types to
    /// write a row; a handler that cannot be reached from argv would leave the
    /// table exactly as inert as it was.
    #[test]
    fn webhook_endpoint_management_parses_from_the_argv_an_operator_types() {
        let added = Cli::try_parse_from([
            "codypendent",
            "webhook",
            "endpoint",
            "add",
            "gh-main",
            "--key-env",
            "GH_WEBHOOK_SECRET",
            "--body-limit-bytes",
            "65536",
        ])
        .expect("`codypendent webhook endpoint add` must parse");
        match added.command {
            Some(TopCommand::Webhook {
                command:
                    WebhookCommand::Endpoint {
                        command:
                            WebhookEndpointCommand::Add {
                                endpoint_id,
                                key_env,
                                body_limit_bytes,
                                replay_window_seconds,
                            },
                    },
            }) => {
                assert_eq!(endpoint_id, "gh-main");
                assert_eq!(key_env, "GH_WEBHOOK_SECRET");
                assert_eq!(body_limit_bytes, Some(65536));
                assert_eq!(replay_window_seconds, None);
            }
            _ => panic!("expected a webhook endpoint add"),
        }

        for argv in [
            vec!["codypendent", "webhook", "endpoint", "list"],
            vec![
                "codypendent",
                "webhook",
                "endpoint",
                "rotate",
                "gh-main",
                "--key-env",
                "GH_WEBHOOK_SECRET_NEXT",
            ],
            vec!["codypendent", "webhook", "endpoint", "disable", "gh-main"],
        ] {
            Cli::try_parse_from(argv.clone())
                .unwrap_or_else(|error| panic!("`{}` must parse: {error}", argv.join(" ")));
        }

        // A key is named, never given: there is no flag that takes key material.
        let mut command = Cli::command();
        let help = command.render_long_help().to_string();
        assert!(help.contains("webhook"));
    }

    #[test]
    fn acp_management_and_legacy_serve_forms_parse() {
        let connect = Cli::try_parse_from([
            "codypendent",
            "acp",
            "connect",
            "vibe-chat",
            "--profile",
            "agents/vibe",
        ])
        .expect("ACP connect must parse");
        assert!(matches!(
            connect.command,
            Some(TopCommand::Acp {
                command: Some(AcpCommand::Connect { agent, profile, .. }),
                ..
            }) if agent == "vibe-chat" && profile.as_deref() == Some("agents/vibe")
        ));

        let probe = Cli::try_parse_from(["codypendent", "acp", "probe", "kimi-code"])
            .expect("ACP live probe must parse");
        assert!(matches!(
            probe.command,
            Some(TopCommand::Acp {
                command: Some(AcpCommand::Probe { agent, .. }),
                ..
            }) if agent == "kimi-code"
        ));

        let antigravity = Cli::try_parse_from([
            "codypendent",
            "acp",
            "connect",
            "antigravity",
            "--accept-community-risk",
        ])
        .expect("Antigravity risk acknowledgement must parse");
        assert!(matches!(
            antigravity.command,
            Some(TopCommand::Acp {
                command: Some(AcpCommand::Connect {
                    agent,
                    accept_community_risk: true,
                    ..
                }),
                ..
            }) if agent == "antigravity"
        ));

        let legacy = Cli::try_parse_from(["codypendent", "acp", "--repo", "."])
            .expect("legacy ACP serve must parse");
        assert!(matches!(
            legacy.command,
            Some(TopCommand::Acp { command: None, .. })
        ));
    }

    #[test]
    fn council_create_and_run_forms_parse() {
        let create = Cli::try_parse_from([
            "codypendent",
            "council",
            "create",
            "design-board",
            "--member",
            "acp/claude=architect",
            "--member",
            "acp/codex=critic",
            "--chair",
            "acp/amp",
            "--rounds",
            "2",
        ])
        .expect("council create must parse");
        assert!(matches!(
            create.command,
            Some(TopCommand::Council {
                command: CouncilCommand::Create { rounds: 2, .. }
            })
        ));

        let run = Cli::try_parse_from([
            "codypendent",
            "council",
            "run",
            "design-board",
            "--objective",
            "Choose an architecture",
            "--json",
        ])
        .expect("council run must parse");
        assert!(matches!(
            run.command,
            Some(TopCommand::Council {
                command: CouncilCommand::Run { json: true, .. }
            })
        ));

        let result = Cli::try_parse_from([
            "codypendent",
            "council",
            "result",
            "019ff6cd-c6a6-7572-a91c-3c3caadd05a0",
            "--json",
        ])
        .expect("council result must parse");
        assert!(matches!(
            result.command,
            Some(TopCommand::Council {
                command: CouncilCommand::Result { json: true, .. }
            })
        ));
    }

    #[test]
    fn secret_declare_accepts_the_documented_workload_identity_spelling() {
        // ValueEnum derives kebab-case by default, which would have silently
        // renamed this backend to `workload-identity` and broken every
        // script and doc written against the underscored spelling used
        // everywhere else (the backend implementation, the daemon's secret
        // gate, prior CLI docs).
        let declare = Cli::try_parse_from([
            "codypendent",
            "secret",
            "declare",
            "github_token",
            "--backend",
            "workload_identity",
            "--locator",
            "vault://path",
            "--capability",
            "network.github",
        ])
        .expect("secret declare must accept the underscored backend spelling");
        assert!(matches!(
            declare.command,
            Some(TopCommand::Secret {
                command: SecretCommand::Declare {
                    backend: SecretBackendArg::WorkloadIdentity,
                    ..
                }
            })
        ));

        assert!(Cli::try_parse_from([
            "codypendent",
            "secret",
            "declare",
            "github_token",
            "--backend",
            "workload-identity",
            "--locator",
            "vault://path",
            "--capability",
            "network.github",
        ])
        .is_err());
    }

    #[test]
    fn session_subcommands_parse_and_default_to_the_narrowest_options() {
        let id = "019ff6cd-c6a6-7572-a91c-3c3caadd05a0";

        let search = Cli::try_parse_from(["codypendent", "session", "search", "migration"])
            .expect("session search must parse");
        assert!(matches!(
            search.command,
            Some(TopCommand::Session {
                command: SessionCommand::Search {
                    limit: None,
                    json: false,
                    ..
                }
            })
        ));

        // A bare `session search` lists the most recent sessions.
        let empty = Cli::try_parse_from(["codypendent", "session", "search"])
            .expect("a bare session search must parse");
        assert!(matches!(
            empty.command,
            Some(TopCommand::Session {
                command: SessionCommand::Search { ref query, .. }
            }) if query.is_empty()
        ));

        // `sessions` is the plural alias, matching the `secrets`/`bundles` precedent.
        assert!(Cli::try_parse_from(["codypendent", "sessions", "search", "x"]).is_ok());

        let delete = Cli::try_parse_from(["codypendent", "session", "delete", id])
            .expect("session delete must parse");
        assert!(
            matches!(
                delete.command,
                Some(TopCommand::Session {
                    command: SessionCommand::Delete {
                        tombstone_only: false,
                        ..
                    }
                }),
            ),
            "deletion defaults to the daemon's retention policy, never a client-chosen weakening"
        );

        let export = Cli::try_parse_from([
            "codypendent",
            "session",
            "export",
            id,
            "--out",
            "/tmp/out.md",
        ])
        .expect("session export must parse");
        match export.command {
            Some(TopCommand::Session {
                command:
                    SessionCommand::Export {
                        include_artifacts,
                        include_internal,
                        ..
                    },
            }) => {
                // An export widens what leaves the daemon, so both switches are
                // off unless explicitly asked for.
                assert!(!include_artifacts);
                assert!(!include_internal);
            }
            other => panic!(
                "expected a session export, got a different command: {}",
                other.is_none()
            ),
        }

        // `--out` is mandatory: an export with nowhere to land must not parse.
        assert!(Cli::try_parse_from(["codypendent", "session", "export", id]).is_err());
    }

    #[test]
    fn bundle_subcommands_parse_with_every_inclusion_switch_closed_by_default() {
        let export =
            Cli::try_parse_from(["codypendent", "bundle", "export", "--out", "/tmp/b.tar"])
                .expect("bundle export must parse");
        match export.command {
            Some(TopCommand::Bundle {
                command:
                    BundleCommand::Export {
                        sessions,
                        include_transcripts,
                        include_routing,
                        include_approvals,
                        include_artifact_manifests,
                        include_patches,
                        include_diagnostics,
                        ..
                    },
            }) => {
                assert!(sessions.is_empty());
                // Fail-closed, exactly as `BundleInclusionPolicy::default()` is
                // on the wire: an omitted flag cannot widen an export.
                assert!(
                    !include_transcripts
                        && !include_routing
                        && !include_approvals
                        && !include_artifact_manifests
                        && !include_patches
                        && !include_diagnostics
                );
            }
            other => panic!("expected a bundle export: {}", other.is_none()),
        }

        let import = Cli::try_parse_from(["codypendent", "bundle", "import", "/tmp/b.tar"])
            .expect("bundle import must parse");
        assert!(matches!(
            import.command,
            Some(TopCommand::Bundle {
                command: BundleCommand::Import {
                    collision: BundleCollisionArg::Remap,
                    ..
                }
            })
        ));

        // `--out` is mandatory for an export, and a file for an import.
        assert!(Cli::try_parse_from(["codypendent", "bundle", "export"]).is_err());
        assert!(Cli::try_parse_from(["codypendent", "bundle", "import"]).is_err());
    }

    #[test]
    fn budget_subcommands_parse_and_default_to_an_enabled_owner_scope() {
        let create = Cli::try_parse_from([
            "codypendent",
            "budget",
            "create",
            "--dimension",
            "cost-micros",
            "--window",
            "day",
            "--threshold",
            "5000000",
        ])
        .expect("budget create must parse");
        match create.command {
            Some(TopCommand::Budget {
                command:
                    BudgetCommand::Create {
                        scope,
                        scope_value,
                        threshold,
                        disabled,
                        ..
                    },
            }) => {
                assert_eq!(threshold, 5_000_000);
                assert!(scope == BudgetScopeArg::Owner && scope_value.is_none());
                // A budget the operator asked for is evaluated: `--disabled` is
                // the opt-in, matching `AnalyticsBudgetDraft`'s wire default.
                assert!(!disabled);
            }
            other => panic!("expected a budget create: {}", other.is_none()),
        }

        // Every measured dimension the daemon can evaluate is expressible, and
        // nothing else is: an unmeasured dimension must not parse at all.
        for dimension in ["cost-micros", "input-tokens", "output-tokens", "latency-ms"] {
            assert!(
                Cli::try_parse_from([
                    "codypendent",
                    "budget",
                    "create",
                    "--dimension",
                    dimension,
                    "--window",
                    "month",
                    "--threshold",
                    "1",
                ])
                .is_ok(),
                "{dimension} is a measured dimension and must parse"
            );
        }
        assert!(Cli::try_parse_from([
            "codypendent",
            "budget",
            "create",
            "--dimension",
            "cached-tokens",
            "--window",
            "day",
            "--threshold",
            "1",
        ])
        .is_err());

        // Threshold, dimension and window are all mandatory: a budget missing
        // any of them has no meaning to send.
        assert!(Cli::try_parse_from([
            "codypendent",
            "budget",
            "create",
            "--dimension",
            "cost-micros",
            "--window",
            "day",
        ])
        .is_err());

        // A narrowed scope needs its value, and `owner` refuses one — the wire
        // type cannot express either mismatch, so the pairing is checked before
        // anything is sent.
        assert!(BudgetScopeArg::Repository.to_wire(None).is_err());
        assert!(BudgetScopeArg::Workflow.to_wire(None).is_err());
        assert!(BudgetScopeArg::Model.to_wire(None).is_err());
        assert!(BudgetScopeArg::Owner
            .to_wire(Some("/repo".to_string()))
            .is_err());
        assert_eq!(
            BudgetScopeArg::Repository
                .to_wire(Some("/repo".to_string()))
                .expect("a repository scope with its id"),
            codypendent_protocol::AnalyticsBudgetScope::Repository {
                repository_id: "/repo".to_string()
            }
        );

        let list =
            Cli::try_parse_from(["codypendent", "budget", "list"]).expect("budget list must parse");
        match list.command {
            Some(TopCommand::Budget {
                command:
                    BudgetCommand::List {
                        enabled,
                        disabled,
                        limit,
                        ..
                    },
            }) => {
                // Neither flag set means NO filter, which is not the same as
                // filtering on `enabled = false`.
                assert!(!enabled && !disabled);
                assert!(limit.is_none());
            }
            other => panic!("expected a budget list: {}", other.is_none()),
        }
        // The two filters are mutually exclusive; asking for both is a
        // contradiction, not an empty listing.
        assert!(
            Cli::try_parse_from(["codypendent", "budget", "list", "--enabled", "--disabled"])
                .is_err()
        );
        assert!(Cli::try_parse_from([
            "codypendent",
            "budget",
            "update",
            "b-1",
            "--enable",
            "--disable"
        ])
        .is_err());

        // `budgets` is the plural alias, matching the `sessions`/`bundles`
        // precedent.
        assert!(Cli::try_parse_from(["codypendent", "budgets", "list"]).is_ok());
        // An id is mandatory for every by-id verb.
        assert!(Cli::try_parse_from(["codypendent", "budget", "get"]).is_err());
        assert!(Cli::try_parse_from(["codypendent", "budget", "delete"]).is_err());
        assert!(Cli::try_parse_from(["codypendent", "budget", "update"]).is_err());
    }
}
