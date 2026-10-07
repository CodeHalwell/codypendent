//! The interactive TUI harness (STEP 1.12).
//!
//! Running `codypendent` with no subcommand opens the Ratatui client attached to
//! the current repository's session (creating it if needed, auto-starting the
//! daemon if needed). The rendering, input mapping, and reducer all live in the
//! pure `codypendent-tui` crate, which performs no I/O; this module is the
//! *harness* that the crate's own docs describe — it owns the protocol
//! connection, the terminal, and the event loop, and it is the only place the
//! two worlds meet.
//!
//! # The loop
//!
//! ```text
//!   crossterm event ─┐                        ┌─▶ reduce(&mut AppState, action)
//!   daemon event ────┼─▶ tokio::select! ─▶ Action        │
//!   200ms tick ──────┘                                   ├─▶ drain outbox → Commands
//!                                                         └─▶ render(frame, &state)
//! ```
//!
//! Two background tasks decouple the socket from the loop so a keystroke never
//! cancels a half-read frame (RULE: no partial-frame loss): a **reader** task
//! owns the read half and forwards each live [`SessionEvent`] to the loop (and
//! answers heartbeat `Ping`s via the writer), and a **writer** task owns the
//! write half and serializes every outgoing envelope — commands from the loop
//! and pongs from the reader. A third OS thread bridges blocking `crossterm`
//! input into the async loop.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use base64::Engine as _;
use codypendent_integrations::unsloth::HfCatalogApi;
use codypendent_knowledge::{
    db as knowledge_db, ActivationOutcome, BlockContent, CapabilityRequest, CollaborationMode,
    DocumentAuthor, DocumentBlock, DocumentReplica, DocumentStore, EvidenceRef, KnowledgeDocument,
    LearningContent, LearningMutationOutcome, LearningPatch, LearningProvenance, LearningQuery,
    LearningRecord, LearningScope, LearningState, LearningStore, MemoryClass, MemoryRecord,
    MemoryStore, Registry, RegistryItem, RegistryItemKind, RegistryStatus, RiskClass, Scope,
    Suggestion, SuggestionStatus, SuggestionStore, TrustTier, Verification,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, BlackboardItemDraft, BlackboardScope, Catchup, ClientId,
    ClientRole, Command, CommandBody, CommandId, DocumentEditLease, DocumentId, DocumentSync,
    Envelope, LearningId, ModelId, Payload, RepositoryId, SessionEvent, SessionId, Subscription,
    UserId, WorkspaceId,
};
use codypendent_runtime::models::{
    load_audio_models, provider_auth_id, MAX_PLAUSIBLE_CONTEXT_TOKENS,
};
use codypendent_tui::{
    accessible_snapshot, accessible_terminal_capabilities_message, map_accessible_input, map_event,
    reduce, render, render_splash, sanitize_accessible_text, terminal_capabilities_message, Action,
    AddModelRow, AppState, BlackboardItemCard, ColorDepth, CouncilMemberSummary,
    CouncilProgressPhase, CouncilRoundSummary, CouncilRunSummary, DocBlockView, DocCard,
    DocSuggestionView, GraphEdgeCard, Intent, KanbanCard, KeyStatus, KeyTarget, LearningCard,
    LearningMutation, MemoryCard, ModelCard, ModelListOrigin, ModelLocationLabel, ModelReadiness,
    ProjectionKind, ProviderCard, SkillCard, TerminalGuard, Theme, UnslothQuantCard,
    UnslothRepoCard, VoiceKeyRow, WorkflowNodeCard, WorkflowNodeUpdate, EDGE_PAGE_SIZE,
};
use crossterm::event::Event as CrosstermEvent;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, watch};
use tokio::{io::AsyncBufReadExt, task::JoinHandle};

use crate::commands;
use crate::connection::Connection;

/// How often the loop wakes with a [`Action::Tick`] for spinner / elapsed-timer
/// animation when nothing else is happening (5 fps — cheap, and the loop redraws
/// immediately on any real event anyway).
const TICK: Duration = Duration::from_millis(200);

/// The splash frame cadence while the TUI boots (D2) — ~12 fps so the stage
/// spinner animates smoothly during a multi-second daemon spawn.
const SPLASH_TICK: Duration = Duration::from_millis(80);

/// Full linear snapshots intentionally run slower than graphical frames.
/// Streaming token events would otherwise replay the whole document fast
/// enough to overwhelm a screen reader or redirected output.
const ACCESSIBLE_REFRESH: Duration = Duration::from_secs(1);

/// The most live events [`GapTracker`] will buffer while a gap repair is in
/// flight before giving up on the incremental replay and re-attaching for a
/// fresh catch-up (FP-2a). A slow client behind a fast producer could otherwise
/// grow this buffer without bound; the ledger is the source of truth, so
/// dropping the buffer and re-attaching from `last_seen` re-fetches the whole
/// span losslessly — we fail toward a fresh catch-up, never toward unbounded
/// memory.
const MAX_GAP_BUFFER: usize = 2048;

/// How long [`GapTracker`] waits for a gap repair's catch-up reply before
/// re-attaching afresh (FP-2b). Without a deadline a dropped catch-up reply
/// (the daemon's fan-out is lossy under lag) would wedge the client in
/// `repairing` forever, silently holding back every later event — worst case an
/// `ApprovalRequested`. On expiry we re-attach from `last_seen`, which re-drives
/// the catch-up.
const REPAIR_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound user effects accumulated while an attachment is being prepared. The
/// active transport remains the authority until the swap, but commands emitted
/// after the user's transition choice must not leak onto that old session.
const MAX_DEFERRED_TRANSITION_INTENTS: usize = 256;

/// The client-facing subscription set for the TUI: it wants the whole session,
/// not one run's trace.
fn default_subscriptions() -> Vec<Subscription> {
    vec![Subscription::SessionSummary, Subscription::AgentActivity]
}

/// Add a live-sync subscription for `document_id` unless this connection already
/// carries one, returning whether the list grew.
///
/// A `true` means the daemon has not been told yet and a re-attach must follow;
/// a `false` means it already knows and re-attaching would only re-send a set it
/// is already honouring.
///
/// This is deliberately keyed off the subscription list rather than off the
/// replica map that shadows it. The two diverge at exactly one point — reconnect
/// clears the replicas so they reseed from the fresh stream, while the
/// subscriptions survive to be re-attached — and keying off the replicas there
/// appended a duplicate for the same document on every reconnect-then-edit
/// cycle, with every duplicate re-sent in full on each later re-attach.
fn subscribe_to_document(subscriptions: &mut Vec<Subscription>, document_id: DocumentId) -> bool {
    let known = subscriptions
        .iter()
        .any(|s| matches!(s, Subscription::Document { document_id: id } if *id == document_id));
    if !known {
        subscriptions.push(Subscription::Document { document_id });
    }
    !known
}

// ---------------------------------------------------------------------------
// Crash logger (crash investigation follow-up).
//
// A prior TUI crash left the terminal in raw mode — proof `TerminalGuard`'s
// `Drop` never ran, so it was either an abort/OS-kill, or a panic whose
// message was lost the instant the alternate screen was torn down alongside
// it. We could not tell which. `install_crash_hook` closes that gap for the
// NEXT occurrence: it installs a `std::panic::set_hook` that appends the
// panic message, its source location, and a force-captured backtrace to
// `<data_dir>/logs/tui-crash.log` — a plain file, independent of the
// terminal — before chaining to the previous (default) hook.
//
// Diagnostic contract for whoever reads the log after a future crash:
//   - A non-empty file (a new entry since the crash) ⇒ the hook ran ⇒ it was
//     a PANIC, and its message/location/backtrace are right there.
//   - The file stays empty ⇒ the hook never ran ⇒ the process was
//     aborted/OS-killed (e.g. out-of-memory) — that points at memory, not a
//     panic, and a panic-message hunt would be a waste of time.
// ---------------------------------------------------------------------------

/// The crash log's filename under `<data_dir>/logs/` (alongside `daemon.log`).
const CRASH_LOG_FILE_NAME: &str = "tui-crash.log";

/// Format one crash-log entry from an already-extracted message, an optional
/// source location, and a backtrace string. Pure (no I/O) and so trivially
/// unit-testable: the panic hook itself (`install_crash_hook`) cannot easily
/// be driven from a test without actually panicking the test process, but all
/// of its formatting decisions live here, in a plain function a test CAN call
/// directly with synthetic inputs.
fn format_crash_entry(message: &str, location: Option<&str>, backtrace: &str) -> String {
    let location = location.unwrap_or("<unknown location>");
    format!(
        "---- codypendent-tui crash {} ----\n\
         message: {message}\n\
         location: {location}\n\
         backtrace:\n{backtrace}\n",
        humantime_now(),
    )
}

/// A dependency-free `now` stamp for the crash entry header. Not meant to be
/// parsed — only to let a human tell two entries in the same file apart — so
/// this deliberately avoids pulling in a time-formatting crate (no new
/// dependency) in favor of the `SystemTime` Unix-epoch offset `std` already
/// gives us.
fn humantime_now() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(since_epoch) => format!("at unix-epoch-seconds {}", since_epoch.as_secs()),
        Err(_) => "at an unknown time".to_string(),
    }
}

/// Append `entry` to the crash log at `path`, creating its parent directory
/// (e.g. `<data_dir>/logs/`) and the file itself if either is missing, in
/// append mode so an earlier crash's entry is never overwritten. Returns the
/// I/O error to the caller rather than swallowing it here — the panic-hook
/// closure (`install_crash_hook`) is the one place that must never propagate
/// a failure (a panic while handling a panic aborts the process outright), so
/// swallowing happens there, not in this otherwise-ordinary, testable writer.
fn write_crash_entry(path: &Path, entry: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(entry.as_bytes())
}

/// The panic hook's own body, split out from the `Box::new(move |info| ...)`
/// closure in `install_crash_hook` so it reads linearly: extract the message
/// (a panic payload is either a `&str` literal or a formatted `String`) and
/// the source location from `info`, format the entry, and write it —
/// best-effort. Any failure (a missing data dir the caller couldn't create, a
/// full disk, permissions) is swallowed: this runs INSIDE the panic hook, and
/// a panic here would abort the process instead of letting the original
/// panic's unwind (and the previous hook's default reporting) continue.
fn append_crash_log(path: &Path, info: &std::panic::PanicHookInfo<'_>, backtrace: &str) {
    let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    };
    let location = info.location().map(|location| location.to_string());
    let entry = format_crash_entry(&message, location.as_deref(), backtrace);
    let _ = write_crash_entry(path, &entry);
}

/// Install the crash-logging panic hook. Called once, as the very first thing
/// [`run`] does — before ANY other client setup, and so well before
/// [`TerminalGuard::enter`] — so it is active for a panic anywhere in the
/// client's lifetime and, critically, fires before the guard's `Drop` blanks
/// the alternate screen back to the caller's cooked terminal (the teardown
/// that made the original crash's panic message unrecoverable).
///
/// The previous hook (Rust's default, unless something upstream already
/// replaced it) is captured via [`std::panic::take_hook`] and chained after
/// the crash-log write, so normal panic reporting (and any other behavior the
/// previous hook carried) is entirely unchanged — this only ever ADDS the
/// file write in front of it.
fn install_crash_hook(paths: &RuntimePaths) {
    let crash_log_path = paths.log_dir.join(CRASH_LOG_FILE_NAME);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        append_crash_log(&crash_log_path, info, &backtrace.to_string());
        previous_hook(info);
    }));
}

/// `codypendent` with no subcommand: open the interactive TUI for `repo`.
///
/// Auto-starts the daemon, resolves (or creates) `repo`'s session, attaches with
/// catch-up, and runs the event loop until the user detaches (`q`) or the daemon
/// closes the stream. Detaching never affects the run — the daemon keeps it
/// going; a later `codypendent` reopens the same session and catches up.
///
/// `theme_override` is `--theme <NAME>` / `CODYPENDENT_THEME` (resolved by the
/// caller, flag winning over env — see `main.rs`): a built-in variant name or
/// a theme-pack id under `<data-dir>/themes/<id>.toml` (STEP 6.6, see
/// `theme_select`). Resolved before any daemon/socket work so a bad name fails
/// fast on a normal cooked terminal instead of after entering raw mode.
pub async fn run(
    paths: &RuntimePaths,
    repo: PathBuf,
    theme_override: Option<String>,
    accessible: bool,
) -> anyhow::Result<()> {
    // Crash logger, installed FIRST — before repo validation, the terminal
    // guard, and the daemon connection — so a panic anywhere in this
    // function's lifetime is diagnosable, and so it fires before the guard's
    // `Drop` tears down the alternate screen (see the module docs above
    // `install_crash_hook` for the full motivation).
    install_crash_hook(paths);

    let repo = repo
        .canonicalize()
        .with_context(|| format!("{}: not a valid, accessible directory", repo.display()))?;
    if !repo.is_dir() {
        bail!("{}: not a directory", repo.display());
    }

    if accessible {
        return run_accessible(paths, repo, theme_override).await;
    }

    // STEP 6.6 wiring: terminal color-depth detection (NO_COLOR/COLORTERM/TERM)
    // with a manual override that always wins, replacing the old hardcoded
    // `Theme::dark()`. The session store is loaded first so the theme the
    // operator last kept in `/theme` participates in that resolution (below an
    // explicit `--theme`/`CODYPENDENT_THEME`, above detection).
    let mut store = SessionStore::load(paths);
    let theme = crate::theme_select::resolve_theme(
        paths,
        theme_override.as_deref(),
        store.theme.as_deref(),
    )?;

    // D2: enter raw mode + the alternate screen EARLY — before any daemon
    // work — so the (possibly multi-second) boot draws a splash instead of
    // leaving the user staring at a blank cooked terminal. RAII restores the
    // terminal on any exit path, including a boot error (whose text then
    // prints to the restored cooked screen) or a panic mid-loop, and the
    // event loop later takes over the SAME terminal — no teardown/re-enter
    // between splash and TUI.
    let mut guard = TerminalGuard::enter().context(
        "the interactive TUI needs a terminal (a TTY); for headless use run \
         `codypendent run --jsonl` instead",
    )?;

    // D2: boot steps publish their stage over this channel; the splash loop
    // below draws the latest stage every SPLASH_TICK until boot completes.
    let (stage_tx, stage_rx) = watch::channel(SplashStage::StartingDaemon);
    // Boot diagnostics (reconcile warnings, loader failure notes) collect
    // BESIDE the stage channel: a `watch` retains only the LATEST value, so a
    // warning published as a stage would be overwritten by the next stage and
    // lost. The splash draws them under the stage line; after boot each one
    // becomes a TUI notice.
    let boot_warnings: BootWarnings = BootWarnings::default();

    let mut state = AppState::new();
    // The `/theme` picker's rows: the built-in variants `AppState::new` seeds,
    // plus every installed data-only pack (the TUI crate does no I/O, so packs
    // are parsed out here). The row matching the theme actually in force is
    // marked current, so the picker opens on what is already on screen.
    // A pack that FAILED to load becomes a boot warning rather than a silent
    // absence — including the security refusal, which the operator has to see.
    let discovered = crate::theme_select::discover_theme_packs(paths);
    for warning in discovered.warnings {
        push_boot_warning(&boot_warnings, warning);
    }
    state
        .themes
        .extend(
            discovered
                .packs
                .into_iter()
                .map(|(id, theme)| codypendent_tui::ThemeChoice {
                    id,
                    summary: "installed theme pack".to_owned(),
                    theme,
                    pack: true,
                }),
        );
    state.theme_selected = theme_override
        .as_deref()
        .or(store.theme.as_deref())
        .and_then(|id| {
            state
                .themes
                .iter()
                .position(|choice| choice.id.eq_ignore_ascii_case(id))
        })
        .or_else(|| state.themes.iter().position(|choice| choice.theme == theme));

    // Drive boot and the splash concurrently: poll the pinned boot future to
    // completion while redrawing the splash on each tick. Once boot is ready,
    // a separate welcome state remains visible until the user deliberately
    // presses Enter. The block scopes the pinned future (it borrows
    // `state`/`store`) so its drop runs before the event loop takes them back.
    let booted = {
        let boot = boot_phase(
            paths,
            &repo,
            &mut state,
            &mut store,
            &stage_tx,
            &boot_warnings,
        );
        tokio::pin!(boot);
        let mut splash_ticker =
            tokio::time::interval_at(tokio::time::Instant::now() + SPLASH_TICK, SPLASH_TICK);
        let mut splash_ticks: u64 = 0;
        loop {
            tokio::select! {
                outcome = &mut boot => break outcome?,
                _ = splash_ticker.tick() => {
                    splash_ticks += 1;
                    let stage = stage_rx.borrow().text();
                    let warnings = boot_warnings
                        .lock()
                        .expect("boot warnings mutex poisoned")
                        .clone();
                    guard
                        .terminal_mut()
                        .draw(|frame| {
                            render_splash(frame, splash_ticks, &stage, &warnings, false, false, &theme)
                        })?;
                }
            }
        }
    };

    let (input_tx, mut input_rx) = mpsc::channel::<ClientInput>(256);
    let input_running = Arc::new(AtomicBool::new(true));
    spawn_input_thread(input_tx, Arc::clone(&input_running));

    let workspace_name = repo
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let ready_stage = splash_ready_stage(workspace_name, state.runnable_models.len());
    let warnings = boot_warnings
        .lock()
        .expect("boot warnings mutex poisoned")
        .clone();
    let needs_setup = state.runnable_models.is_empty();
    if !wait_for_splash_entry(
        &mut guard,
        &theme,
        &ready_stage,
        &warnings,
        needs_setup,
        &mut input_rx,
    )
    .await?
    {
        input_running.store(false, Ordering::Relaxed);
        return Ok(());
    }
    apply_post_boot_onboard_gate(&mut state, &store);

    // Boot diagnostics become the TUI's own notices — ALWAYS when any were
    // collected, whether or not the splash drew a single frame: a reconcile
    // warning or a loader failure note is meaningful after boot regardless
    // (the pre-splash behavior was a persistent stderr print).
    drain_boot_warnings(&mut state, &boot_warnings);
    let Booted {
        session_id,
        workspace_id,
        attach_watermark,
        docs_pool,
        mut live,
    } = booted;

    let (mut width, _) = crossterm::terminal::size().unwrap_or((80, 24));

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let repository = repo.to_string_lossy().into_owned();
    let result = {
        let mut presentation = InteractivePresentation {
            guard: &mut guard,
            theme: &theme,
        };
        event_loop(
            &mut presentation,
            &mut state,
            &mut width,
            &mut live,
            &mut input_rx,
            &mut ticker,
            session_id,
            workspace_id,
            &repository,
            attach_watermark,
            docs_pool.clone(),
            &mut store,
            paths,
        )
        .await
    };

    // Teardown: stop the input thread, restore the terminal *before* any trailing
    // error text reaches the (now cooked) screen, then wind down the socket tasks.
    input_running.store(false, Ordering::Relaxed);
    drop(guard);
    live.shutdown();
    if let Some(pool) = docs_pool {
        pool.close().await;
    }
    // After the guard: the terminal is cooked again, so a lost-daemon report
    // prints where a human can read it.
    report_daemon_unavailable(result)
}

/// Run the full client over ordinary cooked stdin/stdout. Unlike the Ratatui
/// path this never constructs [`TerminalGuard`], so it cannot emit alternate
/// screen, raw-mode, mouse-capture, or bracketed-paste control sequences.
async fn run_accessible(
    paths: &RuntimePaths,
    repo: PathBuf,
    theme_override: Option<String>,
) -> anyhow::Result<()> {
    let mut stdout = io::stdout();
    writeln!(stdout, "Codypendent accessible mode")?;
    writeln!(stdout, "Starting daemon and restoring the session.")?;
    stdout.flush()?;

    let mut state = AppState::new();
    let mut store = SessionStore::load(paths);
    let resolved_theme = crate::theme_select::resolve_theme(
        paths,
        theme_override.as_deref(),
        store.theme.as_deref(),
    )?;
    // Same discovery, same reporting duty as the graphical path — collected
    // here and pushed once `boot_warnings` exists a few lines below.
    let discovered = crate::theme_select::discover_theme_packs(paths);
    let theme_pack_warnings = discovered.warnings;
    state
        .themes
        .extend(
            discovered
                .packs
                .into_iter()
                .map(|(id, theme)| codypendent_tui::ThemeChoice {
                    id,
                    summary: "installed theme pack".to_owned(),
                    theme,
                    pack: true,
                }),
        );
    state.theme_selected = theme_override
        .as_deref()
        .or(store.theme.as_deref())
        .and_then(|id| {
            state
                .themes
                .iter()
                .position(|choice| choice.id.eq_ignore_ascii_case(id))
        })
        .or_else(|| {
            state
                .themes
                .iter()
                .position(|choice| choice.theme == resolved_theme)
        });
    let (stage_tx, mut stage_rx) = watch::channel(SplashStage::StartingDaemon);
    let boot_warnings: BootWarnings = BootWarnings::default();
    for warning in theme_pack_warnings {
        push_boot_warning(&boot_warnings, warning);
    }
    let booted = {
        let boot = boot_phase(
            paths,
            &repo,
            &mut state,
            &mut store,
            &stage_tx,
            &boot_warnings,
        );
        tokio::pin!(boot);
        loop {
            tokio::select! {
                outcome = &mut boot => break outcome?,
                changed = stage_rx.changed() => {
                    if changed.is_ok() {
                        writeln!(stdout, "Boot: {}", ascii_stage(&stage_rx.borrow().text()))?;
                        stdout.flush()?;
                    }
                }
            }
        }
    };
    let accessible_stage = splash_ready_stage(
        repo.file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("workspace"),
        state.runnable_models.len(),
    );
    writeln!(stdout, "Ready: {}", ascii_stage(&accessible_stage))?;
    if apply_post_boot_onboard_gate(&mut state, &store) {
        writeln!(
            stdout,
            "Model setup is open. Choose Hosted, Local endpoint, or ACP agent."
        )?;
    }
    stdout.flush()?;
    drain_boot_warnings(&mut state, &boot_warnings);
    let Booted {
        session_id,
        workspace_id,
        attach_watermark,
        docs_pool,
        mut live,
    } = booted;

    let (input_tx, mut input_rx) = mpsc::channel::<ClientInput>(256);
    let input_task = spawn_accessible_input(input_tx);
    let (mut width, _) = accessible_viewport();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let repository = repo.to_string_lossy().into_owned();
    let mut presentation = AccessiblePresentation::new(stdout);
    let result = event_loop(
        &mut presentation,
        &mut state,
        &mut width,
        &mut live,
        &mut input_rx,
        &mut ticker,
        session_id,
        workspace_id,
        &repository,
        attach_watermark,
        docs_pool.clone(),
        &mut store,
        paths,
    )
    .await;

    input_task.abort();
    live.shutdown();
    if let Some(pool) = docs_pool {
        pool.close().await;
    }
    // Accessible mode never enters the alternate screen, but it reaches the
    // same reconnect exhaustion — and a screen reader announcing a Rust
    // backtrace is the worst version of this defect, not an exempt one.
    report_daemon_unavailable(result)
}

fn ascii_stage(stage: &str) -> String {
    sanitize_accessible_text(stage)
        .replace('…', "...")
        .replace('·', "-")
}

/// Honest completed-boot copy. A connected daemon and loaded workspace are not
/// enough to claim readiness when no configured profile can start a run.
fn splash_ready_stage(workspace_name: &str, runnable_count: usize) -> String {
    if runnable_count == 0 {
        "set up a model to continue".to_owned()
    } else {
        format!("{workspace_name} is ready")
    }
}

/// Apply the single shared first-run decision after boot for both graphical
/// and accessible presentations. The durable preference records only the
/// explicit "skip forever" choice; successful setup is represented by the
/// authoritative runnable projection itself, avoiding a stale second source
/// of truth.
fn apply_post_boot_onboard_gate(state: &mut AppState, store: &SessionStore) -> bool {
    let open = state.runnable_models.is_empty() && !store.onboard_skipped;
    if open {
        reduce(state, Action::OpenOnboard);
    }
    open
}

fn accessible_viewport() -> (u16, u16) {
    let parse = |name: &str, fallback: u16| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(fallback)
    };
    (parse("COLUMNS", 80), parse("LINES", 24))
}

/// Input from either presentation. Keeping cooked lines distinct from
/// crossterm events means accessible mode never has to synthesize key events
/// (or initialize crossterm at all).
enum ClientInput {
    Terminal(CrosstermEvent),
    AccessibleLine(String),
}

/// The terminal-specific edge of the otherwise shared client loop.
trait Presentation {
    fn viewport(&self) -> (u16, u16);
    fn capabilities_message(&self) -> codypendent_protocol::UiWireMessage;
    fn draw(&mut self, state: &AppState, force_prompt: bool) -> io::Result<()>;
    fn copy_text(&mut self, text: &str) -> io::Result<()>;
    fn wants_periodic_draw(&self) -> bool {
        false
    }
}

struct InteractivePresentation<'a> {
    guard: &'a mut TerminalGuard,
    theme: &'a Theme,
}

impl Presentation for InteractivePresentation<'_> {
    fn viewport(&self) -> (u16, u16) {
        crossterm::terminal::size().unwrap_or((80, 24))
    }

    fn capabilities_message(&self) -> codypendent_protocol::UiWireMessage {
        let (width, height) = self.viewport();
        let depth = match ColorDepth::detect() {
            ColorDepth::TrueColor => 24,
            ColorDepth::Ansi256 => 8,
            ColorDepth::Ansi16 => 4,
            ColorDepth::Monochrome => 1,
        };
        terminal_capabilities_message(width, height, depth)
    }

    fn draw(&mut self, state: &AppState, _force_prompt: bool) -> io::Result<()> {
        let title = if let Some(title) = &state.session_title {
            format!("codypendent — {title}")
        } else if let Some(run) = state.selected_run() {
            format!("codypendent — {}", run.objective)
        } else {
            "codypendent".to_string()
        };
        let _ = codypendent_tui::terminal::set_terminal_title(&title);

        self.guard
            .terminal_mut()
            .draw(|frame| render(frame, state, self.theme))
            .map(|_| ())
    }

    fn copy_text(&mut self, text: &str) -> io::Result<()> {
        // OSC 52 is terminal-native and avoids a platform clipboard dependency.
        // Bound before encoding: a malicious provider failure must not turn one
        // copy gesture into an unbounded control sequence.
        const MAX_COPY_BYTES: usize = 64 * 1024;
        let end = text
            .char_indices()
            .take_while(|(index, character)| index + character.len_utf8() <= MAX_COPY_BYTES)
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0)
            .min(text.len());
        let encoded = base64::engine::general_purpose::STANDARD.encode(&text.as_bytes()[..end]);
        let backend = self.guard.terminal_mut().backend_mut();
        write!(backend, "\x1b]52;c;{encoded}\x07")?;
        backend.flush()
    }
}

/// Stable cooked presentation. The first draw prints the complete linear view;
/// subsequent draws emit only the line window that changed. This retains
/// append-only, cursor-free output for screen readers while a streaming token
/// no longer replays the entire conversation once per refresh.
struct AccessiblePresentation<W: Write> {
    output: W,
    last_snapshot: Option<String>,
    last_emitted_at: Option<Instant>,
}

impl<W: Write> AccessiblePresentation<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            last_snapshot: None,
            last_emitted_at: None,
        }
    }
}

impl<W: Write> Presentation for AccessiblePresentation<W> {
    fn viewport(&self) -> (u16, u16) {
        accessible_viewport()
    }

    fn capabilities_message(&self) -> codypendent_protocol::UiWireMessage {
        let (width, height) = self.viewport();
        accessible_terminal_capabilities_message(width, height)
    }

    fn draw(&mut self, state: &AppState, force_prompt: bool) -> io::Result<()> {
        // The refresh budget is checked BEFORE the snapshot is built, because
        // building it is the expensive part: `accessible_snapshot` renders the
        // whole session — transcript, cards, pickers — into a fresh String.
        // Checking it afterwards meant a streaming run rebuilt that entire
        // string for every token and then threw almost all of them away.
        // Nothing here is observable: both orders return without emitting.
        if !force_prompt
            && self
                .last_emitted_at
                .is_some_and(|emitted| emitted.elapsed() < ACCESSIBLE_REFRESH)
        {
            return Ok(());
        }
        let snapshot = accessible_snapshot(state);
        if self.last_snapshot.as_deref() == Some(snapshot.as_str()) {
            if force_prompt {
                writeln!(self.output)?;
                write!(self.output, "command> ")?;
                self.output.flush()?;
            }
            return Ok(());
        }
        writeln!(self.output, "\n--- accessible update ---")?;
        if let Some(previous) = self.last_snapshot.as_deref() {
            writeln!(
                self.output,
                "{}",
                accessible_snapshot_delta(previous, &snapshot)
            )?;
        } else {
            writeln!(self.output, "{snapshot}")?;
        }
        write!(self.output, "command> ")?;
        self.output.flush()?;
        self.last_snapshot = Some(snapshot);
        self.last_emitted_at = Some(Instant::now());
        Ok(())
    }

    fn copy_text(&mut self, text: &str) -> io::Result<()> {
        // Cooked mode cannot reach a terminal clipboard. Treat copied card
        // contents as untrusted before echoing them: notes, patches, and ACP
        // output may contain ANSI/OSC controls. Keep the fallback bounded to
        // the same budget as the interactive OSC 52 path.
        const MAX_COPY_BYTES: usize = 64 * 1024;
        let safe = sanitize_accessible_text(text);
        let end = safe
            .char_indices()
            .take_while(|(index, character)| index + character.len_utf8() <= MAX_COPY_BYTES)
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0)
            .min(safe.len());
        writeln!(
            self.output,
            "\nCopied card (terminal clipboard unavailable):\n{}",
            &safe[..end]
        )?;
        self.output.flush()
    }

    fn wants_periodic_draw(&self) -> bool {
        true
    }
}

/// Minimal, line-aligned delta between two cooked snapshots.
///
/// A stable suffix (normally the controls/prompt chrome) is omitted along with
/// the stable prefix. Removed lines are stated explicitly so a screen reader
/// never has to infer deletion from silence.
fn accessible_snapshot_delta(previous: &str, next: &str) -> String {
    let previous_lines: Vec<&str> = previous.lines().collect();
    let next_lines: Vec<&str> = next.lines().collect();
    let prefix = previous_lines
        .iter()
        .zip(&next_lines)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = previous_lines[prefix..]
        .iter()
        .rev()
        .zip(next_lines[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let previous_end = previous_lines.len().saturating_sub(suffix);
    let next_end = next_lines.len().saturating_sub(suffix);
    let removed = previous_end.saturating_sub(prefix);
    let mut lines = Vec::new();
    if removed > 0 {
        lines.push(format!("{removed} previous line(s) replaced."));
    }
    lines.extend(
        next_lines[prefix..next_end]
            .iter()
            .map(|line| (*line).to_owned()),
    );
    if lines.is_empty() {
        "No visible line changes.".to_owned()
    } else {
        lines.join("\n")
    }
}

fn spawn_accessible_input(tx: mpsc::Sender<ClientInput>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(ClientInput::AccessibleLine(line)).await.is_err() {
                break;
            }
        }
    })
}

/// The boot stages the D2 splash narrates, published by [`boot_phase`] over a
/// `watch` channel; the splash loop in [`run`] draws the latest stage each
/// tick. `Reconciling` carries the build-mismatch warning text for the spinner
/// line — what used to be a cooked-mode `eprintln!` — so nothing prints to
/// stderr from inside the alternate screen. The warning text ALSO lands in
/// [`BootWarnings`]: a `watch` retains only the latest value, so the stage
/// alone would lose a warning the moment the next stage publishes.
#[derive(Clone)]
enum SplashStage {
    StartingDaemon,
    Connecting,
    Reconciling(String),
    RestoringSession,
    LoadingWorkspace,
}

impl SplashStage {
    fn text(&self) -> String {
        match self {
            Self::StartingDaemon => "starting daemon…".to_owned(),
            Self::Connecting => "connecting…".to_owned(),
            Self::Reconciling(message) => message.clone(),
            Self::RestoringSession => "restoring session…".to_owned(),
            Self::LoadingWorkspace => "loading workspace…".to_owned(),
        }
    }
}

/// Boot diagnostics (D2) collected BESIDE the [`SplashStage`] watch channel:
/// a `watch` retains only the LATEST value, so a reconcile warning sent as
/// `SplashStage::Reconciling` is overwritten by the very next stage
/// (`RestoringSession`/`LoadingWorkspace`) almost immediately — and a boot too
/// fast to draw a single frame never shows it at all. This shared vec keeps
/// every diagnostic (reconcile warnings from `warn_stage`, the projection
/// loaders' best-effort failure notes) so the splash can draw them under the
/// stage line and [`run`] can surface each as a post-boot `Action::Notice` —
/// what used to be a cooked-mode `eprintln!`, which the early alternate
/// screen would swallow.
type BootWarnings = Arc<Mutex<Vec<String>>>;

/// Push one boot diagnostic onto the shared [`BootWarnings`] vec.
fn push_boot_warning(warnings: &BootWarnings, message: String) {
    warnings
        .lock()
        .expect("boot warnings mutex poisoned")
        .push(message);
}

/// Drain the collected boot diagnostics into post-boot TUI notices (D2):
/// ALWAYS when any were collected, whether or not the splash drew a frame —
/// a reconcile warning or a loader failure note is meaningful after boot
/// regardless (the pre-splash behavior was a persistent stderr print). One
/// persistent issue per diagnostic.
fn drain_boot_warnings(state: &mut AppState, warnings: &BootWarnings) {
    let collected = std::mem::take(&mut *warnings.lock().expect("boot warnings mutex poisoned"));
    for warning in collected {
        reduce(state, Action::Issue(warning));
    }
}

/// The TUI exhausted its reconnect budget and the daemon is still not
/// answering — the one failure that ends an otherwise healthy session from
/// outside it.
///
/// It is a distinct type purely so [`report_daemon_unavailable`] can recognise
/// it after the terminal is restored. `event_loop` used to return the last
/// connect error bare, and `main`'s `Termination` impl prints an
/// `anyhow::Error` with `{:?}` — Debug, which includes the captured backtrace.
/// The user's reward for `kill`ing a daemon was 36 stack frames ending in
/// `_start`, with no mention of the daemon and nothing to act on. The crash-log
/// machinery does not cover this either: it hooks panics, and this is not one,
/// so `<data_dir>/logs/` stays empty and the next investigator reads that as
/// "aborted or OS-killed".
#[derive(Debug)]
struct DaemonUnavailable {
    socket: PathBuf,
    cause: String,
}

impl std::fmt::Display for DaemonUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "lost the connection to the codypendent daemon ({})",
            self.socket.display()
        )
    }
}

impl std::error::Error for DaemonUnavailable {}

/// If `result` failed because the daemon went away, print an actionable
/// paragraph and swallow the error; otherwise hand it back unchanged.
///
/// Called by both clients AFTER the terminal guard is dropped, so the text
/// lands on the restored cooked screen rather than the alternate one. Exit
/// status stays 0: the session ended for a reason the user has now been told
/// about in full, and the alternative — returning to `main` — is the backtrace
/// dump this exists to prevent.
fn report_daemon_unavailable(result: anyhow::Result<()>) -> anyhow::Result<()> {
    let Err(error) = result else {
        return Ok(());
    };
    let Some(lost) = error.downcast_ref::<DaemonUnavailable>() else {
        return Err(error);
    };
    eprintln!("codypendent: {lost}.");
    eprintln!("  reason: {}", lost.cause);
    eprintln!();
    eprintln!("Your session and its history are safe — they live in the daemon's");
    eprintln!("database, not in this process. To carry on:");
    eprintln!("  codypendent daemon status     # is it running?");
    eprintln!("  codypendent daemon start      # start it again");
    eprintln!("  codypendent                   # reopen the TUI; the session is restored");
    eprintln!();
    eprintln!("If it keeps stopping, its log is the place to look: `codypendent doctor`.");
    Ok(())
}

/// Everything [`boot_phase`] produces that the event loop and teardown still
/// need, bundled so [`run`]'s splash loop stays small.
struct Booted {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    attach_watermark: u64,
    docs_pool: Option<sqlx::SqlitePool>,
    live: LiveIo,
}

/// The socket tasks and channels for the session currently shown by the TUI.
/// Keeping them together makes an in-place conversation switch an atomic
/// transport handoff: the old connection (and all of its old-session
/// forwarders) is dropped as one unit after the replacement is attached.
struct LiveIo {
    client_id: ClientId,
    out_tx: mpsc::Sender<Envelope>,
    event_rx: mpsc::Receiver<ReaderSignal>,
    query_tx: mpsc::Sender<ReaderSignal>,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::JoinHandle<()>,
}

impl LiveIo {
    fn start(conn: Connection) -> (Self, std::collections::VecDeque<Envelope>) {
        let (read_half, write_half, pending, client_id) = conn.into_split();
        let (out_tx, out_rx) = mpsc::channel::<Envelope>(256);
        let (event_tx, event_rx) = mpsc::channel::<ReaderSignal>(256);
        let query_tx = event_tx.clone();
        let reader = tokio::spawn(read_loop(
            read_half,
            event_tx.clone(),
            out_tx.clone(),
            client_id,
        ));
        let writer = tokio::spawn(write_loop(write_half, out_rx, event_tx));
        (
            Self {
                client_id,
                out_tx,
                event_rx,
                query_tx,
                reader,
                writer,
            },
            pending,
        )
    }

    fn shutdown(self) {}
}

impl Drop for LiveIo {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// Every boot step before the event loop (D2): daemon ensure, handshake,
/// build-mismatch reconcile, session resolve, catch-up fold, projection
/// seeding, and the socket reader/writer tasks. Runs as a pinned future
/// inside [`run`]'s splash loop and publishes each stage over `stage_tx`;
/// `state` and `store` are seeded in place so nothing needs moving back out.
/// By the time this runs the terminal is already in the alternate screen, so
/// diagnostics go to the stage channel (spinner line) and the shared
/// `boot_warnings` vec (splash warning lines + post-boot notices) — never to
/// stderr.
async fn boot_phase(
    paths: &RuntimePaths,
    repo: &Path,
    state: &mut AppState,
    store: &mut SessionStore,
    stage_tx: &watch::Sender<SplashStage>,
    boot_warnings: &BootWarnings,
) -> anyhow::Result<Booted> {
    commands::ensure_daemon(paths).await?;
    let _ = stage_tx.send(SplashStage::Connecting);
    let mut conn = Connection::connect(&paths.socket_path).await?;
    let resume = store
        .resume_token
        .clone()
        .map(codypendent_protocol::ResumeToken);
    // Advertise the client's own per-build id (DR1/T4/T9) so both halves of
    // the handshake speak the same id vocabulary — the daemon-auto-restart
    // feature's detection point is the `hello.build_id` this returns.
    let hello = conn
        .handshake("codypendent-tui", codypendent_protocol::BUILD_ID, resume)
        .await?;
    // Store the daemon-issued token so the NEXT launch resumes this client
    // identity (best-effort; an absent token just means a fresh identity).
    if let Some(token) = hello.resume_token {
        store.resume_token = Some(token.0);
        store.save(paths);
    }

    // DR5: detect + (safely) reconcile a daemon build mismatch, right after
    // the handshake and BEFORE resolving/creating a session — the detection
    // point the design doc calls for. On a matching build (the overwhelming
    // common case) this is a single string compare and zero extra round
    // trips; `conn` is left untouched. On a genuine mismatch it may stop the
    // OLD daemon, spawn a fresh one, and reconnect — but ONLY when confirmed
    // idle (never while a run is active, never on uncertainty); otherwise it
    // warns and continues on the existing daemon. A restart that fails, or a
    // reconnect that still mismatches, is a legible hard error — this
    // function never enters the TUI against a broken or half-restarted
    // daemon.
    let resume_for_reconnect = store
        .resume_token
        .clone()
        .map(codypendent_protocol::ResumeToken);
    let mut restart_ops =
        crate::restart::LiveRestartOps::new(paths, conn, "codypendent-tui", resume_for_reconnect);
    // D2: these warnings used to be cooked-mode `eprintln!`s; the terminal is
    // in the alternate screen by now, so they become splash stage text (the
    // spinner line) AND persist in `boot_warnings` — the `watch` stage channel
    // alone would drop each one the moment the next stage publishes.
    let mut warn_stage = |message: &str| {
        push_boot_warning(boot_warnings, message.to_owned());
        let _ = stage_tx.send(SplashStage::Reconciling(message.to_owned()));
    };
    let reconcile_outcome = crate::restart::reconcile_interactive(
        paths,
        codypendent_protocol::BUILD_ID,
        &hello.build_id,
        &mut restart_ops,
        &mut warn_stage,
    )
    .await?;
    // D3: the header shows the running daemon's build id — the handshaken
    // one, unless the reconcile just restarted the daemon onto THIS build
    // (`reconnect_and_assert` guarantees the new daemon matches the client).
    state.daemon_build_id = Some(match reconcile_outcome {
        crate::restart::ReconcileOutcome::Restarted => codypendent_protocol::BUILD_ID.to_owned(),
        _ => hello.build_id.clone(),
    });
    let mut conn = restart_ops.into_connection();

    // Assembly/integration failures are actionable product state, not log-only
    // diagnostics. The daemon returns sanitized, de-duplicated summaries; feed
    // them into the existing persistent Issues rail before the welcome gate.
    match crate::client::daemon_status(&paths.socket_path).await {
        Ok(status) => {
            for issue in status.integration_issues {
                reduce(state, Action::Issue(issue));
            }
        }
        Err(error) => push_boot_warning(
            boot_warnings,
            format!("could not read daemon integration health: {error}"),
        ),
    }

    let _ = stage_tx.send(SplashStage::RestoringSession);
    let (session_id, workspace_id, catchup) =
        resolve_or_create_session(&mut conn, store, paths, repo).await?;

    // Seed the state from catch-up, then from any live event that outraced the
    // attach reply and was buffered during setup — both before the loop reads a
    // single new frame, so no event is dropped or reordered.
    let mut attach_watermark =
        fold_catchup_with_history(state, &mut conn, session_id, catchup, boot_warnings).await;
    let (live, pending) = LiveIo::start(conn);
    for envelope in pending {
        if let Payload::Event(event) = envelope.payload {
            // A paged snapshot restore and the live forwarder share the socket.
            // Events that arrived while the history pages were being read are
            // buffered by `Connection`; ignore any already covered by the page
            // watermark so reopening a busy session cannot duplicate turns.
            if event.sequence > attach_watermark {
                attach_watermark = event.sequence;
                reduce(state, Action::DaemonEvent(Box::new(event)));
            }
        }
    }

    // STEP 2.6 + Phase 4 client wiring: seed the Skill Studio, memory browser,
    // Docs Studio, and code-graph edge-inspector projections. This reads the
    // knowledge fabric's authoritative rows directly from SQLite (WAL allows
    // concurrent reads alongside the daemon) and maps them into the TUI's plain
    // projection structs — the one place the two worlds meet, done here (not in
    // the pure TUI crate, which never depends on `codypendent-knowledge`). A read
    // failure collects a diagnostic (surfaced on the splash and as a post-boot
    // notice) and continues with empty lists; it never fails the TUI.
    let _ = stage_tx.send(SplashStage::LoadingWorkspace);
    let mut loader_warnings = Vec::new();
    let projections = load_knowledge(paths, workspace_id, repo, &mut loader_warnings).await;
    state.skills = projections.skills;
    state.memories = projections.memories;
    state.docs = projections.docs;
    state.edges = projections.edges;
    state.edge_total = projections.edge_total;
    state.blackboard = projections.blackboard;
    // MP1: seed the model-picker projection (models.toml + any measured
    // profile from `model_profiles`), exactly like the projections above.
    state.models = load_model_cards(paths, &mut loader_warnings).await;
    refresh_runnable_models(state, None);
    // Task 8: seed the provider-catalog picker projection (the built-in
    // catalog + any user `providers.toml`), exactly like `load_model_cards`.
    state.providers = load_provider_cards(paths, &mut loader_warnings).await;
    // P12: which local model servers are answering right now, so first-run
    // setup can say "Ollama is answering on localhost:11434" instead of
    // listing three servers as if any might be running. Loopback, a TCP
    // connect only, every candidate at once, bounded by `PROBE_TIMEOUT`.
    state.local_endpoints = crate::local_endpoints::probe_catalog_defaults(paths).await;
    // Rubric 6 TUI wiring: seed the `/council` browser projection (persisted
    // councils.toml definitions), exactly like `load_model_cards` above.
    state.councils = load_council_cards(paths, &mut loader_warnings);
    state.council_results = load_council_result_cards(paths, &mut loader_warnings);
    // D1 (/keys): seed the API-key status projection — auth.json entries +
    // models.toml `api_key_env` declarations (the tui crate does no I/O, so
    // the harness reads the files and folds the projection as an Action, the
    // same Action re-fired after every key write and daemon restart).
    {
        let (models, tavily, voice) = load_key_statuses(paths, &mut loader_warnings);
        reduce(
            state,
            Action::ApiKeyStatusesLoaded {
                models,
                tavily,
                voice,
            },
        );
    }
    // Phase 5 STEP 5.2 + T8: seed the workflow-graph view by compiling the
    // repository's declared workflow manifests, then overlay each workflow's
    // LATEST durable run — its per-node live state, measured cost, and
    // failure/block reason — from the knowledge db (WAL allows a concurrent read
    // alongside the daemon). A malformed manifest collects a diagnostic and is
    // skipped; a db-open failure degrades to the compiled (pre-run) view —
    // neither fails the TUI.
    {
        let overlay_pool = knowledge_db::open(&paths.data_dir.join("codypendent.db"))
            .await
            .ok();
        let user_workflows = paths.data_dir.join("workflows");
        state.workflow = load_workflows(
            repo,
            Some(&user_workflows),
            overlay_pool.as_ref(),
            &mut loader_warnings,
        )
        .await;
    }
    // Publish the loaders' collected diagnostics alongside the reconcile
    // warnings so the splash can draw them and `run` notices them post-boot.
    boot_warnings
        .lock()
        .expect("boot warnings mutex poisoned")
        .append(&mut loader_warnings);

    // A persistent read pool for live document editing (Phase 4 STEP 4.3): the
    // event loop seeds a document's client replica from it and re-reads the
    // review rail's suggestions when a sync arrives. WAL mode lets this read
    // concurrently with the daemon. `None` on failure — document editing then
    // degrades to converging from live syncs alone (no seed, empty review rail).
    let docs_pool = knowledge_db::open(&paths.data_dir.join("codypendent.db"))
        .await
        .ok();

    Ok(Booted {
        session_id,
        workspace_id,
        attach_watermark,
        docs_pool,
        live,
    })
}

/// What [`GapTracker::on_event`] asks the harness to do with one live event.
#[derive(Debug)]
enum GapAction {
    /// Nothing to fold — the event was a duplicate, a sentinel already handled,
    /// or it was buffered pending an in-flight repair.
    Ignore,
    /// Fold this event into state now; the watermark has already advanced.
    /// Boxed to keep the action small (a `SessionEvent` is a large payload).
    Apply(Box<SessionEvent>),
    /// Re-attach with `last_seen_sequence` to replay a missed span (a detected
    /// gap, a buffer overflow, or — via [`GapTracker::on_tick`] — a repair
    /// timeout). Any events to fold afterwards are held inside the tracker.
    Reattach { last_seen_sequence: u64 },
}

/// The result of feeding a gap-repair catch-up reply to
/// [`GapTracker::on_catchup`].
struct CatchupDrain {
    /// Buffered events to fold now, in ascending sequence order, deduped
    /// against the watermark the catch-up advanced.
    apply: Vec<SessionEvent>,
    /// A follow-up re-attach `last_seen_sequence`, set when the buffered tail
    /// still revealed a missing span (more loss occurred while repairing).
    reattach: Option<u64>,
}

/// The reconnect / gap-repair state machine for the TUI's live event fold.
///
/// This is the code that keeps a lagged client from losing an event the daemon
/// dropped from its live fan-out (worst case, an `ApprovalRequested`). It is
/// deliberately **pure** — it owns no socket and no [`AppState`], only the
/// sequence bookkeeping — so the harness can drive it with I/O while the tests
/// drive it directly. The harness feeds it every live event, every gap-repair
/// catch-up reply, and every timer tick, and performs the [`GapAction`] /
/// [`CatchupDrain`] it returns.
///
/// # Sequence numbering
///
/// The daemon assigns ledger sequences 1-based (`COALESCE(MAX(sequence),0)+1`),
/// and even ephemeral events (presence) are appended before fan-out, so every
/// live event on the wire carries a sequence `>= 1`. Sequence `0` is therefore
/// a **sentinel** ("no ledger position"), never a real event: it cannot be a
/// duplicate and cannot open or fill a gap, so it is folded straight through in
/// any state (FP-2c — previously a sentinel arriving mid-repair was buffered and
/// then silently discarded).
struct GapTracker {
    /// The highest real ledger sequence folded so far (the catch-up + live
    /// dedup watermark). `0` means "no baseline yet" — the first live event
    /// seeds it without gap detection.
    last_seen: u64,
    /// Events held back while a repair is in flight. They must NOT fold (or
    /// advance `last_seen`) before the replay lands: advancing the watermark to
    /// the gap-revealing event first made every replayed event read as a
    /// duplicate and silently discarded the whole repair (the original C6 bug).
    gap_buffer: Vec<SessionEvent>,
    /// Whether a gap repair is in flight (awaiting a catch-up reply).
    repairing: bool,
    /// When the in-flight repair should be abandoned for a fresh re-attach
    /// (FP-2b). `None` when not repairing.
    repair_deadline: Option<Instant>,
}

impl GapTracker {
    /// Start tracking from the attach-time watermark (the catch-up's `through`).
    fn new(attach_watermark: u64) -> Self {
        Self {
            last_seen: attach_watermark,
            gap_buffer: Vec::new(),
            repairing: false,
            repair_deadline: None,
        }
    }

    /// The current dedup/catch-up watermark. A proactive re-attach (e.g. adding a
    /// Document subscription on the first edit, Phase 4 STEP 4.3) carries this so
    /// the daemon replays only what this client has not already folded.
    fn last_seen(&self) -> u64 {
        self.last_seen
    }

    /// Feed one live event. `now` anchors the repair deadline when a repair
    /// starts. Returns what the harness should do with it.
    fn on_event(&mut self, event: SessionEvent, now: Instant) -> GapAction {
        // Sentinel (see the type docs): no position to dedup or order by, so
        // fold it straight through, never buffered/discarded, even mid-repair.
        if event.sequence == 0 {
            return GapAction::Apply(Box::new(event));
        }
        // A duplicate of something already folded (catch-up + live overlap).
        if event.sequence <= self.last_seen {
            return GapAction::Ignore;
        }
        if self.repairing {
            // FP-2a: bound the buffer. On overflow, drop the incremental replay
            // and re-attach from `last_seen` — the ledger re-delivers the whole
            // span, so this loses nothing and can never grow without bound.
            if self.gap_buffer.len() >= MAX_GAP_BUFFER {
                self.gap_buffer.clear();
                self.repair_deadline = Some(now + REPAIR_TIMEOUT);
                return GapAction::Reattach {
                    last_seen_sequence: self.last_seen,
                };
            }
            // Hold ordering: nothing folds past the missing span until the
            // replay has landed.
            self.gap_buffer.push(event);
            return GapAction::Ignore;
        }
        if self.last_seen != 0 && event.sequence > self.last_seen + 1 {
            // Gap: re-attach to replay the missed span. Buffer this event
            // instead of folding it now — it is out of order until the span
            // before it has been replayed. Crucially, `last_seen` is NOT
            // advanced to this event, so the re-attach replays from the true
            // watermark (reverting that is the C6 regression the tests pin).
            self.repairing = true;
            self.repair_deadline = Some(now + REPAIR_TIMEOUT);
            let last_seen_sequence = self.last_seen;
            self.gap_buffer.push(event);
            return GapAction::Reattach { last_seen_sequence };
        }
        // In order (or the first event past a 0 baseline): fold it and advance.
        self.last_seen = self.last_seen.max(event.sequence);
        GapAction::Apply(Box::new(event))
    }

    /// Feed a gap-repair catch-up reply's `through` watermark (the harness has
    /// already folded the catch-up's own events into state). Advances the
    /// watermark, ends the repair, and drains the buffered events in order,
    /// asking for another repair if the buffered tail still reveals a gap.
    fn on_catchup(&mut self, through: u64, now: Instant) -> CatchupDrain {
        self.last_seen = self.last_seen.max(through);
        self.repairing = false;
        self.repair_deadline = None;

        let mut buffered = std::mem::take(&mut self.gap_buffer);
        buffered.sort_by_key(|event| event.sequence);

        let mut apply = Vec::new();
        for (index, event) in buffered.iter().enumerate() {
            if event.sequence <= self.last_seen {
                continue; // already folded (via the catch-up or an earlier event)
            }
            if event.sequence > self.last_seen + 1 {
                // Still a hole: repair again, keeping this event and the tail.
                self.repairing = true;
                self.repair_deadline = Some(now + REPAIR_TIMEOUT);
                self.gap_buffer = buffered[index..].to_vec();
                return CatchupDrain {
                    apply,
                    reattach: Some(self.last_seen),
                };
            }
            self.last_seen = event.sequence;
            apply.push(event.clone());
        }
        CatchupDrain {
            apply,
            reattach: None,
        }
    }

    /// Feed a timer tick. Returns a re-attach `last_seen_sequence` when an
    /// in-flight repair has outlived [`REPAIR_TIMEOUT`] (FP-2b): drop the stale
    /// buffer and re-drive the catch-up from the watermark rather than wedging
    /// the client in `repairing` forever.
    fn on_tick(&mut self, now: Instant) -> Option<u64> {
        if self.repairing {
            if let Some(deadline) = self.repair_deadline {
                if now >= deadline {
                    self.gap_buffer.clear();
                    self.repair_deadline = Some(now + REPAIR_TIMEOUT);
                    return Some(self.last_seen);
                }
            }
        }
        None
    }
}

/// Send an `AttachSession` re-attach carrying `last_seen_sequence`, so the
/// daemon replaces this connection's forwarder and replies with a `Catchup`
/// windowed to the missed span. A closed writer is returned to the loop: the
/// operator must see a reconnect/error instead of a silent gap-repair no-op.
async fn send_reattach(
    out_tx: &mpsc::Sender<Envelope>,
    client_id: ClientId,
    session_id: SessionId,
    last_seen_sequence: u64,
    subscriptions: &[Subscription],
) -> anyhow::Result<()> {
    let attach = reattach_envelope(client_id, session_id, last_seen_sequence, subscriptions);
    out_tx
        .send(attach)
        .await
        .map_err(|_| anyhow!("daemon connection closed while requesting transcript repair"))
}

fn reattach_envelope(
    client_id: ClientId,
    session_id: SessionId,
    last_seen_sequence: u64,
    subscriptions: &[Subscription],
) -> Envelope {
    command_envelope(
        client_id,
        CommandBody::AttachSession {
            session_id,
            last_seen_sequence: Some(last_seen_sequence),
            subscriptions: subscriptions.to_vec(),
            requested_role: ClientRole::Controller,
            repository: None,
        },
    )
}

/// The render/reduce/dispatch loop. Broken out from [`run`] so the setup and
/// teardown read linearly and the borrow of every loop input is explicit.
#[allow(clippy::too_many_arguments)]
async fn event_loop<P: Presentation>(
    presentation: &mut P,
    state: &mut AppState,
    width: &mut u16,
    live: &mut LiveIo,
    input_rx: &mut mpsc::Receiver<ClientInput>,
    ticker: &mut tokio::time::Interval,
    mut session_id: SessionId,
    workspace_id: WorkspaceId,
    repository: &str,
    attach_watermark: u64,
    docs_pool: Option<sqlx::SqlitePool>,
    // Persist the replacement session after an in-place "New Conversation"
    // transport swap so the next launch resumes the newly selected thread.
    store: &mut SessionStore,
    paths: &RuntimePaths,
) -> anyhow::Result<()> {
    let capabilities = presentation.capabilities_message();
    if live
        .out_tx
        .send(remote_ui_envelope(live.client_id, session_id, capabilities))
        .await
        .is_err()
    {
        return Err(anyhow!(
            "daemon connection closed during Remote UI capability setup"
        ));
    }
    presentation.draw(state, false)?;

    // Tracks live-fan-out sequence continuity and drives gap repair. Live
    // fan-out is lossy for a slow client (the daemon skips `Lagged` spans), so a
    // jump past `last_seen + 1` means events were dropped from the live view and
    // a re-attach with `last_seen_sequence` replays exactly the gap. The
    // decision logic is extracted into [`GapTracker`] — a pure unit owning no
    // I/O and no `AppState` — so the reconnect/repair path (the code protecting
    // an `ApprovalRequested` from being lost under lag) is deterministically
    // testable; this loop performs the I/O the tracker asks for.
    let mut tracker = GapTracker::new(attach_watermark);

    // The client's live subscription set: seeded with the session views and grown
    // with a `Document` subscription the first time an edit targets one, so a
    // gap-repair re-attach preserves the documents this client is editing (Phase 4
    // STEP 4.3).
    let mut subscriptions = default_subscriptions();
    // Per-open-document client replicas that consume the sync stream. This map
    // is cleared on reconnect (the replicas reseed from the fresh stream) while
    // `subscriptions` deliberately survives, so presence here does NOT imply
    // "not yet subscribed" — the subscription list is its own authority below.
    let mut replicas: HashMap<DocumentId, DocumentReplica> = HashMap::new();
    // Correlate empty blackboard baselines back to the run they were requested
    // for (the reply carries only command_id when `items` is empty).
    let mut blackboard_reads: HashMap<CommandId, String> = HashMap::new();
    // --- voice host (voice v1, rubric 8) -----------------------------------
    // Probes for a recorder ONCE here (the result is cached for the process)
    // and owns every subprocess voice needs, so `codypendent-tui` stays a pure
    // render/reduce crate. With nothing configured this is entirely inert.
    let mut voice = crate::voice::VoiceHost::new(paths);
    // Captured voice notes awaiting their `ArtifactStored` reply, keyed by the
    // `PutArtifact` command that uploaded them; the value is the measured
    // capture duration, carried into the audio block.
    let mut pending_voice: HashMap<CommandId, PendingVoiceUpload> = HashMap::new();
    // Publish replies omit the document id because it is already present in
    // the request. Preserve that exact correlation so the host can construct
    // the ordinary approval card from the reply without guessing.
    let mut pending_document_publishes: HashMap<CommandId, DocumentId> = HashMap::new();
    // Session Library correlation. `SessionSearchResults` carries only the
    // page, so the query it answers (and whether it continues an earlier page)
    // is remembered here; `SessionExported` carries only the artifact, so the
    // session it belongs to is remembered here too. Both are the same
    // correlation discipline as `pending_document_publishes` above.
    // Keyed by the REQUEST's message id, not its command id, so both halves of
    // the outcome can clear it: a `SessionSearchResults` reply and a
    // `CommandRejected` both correlate back to the same message. Keying by
    // command id would leave a refused search's entry — and the overlay's
    // "searching…" state — stuck forever.
    let mut pending_session_searches: HashMap<codypendent_protocol::MessageId, (String, bool)> =
        HashMap::new();
    let mut pending_session_exports: HashMap<CommandId, SessionId> = HashMap::new();
    // Exact first-run request retained until its durable RunStarted arrives.
    // Reconnect retries this same message/command/idempotency identity.
    let mut pending_start_run = PendingStartRunCommand::default();
    // Lifecycle mutations are daemon-owned and can fail after their confirmation
    // overlays have already closed. Retain the request message id so only the
    // matching rejection becomes a persistent issue; unrelated command failures
    // keep their ordinary transient-notice behaviour.
    let mut pending_ui_plugin_commands = PendingUiPluginCommands::default();
    // Command ids of board-scoped `ReadBlackboard`s, so an EMPTY board baseline
    // (a repository whose board has never been written) still clears the pane
    // rather than being mistaken for a workflow-run read.
    let mut board_reads: std::collections::HashSet<CommandId> = std::collections::HashSet::new();
    // MCP warming and webhook listeners finish after boot. Re-read the daemon's
    // sanitized health projection periodically so failures that race initial
    // startup still reach the persistent Issues rail.
    let mut next_integration_health_tick = state.tick.saturating_add(150);
    // Every provider discovery/ACP operation is stamped in the harness. Only
    // the newest terminal result for a provider may reach the reducer or write
    // configuration, so a slow handshake cannot connect an agent after the
    // operator has retried or chosen another model from that provider.
    let mut provider_requests = ProviderRequestGenerations::default();
    // Disk-backed projections are loaded on background tasks so SQLite and
    // workflow-manifest reads never stall keyboard input or daemon events. A
    // single monotonic generation makes the most recent view request the only
    // one allowed to mutate presentation state.
    let mut projection_generation = 0_u64;
    // Snapshot history can require many paged daemon/database reads. Keep that
    // work off the input loop and accept only the latest result for the session
    // that is still attached.
    let mut history_generation = 0_u64;
    let mut session_transitions = SessionTransitionCoordinator::default();

    loop {
        // A CRDT sync needs an async merge (+ a suggestion re-read) that cannot run
        // inside the `select!` arm, so the arm stashes it here and the loop body
        // folds it just after.
        let mut pending_sync: Option<Box<DocumentSync>> = None;
        let mut started_workflow: Option<String> = None;
        let mut connected_acp: Option<(String, String, Result<String, String>)> = None;
        let mut completed_session_transition: Option<Box<SessionTransitionCompletion>> = None;
        let selected = tokio::select! {
            signal = live.event_rx.recv() => PendingActions::One(match signal {
                Some(ReaderSignal::Event(event)) => {
                    // Voice v1 (rubric 8): the host watches the same durable
                    // stream the reducer folds, accumulating each run's
                    // assistant text and speaking it only once that run is
                    // FINISHED. Reading half a sentence aloud is worse than
                    // silence, so nothing is spoken mid-stream.
                    voice.observe_event(&event, state.voice.speak_replies);
                    match tracker.on_event(*event, Instant::now()) {
                        GapAction::Ignore => Action::NoOp,
                        GapAction::Apply(event) => Action::DaemonEvent(event),
                        GapAction::Reattach { last_seen_sequence } => {
                            // Re-attach with the *grown* subscription set (Phase 4
                            // STEP 4.3) so a gap-repair preserves the Document
                            // subscriptions this client added while editing.
                            send_reattach(
                                &live.out_tx,
                                live.client_id,
                                session_id,
                                last_seen_sequence,
                                &subscriptions,
                            )
                            .await?;
                            Action::NoOp
                        }
                    }
                }
                Some(ReaderSignal::Rejected {
                    code,
                    message,
                    correlation_id,
                }) => {
                    if pending_start_run.matches_rejection(correlation_id) {
                        pending_start_run.clear();
                        Action::RunStartRejected {
                            reason: format!("{message} ({code})"),
                        }
                    } else if let Some(command_id) = correlation_id.and_then(|message_id| {
                        pending_voice.iter().find_map(|(command_id, pending)| {
                            (pending.request_message_id == message_id).then_some(*command_id)
                        })
                    }) {
                        pending_voice.remove(&command_id);
                        Action::Notice(format!("voice upload rejected: {message} ({code})"))
                    } else if let Some(pending) =
                        pending_ui_plugin_commands.resolve(correlation_id)
                    {
                        Action::Issue(pending.rejection_message(&code, &message))
                    } else if let Some((query, _)) =
                        correlation_id.and_then(|id| pending_session_searches.remove(&id))
                    {
                        // A refused search must end the library's "searching…"
                        // state. Left alone it would sit there implying a page
                        // is still on its way.
                        Action::SessionSearchFailed {
                            query,
                            reason: format!("{message} ({code})"),
                        }
                    } else {
                        Action::Notice(format!("command rejected: {message} ({code})"))
                    }
                }
                Some(ReaderSignal::RemoteUi(message)) => Action::RemoteUiMessage(message),
                Some(ReaderSignal::UiPlugins {
                    plugins,
                    correlation_id,
                }) => {
                    pending_ui_plugin_commands.resolve(correlation_id);
                    Action::UiPluginsLoaded(plugins)
                }
                // Phase 4 STEP 4.3 live document editing. A sync is merged after the
                // select (it needs an async replica merge + suggestion re-read); the
                // lease replies fold directly.
                Some(ReaderSignal::DocumentSync(sync)) => {
                    pending_sync = Some(sync);
                    Action::NoOp
                }
                Some(ReaderSignal::DocumentLeaseGranted {
                    document_id,
                    lease_id,
                }) => Action::DocumentLeaseGranted {
                    document_id,
                    lease_id,
                },
                Some(ReaderSignal::DocumentLeaseBlocked) => Action::DocumentLeaseBlocked,
                Some(ReaderSignal::DocumentCreated { document_id }) => {
                    Action::DocumentCreated { document_id }
                }
                Some(ReaderSignal::DocumentPublishPrepared {
                    command_id,
                    approval_id,
                    target,
                    changed_files,
                    git_action,
                }) => match pending_document_publishes.remove(&command_id) {
                    Some(document_id) => Action::DocumentPublishPrepared {
                        approval_id,
                        document_id,
                        target,
                        changed_files,
                        git_action,
                    },
                    None => Action::Notice(format!(
                        "publish awaiting approval {approval_id}: {target} · {} file(s) · {git_action}",
                        changed_files.len()
                    )),
                },
                Some(ReaderSignal::ProviderModels { provider_id, request_id, result }) => {
                    if !provider_requests.is_current(&provider_id, request_id) {
                        Action::NoOp
                    } else {
                        match result {
                            Ok((models, origin)) => Action::ProviderModelsLoaded { provider_id, models, origin },
                            Err(reason) => Action::ProviderModelsFailed { provider_id, reason },
                        }
                    }
                },
                Some(ReaderSignal::UnslothRepos(result)) => match result {
                    Ok(repos) => Action::UnslothReposLoaded(
                        repos.into_iter().map(unsloth_repo_card).collect(),
                    ),
                    Err(reason) => Action::UnslothReposFailed(reason),
                },
                Some(ReaderSignal::UnslothQuants { repo_id, result }) => match result {
                    Ok(quants) => Action::UnslothQuantsLoaded {
                        repo_id,
                        quants: quants.into_iter().map(unsloth_quant_card).collect(),
                    },
                    Err(reason) => Action::UnslothQuantsFailed { repo_id, reason },
                },
                Some(ReaderSignal::UnslothPullProgress {
                    repo_id,
                    quant,
                    line,
                }) => Action::UnslothPullProgress {
                    repo_id,
                    quant,
                    line,
                },
                Some(ReaderSignal::UnslothPullFinished {
                    repo_id,
                    quant,
                    result,
                }) => Action::UnslothPullFinished {
                    repo_id,
                    quant,
                    result,
                },
                Some(ReaderSignal::ModelKeyVerified { model_id, result }) => match result {
                    Ok(()) => Action::ModelKeyVerified { model_id, ok: true, reason: String::new() },
                    Err(reason) => Action::ModelKeyVerified { model_id, ok: false, reason },
                },
                Some(ReaderSignal::AcpConnected { display_id, provider_id, request_id, result }) => {
                    if provider_requests.is_current(&provider_id, request_id) {
                        connected_acp = Some((display_id, provider_id, result));
                    }
                    Action::NoOp
                }
                Some(ReaderSignal::ProjectionLoaded { generation, projection }) => {
                    if generation != projection_generation {
                        Action::NoOp
                    } else {
                        apply_projection(state, projection);
                        Action::NoOp
                    }
                }
                Some(ReaderSignal::WorkflowRunStarted { workflow_run_id }) => {
                    started_workflow = Some(workflow_run_id);
                    Action::Notice("workflow started — attaching live view".to_owned())
                }
                Some(ReaderSignal::WorkflowSnapshot(snapshot)) => {
                    workflow_snapshot_action(*snapshot)
                }
                Some(ReaderSignal::WorkflowEvent(event)) => workflow_event_action(event),
                Some(ReaderSignal::BlackboardItems { command_id, items }) => {
                    if board_reads.remove(&command_id) {
                        // The repository task board (rubric 10): the same command
                        // and the same rows, read at board scope.
                        Action::BoardLoaded(wire_board_cards(&items))
                    } else {
                        let workflow_run_id = blackboard_reads
                            .remove(&command_id)
                            .or_else(|| items.first().map(|item| item.workflow_run_id.clone()));
                        match workflow_run_id {
                            Some(workflow_run_id) => {
                                // The request correlation is the authority for
                                // this baseline. A malformed/stale response may
                                // not smuggle another run's rows into the pane.
                                let items = items
                                    .into_iter()
                                    .filter(|item| item.workflow_run_id == workflow_run_id)
                                    .collect::<Vec<_>>();
                                Action::BlackboardLoaded {
                                    items: wire_blackboard_cards(state, &items),
                                    workflow_run_id,
                                }
                            }
                            None => Action::NoOp,
                        }
                    }
                }
                Some(ReaderSignal::BlackboardPosted(item)) => {
                    // One live channel serves both surfaces: a board card carries
                    // `board_scope`, a workflow artifact does not, so the delivery
                    // routes itself without a second subscription kind.
                    if item.board_scope.is_some() {
                        Action::BoardCardUpdated {
                            card: wire_board_card(&item),
                            superseded: item.superseded_by.is_some(),
                        }
                    } else {
                        let label = workflow_label_for_run(state, &item.workflow_run_id);
                        Action::BlackboardItemUpdated(wire_blackboard_item_card(label, &item))
                    }
                }
                Some(ReaderSignal::CouncilProgress { name, result_id, phase, occurred_at, message, active_subagents }) => {
                    Action::CouncilProgress {
                        name,
                        result_id,
                        phase,
                        occurred_at,
                        message,
                        active_subagents,
                    }
                }
                Some(ReaderSignal::CouncilRunFinished { name, result }) => {
                    let result = match result {
                        Ok(summary) => Ok(summary),
                        Err(failure) => match failure.handle {
                            Some(handle) => match crate::council::result_by_id(paths, handle.result_id) {
                                Ok(Some(stored)) => Ok(Box::new(council_stored_summary(stored))),
                                Ok(None) => Err(format!("{} · durable result {} was not found", failure.message, handle.result_id)),
                                Err(error) => Err(format!("{} · could not reload result {}: {error:#}", failure.message, handle.result_id)),
                            },
                            None => Err(failure.message),
                        },
                    };
                    Action::CouncilRunFinished { name, result }
                }
                // Voice v1 (rubric 8): the upload landed, so submit the turn
                // that references it. The envelope carries NO transcript — the
                // daemon produces one behind its classification gate, and the
                // client must never pretend to know what was said.
                Some(ReaderSignal::ArtifactStored {
                    command_id,
                    artifact,
                }) => match pending_voice.remove(&command_id) {
                    Some(pending) => {
                        let envelope =
                            crate::voice::voice_envelope(artifact, pending.duration_ms);
                        let submit = command_envelope(
                            live.client_id,
                            CommandBody::SubmitUserInput {
                                session_id,
                                // Empty on purpose: the transcript the daemon
                                // produces becomes the run's objective.
                                text: String::new(),
                                mode: state.default_mode,
                                model: state.pending_model.clone(),
                                envelope: Some(envelope),
                            },
                        );
                        if live.out_tx.send(submit).await.is_err() {
                            return Err(anyhow!(
                                "daemon connection closed while submitting a voice note"
                            ));
                        }
                        Action::Notice("voice note sent \u{2014} transcribing\u{2026}".to_owned())
                    }
                    // An artifact this loop did not upload (or a duplicate
                    // reply): nothing to submit, and nothing to complain about.
                    None => Action::NoOp,
                },
                Some(ReaderSignal::SessionList(sessions)) => {
                    // `ListSessions` carries no excerpt, so every row's is
                    // `None` — absent, not an empty string standing in for one.
                    let rows = sessions
                        .into_iter()
                        .map(|s| codypendent_tui::state::SessionRow::from_summary(s, None))
                        .collect();
                    Action::SessionListLoaded(rows)
                }
                Some(ReaderSignal::SessionSearchResults {
                    correlation_id,
                    page,
                }) => {
                    match correlation_id.and_then(|id| pending_session_searches.remove(&id)) {
                        Some((query, append)) => {
                            let rows = page
                                .items
                                .into_iter()
                                .map(|hit| {
                                    codypendent_tui::state::SessionRow::from_summary(
                                        hit.session,
                                        hit.excerpt,
                                    )
                                })
                                .collect();
                            Action::SessionSearchLoaded {
                                query,
                                rows,
                                next_cursor: page.next_cursor,
                                append,
                            }
                        }
                        // A page this loop did not ask for (or a duplicate
                        // reply): nothing to fold it under, so it is dropped
                        // rather than attributed to the current query.
                        None => Action::NoOp,
                    }
                }
                Some(ReaderSignal::SessionLifecycleApplied(session)) => {
                    Action::SessionLifecycleApplied(Box::new(
                        codypendent_tui::state::SessionRow::from_summary(*session, None),
                    ))
                }
                Some(ReaderSignal::SessionDeleted {
                    session_id,
                    tombstoned,
                }) => Action::SessionLifecycleDeleted {
                    session_id,
                    tombstoned,
                },
                Some(ReaderSignal::SessionExported {
                    command_id,
                    artifact,
                }) => match pending_session_exports.remove(&command_id) {
                    Some(exported_session) => {
                        // The bytes live in the daemon's artifact store, so the
                        // harness (never the pure reducer) pulls them over a
                        // second, short-lived Controller connection and writes
                        // them where the operator can find them. A failed or
                        // corrupted download becomes an issue, never a notice
                        // claiming a file that is not there.
                        let target = paths
                            .data_dir
                            .join("exports")
                            .join(format!("session-{exported_session}.md"));
                        match crate::commands::download_artifact_to(paths, &artifact, &target).await
                        {
                            Ok(_) => Action::SessionExported {
                                session_id: exported_session,
                                path: target.display().to_string(),
                            },
                            Err(error) => Action::Issue(format!(
                                "session {exported_session} exported, but the artifact could not \
                                 be written: {error}"
                            )),
                        }
                    }
                    None => Action::NoOp,
                },
                Some(ReaderSignal::FileSearchResults {
                    query,
                    matches,
                    truncated,
                }) => Action::FileSearchResults {
                    query,
                    matches,
                    truncated,
                },
                Some(ReaderSignal::Catchup(catchup)) => {
                    // Fold the gap replay (the daemon already windowed it to
                    // `(last_seen, through]`, and a too-large gap arrives as a
                    // snapshot), then drain the events buffered while the repair
                    // was in flight — watermark-deduped, in order. If the buffer
                    // itself still reveals a missing span (more loss while
                    // repairing), the tracker asks to repair again and keeps the
                    // tail.
                    let missing_range = match catchup.as_ref() {
                        Catchup::Snapshot { through, .. } if tracker.last_seen() < *through => {
                            Some((tracker.last_seen(), *through))
                        }
                        _ => None,
                    };
                    let through = fold_catchup(state, *catchup);
                    if let Some((after, target)) = missing_range {
                        history_generation = history_generation.saturating_add(1);
                        let generation = history_generation;
                        let requested_session = session_id;
                        let paths = paths.clone();
                        let tx = live.query_tx.clone();
                        tokio::spawn(async move {
                            let result = read_session_event_range(
                                &paths,
                                requested_session,
                                after,
                                target,
                            )
                            .await
                            .map_err(|error| format!("{error:#}"));
                            let _ = tx
                                .send(ReaderSignal::HistoryLoaded {
                                    generation,
                                    session_id: requested_session,
                                    through,
                                    result,
                                })
                                .await;
                        });
                    } else {
                        let drain = tracker.on_catchup(through, Instant::now());
                        for event in drain.apply {
                            reduce(state, Action::DaemonEvent(Box::new(event)));
                        }
                        if let Some(last_seen_sequence) = drain.reattach {
                            // Same grown subscription set on a
                            // repair-during-repair re-attach.
                            send_reattach(
                                &live.out_tx,
                                live.client_id,
                                session_id,
                                last_seen_sequence,
                                &subscriptions,
                            )
                            .await?;
                        }
                    }
                    Action::NoOp
                }
                Some(ReaderSignal::HistoryLoaded {
                    generation,
                    session_id: restored_session,
                    through,
                    result,
                }) => {
                    if generation != history_generation || restored_session != session_id {
                        Action::NoOp
                    } else {
                        let history_restored = match result {
                            Ok(events) => {
                                for event in events {
                                    reduce(state, Action::DaemonEvent(Box::new(event)));
                                }
                                true
                            }
                            Err(error) => {
                                reduce(
                                    state,
                                    Action::Issue(format!(
                                        "could not restore the live event gap; retrying: {error}"
                                    )),
                                );
                                false
                            }
                        };
                        let drain = if history_restored {
                            tracker.on_catchup(through, Instant::now())
                        } else {
                            CatchupDrain {
                                apply: Vec::new(),
                                reattach: Some(tracker.last_seen()),
                            }
                        };
                        for event in drain.apply {
                            reduce(state, Action::DaemonEvent(Box::new(event)));
                        }
                        if let Some(last_seen_sequence) = drain.reattach {
                            send_reattach(
                                &live.out_tx,
                                live.client_id,
                                session_id,
                                last_seen_sequence,
                                &subscriptions,
                            )
                            .await?;
                        }
                        Action::NoOp
                    }
                }
                Some(ReaderSignal::SessionTransition(completion)) => {
                    completed_session_transition = Some(completion);
                    Action::NoOp
                }
                Some(ReaderSignal::Closed) | None => {
                    session_transitions.mark_transport_lost();
                    // A user-requested new/switch/fork already owns a fresh
                    // auxiliary connection. Let it finish; starting a competing
                    // reconnect would only create a stale socket. Duplicate
                    // Closed signals while reconnecting are likewise inert.
                    if !session_transitions.is_active() {
                        let request = SessionTransitionRequest::Reconnect {
                            session_id,
                            last_seen: tracker.last_seen(),
                        };
                        let generation =
                            session_transitions.begin(SessionTransitionKind::Reconnect);
                        let task = spawn_session_transition(
                            generation,
                            request,
                            paths.clone(),
                            repository.to_owned(),
                            subscriptions.clone(),
                            store
                                .resume_token
                                .clone()
                                .map(codypendent_protocol::ResumeToken),
                            live.query_tx.clone(),
                        );
                        session_transitions.track_task(generation, task);
                    }
                    // Dispatched whether or not THIS closure started the
                    // reconnect. The socket is gone either way: a new/switch/
                    // fork already in flight means someone else is bringing a
                    // connection up, not that the link is still healthy. The
                    // `else` arm used to return `NoOp`, so a drop during one of
                    // those transitions left the status row saying Connected
                    // for the whole outage — and if the transition then failed,
                    // its recovery posted a five-second notice that expired
                    // while the screen still looked perfectly healthy.
                    //
                    // A persistent status-row state (`Link::Reconnecting`), and
                    // idempotent: the reducer leaves an existing one alone, so
                    // repeated Closed signals do not restart its clock.
                    Action::LinkLost
                }
            }),
            input = input_rx.recv() => match input {
                // Track width for mouse-column → pane mapping; the draw below
                // re-reads the real size, so a resize just needs a redraw.
                Some(ClientInput::Terminal(CrosstermEvent::Resize(w, h))) => {
                    *width = w;
                    PendingActions::One(Action::RemoteUiViewport { width: w, height: h })
                }
                Some(ClientInput::Terminal(event))
                    if voice.is_push_to_talk(&event) && session_transitions.is_active() =>
                {
                    if voice.is_recording() {
                        let _ = voice.toggle().await;
                        PendingActions::Many(vec![
                            Action::VoiceRecording(false),
                            Action::Notice(
                                "voice capture cancelled while changing sessions".to_owned(),
                            ),
                        ])
                    } else {
                        PendingActions::One(Action::Notice(
                            "voice capture is unavailable while changing sessions".to_owned(),
                        ))
                    }
                }
                // Voice v1 (rubric 8): push-to-talk is intercepted BEFORE the
                // key reaches the reducer, so `codypendent-tui` never has to
                // know about recorders, subprocesses, or audio. A stop reads
                // the captured WAV, uploads it with `PutArtifact`, and remembers
                // the command so the reply can carry the submission (see the
                // `ArtifactStored` arm above). Every failure becomes a visible,
                // actionable notice — the one thing a dead microphone must not
                // do is nothing.
                Some(ClientInput::Terminal(event)) if voice.is_push_to_talk(&event) => {
                    match voice.toggle().await {
                        crate::voice::CaptureOutcome::Started => {
                            PendingActions::One(Action::VoiceRecording(true))
                        }
                        crate::voice::CaptureOutcome::Captured { bytes, duration_ms } => {
                            let upload = command_envelope(
                                live.client_id,
                                CommandBody::PutArtifact {
                                    media_type: crate::voice::CAPTURE_MEDIA_TYPE.to_owned(),
                                    bytes_base64: {
                                        use base64::Engine as _;
                                        base64::engine::general_purpose::STANDARD.encode(&bytes)
                                    },
                                    // Captured media is Confidential by default,
                                    // so a recording never leaves the device
                                    // unless the operator's ceiling allows it.
                                    sensitivity: crate::voice::capture_classification(),
                                },
                            );
                            if let Payload::Command(command) = &upload.payload {
                                pending_voice.insert(
                                    command.command_id,
                                    PendingVoiceUpload {
                                        duration_ms,
                                        request_message_id: upload.message_id,
                                        upload: upload.clone(),
                                    },
                                );
                            }
                            if live.out_tx.send(upload).await.is_err() {
                                return Err(anyhow!(
                                    "daemon connection closed while uploading a voice note"
                                ));
                            }
                            PendingActions::One(Action::VoiceRecording(false))
                        }
                        crate::voice::CaptureOutcome::Failed(message) => PendingActions::Many(
                            vec![Action::VoiceRecording(false), Action::Notice(message)],
                        ),
                    }
                }
                Some(ClientInput::Terminal(event)) => PendingActions::One(map_event(
                    &event,
                    state.input_mode(),
                    *width,
                    &state.hit_map.borrow(),
                )),
                Some(ClientInput::AccessibleLine(line)) => {
                    PendingActions::Many(map_accessible_input(&line, state.input_mode()))
                }
                None => return Ok(()), // input bridge ended
            },
            _ = ticker.tick() => PendingActions::One({
                // FP-2b: a repair whose catch-up reply never arrived (the
                // daemon's fan-out drops spans under lag) must not wedge the
                // client in `repairing` forever — once the deadline passes,
                // re-attach afresh to re-drive the catch-up.
                if !session_transitions.is_active() {
                    if let Some(last_seen_sequence) = tracker.on_tick(Instant::now()) {
                        send_reattach(
                            &live.out_tx,
                            live.client_id,
                            session_id,
                            last_seen_sequence,
                            &subscriptions,
                        )
                        .await?;
                    }
                }
                Action::Tick
            })
        };
        let force_prompt = matches!(&selected, PendingActions::Many(_));
        let actions = match selected {
            PendingActions::One(action) => vec![action],
            PendingActions::Many(actions) => actions,
        };
        let tick_action = actions.len() == 1 && matches!(&actions[0], Action::Tick);
        let notice_before = state.notice.clone();

        // Fold a merged document sync (its async merge could not run in the arm).
        if let Some(sync) = pending_sync.take() {
            if let Some(synced) =
                merge_document_sync(&mut replicas, docs_pool.as_ref(), *sync).await
            {
                reduce(state, synced);
            }
        }

        for action in actions {
            reduce(state, action);
        }

        if let Some(completion) = completed_session_transition.take() {
            let SessionTransitionCompletion {
                generation,
                kind,
                result,
            } = *completion;
            if session_transitions.finish(generation, kind) {
                match result {
                    Err(cause) if kind == SessionTransitionKind::Reconnect => {
                        return Err(anyhow::Error::new(DaemonUnavailable {
                            socket: paths.socket_path.clone(),
                            cause,
                        }));
                    }
                    Err(error) => {
                        let operation = match kind {
                            SessionTransitionKind::New => "create a fresh conversation",
                            SessionTransitionKind::Switch => "switch session",
                            SessionTransitionKind::Fork => "fork session",
                            SessionTransitionKind::Reconnect => unreachable!(),
                        };
                        reduce(
                            state,
                            Action::Issue(format!("could not {operation}: {error}")),
                        );

                        if session_transitions.transport_lost {
                            let request = SessionTransitionRequest::Reconnect {
                                session_id,
                                last_seen: tracker.last_seen(),
                            };
                            let reconnect_generation =
                                session_transitions.begin(SessionTransitionKind::Reconnect);
                            let task = spawn_session_transition(
                                reconnect_generation,
                                request,
                                paths.clone(),
                                repository.to_owned(),
                                subscriptions.clone(),
                                store
                                    .resume_token
                                    .clone()
                                    .map(codypendent_protocol::ResumeToken),
                                live.query_tx.clone(),
                            );
                            session_transitions.track_task(reconnect_generation, task);
                            reduce(
                                state,
                                Action::Notice(
                                    "session change failed · reconnecting previous session…"
                                        .to_owned(),
                                ),
                            );
                        } else {
                            let mut deferred = session_transitions.take_deferred();
                            deferred.extend(state.drain_outbox());
                            state.outbox = deferred;
                        }
                    }
                    Ok(prepared) => {
                        let PreparedSessionTransition {
                            request,
                            subscriptions: next_subscriptions,
                            fresh,
                            history,
                        } = *prepared;
                        let FreshLiveSession {
                            session_id: next_session_id,
                            catchup,
                            pending,
                            resume_token,
                            live: next_live,
                        } = fresh;
                        let reconnect_last_seen = match &request {
                            SessionTransitionRequest::Reconnect { last_seen, .. } => {
                                Some(*last_seen)
                            }
                            _ => None,
                        };
                        let is_reconnect = reconnect_last_seen.is_some();

                        if !is_reconnect {
                            state.begin_new_session();
                            pending_start_run.clear();
                            if let SessionTransitionRequest::Fork { prompt, .. } = &request {
                                state.composer = prompt.clone();
                                state.composer_cursor = state.composer.len();
                            }
                        }

                        let already_seen = reconnect_last_seen
                            .map(|_| tracker.last_seen())
                            .unwrap_or(0);
                        let mut watermark = if is_reconnect {
                            fold_reconnect_catchup(state, catchup, already_seen)
                        } else {
                            fold_catchup(state, catchup)
                        };
                        let history_restored = match history {
                            None => true,
                            Some(Ok(events)) => {
                                for event in events {
                                    if !is_reconnect
                                        || event.sequence == 0
                                        || event.sequence > already_seen
                                    {
                                        reduce(state, Action::DaemonEvent(Box::new(event)));
                                    }
                                }
                                true
                            }
                            Some(Err(error)) => {
                                if reconnect_last_seen.is_some() {
                                    watermark = already_seen;
                                    reduce(
                                        state,
                                        Action::Issue(format!(
                                            "reconnected, but complete history is still restoring: {error}"
                                        )),
                                    );
                                } else {
                                    reduce(
                                        state,
                                        Action::Issue(format!(
                                            "opened the session, but its earlier history could not be restored: {error}"
                                        )),
                                    );
                                }
                                false
                            }
                        };
                        for envelope in pending {
                            if let Payload::Event(event) = envelope.payload {
                                if !is_reconnect || (history_restored && event.sequence > watermark)
                                {
                                    watermark = watermark.max(event.sequence);
                                    reduce(state, Action::DaemonEvent(Box::new(event)));
                                }
                            }
                        }

                        let old_live = std::mem::replace(live, next_live);
                        old_live.shutdown();
                        session_id = next_session_id;
                        subscriptions = next_subscriptions;
                        tracker = GapTracker::new(watermark);
                        replicas.clear();
                        blackboard_reads.clear();
                        board_reads.clear();
                        pending_document_publishes.clear();
                        pending_session_searches.clear();
                        pending_session_exports.clear();
                        pending_ui_plugin_commands.clear();
                        projection_generation = projection_generation.saturating_add(1);
                        history_generation = history_generation.saturating_add(1);

                        live.out_tx
                            .try_send(remote_ui_envelope(
                                live.client_id,
                                session_id,
                                presentation.capabilities_message(),
                            ))
                            .map_err(|_| {
                                anyhow!("replacement socket closed during capability setup")
                            })?;

                        if is_reconnect {
                            for pending in pending_voice.values() {
                                let mut upload = pending.upload.clone();
                                upload.client_id = live.client_id;
                                live.out_tx.try_send(upload).map_err(|_| {
                                    anyhow!(
                                        "reconnected socket closed while resuming a voice upload"
                                    )
                                })?;
                            }
                            if state.pending_run_start.is_none() {
                                pending_start_run.clear();
                            } else if let Some(envelope) =
                                pending_start_run.retry_envelope(live.client_id)
                            {
                                live.out_tx.try_send(envelope).map_err(|_| {
                                    anyhow!(
                                        "reconnected socket closed while retrying the pending run"
                                    )
                                })?;
                            }
                            if !history_restored {
                                live.out_tx
                                    .try_send(reattach_envelope(
                                        live.client_id,
                                        session_id,
                                        watermark,
                                        &subscriptions,
                                    ))
                                    .map_err(|_| {
                                        anyhow!("reconnected socket closed while retrying history")
                                    })?;
                            }
                        } else if !pending_voice.is_empty() {
                            pending_voice.clear();
                            reduce(
                                state,
                                Action::Issue(
                                    "a voice note upload was cancelled when the conversation changed"
                                        .to_owned(),
                                ),
                            );
                        }

                        if let Some(token) = resume_token {
                            store.resume_token = Some(token);
                        }
                        if let Some(workspace_id) = request.workspace_id() {
                            store.sessions.insert(
                                repository.to_owned(),
                                StoredSession {
                                    session_id,
                                    workspace_id,
                                },
                            );
                        }
                        store.save(paths);
                        session_transitions.mark_transport_ready();

                        let notice = match request {
                            SessionTransitionRequest::New { .. } => {
                                "fresh conversation ready".to_owned()
                            }
                            SessionTransitionRequest::Switch { .. } => {
                                format!("switched session to {session_id}")
                            }
                            SessionTransitionRequest::Fork { .. } => {
                                "forked from checkpoint".to_owned()
                            }
                            SessionTransitionRequest::Reconnect { .. } => {
                                "reconnected · session restored".to_owned()
                            }
                        };
                        reduce(state, Action::LinkRestored);
                        reduce(state, Action::Notice(notice));

                        let mut deferred = session_transitions.take_deferred();
                        deferred.extend(state.drain_outbox());
                        state.outbox = deferred;
                    }
                }
            }
            // A stale completion is dropped here. Its boxed LiveIo is also
            // dropped, aborting both auxiliary socket tasks without touching
            // the active generation or its deferred intent queue.
        }
        // RunStarted and an explicit fresh-session reset are authoritative over
        // any client-local retry record.
        if state.pending_run_start.is_none() {
            pending_start_run.clear();
        }
        if tick_action && state.tick >= next_integration_health_tick {
            next_integration_health_tick = state.tick.saturating_add(150);
            if let Ok(status) = crate::client::daemon_status(&paths.socket_path).await {
                for issue in status.integration_issues {
                    reduce(state, Action::Issue(issue));
                }
            }
        }
        // Voice v1 (rubric 8). A TUI cannot log — stderr would corrupt the
        // display — so the off-thread speech worker's failures surface as
        // status-line notices here rather than vanishing. And turning the
        // palette toggle on with nothing configured says exactly what is
        // missing instead of appearing to work and staying silent.
        if let Some(message) = voice.take_speech_error() {
            reduce(state, Action::Notice(message));
        }
        if state.voice.speak_replies && !voice.can_speak() {
            state.voice.speak_replies = false;
            reduce(state, Action::Notice(voice.speech_unavailable_message()));
        }
        if let Some((display_id, provider_id, result)) = connected_acp.take() {
            let model_id = ModelId(display_id.clone());
            // A no-model-selector ACP query completes directly in the harness,
            // without passing through the reducer's ordinary `AddModel` queue.
            // Bind that accepted (generation-checked) terminal result to the
            // active onboarding flow before success/failure is folded.
            if let Some(flow) = &mut state.onboard_flow {
                if flow.provider_id.as_deref() == Some(provider_id.as_str())
                    && flow.awaiting_model.is_none()
                {
                    flow.awaiting_model = Some(model_id.clone());
                }
            }
            let onboarding = state.onboard_flow.is_some();
            // A connect can land while an add-model overlay is still open —
            // either the "connecting…" step or the "fetching models…" step an
            // agent with no model selector short-circuits out of. The flow is
            // over either way, so close it rather than leaving the user parked
            // on a step that will never resolve.
            if !onboarding
                && matches!(
                    state.overlay,
                    codypendent_tui::Overlay::AddModelQuerying { .. }
                        | codypendent_tui::Overlay::AddModelPick { .. }
                )
            {
                state.overlay = codypendent_tui::Overlay::None;
            }
            match result {
                Ok(coordinate) => {
                    match write_add_model(paths, &display_id, &provider_id, &coordinate, None, None)
                    {
                        Ok(()) => {
                            let mut warnings = Vec::new();
                            state.models = load_model_cards(paths, &mut warnings).await;
                            refresh_runnable_models(state, Some(model_id));
                            state.providers = load_provider_cards(paths, &mut warnings).await;
                            state.local_endpoints =
                                crate::local_endpoints::probe_catalog_defaults(paths).await;
                            for warning in warnings {
                                reduce(state, Action::Issue(warning));
                            }
                            // A successful agent install/handshake makes its
                            // catalogue immediately browseable without a TUI
                            // restart. Selecting a supplier-owned model is a
                            // model-selection action, so stage the resulting
                            // concrete profile for the next run as well.
                            if let Some(provider) = state
                                .providers
                                .iter_mut()
                                .find(|provider| provider.id == provider_id)
                            {
                                provider.available = true;
                                provider.can_list_models = true;
                            }
                            state.pending_model = Some(ModelId(display_id.clone()));
                            reload_key_statuses(state, paths);
                            reduce(
                                state,
                                Action::Notice(format!(
                                    "connected and selected {display_id} for your next run"
                                )),
                            );
                        }
                        Err(error) => {
                            reduce(
                                state,
                                Action::OnboardModelAddFailed {
                                    model_id: model_id.clone(),
                                    reason: error.to_string(),
                                },
                            );
                            reduce(
                                state,
                                Action::Notice(format!("could not save ACP profile: {error}")),
                            );
                        }
                    }
                }
                Err(error) => {
                    reduce(
                        state,
                        Action::OnboardModelAddFailed {
                            model_id,
                            reason: error.to_string(),
                        },
                    );
                    reduce(
                        state,
                        Action::Notice(format!("could not connect ACP agent: {error}")),
                    );
                }
            }
        }
        // A steady shell has no frame-based animation, so most ticks need no
        // frame: input and daemon events redraw immediately through the
        // non-tick path. But while a spinner is on screen — a run thinking or
        // running a tool, a graph page loading, a model list being fetched —
        // every tick must draw, or the "spinner" advances one frame per 25
        // ticks (~5s) and reads as a frozen UI. The 25-tick beat stays as the
        // keep-alive for notice expiry and the periodic projection refresh.
        let redraw = !tick_action
            || state.notice != notice_before
            || state.is_animating()
            || state.tick.is_multiple_of(25)
            || presentation.wants_periodic_draw();

        // `StartWorkflow` replies with the new run id after the durable rows are
        // committed. Reload once so the manifest cards bind to that exact run,
        // then queue the same watch path used when an existing run is focused.
        if let Some(workflow_run_id) = started_workflow.take() {
            let mut warnings = Vec::new();
            let user_workflows = paths.data_dir.join("workflows");
            state.workflow = load_workflows(
                Path::new(repository),
                Some(&user_workflows),
                docs_pool.as_ref(),
                &mut warnings,
            )
            .await;
            for warning in warnings {
                reduce(state, Action::Issue(warning));
            }
            state.outbox.push(Intent::WatchWorkflow { workflow_run_id });
        }

        for intent in state.drain_outbox() {
            let workspace_id = store
                .sessions
                .get(repository)
                .map_or(workspace_id, |stored| stored.workspace_id);
            let transition_request = match &intent {
                Intent::NewConversation => Some(SessionTransitionRequest::New { workspace_id }),
                Intent::SwitchSession(target_session_id) => {
                    Some(SessionTransitionRequest::Switch {
                        session_id: *target_session_id,
                        workspace_id,
                    })
                }
                Intent::ForkSession { checkpoint, prompt } => {
                    Some(SessionTransitionRequest::Fork {
                        source_session_id: session_id,
                        checkpoint: *checkpoint,
                        prompt: prompt.clone(),
                        workspace_id,
                    })
                }
                _ => None,
            };
            if let Some(request) = transition_request {
                let kind = request.kind();
                let generation = session_transitions.begin(kind);
                let next_subscriptions = default_subscriptions();
                let task = spawn_session_transition(
                    generation,
                    request,
                    paths.clone(),
                    repository.to_owned(),
                    next_subscriptions,
                    store
                        .resume_token
                        .clone()
                        .map(codypendent_protocol::ResumeToken),
                    live.query_tx.clone(),
                );
                session_transitions.track_task(generation, task);
                let message = match kind {
                    SessionTransitionKind::New => "creating fresh conversation…",
                    SessionTransitionKind::Switch => "switching session…",
                    SessionTransitionKind::Fork => "forking from checkpoint…",
                    SessionTransitionKind::Reconnect => unreachable!(),
                };
                reduce(state, Action::Notice(message.to_owned()));
                continue;
            }
            if session_transitions.is_active() {
                if session_transitions.defer(intent).is_some() {
                    reduce(
                        state,
                        Action::Issue(
                            "too many actions were queued while changing sessions; the oldest was discarded"
                                .to_owned(),
                        ),
                    );
                }
                continue;
            }
            if let Intent::LoadCouncilResults { selector } = &intent {
                let loaded = match selector {
                    Some(selector) => crate::council::result_by_name_or_id(paths, selector)
                        .map(|result| result.into_iter().map(council_stored_summary).collect()),
                    None => {
                        let mut warnings = Vec::new();
                        Ok(load_council_result_cards(paths, &mut warnings))
                    }
                };
                reduce(
                    state,
                    match loaded {
                        Ok(results) if !results.is_empty() => Action::CouncilResultsLoaded(results),
                        Ok(_) => Action::CouncilResultsFailed(
                            "no matching durable council result".to_owned(),
                        ),
                        Err(error) => Action::CouncilResultsFailed(format!("{error:#}")),
                    },
                );
                continue;
            }
            if let Intent::RemoteUiMessage(message) = intent {
                if live
                    .out_tx
                    .send(remote_ui_envelope(live.client_id, session_id, *message))
                    .await
                    .is_err()
                {
                    return Err(anyhow!(
                        "daemon connection closed while sending Remote UI data"
                    ));
                }
                continue;
            }
            // `AddModel` is the one client-only intent: apply it locally (models.toml
            // + auth.json) and skip the daemon-command mapping entirely.
            if let Intent::AddModel {
                display_id,
                provider_id,
                model,
                api_key,
                context_tokens,
            } = &intent
            {
                if is_acp_provider(paths, provider_id) {
                    // The picker sends `model == provider_id` when the agent
                    // exposed no model list; anything else is one of the
                    // agent's OWN model ids, chosen from the pick overlay.
                    let pin = (model != provider_id).then(|| model.clone());
                    let display_id = acp_profile_id(provider_id, pin.as_deref());
                    let paths = paths.clone();
                    let repository = PathBuf::from(repository);
                    let provider_id = provider_id.clone();
                    let request_id = provider_requests.begin(&provider_id);
                    let tx = live.query_tx.clone();
                    tokio::spawn(async move {
                        let result =
                            connect_acp_agent(&paths, &provider_id, &repository, pin.as_deref())
                                .await
                                .map_err(|error| error.to_string());
                        let _ = tx
                            .send(ReaderSignal::AcpConnected {
                                display_id,
                                provider_id,
                                request_id,
                                result,
                            })
                            .await;
                    });
                    continue;
                }
                apply_add_model(
                    state,
                    paths,
                    display_id,
                    provider_id,
                    model,
                    api_key.as_ref().map(|k| k.0.as_str()),
                    *context_tokens,
                )
                .await;
                continue;
            }
            // Model removal is local configuration, never a daemon command.
            // Keep it beside AddModel so the same live picker projection is
            // reloaded immediately after the atomic models.toml edit.
            if let Intent::RemoveModel { model_id } = &intent {
                apply_remove_model(state, paths, model_id).await;
                continue;
            }
            // `QueryProviderModels` is the other client-only intent (model
            // discovery). Three steps, none of them on the UI thread beyond a
            // cache read: seed the pick-list from `<data_dir>/model_lists/`
            // when a previous fetch left one there (instant), then spawn the
            // `<base_url>/models` GET and feed its merged result back as
            // `ReaderSignal::ProviderModels`. Never a daemon command. The
            // spawned task owns the key for the request and drops it — it is
            // never sent back.
            // P12: first-run setup just (re)opened. Look at the local ports
            // again, so a server started since boot is seen by this setup
            // rather than by the next launch. Client-only, like the intents
            // below it: nothing crosses the wire for it.
            if matches!(intent, Intent::ProbeLocalEndpoints) {
                let endpoints = crate::local_endpoints::probe_catalog_defaults(paths).await;
                reduce(state, Action::LocalEndpointsProbed(endpoints));
                continue;
            }
            if let Intent::QueryProviderModels {
                provider_id,
                api_key,
                refresh,
            } = &intent
            {
                let request_id = provider_requests.begin(provider_id);
                // An ACP agent has no `/models` endpoint: its model list comes
                // from the session-config handshake, so listing means spawning
                // it. Same off-thread shape, same return signal — the pick
                // overlay never learns the difference.
                if is_acp_provider(paths, provider_id) {
                    let paths = paths.clone();
                    let repository = PathBuf::from(repository);
                    let provider_id = provider_id.clone();
                    let tx = live.query_tx.clone();
                    tokio::spawn(async move {
                        let signal = match probe_acp_agent(&paths, &provider_id, &repository).await
                        {
                            // No model selector: there is nothing to pick,
                            // so connect the bare profile directly rather
                            // than opening an empty list.
                            Ok(probe) if probe.models.is_empty() => ReaderSignal::AcpConnected {
                                display_id: acp_profile_id(&provider_id, None),
                                provider_id,
                                request_id,
                                result: Ok(probe.coordinate(None)),
                            },
                            Ok(probe) => ReaderSignal::ProviderModels {
                                provider_id: provider_id.clone(),
                                request_id,
                                // An agent advertises ids only — there is no
                                // catalog metadata for a model that lives
                                // inside someone else's agent.
                                result: Ok((
                                    probe
                                        .models
                                        .into_iter()
                                        .map(|model| {
                                            let mut row = AddModelRow::live(model);
                                            if provider_id == "amp-acp" {
                                                row.name = Some(
                                                    "Amp effort level · underlying model selected by Amp"
                                                        .to_string(),
                                                );
                                            }
                                            row
                                        })
                                        .collect(),
                                    ModelListOrigin::Live,
                                )),
                            },
                            Err(error) => ReaderSignal::ProviderModels {
                                provider_id,
                                request_id,
                                result: Err(error.to_string()),
                            },
                        };
                        let _ = tx.send(signal).await;
                    });
                    continue;
                }
                use codypendent_providers::{AuthMethod, Catalog};
                let catalog =
                    Catalog::load_with_user_overrides(&paths.data_dir.join("providers.toml"))
                        .unwrap_or_else(|_| Catalog::builtin());
                let curated = catalog_rows_for(&catalog, provider_id);
                let (base_url, header, prefix, env_names, extra_headers, listable) =
                    match catalog.get(provider_id) {
                        Some(provider) => {
                            let base = provider.base_url.clone().unwrap_or_default();
                            let (header, prefix, env_names) = provider
                                .auth
                                .iter()
                                .find_map(|auth| match auth {
                                    AuthMethod::ApiKey {
                                        env,
                                        header,
                                        prefix,
                                    } => Some((header.clone(), prefix.clone(), env.clone())),
                                    _ => None,
                                })
                                .unwrap_or_else(|| {
                                    (
                                        "Authorization".to_string(),
                                        "Bearer ".to_string(),
                                        Vec::new(),
                                    )
                                });
                            (
                                base,
                                header,
                                prefix,
                                env_names,
                                provider.extra_headers.clone(),
                                provider_can_list_models(provider),
                            )
                        }
                        None => (
                            String::new(),
                            "Authorization".to_string(),
                            "Bearer ".to_string(),
                            Vec::new(),
                            BTreeMap::new(),
                            false,
                        ),
                    };
                // Instant seed: the cached listing, catalog metadata merged in.
                if !*refresh {
                    if let Some((cached, age)) = read_model_list_cache(&paths.data_dir, provider_id)
                    {
                        reduce(
                            state,
                            Action::ProviderModelsLoaded {
                                provider_id: provider_id.clone(),
                                models: merge_catalog_rows(cached, &curated),
                                origin: ModelListOrigin::Cached(age),
                            },
                        );
                    }
                }
                // A provider with no listing endpoint (Perplexity) never
                // reaches the network at all: its curated rows ARE the answer.
                if !listable {
                    reduce(
                        state,
                        if curated.is_empty() {
                            Action::ProviderModelsFailed {
                                provider_id: provider_id.clone(),
                                reason: "this provider has no model-list endpoint".to_owned(),
                            }
                        } else {
                            Action::ProviderModelsLoaded {
                                provider_id: provider_id.clone(),
                                models: curated,
                                origin: ModelListOrigin::Catalog("no listing endpoint".to_owned()),
                            }
                        },
                    );
                    continue;
                }
                let provider_id = provider_id.clone();
                // No key in hand: fall back to the provider-wide key a previous
                // add already stored, so the same key is never asked for twice.
                let auth =
                    codypendent_runtime::auth::AuthStore::load(&paths.data_dir).unwrap_or_default();
                let key = api_key
                    .as_ref()
                    .map(|key| key.0.trim())
                    .filter(|key| !key.is_empty())
                    .map(str::to_owned)
                    .or_else(|| resolve_provider_api_key(&provider_id, &auth, &env_names));
                let tx = live.query_tx.clone();
                let data_dir = paths.data_dir.clone();
                tokio::spawn(async move {
                    let result = match query_provider_models(
                        &base_url,
                        &header,
                        &prefix,
                        &extra_headers,
                        key.as_deref(),
                    )
                    .await
                    {
                        Ok(live_rows) => {
                            write_model_list_cache(&data_dir, &provider_id, &live_rows);
                            Ok((
                                merge_catalog_rows(live_rows, &curated),
                                ModelListOrigin::Live,
                            ))
                        }
                        // A failed fetch still has the curated rows to
                        // offer — the picker never becomes a dead end.
                        Err(reason) if !curated.is_empty() => {
                            Ok((curated, ModelListOrigin::Catalog(reason)))
                        }
                        Err(reason) => Err(reason),
                    };
                    let _ = tx
                        .send(ReaderSignal::ProviderModels {
                            provider_id,
                            request_id,
                            result,
                        })
                        .await;
                });
                continue;
            }
            // Local models: Unsloth catalog (client-only intents, off-thread —
            // the same `tokio::spawn` + `ReaderSignal` round trip as
            // `QueryProviderModels` above). `ListUnslothRepos`/
            // `ListUnslothQuants` are read-only Hub GETs; `PullUnslothModel`
            // drives the real `ollama pull` subprocess via
            // `codypendent_cli::models_pull`, streaming each parsed line back
            // before the terminal result.
            if let Intent::ListUnslothRepos = &intent {
                let tx = live.query_tx.clone();
                tokio::spawn(async move {
                    let result = match codypendent_integrations::unsloth::HfHubClient::hub() {
                        Ok(client) => client
                            .list_gguf_repos(
                                codypendent_integrations::unsloth::DEFAULT_UNSLOTH_ORG,
                                30,
                            )
                            .await
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = tx.send(ReaderSignal::UnslothRepos(result)).await;
                });
                continue;
            }
            if let Intent::ListUnslothQuants { repo_id } = &intent {
                let repo_id = repo_id.clone();
                let tx = live.query_tx.clone();
                tokio::spawn(async move {
                    let result = match codypendent_integrations::unsloth::HfHubClient::hub() {
                        Ok(client) => client
                            .list_quant_variants(&repo_id)
                            .await
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = tx
                        .send(ReaderSignal::UnslothQuants { repo_id, result })
                        .await;
                });
                continue;
            }
            if let Intent::PullUnslothModel { repo_id, quant } = &intent {
                let repo_id = repo_id.clone();
                let quant = quant.clone();
                let paths = paths.clone();
                let tx = live.query_tx.clone();
                tokio::spawn(async move {
                    let (progress_tx, mut progress_rx) =
                        tokio::sync::mpsc::unbounded_channel::<String>();
                    let pull_repo = repo_id.clone();
                    let pull_quant = quant.clone();
                    let pull_task = tokio::spawn(async move {
                        crate::models_pull::pull_via_ollama(
                            crate::models_pull::OLLAMA_BIN,
                            &pull_repo,
                            &pull_quant,
                            progress_tx,
                        )
                        .await
                    });
                    while let Some(line) = progress_rx.recv().await {
                        let _ = tx
                            .send(ReaderSignal::UnslothPullProgress {
                                repo_id: repo_id.clone(),
                                quant: quant.clone(),
                                line,
                            })
                            .await;
                    }
                    let result = match pull_task.await {
                        Ok(Ok(())) => {
                            // Best-effort: a failed/absent metadata lookup
                            // registers with `context_tokens: None` rather
                            // than losing an otherwise-successful pull.
                            let context_tokens =
                                match codypendent_integrations::unsloth::HfHubClient::hub() {
                                    Ok(client) => client
                                        .repo_metadata(&repo_id)
                                        .await
                                        .ok()
                                        .and_then(|metadata| metadata.context_length),
                                    Err(_) => None,
                                };
                            crate::models_pull::register_pulled_model(
                                &paths,
                                &repo_id,
                                &quant,
                                context_tokens,
                            )
                            .map_err(|error| error.to_string())
                        }
                        Ok(Err(pull_error)) => Err(pull_error.to_string()),
                        Err(join_error) => Err(format!("pull task panicked: {join_error}")),
                    };
                    let _ = tx
                        .send(ReaderSignal::UnslothPullFinished {
                            repo_id,
                            quant,
                            result,
                        })
                        .await;
                });
                continue;
            }
            // `VerifyApiKey` (`/keys`, `Ctrl-T`) is client-only for the same
            // reason `SetApiKey` is: the key stays on this machine. One
            // `/models` call through the real registry — the same credential
            // precedence and headers a run would use — answered back as
            // `Action::ModelKeyVerified`.
            if let Intent::VerifyApiKey { model_id } = &intent {
                let model_id = model_id.clone();
                let data_dir = paths.data_dir.clone();
                let tx = live.query_tx.clone();
                tokio::spawn(async move {
                    let result = verify_model_key(&data_dir, &model_id).await;
                    let _ = tx
                        .send(ReaderSignal::ModelKeyVerified { model_id, result })
                        .await;
                });
                continue;
            }
            // `SetApiKey` / `RemoveApiKey` (D1, `/keys`) are client-only intents
            // (the keep-secrets-off-the-wire invariant, exactly like `AddModel`):
            // apply them to `auth.json` locally (load-before-write, atomic,
            // 0600), re-fire the status projection, and skip the daemon-command
            // mapping entirely.
            if let Intent::SetApiKey { target, key } = &intent {
                apply_set_api_key(state, paths, target, &key.0);
                continue;
            }
            if let Intent::RemoveApiKey { target } = &intent {
                apply_remove_api_key(state, paths, target);
                continue;
            }
            // `/theme` is a display preference, not a daemon command: the live
            // switch already happened (the renderer draws in the picked theme),
            // so all that remains is to remember it for the next launch.
            // `theme_select` reads it back below `--theme`/`CODYPENDENT_THEME`.
            if let Intent::SetTheme { id } = &intent {
                store.theme = Some(id.clone());
                store.save(paths);
                continue;
            }
            // Onboarding preferences are local presentation state. Successful
            // completion clears a prior skip, but needs no separate
            // `onboarded` bit: the freshly-resolved runnable set is the source
            // of truth on every launch.
            if apply_onboard_preference(store, &intent) {
                store.save(paths);
                continue;
            }
            // Council creation is local, private configuration just like the
            // model/key flows above. Keep it off the daemon wire and reuse the
            // CLI council module's exact validation + atomic 0600 persistence.
            if let Intent::CreateCouncil {
                name,
                description,
                members,
                chair,
                rounds,
            } = &intent
            {
                let definition = crate::council::CouncilDefinition {
                    name: name.clone(),
                    description: description.clone(),
                    chair: chair.clone(),
                    rounds: *rounds,
                    // The wizard has no quorum step, so take the default rule
                    // (a simple majority of members) rather than pinning a
                    // number the user was never shown.
                    quorum: None,
                    // The host-owned wizard has no evidence-mode step yet; a
                    // council created this way keeps rubric 6's "default
                    // behavior unchanged" and can still be flipped on later by
                    // editing councils.toml or recreating via the CLI's
                    // `--evidence` flag.
                    evidence: false,
                    // The independent pass is on by default for every council,
                    // however it was created — the wizard has no step to turn
                    // it off precisely because a check you must remember to
                    // enable is off when it would have mattered.
                    review: true,
                    // No explicit reviewer: the run picks a member that is not
                    // the chair.
                    reviewer: None,
                    members: members
                        .iter()
                        .map(|(model, role)| crate::council::CouncilMember {
                            model: model.clone(),
                            role: role.clone(),
                        })
                        .collect(),
                };
                match crate::council::persist_definition(paths, definition) {
                    Ok(created) => {
                        let mut warnings = Vec::new();
                        state.councils = load_council_cards(paths, &mut warnings);
                        for warning in warnings {
                            reduce(state, Action::Issue(warning));
                        }
                        reduce(
                            state,
                            Action::CouncilCreated {
                                name: created.name,
                                members: created.members.len(),
                                rounds: created.rounds,
                            },
                        );
                    }
                    Err(error) => reduce(
                        state,
                        Action::CouncilCreateFailed {
                            name: name.clone(),
                            error: error.to_string(),
                        },
                    ),
                }
                continue;
            }
            // Council removal (rubric 6 TUI wiring): same client-only shape as
            // creation above — apply locally, reload the browser projection,
            // and skip the daemon-command mapping entirely. Saved run reports
            // are left on disk (only the definition is removed).
            if let Intent::DeleteCouncil { name } = &intent {
                match crate::council::remove_definition(paths, name) {
                    Ok(()) => {
                        let mut warnings = Vec::new();
                        state.councils = load_council_cards(paths, &mut warnings);
                        for warning in warnings {
                            reduce(state, Action::Issue(warning));
                        }
                        reduce(state, Action::CouncilDeleted { name: name.clone() });
                    }
                    Err(error) => reduce(
                        state,
                        Action::CouncilDeleteFailed {
                            name: name.clone(),
                            error: error.to_string(),
                        },
                    ),
                }
                continue;
            }
            // Run a persisted council's deliberation (rubric 6 TUI wiring).
            // Member/chair runs are independent daemon sessions the base
            // transcript never subscribes to, so `council::run_with_progress`
            // is driven off-thread over its OWN connection — exactly the
            // `AddModel`/ACP-connect off-thread shape above — streaming each
            // round/member/chair transition back as `ReaderSignal::CouncilProgress`
            // and the final outcome (or a formatted failure) as
            // `ReaderSignal::CouncilRunFinished`. `evidence: false` here always
            // defers to the council's OWN stored flag (`run_with_progress` ORs
            // the two) — there is no per-run override from this prompt.
            if let Intent::RunCouncil { name, objective } = &intent {
                let paths = paths.clone();
                let name = name.clone();
                let objective = objective.clone();
                let repository = PathBuf::from(repository);
                let tx = live.query_tx.clone();
                let progress_tx = tx.clone();
                let progress_name = name.clone();
                tokio::spawn(async move {
                    let active_subagents =
                        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let progress = move |progress: crate::council::CouncilProgress| {
                        use std::sync::atomic::Ordering;
                        let active = match &progress.event {
                            crate::council::CouncilEvent::RoundStarted { members, .. } => {
                                active_subagents.store(*members, Ordering::Relaxed);
                                *members
                            }
                            crate::council::CouncilEvent::MemberCompleted { .. }
                            | crate::council::CouncilEvent::MemberFailed { .. } => active_subagents
                                .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                                    Some(value.saturating_sub(1))
                                })
                                .unwrap_or(0)
                                .saturating_sub(1),
                            crate::council::CouncilEvent::ChairRuled { .. }
                            | crate::council::CouncilEvent::ReviewStarted { .. }
                            | crate::council::CouncilEvent::ChairStarted { .. } => {
                                active_subagents.store(1, Ordering::Relaxed);
                                1
                            }
                            crate::council::CouncilEvent::Warning { .. } => {
                                active_subagents.load(Ordering::Relaxed)
                            }
                        };
                        let _ = progress_tx.try_send(ReaderSignal::CouncilProgress {
                            name: progress_name.clone(),
                            result_id: progress.result_id.to_string(),
                            phase: council_progress_phase(&progress.event),
                            occurred_at: progress.occurred_at,
                            message: council_progress_message(&progress.event),
                            active_subagents: active,
                        });
                    };
                    let result = crate::council::run_with_progress_linked(
                        &paths,
                        &name,
                        objective,
                        repository,
                        Some(session_id),
                        false,
                        progress,
                    )
                    .await;
                    let result = match result {
                        Ok(run) => match crate::council::result_by_id(&paths, run.handle.result_id)
                        {
                            Ok(Some(stored)) => Ok(Box::new(council_stored_summary(stored))),
                            _ => Ok(Box::new(council_run_summary(run))),
                        },
                        Err(error) => {
                            let handle = error
                                .downcast_ref::<crate::council::CouncilRunFailure>()
                                .map(|failure| failure.handle.clone());
                            Err(CouncilTerminalFailure {
                                message: format!("{error:#}"),
                                handle,
                            })
                        }
                    };
                    let _ = tx
                        .send(ReaderSignal::CouncilRunFinished { name, result })
                        .await;
                });
                continue;
            }
            if let Intent::CopyText { text } = &intent {
                if let Err(error) = presentation.copy_text(text) {
                    // A transient corrective notice, not a permanent Issue:
                    // the reducer optimistically said "copied" the moment it
                    // asked, so the failure must land in the same place the
                    // claim did (and a one-off OSC-52 failure is not a
                    // setup diagnostic to triage later).
                    reduce(state, Action::Notice(format!("copy failed: {error}")));
                }
                continue;
            }
            if let Intent::Notify { message } = &intent {
                let method = codypendent_tui::terminal::detect_notify_method();
                let _ = codypendent_tui::terminal::notify(message, method);
                continue;
            }
            if matches!(intent, Intent::ListSessions) {
                let envelope = command_envelope(
                    live.client_id,
                    CommandBody::ListSessions {
                        workspace: None,
                        limit: None,
                    },
                );
                live.out_tx
                    .send(envelope)
                    .await
                    .map_err(|_| anyhow!("daemon connection closed while listing sessions"))?;
                continue;
            }
            // The Session Library's ranked search. Framed here rather than in
            // `intent_to_command` because `Payload::SessionSearchResults`
            // echoes back only the page — the query it answers, and whether it
            // is a continuation, live in this correlation map. Without it a
            // page for an abandoned query would be folded under the query the
            // operator has since typed.
            if let Intent::SearchSessions { query, cursor } = &intent {
                let envelope = command_envelope(
                    live.client_id,
                    CommandBody::SearchSessions {
                        query: codypendent_protocol::SessionSearchQuery {
                            query: query.clone(),
                            filters: codypendent_protocol::SessionSearchFilters::default(),
                            // 0 asks the daemon for its own page size; the
                            // client does not get to widen the server's cap.
                            limit: 0,
                            cursor: cursor.clone(),
                        },
                    },
                );
                pending_session_searches
                    .insert(envelope.message_id, (query.clone(), cursor.is_some()));
                if live.out_tx.send(envelope).await.is_err() {
                    pending_session_searches.clear();
                    return Err(anyhow!("daemon connection closed while searching sessions"));
                }
                continue;
            }
            if let Intent::MutateSession { session_id, action } = &intent {
                let envelope = command_envelope(
                    live.client_id,
                    CommandBody::MutateSessionLifecycle {
                        session_id: *session_id,
                        action: action.clone(),
                    },
                );
                // Only an export needs correlation: `Payload::SessionExported`
                // names the artifact but not the session it came from, and the
                // harness has to write the bytes somewhere legible.
                if let (
                    Payload::Command(command),
                    codypendent_protocol::SessionLifecycleAction::Export { .. },
                ) = (&envelope.payload, action)
                {
                    pending_session_exports.insert(command.command_id, *session_id);
                }
                if live.out_tx.send(envelope).await.is_err() {
                    pending_session_exports.clear();
                    return Err(anyhow!("daemon connection closed while changing a session"));
                }
                continue;
            }
            if let Intent::SearchFiles { query } = &intent {
                let envelope = command_envelope(
                    live.client_id,
                    CommandBody::SearchWorkspaceFiles {
                        repository: repository.to_owned(),
                        query: query.clone(),
                        limit: None,
                    },
                );
                live.out_tx.send(envelope).await.map_err(|_| {
                    anyhow!("daemon connection closed while searching workspace files")
                })?;
                continue;
            }
            if let Intent::RefreshProjection { kind } = &intent {
                let Some(pool) = docs_pool.as_ref().cloned() else {
                    reduce(
                        state,
                        Action::Issue(
                            "advanced views unavailable: knowledge database is closed".into(),
                        ),
                    );
                    continue;
                };
                projection_generation = projection_generation.saturating_add(1);
                let generation = projection_generation;
                let repository_id = crate::repo_anchor::anchor_repository_id(Path::new(repository));
                let scopes = vec![
                    Scope::System,
                    Scope::Workspace(workspace_id),
                    Scope::Repository(repository_id),
                ];
                let repository = PathBuf::from(repository);
                let user_workflows = paths.data_dir.join("workflows");
                let kind = *kind;
                let tx = live.query_tx.clone();
                tokio::spawn(async move {
                    let projection =
                        load_projection(kind, pool, scopes, repository, user_workflows).await;
                    let _ = tx
                        .send(ReaderSignal::ProjectionLoaded {
                            generation,
                            projection,
                        })
                        .await;
                });
                continue;
            }
            if let Intent::MutateLearning {
                id,
                revision,
                mutation,
            } = &intent
            {
                let Some(pool) = docs_pool.as_ref() else {
                    reduce(
                        state,
                        Action::Issue(
                            "learning journey unavailable: knowledge database is closed".into(),
                        ),
                    );
                    continue;
                };
                let result = mutate_learning(pool, id, *revision, mutation).await;
                match result {
                    Ok(message) => {
                        let selected = state.focused_learning().map(|card| card.id.clone());
                        match load_journey(pool, Path::new(repository)).await {
                            Ok(cards) => {
                                state.learnings = cards;
                                state.selected_learning = selected
                                    .as_deref()
                                    .and_then(|id| {
                                        state.learnings.iter().position(|card| card.id == id)
                                    })
                                    .unwrap_or_else(|| {
                                        state
                                            .selected_learning
                                            .min(state.learnings.len().saturating_sub(1))
                                    });
                                state.pending_learning_review = state
                                    .learnings
                                    .iter()
                                    .filter(|card| card.state == "proposed")
                                    .count()
                                    as u32;
                                reduce(state, Action::Notice(message));
                            }
                            Err(error) => reduce(
                                state,
                                Action::Issue(format!(
                                    "learning changed but refresh failed: {error}"
                                )),
                            ),
                        }
                    }
                    Err(error) => reduce(
                        state,
                        Action::Issue(format!("learning change failed: {error}")),
                    ),
                }
                continue;
            }
            if let Intent::SearchEdges { query, page } = &intent {
                if let Some(pool) = docs_pool.as_ref() {
                    let repository_id =
                        crate::repo_anchor::anchor_repository_id(Path::new(repository));
                    let mut warnings = Vec::new();
                    let (edges, total, page) =
                        load_edge_page(pool, repository_id, query, *page, &mut warnings).await;
                    // An unfiltered read that finds nothing means the graph was
                    // never folded for this checkout, which the overlay renders
                    // as the bare line "no edges in this repository" — a
                    // statement of fact that reads as a verdict and offers no
                    // next step. Say which command builds it. (The filtered case
                    // is left alone: there, zero really does mean "your query
                    // matched nothing".)
                    if total == 0 && query.trim().is_empty() {
                        warnings.push(
                            "this repository's code graph is empty — run `codypendent graph build` \
                             to fold it (it reports which files were walked and which produced \
                             nothing)"
                                .to_string(),
                        );
                    }
                    reduce(
                        state,
                        Action::EdgesLoaded {
                            edges,
                            total,
                            query: query.clone(),
                            page,
                        },
                    );
                    for warning in warnings {
                        reduce(state, Action::Issue(warning));
                    }
                } else {
                    reduce(
                        state,
                        Action::Issue(
                            "code graph unavailable: knowledge database is closed".into(),
                        ),
                    );
                }
                continue;
            }
            // Opening/focusing a workflow run grows this connection's live
            // workflow + blackboard subscriptions and reads authoritative
            // baselines. Repeated watches skip the re-attach but deliberately
            // re-read both baselines, so reopening a panel is immediately fresh.
            if let Intent::WatchWorkflow { workflow_run_id } = &intent {
                let has_workflow = subscriptions.iter().any(|subscription| {
                    matches!(
                        subscription,
                        Subscription::Workflow { workflow_run_id: id } if id == workflow_run_id
                    )
                });
                let has_blackboard = subscriptions.iter().any(|subscription| {
                    matches!(
                        subscription,
                        Subscription::Blackboard { workflow_run_id: id } if id == workflow_run_id
                    )
                });
                if !has_workflow {
                    subscriptions.push(Subscription::Workflow {
                        workflow_run_id: workflow_run_id.clone(),
                    });
                }
                if !has_blackboard {
                    subscriptions.push(Subscription::Blackboard {
                        workflow_run_id: workflow_run_id.clone(),
                    });
                }
                if !has_workflow || !has_blackboard {
                    let attach = command_envelope(
                        live.client_id,
                        CommandBody::AttachSession {
                            session_id,
                            last_seen_sequence: Some(tracker.last_seen()),
                            subscriptions: subscriptions.clone(),
                            requested_role: ClientRole::Controller,
                            repository: None,
                        },
                    );
                    if live.out_tx.send(attach).await.is_err() {
                        return Err(anyhow!(
                            "daemon connection closed while attaching a workflow"
                        ));
                    }
                }

                let snapshot = command_envelope(
                    live.client_id,
                    CommandBody::ReadWorkflowRun {
                        workflow_run_id: workflow_run_id.clone(),
                    },
                );
                if live.out_tx.send(snapshot).await.is_err() {
                    return Err(anyhow!("daemon connection closed while reading a workflow"));
                }
                let board = command_envelope(
                    live.client_id,
                    CommandBody::ReadBlackboard {
                        workflow_run_id: workflow_run_id.clone(),
                        kind: None,
                        include_superseded: true,
                        board_repository: None,
                    },
                );
                if let Payload::Command(command) = &board.payload {
                    blackboard_reads.insert(command.command_id, workflow_run_id.clone());
                }
                if live.out_tx.send(board).await.is_err() {
                    return Err(anyhow!(
                        "daemon connection closed while reading a blackboard"
                    ));
                }
                continue;
            }
            // Opening the task board grows this connection's subscription to the
            // repository BOARD's channel and reads its baseline (rubric 10). The
            // board rides the existing per-run blackboard machinery: its channel
            // key is the synthetic board run id, so nothing new is needed on the
            // wire beyond the board-scoped read.
            if matches!(intent, Intent::WatchBoard) {
                let anchored = board_repository(repository);
                let board_id = codypendent_protocol::board_scope_id(&anchored);
                let subscribed = subscriptions.iter().any(|subscription| {
                    matches!(
                        subscription,
                        Subscription::Blackboard { workflow_run_id: id } if *id == board_id
                    )
                });
                if !subscribed {
                    subscriptions.push(Subscription::Blackboard {
                        workflow_run_id: board_id,
                    });
                    let attach = command_envelope(
                        live.client_id,
                        CommandBody::AttachSession {
                            session_id,
                            last_seen_sequence: Some(tracker.last_seen()),
                            subscriptions: subscriptions.clone(),
                            requested_role: ClientRole::Controller,
                            repository: None,
                        },
                    );
                    if live.out_tx.send(attach).await.is_err() {
                        return Err(anyhow!(
                            "daemon connection closed while attaching the board"
                        ));
                    }
                }
                let read = command_envelope(
                    live.client_id,
                    CommandBody::ReadBlackboard {
                        workflow_run_id: String::new(),
                        kind: Some("task".to_owned()),
                        include_superseded: false,
                        board_repository: Some(anchored),
                    },
                );
                if let Payload::Command(command) = &read.payload {
                    board_reads.insert(command.command_id);
                }
                if live.out_tx.send(read).await.is_err() {
                    return Err(anyhow!("daemon connection closed while reading the board"));
                }
                continue;
            }
            // The first edit on a document subscribes this client to its live sync
            // stream (a re-attach carrying the grown subscription set) and seeds its
            // replica, so the edit's own resulting sync — and every other writer's —
            // comes back. Done before the edit command is sent.
            if let Some(document_id) = doc_intent_target(&intent) {
                // Seeding and subscribing are asked separately: reconnect
                // clears the replicas but keeps the subscriptions, so after one
                // the replica needs seeding and the subscription does not.
                if let std::collections::hash_map::Entry::Vacant(slot) = replicas.entry(document_id)
                {
                    slot.insert(seed_replica(docs_pool.as_ref(), document_id).await);
                }
                if subscribe_to_document(&mut subscriptions, document_id) {
                    let attach = command_envelope(
                        live.client_id,
                        CommandBody::AttachSession {
                            session_id,
                            last_seen_sequence: Some(tracker.last_seen()),
                            subscriptions: subscriptions.clone(),
                            requested_role: ClientRole::Controller,
                            // Re-attach to grow this connection's own
                            // subscription set (a new Document edit) on an
                            // already-open session: no new repo context to warm.
                            repository: None,
                        },
                    );
                    if live.out_tx.send(attach).await.is_err() {
                        return Err(anyhow!(
                            "daemon connection closed while watching a document"
                        ));
                    }
                }
            }
            if matches!(intent, Intent::WatchDocument { .. }) {
                continue;
            }
            let intent_publish_document = match &intent {
                Intent::PublishDocument { document_id, .. } => Some(*document_id),
                _ => None,
            };
            let intent_starts_run = matches!(&intent, Intent::StartRun { .. });
            let pending_ui_plugin = PendingUiPluginCommand::from_intent(&intent);
            let mut envelope = command_envelope(
                live.client_id,
                intent_to_command(intent, session_id, repository),
            );
            // Session-bound intercepted commands (notably document publish)
            // need the caller's attached session so durable approvals land on
            // the visible rail instead of an unrelated synthetic session.
            envelope.session_id = Some(session_id);
            if let (Some(document_id), Payload::Command(command)) =
                (intent_publish_document, &envelope.payload)
            {
                pending_document_publishes.insert(command.command_id, document_id);
            }
            if intent_starts_run {
                pending_start_run.observe_outbound(&envelope);
            }
            if let Some(pending) = pending_ui_plugin {
                pending_ui_plugin_commands.observe(envelope.message_id, pending);
            }
            if live.out_tx.send(envelope).await.is_err() {
                if intent_starts_run {
                    // LiveIo's writer also emits `Closed`; retain the exact
                    // request until that arm replaces the transport and retries.
                    reduce(
                        state,
                        Action::Notice(
                            "connection lost while starting the run · reconnecting…".to_owned(),
                        ),
                    );
                    continue;
                }
                return Err(anyhow!(
                    "daemon connection closed while sending a command; reopen to reconnect"
                ));
            }
        }

        // A durable SessionClosed event is state to present, not an implicit
        // process-exit instruction. Keep the final transcript/status visible;
        // Esc or the explicit detach action still leaves normally.
        if state.should_detach {
            return Ok(());
        }

        if redraw {
            presentation.draw(state, force_prompt)?;
        }
    }
}

enum PendingActions {
    One(Action),
    Many(Vec<Action>),
}

/// Structured terminal failure retained between the off-thread council runner
/// and the reducer-facing summary. The current reducer still accepts a string;
/// the durable handle is folded into that string until the richer council
/// results browser adds its own Action/projection.
struct CouncilTerminalFailure {
    message: String,
    handle: Option<crate::council::CouncilReportHandle>,
}

/// Monotonic correlation for all provider-owned background work. Kept in the
/// I/O harness so neither ids nor credential-bearing requests leak into the
/// pure presentation state. The latest generation is authoritative for model
/// listing and ACP terminal completion alike.
#[derive(Default)]
struct ProviderRequestGenerations {
    next: u64,
    latest: HashMap<String, u64>,
}

impl ProviderRequestGenerations {
    fn begin(&mut self, provider_id: &str) -> u64 {
        self.next = self.next.saturating_add(1);
        let request_id = self.next;
        self.latest.insert(provider_id.to_owned(), request_id);
        request_id
    }

    fn is_current(&self, provider_id: &str, request_id: u64) -> bool {
        self.next == request_id && self.latest.get(provider_id).copied() == Some(request_id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionTransitionKind {
    New,
    Switch,
    Fork,
    Reconnect,
}

#[derive(Clone)]
enum SessionTransitionRequest {
    New {
        workspace_id: WorkspaceId,
    },
    Switch {
        session_id: SessionId,
        workspace_id: WorkspaceId,
    },
    Fork {
        source_session_id: SessionId,
        checkpoint: codypendent_protocol::CheckpointId,
        prompt: String,
        workspace_id: WorkspaceId,
    },
    Reconnect {
        session_id: SessionId,
        last_seen: u64,
    },
}

impl SessionTransitionRequest {
    fn kind(&self) -> SessionTransitionKind {
        match self {
            Self::New { .. } => SessionTransitionKind::New,
            Self::Switch { .. } => SessionTransitionKind::Switch,
            Self::Fork { .. } => SessionTransitionKind::Fork,
            Self::Reconnect { .. } => SessionTransitionKind::Reconnect,
        }
    }

    fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            Self::New { workspace_id }
            | Self::Switch { workspace_id, .. }
            | Self::Fork { workspace_id, .. } => Some(*workspace_id),
            Self::Reconnect { .. } => None,
        }
    }

    fn history_after(&self) -> u64 {
        match self {
            Self::Reconnect { last_seen, .. } => *last_seen,
            _ => 0,
        }
    }
}

/// Harness-only coordination for background attachment work. This contains no
/// I/O and is tested as a small state machine: newer requests supersede older
/// generations, while session-bound effects wait in FIFO order for the one
/// authoritative completion.
#[derive(Default)]
struct SessionTransitionCoordinator {
    next_generation: u64,
    active: Option<(u64, SessionTransitionKind)>,
    active_task: Option<tokio::task::JoinHandle<()>>,
    deferred: std::collections::VecDeque<Intent>,
    transport_lost: bool,
}

impl SessionTransitionCoordinator {
    fn begin(&mut self, kind: SessionTransitionKind) -> u64 {
        if let Some(task) = self.active_task.take() {
            task.abort();
        }
        self.next_generation = self.next_generation.saturating_add(1);
        self.active = Some((self.next_generation, kind));
        self.next_generation
    }

    fn track_task(&mut self, generation: u64, task: tokio::task::JoinHandle<()>) {
        if self.active.is_some_and(|(active, _)| active == generation) {
            self.active_task = Some(task);
        } else {
            task.abort();
        }
    }

    fn is_active(&self) -> bool {
        self.active.is_some()
    }

    fn accepts(&self, generation: u64, kind: SessionTransitionKind) -> bool {
        self.active == Some((generation, kind))
    }

    fn finish(&mut self, generation: u64, kind: SessionTransitionKind) -> bool {
        if !self.accepts(generation, kind) {
            return false;
        }
        self.active = None;
        self.active_task = None;
        true
    }

    fn mark_transport_lost(&mut self) {
        self.transport_lost = true;
    }

    fn mark_transport_ready(&mut self) {
        self.transport_lost = false;
    }

    fn defer(&mut self, intent: Intent) -> Option<Intent> {
        let dropped = (self.deferred.len() >= MAX_DEFERRED_TRANSITION_INTENTS)
            .then(|| self.deferred.pop_front())
            .flatten();
        self.deferred.push_back(intent);
        dropped
    }

    fn take_deferred(&mut self) -> Vec<Intent> {
        self.deferred.drain(..).collect()
    }
}

impl Drop for SessionTransitionCoordinator {
    fn drop(&mut self) {
        if let Some(task) = self.active_task.take() {
            task.abort();
        }
    }
}

struct PreparedSessionTransition {
    request: SessionTransitionRequest,
    subscriptions: Vec<Subscription>,
    fresh: FreshLiveSession,
    /// `None` when catch-up carried events. A snapshot pages the exact missing
    /// range before this result reaches the UI loop.
    history: Option<Result<Vec<SessionEvent>, String>>,
}

struct SessionTransitionCompletion {
    generation: u64,
    kind: SessionTransitionKind,
    result: Result<Box<PreparedSessionTransition>, String>,
}

/// A completed disk-backed projection load. The I/O result stays outside the
/// pure reducer, while the event loop applies it only after checking its
/// generation against the latest requested view.
enum ProjectionLoad {
    Skills(Result<Vec<SkillCard>, String>),
    Memory(Result<Vec<MemoryCard>, String>),
    Journey(Result<Vec<LearningCard>, String>),
    Docs {
        cards: Vec<DocCard>,
        warnings: Vec<String>,
    },
    Workflow {
        cards: Vec<WorkflowNodeCard>,
        warnings: Vec<String>,
    },
}

async fn load_projection(
    kind: ProjectionKind,
    pool: sqlx::SqlitePool,
    scopes: Vec<Scope>,
    repository: PathBuf,
    user_workflows: PathBuf,
) -> ProjectionLoad {
    match kind {
        ProjectionKind::Skills => ProjectionLoad::Skills(
            Registry::new()
                .list(&pool)
                .await
                .map(|items| items.iter().map(skill_card).collect())
                .map_err(|error| error.to_string()),
        ),
        ProjectionKind::Memory => ProjectionLoad::Memory(
            MemoryStore::new()
                .query(&pool, &scopes, None)
                .await
                .map(|records| records.iter().map(memory_card).collect())
                .map_err(|error| error.to_string()),
        ),
        ProjectionKind::Journey => ProjectionLoad::Journey(
            load_journey(&pool, &repository)
                .await
                .map_err(|error| error.to_string()),
        ),
        ProjectionKind::Docs => {
            let mut warnings = Vec::new();
            let cards = load_docs(&pool, &scopes, &mut warnings).await;
            ProjectionLoad::Docs { cards, warnings }
        }
        ProjectionKind::Workflow => {
            let mut warnings = Vec::new();
            let cards = load_workflows(
                &repository,
                Some(&user_workflows),
                Some(&pool),
                &mut warnings,
            )
            .await;
            ProjectionLoad::Workflow { cards, warnings }
        }
    }
}

fn apply_projection(state: &mut AppState, projection: ProjectionLoad) {
    let mut warnings = Vec::new();
    match projection {
        ProjectionLoad::Skills(result) => {
            let selected = state.focused_skill().map(|card| card.name.clone());
            match result {
                Ok(cards) => {
                    state.skills = cards;
                    state.selected_skill = selected
                        .as_deref()
                        .and_then(|name| state.skills.iter().position(|card| card.name == name))
                        .unwrap_or(0);
                }
                Err(error) => warnings.push(format!("could not refresh skills: {error}")),
            }
        }
        ProjectionLoad::Memory(result) => {
            let selected = state.focused_memory().map(|card| card.statement.clone());
            match result {
                Ok(cards) => {
                    state.memories = cards;
                    state.selected_memory = selected
                        .as_deref()
                        .and_then(|statement| {
                            state
                                .memories
                                .iter()
                                .position(|card| card.statement == statement)
                        })
                        .unwrap_or(0);
                }
                Err(error) => warnings.push(format!("could not refresh memories: {error}")),
            }
        }
        ProjectionLoad::Journey(result) => {
            let selected = state.focused_learning().map(|card| card.id.clone());
            match result {
                Ok(cards) => {
                    state.learnings = cards;
                    state.selected_learning = selected
                        .as_deref()
                        .and_then(|id| state.learnings.iter().position(|card| card.id == id))
                        .unwrap_or(0);
                    state.pending_learning_review = state
                        .learnings
                        .iter()
                        .filter(|card| card.state == "proposed")
                        .count() as u32;
                }
                Err(error) => {
                    warnings.push(format!("could not refresh learning journey: {error}"));
                }
            }
        }
        ProjectionLoad::Docs {
            cards,
            warnings: load_warnings,
        } => {
            warnings = load_warnings;
            let selected = state
                .pending_document_selection
                .or_else(|| state.focused_doc().map(|card| card.document_id));
            state.docs = cards;
            state.selected_doc = selected
                .and_then(|document_id| {
                    state
                        .docs
                        .iter()
                        .position(|card| card.document_id == document_id)
                })
                .unwrap_or(0);
            if selected.is_some_and(|document_id| {
                state
                    .docs
                    .get(state.selected_doc)
                    .is_some_and(|doc| doc.document_id == document_id)
            }) {
                state.pending_document_selection = None;
            }
            if let Some(document_id) = state.focused_doc().map(|card| card.document_id) {
                state.outbox.push(Intent::WatchDocument { document_id });
            }
        }
        ProjectionLoad::Workflow {
            cards,
            warnings: load_warnings,
        } => {
            warnings = load_warnings;
            let selected = state
                .focused_node()
                .map(|card| (card.workflow_id.clone(), card.id.clone()));
            state.workflow = cards;
            state.selected_node = selected
                .as_ref()
                .and_then(|(workflow_id, node_id)| {
                    state
                        .workflow
                        .iter()
                        .position(|card| &card.workflow_id == workflow_id && &card.id == node_id)
                })
                .unwrap_or(0);
            if let Some(workflow_run_id) = state
                .focused_node()
                .and_then(|card| card.workflow_run_id.clone())
            {
                state.outbox.push(Intent::WatchWorkflow { workflow_run_id });
            }
        }
    }
    for warning in warnings {
        reduce(state, Action::Issue(warning));
    }
}

/// What the reader task forwards to the loop.
enum ReaderSignal {
    /// A validated Remote UI frame for the reducer-owned host session.
    RemoteUi(Box<codypendent_protocol::UiWireMessage>),
    /// Host-owned lifecycle projection for `/plugins`.
    UiPlugins {
        plugins: Vec<codypendent_protocol::UiPluginLifecycleStatus>,
        correlation_id: Option<codypendent_protocol::MessageId>,
    },
    /// A live session event to fold into state (boxed: it is a large payload and
    /// every other message here is tiny).
    Event(Box<SessionEvent>),
    /// The daemon rejected a command this TUI sent (code + message). Surfaced
    /// as a transient status notice — silence here meant a rejected StartRun
    /// showed the user nothing at all.
    Rejected {
        code: String,
        message: String,
        correlation_id: Option<codypendent_protocol::MessageId>,
    },
    /// A catch-up reply (from the loop's own gap-triggered re-attach).
    Catchup(Box<Catchup>),
    /// A paged transcript range loaded off the input loop for a snapshot gap.
    /// Both generation and session must still match before these events fold.
    HistoryLoaded {
        generation: u64,
        session_id: SessionId,
        through: u64,
        result: Result<Vec<SessionEvent>, String>,
    },
    /// Completion of a fully prepared new/switch/fork/reconnect transport.
    /// The boxed result owns its reader/writer tasks until the event loop either
    /// accepts its generation or drops it as stale.
    SessionTransition(Box<SessionTransitionCompletion>),
    /// A collaborative document's live CRDT sync (Phase 4 STEP 4.3). Boxed — it
    /// carries opaque CRDT bytes and every other signal here is tiny. The loop
    /// merges it into the document's client replica.
    DocumentSync(Box<DocumentSync>),
    /// The daemon granted an edit lease this client requested.
    DocumentLeaseGranted {
        document_id: DocumentId,
        lease_id: String,
    },
    /// The daemon refused an edit lease: the block range is held by another writer
    /// (`document.range-leased`) — surfaced as the presence-lite "blocked" signal.
    DocumentLeaseBlocked,
    /// A newly-created collaborative document. The reducer refreshes the Docs
    /// projection and selects the resulting row once it is loaded.
    DocumentCreated {
        document_id: DocumentId,
    },
    /// Human-readable acknowledgement that the publish plan is now parked on
    /// the ordinary approval rail; no document write has happened yet.
    DocumentPublishPrepared {
        command_id: CommandId,
        approval_id: codypendent_protocol::ApprovalId,
        target: String,
        changed_files: Vec<String>,
        git_action: String,
    },
    /// A provider's fetched model list (model-discovery): the result of the
    /// spawned `<base_url>/models` GET merged with the curated catalog rows,
    /// keyed by `provider_id`, plus where the rows came from. Mapped by the
    /// loop's `select!` to `Action::ProviderModelsLoaded` (Ok) /
    /// `ProviderModelsFailed` (Err — the fetch failed AND the catalog had
    /// nothing for this provider). Carries NO key.
    ProviderModels {
        provider_id: String,
        request_id: u64,
        result: Result<(Vec<AddModelRow>, ModelListOrigin), String>,
    },
    /// A generation-qualified result from a disk-backed projection task.
    ProjectionLoaded {
        generation: u64,
        projection: ProjectionLoad,
    },
    /// The result of a one-shot `/keys` key verification (`Ctrl-T`): `Ok` when
    /// the provider listed the configured model with the stored key, `Err`
    /// with a key-free reason otherwise.
    ModelKeyVerified {
        model_id: String,
        result: Result<(), String>,
    },
    /// Completion of an off-UI-thread ACP install + handshake + typed profile
    /// write. The loop refreshes model/key projections after success.
    AcpConnected {
        display_id: String,
        provider_id: String,
        request_id: u64,
        /// Immutable `registry-id@version` persisted after a successful handshake.
        result: Result<String, String>,
    },
    /// A newly-created durable workflow run. The loop reloads the compiled/live
    /// projection, then immediately subscribes and reads its baselines.
    WorkflowRunStarted {
        workflow_run_id: String,
    },
    WorkflowSnapshot(Box<codypendent_protocol::WorkflowRunSnapshot>),
    WorkflowEvent(codypendent_protocol::WorkflowEvent),
    BlackboardItems {
        command_id: CommandId,
        items: Vec<codypendent_protocol::BlackboardItemView>,
    },
    BlackboardPosted(codypendent_protocol::BlackboardItemView),
    /// One pre-formatted progress line from an off-thread council run (rubric
    /// 6 TUI wiring): a round starting, a member completing/failing, or the
    /// chair beginning synthesis. Mapped to `Action::CouncilProgress`, which
    /// folds it into the active run's transcript as a Note.
    CouncilProgress {
        name: String,
        result_id: String,
        phase: CouncilProgressPhase,
        occurred_at: String,
        message: String,
        active_subagents: usize,
    },
    /// An off-thread council run finished. `Ok` carries the pre-formatted
    /// chair synthesis, attributed participants, and measured-cost line;
    /// `Err` is a human-readable failure (already naming any partial report
    /// path `run_with_progress` managed to save). Boxed like `WorkflowSnapshot`
    /// — the synthesis text can be large and every other signal here is tiny.
    CouncilRunFinished {
        name: String,
        result: Result<Box<codypendent_tui::state::CouncilRunSummary>, CouncilTerminalFailure>,
    },
    /// The Unsloth org's GGUF repo listing, fetched from the Hugging Face Hub
    /// (Local models catalog browse). Mapped by the loop's `select!` to
    /// `Action::UnslothReposLoaded` (Ok, pre-rendered into `UnslothRepoCard`s)
    /// / `UnslothReposFailed` (Err).
    UnslothRepos(Result<Vec<codypendent_integrations::unsloth::HfRepoSummary>, String>),
    /// A repo's quant-variant listing, keyed by `repo_id` so a stale reply
    /// (the operator navigated to a different repo before it landed) is
    /// dropped — mirrors `ProviderModels`'s `provider_id` guard.
    UnslothQuants {
        repo_id: String,
        result: Result<Vec<codypendent_integrations::unsloth::QuantVariant>, String>,
    },
    /// One parsed line of `ollama pull` output.
    UnslothPullProgress {
        repo_id: String,
        quant: String,
        line: String,
    },
    /// The pull (and, on success, the `models.toml` registration) finished.
    UnslothPullFinished {
        repo_id: String,
        quant: String,
        result: Result<String, String>,
    },
    /// A `PutArtifact` reply (voice v1, rubric 8): the daemon stored a captured
    /// voice note and minted its ref. The loop turns this into the follow-up
    /// `SubmitUserInput` whose audio envelope references it — the second half of
    /// the two-step upload-then-submit the audio path requires.
    ArtifactStored {
        command_id: CommandId,
        artifact: codypendent_protocol::ArtifactRef,
    },
    /// Session list returned from the daemon (Adoption 11 S1).
    SessionList(Vec<codypendent_protocol::SessionSummary>),
    /// One ranked Session Library page. `correlation_id` is what the loop uses
    /// to recover the query the page answers — the payload does not repeat it.
    SessionSearchResults {
        correlation_id: Option<codypendent_protocol::MessageId>,
        page: codypendent_protocol::SessionSearchPage,
    },
    /// The daemon's authoritative projection after a lifecycle mutation.
    SessionLifecycleApplied(Box<codypendent_protocol::SessionSummary>),
    /// A session was deleted; `tombstoned` reports which retention outcome the
    /// daemon actually applied, never what the client asked for.
    SessionDeleted {
        session_id: SessionId,
        tombstoned: bool,
    },
    /// A lifecycle export produced an artifact.
    SessionExported {
        command_id: CommandId,
        artifact: codypendent_protocol::ArtifactRef,
    },
    /// Workspace file search results returned from the daemon (Adoption 11 M2).
    FileSearchResults {
        query: String,
        matches: Vec<codypendent_protocol::FileMatchWire>,
        truncated: bool,
    },
    /// The daemon closed the connection.
    Closed,
}

#[derive(Clone, Debug)]
struct PendingVoiceUpload {
    duration_ms: u64,
    request_message_id: codypendent_protocol::MessageId,
    upload: Envelope,
}

fn workflow_phase_label(phase: codypendent_protocol::WorkflowRunPhase) -> &'static str {
    use codypendent_protocol::WorkflowRunPhase;
    match phase {
        WorkflowRunPhase::Pending => "pending",
        WorkflowRunPhase::Running => "running",
        WorkflowRunPhase::Paused => "paused",
        WorkflowRunPhase::Completed => "completed",
        WorkflowRunPhase::Failed => "failed",
        WorkflowRunPhase::Cancelled => "cancelled",
        _ => "unknown",
    }
}

fn workflow_node_state_label(state: codypendent_protocol::WorkflowNodeState) -> &'static str {
    use codypendent_protocol::WorkflowNodeState;
    match state {
        WorkflowNodeState::Pending => "pending",
        WorkflowNodeState::Running => "running",
        WorkflowNodeState::WaitingApproval => "waiting_approval",
        WorkflowNodeState::Blocked => "blocked",
        WorkflowNodeState::Completed => "completed",
        WorkflowNodeState::Failed => "failed",
        WorkflowNodeState::Skipped => "skipped",
        _ => "unknown",
    }
}

fn workflow_node_update(node: codypendent_protocol::WorkflowNodeView) -> WorkflowNodeUpdate {
    WorkflowNodeUpdate {
        node_id: node.node_id,
        state: workflow_node_state_label(node.state).to_owned(),
        cost: node
            .cost
            .as_ref()
            .map_or_else(|| "\u{2014}".to_owned(), render_node_cost),
        error: node.error.unwrap_or_else(|| "\u{2014}".to_owned()),
    }
}

fn workflow_snapshot_action(snapshot: codypendent_protocol::WorkflowRunSnapshot) -> Action {
    Action::WorkflowSnapshotLoaded {
        workflow_run_id: snapshot.workflow_run_id,
        phase: workflow_phase_label(snapshot.phase).to_owned(),
        nodes: snapshot
            .nodes
            .into_iter()
            .map(workflow_node_update)
            .collect(),
    }
}

fn workflow_event_action(event: codypendent_protocol::WorkflowEvent) -> Action {
    use codypendent_protocol::WorkflowEvent;
    match event {
        WorkflowEvent::NodeTransitioned(node) => {
            let workflow_run_id = node.workflow_run_id.clone();
            let node = workflow_node_update(node);
            Action::WorkflowNodeUpdated {
                workflow_run_id,
                node_id: node.node_id,
                state: node.state,
                cost: node.cost,
                error: node.error,
            }
        }
        WorkflowEvent::RunPhaseChanged {
            workflow_run_id,
            phase,
        } => Action::WorkflowPhaseUpdated {
            workflow_run_id,
            phase: workflow_phase_label(phase).to_owned(),
        },
        _ => Action::NoOp,
    }
}

fn workflow_label_for_run<'a>(state: &'a AppState, workflow_run_id: &str) -> Option<&'a str> {
    state
        .workflow
        .iter()
        .find(|card| card.workflow_run_id.as_deref() == Some(workflow_run_id))
        .map(|card| card.workflow.as_str())
}

fn wire_blackboard_cards(
    state: &AppState,
    items: &[codypendent_protocol::BlackboardItemView],
) -> Vec<BlackboardItemCard> {
    items
        .iter()
        .map(|item| {
            wire_blackboard_item_card(workflow_label_for_run(state, &item.workflow_run_id), item)
        })
        .collect()
}

/// Project one Hub-fetched repo summary into its pre-rendered display card
/// (Local models: Unsloth catalog) — the tui crate performs no formatting, so
/// this happens here, mirroring `model_card`/`load_provider_cards`.
fn unsloth_repo_card(repo: codypendent_integrations::unsloth::HfRepoSummary) -> UnslothRepoCard {
    UnslothRepoCard {
        id: repo.id,
        downloads_label: format!("{} downloads", human_count(repo.downloads)),
        likes_label: format!("{} likes", human_count(repo.likes)),
        // The Hub reports an ISO-8601 timestamp; only the date is shown. A
        // missing value renders as "unknown" — never a fabricated date.
        updated_label: match repo.updated_at.as_deref().and_then(|s| s.split('T').next()) {
            Some(date) if !date.is_empty() => format!("updated {date}"),
            _ => "updated unknown".to_string(),
        },
    }
}

/// Project one Hub-fetched quant variant into its pre-rendered display card,
/// mirroring [`unsloth_repo_card`].
fn unsloth_quant_card(
    variant: codypendent_integrations::unsloth::QuantVariant,
) -> UnslothQuantCard {
    UnslothQuantCard {
        quant: variant.quant,
        size_label: human_bytes(variant.total_size_bytes),
        file_count: variant.files.len(),
        size_bytes: variant.total_size_bytes,
    }
}

/// A compact human count (`891` / `6.6K` / `6.6M`), for the catalog browser's
/// downloads/likes columns. Display-only, matching `render_node_cost`'s own
/// "the harness pre-renders" convention.
fn human_count(n: u64) -> String {
    const THOUSAND: f64 = 1_000.0;
    const MILLION: f64 = 1_000_000.0;
    let n = n as f64;
    if n >= MILLION {
        format!("{:.1}M", n / MILLION)
    } else if n >= THOUSAND {
        format!("{:.1}K", n / THOUSAND)
    } else {
        format!("{n:.0}")
    }
}

/// A human-readable byte size (`18.7 GB` / `512.0 MB` / `900 B`), for a
/// quant's download size. Binary (1024-based) units, matching how disk/RAM
/// capacity is conventionally reported.
fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Own the read half: forward each live [`SessionEvent`], answer heartbeat
/// `Ping`s through the writer, and signal `Closed` on EOF or error. Runs to
/// completion of each `read_envelope` (never cancelled by the loop's `select!`),
/// so a frame is never torn in half.
async fn read_loop(
    mut read_half: OwnedReadHalf,
    event_tx: mpsc::Sender<ReaderSignal>,
    out_tx: mpsc::Sender<Envelope>,
    client_id: ClientId,
) {
    loop {
        match read_envelope(&mut read_half).await {
            Ok(Some(envelope)) => {
                let correlation_id = envelope.correlation_id;
                match envelope.payload {
                    Payload::Ping => {
                        let pong = Envelope::request(client_id, Payload::Pong);
                        if out_tx.send(pong).await.is_err() {
                            break;
                        }
                    }
                    Payload::Event(event) => {
                        if event_tx
                            .send(ReaderSignal::Event(Box::new(event)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::RemoteUi { message } => {
                        if event_tx
                            .send(ReaderSignal::RemoteUi(message))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::UiPluginLifecycle { plugins, .. } => {
                        if event_tx
                            .send(ReaderSignal::UiPlugins {
                                plugins,
                                correlation_id,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::CommandRejected(error) => {
                        // A refused edit lease drives the presence-lite "blocked"
                        // indicator; every other rejection is a transient notice.
                        let signal = if error.code == "document.range-leased" {
                            ReaderSignal::DocumentLeaseBlocked
                        } else {
                            ReaderSignal::Rejected {
                                code: error.code,
                                message: error.message,
                                correlation_id,
                            }
                        };
                        if event_tx.send(signal).await.is_err() {
                            break;
                        }
                    }
                    Payload::DocumentLeaseGranted { grant, .. } => {
                        if event_tx
                            .send(ReaderSignal::DocumentLeaseGranted {
                                document_id: grant.document_id,
                                lease_id: grant.lease_id,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::DocumentCreated { document_id, .. } => {
                        if event_tx
                            .send(ReaderSignal::DocumentCreated { document_id })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::DocumentPublishRequested {
                        command_id,
                        approval_id,
                        target,
                        changed_files,
                        git_action,
                        ..
                    } => {
                        if event_tx
                            .send(ReaderSignal::DocumentPublishPrepared {
                                command_id,
                                approval_id,
                                target,
                                changed_files,
                                git_action,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::DocumentSync(sync) => {
                        if event_tx
                            .send(ReaderSignal::DocumentSync(Box::new(sync)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::WorkflowRunStarted {
                        workflow_run_id, ..
                    } => {
                        if event_tx
                            .send(ReaderSignal::WorkflowRunStarted { workflow_run_id })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::WorkflowRunSnapshot { snapshot, .. } => {
                        if event_tx
                            .send(ReaderSignal::WorkflowSnapshot(Box::new(snapshot)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::WorkflowEvent { event } => {
                        if event_tx
                            .send(ReaderSignal::WorkflowEvent(event))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::BlackboardItems { command_id, items } => {
                        if event_tx
                            .send(ReaderSignal::BlackboardItems { command_id, items })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // Voice v1 (rubric 8): a stored voice note's ref comes back so
                    // the loop can submit the envelope that references it.
                    Payload::ArtifactStored {
                        command_id,
                        artifact,
                    } => {
                        if event_tx
                            .send(ReaderSignal::ArtifactStored {
                                command_id,
                                artifact,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::BlackboardPosted(item) => {
                        if event_tx
                            .send(ReaderSignal::BlackboardPosted(item))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::Catchup { catchup } => {
                        if event_tx
                            .send(ReaderSignal::Catchup(Box::new(catchup)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::SessionList { sessions, .. } => {
                        if event_tx
                            .send(ReaderSignal::SessionList(sessions))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::SessionSearchResults { page, .. } => {
                        if event_tx
                            .send(ReaderSignal::SessionSearchResults {
                                correlation_id,
                                page,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::SessionLifecycleApplied { session, .. } => {
                        if event_tx
                            .send(ReaderSignal::SessionLifecycleApplied(Box::new(session)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::SessionDeleted {
                        session_id,
                        tombstoned,
                        ..
                    } => {
                        if event_tx
                            .send(ReaderSignal::SessionDeleted {
                                session_id,
                                tombstoned,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::SessionExported {
                        command_id,
                        artifact,
                    } => {
                        if event_tx
                            .send(ReaderSignal::SessionExported {
                                command_id,
                                artifact,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Payload::FileSearchResults {
                        query,
                        matches,
                        truncated,
                        ..
                    } => {
                        if event_tx
                            .send(ReaderSignal::FileSearchResults {
                                query,
                                matches,
                                truncated,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // Everything else (CommandAccepted, stray replies) is not an
                    // input to the reducer's live loop — the TUI's state is driven
                    // by durable events (Chapter 03). Drop it.
                    _ => {}
                }
            }
            Ok(None) | Err(_) => {
                let _ = event_tx.send(ReaderSignal::Closed).await;
                break;
            }
        }
    }
}

/// Own the write half: serialize every outgoing envelope (loop commands + reader
/// pongs) so the two producers never interleave a frame on the socket.
async fn write_loop(
    mut write_half: OwnedWriteHalf,
    mut out_rx: mpsc::Receiver<Envelope>,
    event_tx: mpsc::Sender<ReaderSignal>,
) {
    while let Some(envelope) = out_rx.recv().await {
        if write_envelope(&mut write_half, &envelope).await.is_err() {
            let _ = event_tx.send(ReaderSignal::Closed).await;
            break;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SplashGateDecision {
    Continue,
    Quit,
    Redraw,
    Ignore,
}

/// Interpret only the small, host-owned input vocabulary of the welcome gate.
/// Enter is the sole transition into the workspace; Escape/Ctrl-C remain a
/// humane way out, and resize/focus events repaint without leaking a key into
/// the main composer after startup.
fn splash_gate_decision(event: &CrosstermEvent) -> SplashGateDecision {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    match event {
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Release => SplashGateDecision::Ignore,
        CrosstermEvent::Key(key) if key.code == KeyCode::Enter => SplashGateDecision::Continue,
        CrosstermEvent::Key(key)
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)) =>
        {
            SplashGateDecision::Quit
        }
        CrosstermEvent::Resize(_, _) | CrosstermEvent::FocusGained => SplashGateDecision::Redraw,
        _ => SplashGateDecision::Ignore,
    }
}

/// Hold the completed startup screen until the user explicitly enters the
/// workspace. The existing input thread owns crossterm reads, so this gate and
/// the main event loop share one lossless stream with no terminal teardown.
async fn wait_for_splash_entry(
    guard: &mut TerminalGuard,
    theme: &Theme,
    ready_stage: &str,
    warnings: &[String],
    needs_setup: bool,
    input_rx: &mut mpsc::Receiver<ClientInput>,
) -> anyhow::Result<bool> {
    let draw = |guard: &mut TerminalGuard| -> io::Result<()> {
        guard.terminal_mut().draw(|frame| {
            render_splash(frame, 0, ready_stage, warnings, true, needs_setup, theme);
        })?;
        Ok(())
    };
    draw(guard)?;

    loop {
        match input_rx.recv().await {
            Some(ClientInput::Terminal(event)) => match splash_gate_decision(&event) {
                SplashGateDecision::Continue => return Ok(true),
                SplashGateDecision::Quit => return Ok(false),
                SplashGateDecision::Redraw => draw(guard)?,
                SplashGateDecision::Ignore => {}
            },
            Some(ClientInput::AccessibleLine(_)) => {}
            None => bail!("terminal input closed before the workspace was opened"),
        }
    }
}

/// Bridge blocking `crossterm` input into the async loop on a dedicated OS
/// thread. Polls with a short timeout so it observes `running` going false
/// promptly; sends each event over `tx` until the loop drops the receiver.
fn spawn_input_thread(tx: mpsc::Sender<ClientInput>, running: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(event) => {
                        if tx.blocking_send(ClientInput::Terminal(event)).is_err() {
                            break; // loop ended
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => continue, // timed out; re-check `running`
                Err(_) => break,
            }
        }
    });
}

/// The repository task board's durable key for the directory the TUI was opened
/// in: the **checkout**, via [`crate::repo_anchor::anchor_repository_path`].
///
/// The board's channel id is `board:{path}` (`board_scope_id`), so the path IS
/// the identity. Sending the opened directory instead — which every board call
/// site did until the 2026-08-13 review — minted a second board per
/// subdirectory: `repo/` showed 6 cards, `repo/src` showed 0, and a card
/// created from `src/` was invisible from the root forever, with nothing
/// reporting a problem. The daemon canonicalizes what it receives
/// (`codypendentd::blackboard::repository_board_id`) but has no notion of a
/// repository, so it cannot save a client that hands it a subdirectory; the
/// anchoring has to happen where the checkout is known.
///
/// Every board-bound `repository` in this file goes through here — the read,
/// the subscription's channel id, the card create, and the column move — so no
/// one of them can drift back to the raw directory on its own.
fn board_repository(repository: &str) -> String {
    crate::repo_anchor::anchor_repository_path(Path::new(repository))
        .to_string_lossy()
        .into_owned()
}

/// Map a reducer [`Intent`] to the wire [`CommandBody`], binding the session id
/// the TUI is attached to. A pure 1:1 translation — the whole point of the
/// outbox is that `reduce` stays I/O-free and this is the only place intents
/// become protocol.
fn intent_to_command(intent: Intent, session_id: SessionId, repository: &str) -> CommandBody {
    match intent {
        Intent::RemoteUiMessage(_) => unreachable!(
            "RemoteUiMessage is framed directly by the harness, never mapped to a CommandBody"
        ),
        Intent::ListUiPlugins => CommandBody::ListUiPlugins,
        Intent::SmokeTestUiPlugin { plugin_id } => {
            CommandBody::SmokeTestUiPlugin { plugin_id }
        }
        Intent::EnableUiPlugin { plugin_id, scope } => CommandBody::EnableUiPlugin {
            plugin_id,
            session_id: (scope != "user").then_some(session_id),
            scope,
        },
        Intent::ApproveUiPluginUpdate { plugin_id, receipt } => {
            CommandBody::ApproveUiPluginUpdate {
                plugin_id,
                approval_receipt: receipt,
            }
        }
        Intent::RejectUiPluginUpdate { plugin_id, receipt } => {
            CommandBody::RejectUiPluginUpdate {
                plugin_id,
                approval_receipt: receipt,
            }
        }
        Intent::RevokeUiPlugin { plugin_id } => CommandBody::RevokeUiPlugin { plugin_id },
        Intent::StartRun {
            objective,
            mode,
            model,
        } => CommandBody::StartRun {
            session_id,
            objective,
            mode,
            // Attribute the run to the repository this TUI is attached to, so a
            // shared daemon does not store its memories under its own directory
            // (issue #6 item 1).
            repository: Some(repository.to_owned()),
            // Carry the operator's pinned model (STEP MP2) onto the wire; `None`
            // lets the daemon resolve/route as before.
            model,
        },
        // Task 5 (continuous-session plan): a follow-up once the selected run
        // is terminal. Carries the operator's current `model` pin so a
        // mid-conversation switch is instant (the re-pick applies to this very
        // follow-up); `None` lets the daemon INHERIT the session's model (I-2)
        // from its provenance, unchanged. Repository (I-1) is still not on this
        // wire shape: the daemon RECOVERS it from the session's originating
        // `StartRun` (see `commands::session_run_provenance`), so a follow-up
        // runs against the session's real checkout, not the daemon's frozen
        // `current_dir()`.
        Intent::SubmitUserInput { text, mode, model } => CommandBody::SubmitUserInput {
            session_id,
            text,
            mode,
            model,
            // The composer path submits plain text; the voice host builds its
            // audio-envelope submission directly (see `crate::voice`), never
            // through this intent mapping.
            envelope: None,
        },
        Intent::ResolveApproval {
            approval_id,
            decision,
            scope,
        } => CommandBody::ResolveApproval {
            approval_id,
            decision,
            scope,
        },
        Intent::ResolveQuestion {
            question_id,
            outcome,
        } => CommandBody::ResolveQuestion {
            question_id,
            outcome,
        },
        Intent::PauseRun { run_id } => CommandBody::PauseRun { run_id },
        Intent::ResumeRun { run_id } => CommandBody::ResumeRun { run_id },
        Intent::CancelRun { run_id } => CommandBody::CancelRun { run_id },
        Intent::QueueSteering { run_id, text } => CommandBody::QueueSteering { run_id, text },
        Intent::QueuePrompt {
            text,
            mode,
            delivery,
        } => CommandBody::QueuePrompt {
            session_id,
            text,
            mode,
            delivery,
        },
        Intent::UpdateQueuedPrompt { prompt_id, text } => CommandBody::UpdateQueuedPrompt {
            session_id,
            prompt_id,
            text: Some(text),
            delivery: None,
        },
        Intent::PromoteQueuedPrompt { prompt_id } => CommandBody::PromoteQueuedPrompt {
            session_id,
            prompt_id,
        },
        Intent::DeleteQueuedPrompt { prompt_id } => CommandBody::DeleteQueuedPrompt {
            session_id,
            prompt_id,
        },
        // Phase 4 STEP 4.3: document editing. Subscription to the document's sync
        // stream is arranged separately (in the drain loop) before the first of
        // these is sent, so the client sees its own edit's authoritative result.
        Intent::AcquireDocumentLease {
            document_id,
            block_id,
        } => CommandBody::AcquireDocumentLease {
            lease: DocumentEditLease {
                document_id,
                block_id,
            },
            ttl_seconds: None,
        },
        Intent::ReleaseDocumentLease { lease_id } => CommandBody::ReleaseDocumentLease { lease_id },
        Intent::MutateDocument {
            document_id,
            mutation,
        } => CommandBody::MutateDocument {
            document_id,
            mutation,
        },
        Intent::PublishDocument {
            document_id,
            target,
        } => CommandBody::PublishDocument {
            document_id,
            target,
        },
        // Creation names no document (there is none yet), so the scope defaults
        // to this checkout's repository — the created document lives with the
        // code it documents.
        Intent::CreateDocument { title } => CommandBody::CreateDocument {
            title,
            scope: None,
            repository: Some(repository.to_owned()),
            initial_markdown: None,
        },
        Intent::WatchDocument { .. } => unreachable!(
            "WatchDocument is applied locally by the harness, never sent to the daemon"
        ),
        Intent::SearchEdges { .. } => unreachable!(
            "SearchEdges is applied locally by the harness, never sent to the daemon"
        ),
        Intent::RefreshProjection { .. } => unreachable!(
            "RefreshProjection is applied locally by the harness, never sent to the daemon"
        ),
        Intent::MutateLearning { .. } => unreachable!(
            "MutateLearning is applied locally by the harness, never sent to the daemon"
        ),
        Intent::StartWorkflow {
            workflow_id,
            inputs,
        } => CommandBody::StartWorkflow {
            manifest: String::new(),
            workflow_id: Some(workflow_id),
            inputs,
            repository: Some(repository.to_owned()),
        },
        Intent::WatchWorkflow { .. } => unreachable!(
            "WatchWorkflow is applied locally by the harness, never sent to the daemon"
        ),
        Intent::WatchBoard => unreachable!(
            "WatchBoard is applied locally by the harness, never sent to the daemon"
        ),
        // A column move is a SUPERSESSION server-side: the daemon carries the
        // card's body forward, re-ordinals it to the end of its new column, and
        // publishes the replacement — so the pane never edits its own copy.
        Intent::MoveBoardCard { item_id, status } => CommandBody::UpdateBlackboardItem {
            scope: codypendent_protocol::BlackboardScope::RepositoryBoard {
                repository: board_repository(repository),
            },
            item_id,
            status: Some(status),
            assignee: None,
            ordinal: None,
            payload: None,
        },
        Intent::CreateBoardCard { title } => CommandBody::PostBlackboardItem {
            scope: BlackboardScope::RepositoryBoard {
                repository: board_repository(repository),
            },
            item: BlackboardItemDraft {
                kind: "task".to_owned(),
                payload: serde_json::json!({ "title": title, "description": "" }),
                confidence: None,
                evidence: Vec::new(),
                status: Some("todo".to_owned()),
                assignee: None,
                ordinal: None,
            },
        },
        Intent::PostBlackboardQuestion {
            workflow_run_id,
            text,
        } => CommandBody::PostBlackboardItem {
            scope: BlackboardScope::WorkflowRun { workflow_run_id },
            item: BlackboardItemDraft {
                kind: "open_question".to_owned(),
                payload: serde_json::json!({ "question": text }),
                confidence: None,
                evidence: Vec::new(),
                status: None,
                assignee: None,
                ordinal: None,
            },
        },
        Intent::PauseWorkflow { workflow_run_id } => {
            CommandBody::PauseWorkflow { workflow_run_id }
        }
        Intent::ResumeWorkflow { workflow_run_id } => {
            CommandBody::ResumeWorkflow { workflow_run_id }
        }
        Intent::RetryWorkflowNode {
            workflow_run_id,
            node_id,
        } => CommandBody::RetryWorkflowNode {
            workflow_run_id,
            node_id,
        },
        Intent::CancelWorkflow { workflow_run_id } => {
            CommandBody::CancelWorkflow { workflow_run_id }
        }
        Intent::RunUserShell { command } => CommandBody::RunUserShell {
            session_id,
            command,
        },
        Intent::RememberMemory { text } => CommandBody::RememberMemory {
            session_id,
            text,
        },
        // `AddModel` and `QueryProviderModels` are CLIENT-ONLY intents applied
        // locally by the harness (see the drain loop's interceptions); neither
        // becomes a daemon command, so these mappings are never reached.
        Intent::AddModel { .. } => unreachable!(
            "AddModel is applied locally by the harness (write_add_model), never sent to the daemon"
        ),
        Intent::RemoveModel { .. } => unreachable!(
            "RemoveModel is applied locally by the harness (write_remove_model), never sent to the daemon"
        ),
        Intent::QueryProviderModels { .. } => unreachable!(
            "QueryProviderModels is applied locally by the harness (background GET), never sent to the daemon"
        ),
        Intent::ProbeLocalEndpoints => unreachable!(
            "ProbeLocalEndpoints is a local TCP probe performed by the harness, never sent to the daemon"
        ),
        // D1: the `/keys` intents are CLIENT-ONLY for the same reason (the key
        // never crosses the wire; adapters resolve auth.json at use time) —
        // intercepted in the drain loop, never mapped.
        Intent::SetApiKey { .. } | Intent::RemoveApiKey { .. } => unreachable!(
            "SetApiKey/RemoveApiKey are applied locally by the harness (write_api_key), never sent to the daemon"
        ),
        Intent::VerifyApiKey { .. } => unreachable!(
            "VerifyApiKey is probed locally by the harness (verify_model_key), never sent to the daemon"
        ),
        Intent::SetTheme { .. } => unreachable!(
            "SetTheme is a local display preference persisted by the harness, never sent to the daemon"
        ),
        Intent::SetOnboardComplete | Intent::SetOnboardSkipped => unreachable!(
            "onboarding preferences are persisted locally by the harness, never sent to the daemon"
        ),
        Intent::CreateCouncil { .. } => unreachable!(
            "CreateCouncil is validated and persisted locally by the harness, never sent to the daemon"
        ),
        Intent::DeleteCouncil { .. } => unreachable!(
            "DeleteCouncil is applied locally by the harness (council::remove_definition), never sent to the daemon"
        ),
        Intent::RunCouncil { .. } => unreachable!(
            "RunCouncil is driven off-thread by the harness over its own connection, never sent to the daemon"
        ),
        Intent::CopyText { .. } => unreachable!(
            "CopyText is applied by the terminal presentation, never sent to the daemon"
        ),
        Intent::LoadCouncilResults { .. } => unreachable!(
            "council result reads are applied locally by the TUI harness"
        ),
        Intent::NewConversation => unreachable!(
            "NewConversation is applied locally by the harness, never sent to the daemon"
        ),
        Intent::ForkSession { .. } => unreachable!(
            "ForkSession is applied locally by the harness (fork_session_live), never sent to the daemon"
        ),
        // Local models: Unsloth catalog — CLIENT-ONLY for the same reason as
        // `AddModel`/`QueryProviderModels` above (Hub GETs and the `ollama
        // pull` subprocess run off-thread in the harness; intercepted in the
        // drain loop, never mapped to a daemon command).
        Intent::ListUnslothRepos => unreachable!(
            "ListUnslothRepos is applied locally by the harness (background Hub GET), never sent to the daemon"
        ),
        Intent::ListUnslothQuants { .. } => unreachable!(
            "ListUnslothQuants is applied locally by the harness (background Hub GET), never sent to the daemon"
        ),
        Intent::PullUnslothModel { .. } => unreachable!(
            "PullUnslothModel is applied locally by the harness (ollama pull subprocess), never sent to the daemon"
        ),
        Intent::Notify { .. } => unreachable!(
            "Notify is applied locally by the presentation harness, never sent to the daemon"
        ),
        Intent::ListSessions => unreachable!(
            "ListSessions is framed directly by the harness, never mapped in intent_to_command"
        ),
        Intent::SearchSessions { .. } => unreachable!(
            "SearchSessions is framed directly by the harness (its reply needs the query correlated), never mapped in intent_to_command"
        ),
        Intent::MutateSession { .. } => unreachable!(
            "MutateSession is framed directly by the harness (an export needs its session correlated), never mapped in intent_to_command"
        ),
        Intent::SearchFiles { .. } => unreachable!(
            "SearchFiles is framed directly by the harness, never mapped in intent_to_command"
        ),
        Intent::SwitchSession(..) => unreachable!(
            "SwitchSession is applied locally by the harness, never sent to the daemon"
        ),
    }
}

/// Apply an `Intent::AddModel` to the local config: append (or update in place) a
/// `[[model]]` entry in `<data_dir>/models.toml`, and, when a non-blank key was
/// entered, store it in `<data_dir>/auth.json` (mode `0600`). This is the
/// harness's job because the `tui` crate performs no I/O and never touches the
/// key.
///
/// The written entry is always `provider = "openai-compatible"` (the only wire
/// adapter `ModelConfig`/`client_for` supports today); `base_url` is read from the
/// catalog provider (`<data_dir>/providers.toml` layered over the built-ins). A
/// duplicate `display_id` UPDATES its entry rather than duplicating it. Both files
/// are written atomically (temp + rename) so a concurrent daemon read never sees a
/// torn file.
///
/// Two guards, deliberate and load-bearing (not just brief follow-through):
/// - A blank/whitespace-only `display_id` is rejected outright — nothing is
///   written (neither file). Model ids double as the `auth.json` key, so a blank
///   one is never a legitimate profile, just an empty prompt submitted as-is.
/// - A blank/whitespace-only `api_key` is treated exactly like `None` — the
///   `auth.json` write is skipped entirely. Storing `AuthStore::set(id, "")` would
///   silently shadow a valid `api_key_env` into "no key" at resolution time
///   (`ModelRegistry::client_for` prefers a *present* `auth.json` entry over the
///   env var unconditionally) — a real regression a prior review flagged (SDD
///   ledger M1) and this function must not reintroduce.
///
/// All-or-nothing when a key is entered (SDD ledger M3): `AuthStore::load` is
/// fallible (a hand-corrupted `auth.json` surfaces as `Err`), so it is called
/// BEFORE `models.toml` is written — a corrupt pre-existing `auth.json` aborts
/// the whole add before anything is written, rather than leaving a keyless
/// `models.toml` entry behind while the key silently fails to save. A keyless
/// add never loads `auth.json` at all, exactly as before.
///
/// The entry also records `provider_id`, so the runtime resolves this
/// provider's auth header/prefix and extra headers from the catalog instead of
/// flattening every provider to `Authorization: Bearer` (Azure OpenAI's
/// `api-key` header would otherwise 401 on the first run), and
/// `context_tokens` when the picked row knew it — never a guess. The key is
/// stored twice: under the model's display id (today's behavior) and under the
/// provider-wide `provider/<id>` entry, so the next model added from the same
/// provider does not re-prompt for the same key.
fn write_add_model(
    paths: &RuntimePaths,
    display_id: &str,
    provider_id: &str,
    model: &str,
    api_key: Option<&str>,
    context_tokens: Option<u64>,
) -> anyhow::Result<()> {
    use codypendent_providers::Catalog;
    use codypendent_runtime::auth::AuthStore;
    use codypendent_runtime::models::ModelConfig;

    if display_id.trim().is_empty() {
        bail!("model id must not be blank");
    }

    let data_dir = &paths.data_dir;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating the data dir {}", data_dir.display()))?;

    // A blank/whitespace-only key is treated exactly like `None` (see the doc
    // comment's second guard) — filtered up front so the load-order guard right
    // below and the final write agree on whether a key is really present.
    let key = api_key.filter(|k| !k.trim().is_empty());

    // M3 (all-or-nothing): when a key is present, load auth.json NOW, before
    // models.toml is written. `AuthStore::load` is fallible — a hand-corrupted
    // `auth.json` surfaces as `Err` right here, aborting the whole add before
    // anything is written, rather than leaving a keyless `models.toml` entry
    // behind while the key silently fails to save. A keyless add loads no
    // auth.json at all, exactly as before.
    let auth = key
        .is_some()
        .then(|| {
            AuthStore::load(data_dir)
                .with_context(|| format!("reading {}", data_dir.join("auth.json").display()))
        })
        .transpose()?;

    // Resolve the catalog base_url for the chosen provider (built-ins layered with
    // any user providers.toml; a load failure falls back to the built-ins).
    let acp_store = codypendent_integrations::acp_registry::AcpRegistryStore::new(data_dir);
    let acp_agent = is_acp_provider(paths, provider_id);
    let catalog = Catalog::load_with_user_overrides(&data_dir.join("providers.toml"))
        .unwrap_or_else(|_| Catalog::builtin());
    let (runtime_provider, base_url, provider_model, catalog_provider) = if acp_agent {
        let coordinate = if codypendent_integrations::acp_registry::agent_id_from_coordinate(model)
            == provider_id
        {
            model.to_string()
        } else {
            provider_id.to_string()
        };
        acp_store
            .launch_spec(&coordinate)
            .with_context(|| format!("ACP agent `{provider_id}` is not launchable"))?;
        ("acp".to_string(), String::new(), coordinate, false)
    } else {
        let provider = catalog
            .get(provider_id)
            .ok_or_else(|| anyhow!("provider `{provider_id}` is not in the catalog"))?;
        if !provider_runtime_supported(provider) {
            bail!(
                "provider `{provider_id}` uses {} and is not executable by this build",
                protocol_label(provider.protocol)
            );
        }
        (
            "openai-compatible".to_string(),
            // Normalized on persist: a catalog `base_url` written with a
            // trailing slash (`…/v1/`) would otherwise reach the chat client
            // as `…/v1//chat/completions`.
            normalize_base_url(
                provider
                    .base_url
                    .as_deref()
                    .expect("runtime-supported providers have a non-blank base URL"),
            ),
            model.to_string(),
            true,
        )
    };
    // A catalog row for this exact model fills in the context window when the
    // caller did not already know it (the picker passes what it displayed).
    let context_tokens = context_tokens.or_else(|| {
        catalog
            .model(provider_id, model)
            .and_then(|row| row.context_tokens)
    });

    let models_path = data_dir.join("models.toml");
    let config = ModelConfig {
        id: ModelId(display_id.to_string()),
        provider: runtime_provider,
        base_url,
        model: provider_model,
        api_key_env: String::new(),
        // Only a catalog provider's auth is resolvable from the catalog; an
        // ACP agent's provider id names a registry agent, not a provider.
        provider_id: catalog_provider.then(|| provider_id.to_string()),
        context_tokens,
    };
    crate::models_file::update_model_entries(&models_path, |configs| {
        configs.retain(|c| c.id.0 != display_id);
        configs.push(config);
        Ok(())
    })?;

    // Store the key (hosted providers only) in auth.json at 0600 — loaded
    // above, BEFORE models.toml was written, so a corrupt pre-existing
    // auth.json already aborted the whole operation before this point (M3).
    if let Some(key) = key {
        debug_assert!(
            auth.is_some(),
            "loaded above because `key` is Some (M3 ordering)"
        );
        // The early load above is the M3 abort-on-corrupt check and stays where
        // it is. The write RE-loads under the cross-writer hold, because every
        // save renames a whole map back: without it, a key the desktop shell
        // saved between that load and this save was silently discarded here.
        codypendent_runtime::auth::update(data_dir, |auth| -> anyhow::Result<_> {
            auth.set(display_id, key);
            // Also store it provider-wide, so adding a second model from the
            // same provider needs no second paste of the same key. The runtime
            // reads this entry after the per-model one (`provider_auth_id`).
            if catalog_provider {
                auth.set(provider_auth_id(provider_id), key);
            }
            Ok((codypendent_runtime::auth::Save::Yes, ()))
        })
        .with_context(|| format!("writing {}", data_dir.join("auth.json").display()))?;
    }
    Ok(())
}

/// A provider `base_url` as it should be persisted: trailing slashes trimmed.
/// The catalog stores a few with one (`…/v1/`), and the chat client joins
/// `{base}/chat/completions` — the raw value would produce a double slash on
/// every request. A blank/whitespace-only URL is returned as an empty string
/// (the caller's own validation reports it).
fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// The first of a provider's documented API-key environment variable NAMES
/// that is set to a non-blank value in this process, or `None`. Presence only:
/// the value is read to test it and dropped. Shared by `/keys`, `models list`
/// and the provider cards so all three answer "is a key in hand" the same way.
pub fn provider_env_in_use(provider: &codypendent_providers::Provider) -> Option<String> {
    provider.auth.iter().find_map(|method| match method {
        codypendent_providers::AuthMethod::ApiKey { env, .. } => env
            .iter()
            .find(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
            .cloned(),
        _ => None,
    })
}

/// Resolve provider discovery credentials without leaking them into TUI state.
/// Precedence mirrors the provider-wide portion of the live model runtime:
/// `auth.json[provider/<id>]`, then the first configured non-blank environment
/// value. Empty and whitespace-only values are absent, never valid headers.
fn resolve_provider_api_key(
    provider_id: &str,
    auth: &codypendent_runtime::auth::AuthStore,
    env_names: &[String],
) -> Option<String> {
    if let Some(key) = auth
        .get(&provider_auth_id(provider_id))
        .filter(|key| !key.trim().is_empty())
    {
        return Some(key.to_owned());
    }
    env_names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|key| !key.trim().is_empty())
    })
}

/// Non-secret provider-card projection of [`resolve_provider_api_key`].
fn provider_has_resolvable_key(
    provider_id: &str,
    auth: &codypendent_runtime::auth::AuthStore,
    env_names: &[String],
) -> bool {
    resolve_provider_api_key(provider_id, auth, env_names).is_some()
}

/// One-shot verification of a configured model's credentials (`/keys`,
/// `Ctrl-T`): run the real [`ModelRegistry::check_model`], which resolves the
/// key through the same precedence a run uses and sends the same
/// catalog-declared headers, so an "ok" here means the run would authenticate.
/// The returned reason is the registry's own error text, which never contains
/// key material.
async fn verify_model_key(data_dir: &Path, model_id: &str) -> Result<(), String> {
    use codypendent_runtime::models::{load_models, ModelRegistry};

    let models_path = data_dir.join("models.toml");
    let configs = load_models(&models_path).map_err(|error| error.to_string())?;
    let auth = codypendent_runtime::auth::AuthStore::load(data_dir).unwrap_or_default();
    let catalog =
        codypendent_providers::Catalog::load_with_user_overrides(&data_dir.join("providers.toml"))
            .unwrap_or_else(|_| codypendent_providers::Catalog::builtin());
    ModelRegistry::new(configs)
        .with_auth(auth)
        .with_catalog(catalog)
        .check_model(&ModelId(model_id.to_owned()))
        .await
        .map_err(|error| error.to_string())
}

/// Whether `provider_id` names an agent from the official ACP registry (rather
/// than a catalog provider). Read from the CACHED registry only: the picker
/// already refreshed it when it built its cards, and the intent-drain loop must
/// never block on the network.
fn is_acp_provider(paths: &RuntimePaths, provider_id: &str) -> bool {
    codypendent_integrations::acp_registry::AcpRegistryStore::new(&paths.data_dir)
        .load_cached()
        .ok()
        .is_some_and(|registry| registry.get(provider_id).is_some())
        || codypendent_integrations::acp_registry::local_acp_agent_spec(provider_id).is_some()
        || codypendent_integrations::acp_registry::community_acp_agent(provider_id).is_some()
}

/// The `models.toml` profile id for a connected ACP agent — `acp/<agent>`, or
/// `acp/<agent>#<model>` when pinned to one of the agent's own models, mirroring
/// the coordinate the profile stores.
fn acp_profile_id(provider_id: &str, model: Option<&str>) -> String {
    match model {
        Some(model) => format!("acp/{provider_id}#{model}"),
        None => format!("acp/{provider_id}"),
    }
}

/// One official ACP agent, installed (when necessary) and handshaken away from
/// the render loop: its pinned coordinate plus the models it advertises over
/// the session-config handshake. The single place the TUI talks to a live
/// agent — both the add-model model list and the connect itself go through it,
/// so neither can disagree with the other about what the agent offers.
async fn probe_acp_agent(
    paths: &RuntimePaths,
    provider_id: &str,
    repository: &Path,
) -> anyhow::Result<AcpProbe> {
    let store = codypendent_integrations::acp_registry::AcpRegistryStore::new(&paths.data_dir);
    let spec = store.install(provider_id, false).await?;
    let command = spec.command.to_string_lossy().into_owned();
    let client = codypendent_integrations::acp_client::AcpClient::spawn(
        &command,
        &spec.args,
        &spec.env,
        repository.to_string_lossy().as_ref(),
    )
    .await
    .with_context(|| format!("ACP handshake with `{provider_id}` failed"))?;
    let models = client
        .discovered_models()
        .into_iter()
        .map(|model| model.id)
        .collect();
    drop(client);
    Ok(AcpProbe {
        registry_id: spec.registry_id,
        version: spec.version,
        models,
    })
}

/// What a live handshake with an ACP agent taught the TUI.
struct AcpProbe {
    registry_id: String,
    version: String,
    /// The agent's own model ids, in the agent's order. Empty when it exposes
    /// no model selector — its default model then applies, as before.
    models: Vec<String>,
}

impl AcpProbe {
    /// The coordinate a profile stores: `id@version`, or `id@version#model`
    /// when the user picked one of the agent's own models.
    fn coordinate(&self, model: Option<&str>) -> String {
        match model {
            Some(model) => codypendent_integrations::acp_registry::agent_coordinate_with_model(
                &self.registry_id,
                &self.version,
                model,
            ),
            None => codypendent_integrations::acp_registry::agent_coordinate(
                &self.registry_id,
                &self.version,
            ),
        }
    }
}

/// Install, handshake, and resolve the coordinate one ACP profile stores. The
/// caller writes `models.toml` only after this succeeds, so a bad archive,
/// missing runner, or incompatible agent never leaves a broken selectable
/// profile behind. A `model` pin is verified against what the agent actually
/// advertised — a pin it would not honor is never persisted.
async fn connect_acp_agent(
    paths: &RuntimePaths,
    provider_id: &str,
    repository: &Path,
    model: Option<&str>,
) -> anyhow::Result<String> {
    let probe = probe_acp_agent(paths, provider_id, repository).await?;
    if let Some(model) = model {
        if !probe.models.iter().any(|advertised| advertised == model) {
            bail!("`{provider_id}` does not advertise a model called `{model}`");
        }
    }
    Ok(probe.coordinate(model))
}

/// Apply the client-only `Intent::AddModel` (the event-loop drain arm,
/// extracted so the behavior is directly testable): write `models.toml` +
/// `auth.json` locally, then re-seed the model picker AND re-fire the `/keys`
/// status projection — a model added WITH a key must show `Stored` in `/keys`
/// without a TUI restart. Any loader diagnostic surfaces as a notice.
async fn apply_add_model(
    state: &mut AppState,
    paths: &RuntimePaths,
    display_id: &str,
    provider_id: &str,
    model: &str,
    api_key: Option<&str>,
    context_tokens: Option<u64>,
) {
    match write_add_model(
        paths,
        display_id,
        provider_id,
        model,
        api_key,
        context_tokens,
    ) {
        Ok(()) => {
            // Re-seed the model picker so the new model shows immediately.
            let mut warnings = Vec::new();
            state.models = load_model_cards(paths, &mut warnings).await;
            refresh_runnable_models(state, Some(ModelId(display_id.to_owned())));
            for warning in warnings {
                reduce(state, Action::Issue(warning));
            }
            reload_key_statuses(state, paths);
            reduce(state, Action::Notice(format!("added model {display_id}")));
        }
        Err(error) => {
            reduce(
                state,
                Action::OnboardModelAddFailed {
                    model_id: ModelId(display_id.to_owned()),
                    reason: error.to_string(),
                },
            );
            reduce(
                state,
                Action::Notice(format!("could not add model: {error}")),
            );
        }
    }
}

/// Remove one exact `[[model]]` entry without reserializing the surrounding
/// document. `models.toml` also contains voice, speech, embedding, retrieval,
/// and user-defined future tables; `toml_edit` preserves all of them — plus
/// comments and formatting — while only the matching array-of-tables row is
/// removed.
///
/// The model-specific `auth.json` credential is removed as part of the same
/// operation. Provider-wide credentials remain because other models from that
/// provider may still use them. Both inputs are fully parsed before either is
/// changed, so malformed configuration cannot cause a partial deletion.
fn write_remove_model(paths: &RuntimePaths, model_id: &str) -> anyhow::Result<()> {
    use codypendent_runtime::auth::AuthStore;

    let model_id = model_id.trim();
    if model_id.is_empty() {
        bail!("model id must not be blank");
    }
    let models_path = paths.data_dir.join("models.toml");

    // Held across the WHOLE two-file transaction below, load included: this
    // writes `auth.json` and `models.toml` together and undoes one half when
    // the other fails, so another writer landing between those steps would see
    // — or overwrite — a half-committed pair. Released when this returns.
    let _auth_hold = codypendent_runtime::auth::hold(&paths.data_dir)
        .with_context(|| format!("locking {}", paths.data_dir.join("auth.json").display()))?;

    // Validate the key store before touching models.toml. A corrupt auth.json
    // must never be silently overwritten or leave the operator unsure which
    // half of the requested cleanup happened.
    let mut auth = AuthStore::load(&paths.data_dir)
        .with_context(|| format!("reading {}", paths.data_dir.join("auth.json").display()))?;
    let original_auth = auth.clone();
    let removed_key = auth.remove(model_id);

    let parent = models_path
        .parent()
        .ok_or_else(|| anyhow!("{}: has no parent directory", models_path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let lock_path = parent.join(".models.toml.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", lock_path.display()))?;
    }
    fs4::FileExt::lock_exclusive(&lock)
        .with_context(|| format!("locking {}", lock_path.display()))?;

    let raw = std::fs::read_to_string(&models_path)
        .with_context(|| format!("reading {}", models_path.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", models_path.display()))?;

    // Remove EVERY entry carrying this id, not just the first: a hand-edited
    // `models.toml` can list the same id twice, and `auth.remove` above is
    // unconditional — leaving one copy behind would leave a listed model with
    // no credential.
    let emptied = {
        let Some(item) = doc.get_mut("model") else {
            bail!("model `{model_id}` is not configured");
        };
        let Some(array) = item.as_array_of_tables_mut() else {
            bail!(
                "`model` entry in {} is not an array of tables",
                models_path.display()
            );
        };
        let matching: Vec<usize> = array
            .iter()
            .enumerate()
            .filter_map(|(index, table)| {
                (table.get("id").and_then(toml_edit::Item::as_str) == Some(model_id))
                    .then_some(index)
            })
            .collect();
        if matching.is_empty() {
            bail!("model `{model_id}` is not configured");
        }
        for index in matching.into_iter().rev() {
            array.remove(index);
        }
        array.is_empty()
    };
    if emptied {
        doc.remove("model");
    }

    // Unique per write, not per process: two removals inside one process
    // (the TUI's own and a background reload) collided on a pid-only name,
    // exactly the hazard `models_file::update_model_entries` documents.
    static REMOVE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ticket = REMOVE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = parent.join(format!(".models-{}-{ticket}.toml.tmp", std::process::id()));
    let write_tmp = || -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(doc.to_string().as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp_path, doc.to_string())?;
        Ok(())
    };
    if let Err(error) = write_tmp() {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error).with_context(|| format!("writing {}", tmp_path.display()));
    }

    // Commit the secret cleanup first, then make the already-written model temp
    // visible. Either half can still fail, so each failure path undoes the
    // other half: a failed `auth.save` leaves models.toml untouched (the temp is
    // discarded, never renamed), and a failed rename restores the original key
    // store. The operation is all-or-nothing in both directions.
    if removed_key {
        if let Err(error) = auth.save(&paths.data_dir) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error).with_context(|| {
                format!("writing {}", paths.data_dir.join("auth.json").display())
            });
        }
    }
    if let Err(error) = std::fs::rename(&tmp_path, &models_path) {
        let _ = std::fs::remove_file(&tmp_path);
        if removed_key {
            original_auth.save(&paths.data_dir).with_context(|| {
                format!(
                    "restoring {} after models.toml replacement failed",
                    paths.data_dir.join("auth.json").display()
                )
            })?;
        }
        return Err(error).with_context(|| format!("replacing {}", models_path.display()));
    }
    let _ = fs4::FileExt::unlock(&lock);

    Ok(())
}

/// Apply the model-removal intent and keep the open picker coherent. A removed
/// pending pin is cleared so the next run cannot reference a configuration row
/// that no longer exists; an already-running model is unaffected.
async fn apply_remove_model(state: &mut AppState, paths: &RuntimePaths, model_id: &str) {
    match write_remove_model(paths, model_id) {
        Ok(()) => {
            if state
                .pending_model
                .as_ref()
                .is_some_and(|id| id.0 == model_id)
            {
                state.pending_model = None;
            }
            let mut warnings = Vec::new();
            state.models = load_model_cards(paths, &mut warnings).await;
            refresh_runnable_models(state, None);
            for warning in warnings {
                reduce(state, Action::Issue(warning));
            }
            reload_key_statuses(state, paths);
            if let codypendent_tui::Overlay::ModelPicker { query, selected } = &mut state.overlay {
                let needle = query.to_ascii_lowercase();
                let matches: Vec<usize> = state
                    .models
                    .iter()
                    .enumerate()
                    .filter_map(|(index, card)| {
                        (needle.is_empty()
                            || card.id.0.to_ascii_lowercase().contains(&needle)
                            || card.provider.to_ascii_lowercase().contains(&needle))
                        .then_some(index)
                    })
                    .collect();
                *selected = (*selected).min(matches.len().saturating_sub(1));
                state.selected_model = matches.get(*selected).copied().unwrap_or(0);
            } else {
                state.selected_model = state
                    .selected_model
                    .min(state.models.len().saturating_sub(1));
            }
            reduce(state, Action::Notice(format!("removed model {model_id}")));
        }
        Err(error) => reduce(
            state,
            Action::Notice(format!("could not remove model {model_id}: {error}")),
        ),
    }
}

/// The `auth.json` entry id a [`KeyTarget`] addresses (D1): a model target's
/// id doubles as the entry key (the add-model flow's convention); the Tavily
/// target maps onto the reserved, collision-proof `integrations/tavily` id the
/// daemon's `TavilyKey::discover` reads.
///
/// The two voice ids are the `models.toml` TABLE names, and they are not free
/// choices: `codypendent_runtime::models`'s `audio_api_key` looks the key up as
/// `auth.get("transcription")` / `auth.get("speech")` (the same literals its
/// `AudioTranscriber`/`AudioSynthesizer` pass), so any other spelling here
/// would save a key that reads back as absent — worse than today's missing
/// write path, because it would look like it worked. Pinned end to end by
/// `a_transcription_key_saved_through_keys_is_the_one_the_transcriber_sends`.
fn key_target_auth_id(target: &KeyTarget) -> String {
    match target {
        KeyTarget::Model(id) => id.clone(),
        KeyTarget::Tavily => codypendent_integrations::search::TAVILY_AUTH_ID.to_owned(),
        KeyTarget::Transcription => TRANSCRIPTION_AUTH_ID.to_owned(),
        KeyTarget::Speech => SPEECH_AUTH_ID.to_owned(),
    }
}

/// The host part of a `base_url`, for display only: scheme and path stripped,
/// e.g. `https://api.groq.com/openai/v1` -> `api.groq.com`. Deliberately a
/// string trim rather than a URL parse — this feeds a label, and a `base_url`
/// this cannot make sense of should render as itself, not vanish.
fn endpoint_host(base_url: &str) -> &str {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .trim_start_matches('/');
    let host = rest.split('/').next().unwrap_or(rest);
    if host.is_empty() {
        base_url
    } else {
        host
    }
}

/// The `auth.json` entry the `[transcription]` (speech-to-text) endpoint reads.
/// See [`key_target_auth_id`] for why this string is load-bearing.
const TRANSCRIPTION_AUTH_ID: &str = "transcription";

/// The `auth.json` entry the `[speech]` (text-to-speech) endpoint reads.
/// See [`key_target_auth_id`] for why this string is load-bearing.
const SPEECH_AUTH_ID: &str = "speech";

/// Apply an `Intent::SetApiKey` (`Some(key)`) or `Intent::RemoveApiKey`
/// (`None`) to `<data_dir>/auth.json` (D1). This is the harness's job because
/// the `tui` crate performs no I/O and never touches the key.
///
/// The same load-before-write guard as [`write_add_model`] (M3) applies to
/// BOTH operations: `AuthStore::load` is fallible, so a hand-corrupted
/// pre-existing `auth.json` aborts here with a legible error rather than
/// being silently replaced (a blind `set` on a fresh store would destroy the
/// other entries). The save is atomic at mode `0600` (`AuthStore::save`). A
/// blank/whitespace-only key is rejected outright — storing `set(id, "")`
/// would silently shadow a valid `api_key_env` (the M1 guard). A remove of an
/// absent entry skips the save (nothing changed, and no empty `auth.json` is
/// created for a store that never existed).
fn write_api_key(
    paths: &RuntimePaths,
    target: &KeyTarget,
    key: Option<&str>,
) -> anyhow::Result<()> {
    use codypendent_runtime::auth::Save;

    let data_dir = &paths.data_dir;
    let id = key_target_auth_id(target);
    // Rejected before the hold is taken: a blank key is bad input, not a
    // conflict, and there is nothing to serialize against.
    let key = match key {
        Some(key) if key.trim().is_empty() => bail!("key must not be blank"),
        other => other.map(str::trim),
    };
    // Under the shared hold: this shell, the desktop and `codypendent models`
    // all write `auth.json`, and each write renames a whole map back, so an
    // unserialized load-then-save silently dropped whichever credential the
    // other writer had just stored.
    codypendent_runtime::auth::update(data_dir, |auth| -> anyhow::Result<_> {
        match key {
            Some(key) => {
                auth.set(id, key);
                Ok((Save::Yes, ()))
            }
            // Removing an absent entry writes nothing, so a store that never
            // existed is not created empty.
            None if auth.remove(&id) => Ok((Save::Yes, ())),
            None => Ok((Save::No, ())),
        }
    })
    .with_context(|| format!("writing {}", data_dir.join("auth.json").display()))?;
    Ok(())
}

/// Apply the client-only `Intent::SetApiKey` (the event-loop drain arm,
/// extracted so the behavior is directly testable): write the key to
/// `auth.json` and re-fire the status projection. Model and Tavily credentials
/// are both resolved lazily by the daemon, so neither path needs a restart.
fn apply_set_api_key(state: &mut AppState, paths: &RuntimePaths, target: &KeyTarget, key: &str) {
    match write_api_key(paths, target, Some(key)) {
        Ok(()) => {
            reload_key_statuses(state, paths);
            match target {
                KeyTarget::Model(id) => {
                    // The daemon re-reads auth.json per run, so the
                    // key applies to the NEXT run — no restart.
                    reduce(
                        state,
                        Action::Notice(format!("key saved for {id} — applies to the next run")),
                    );
                }
                KeyTarget::Tavily => {
                    reduce(
                        state,
                        Action::Notice("Tavily key saved — web search is ready".to_owned()),
                    );
                }
                // The transcriber/synthesizer are constructed once, when the
                // daemon (STT) and the TUI (TTS) start, so unlike a model key
                // this one does NOT apply to the next run — say so rather than
                // letting the operator conclude voice is broken.
                KeyTarget::Transcription => {
                    reduce(
                        state,
                        Action::Notice(
                            "voice input key saved — restart the daemon to use it".to_owned(),
                        ),
                    );
                }
                KeyTarget::Speech => {
                    reduce(
                        state,
                        Action::Notice(
                            "voice output key saved — restart the TUI to use it".to_owned(),
                        ),
                    );
                }
            }
        }
        Err(error) => {
            reduce(
                state,
                Action::Notice(format!("could not save key: {error}")),
            );
        }
    }
}

/// Apply the client-only `Intent::RemoveApiKey` (the event-loop drain arm,
/// extracted so the behavior is directly testable): remove the entry from
/// `auth.json` and re-fire the status projection. Tavily resolves per call, so
/// removal also applies immediately.
fn apply_remove_api_key(state: &mut AppState, paths: &RuntimePaths, target: &KeyTarget) {
    match write_api_key(paths, target, None) {
        Ok(()) => {
            reload_key_statuses(state, paths);
            match target {
                KeyTarget::Model(id) => {
                    reduce(
                        state,
                        Action::Notice(format!("key for {id} removed — applies to the next run")),
                    );
                }
                KeyTarget::Tavily => {
                    reduce(
                        state,
                        Action::Notice("Tavily key removed — web search is disabled".to_owned()),
                    );
                }
                // Same snapshot-at-startup caveat as the set path: the running
                // client still holds the key it was built with.
                KeyTarget::Transcription => {
                    reduce(
                        state,
                        Action::Notice(
                            "voice input key removed — restart the daemon to apply".to_owned(),
                        ),
                    );
                }
                KeyTarget::Speech => {
                    reduce(
                        state,
                        Action::Notice(
                            "voice output key removed — restart the TUI to apply".to_owned(),
                        ),
                    );
                }
            }
        }
        Err(error) => {
            reduce(
                state,
                Action::Notice(format!("could not remove key: {error}")),
            );
        }
    }
}

/// Read the `/keys` status projection (D1): one `(model_id, status)` per
/// `models.toml` model, plus the Tavily row's status — `Stored` when an
/// `auth.json` entry exists, else `Env(NAME)` when the model declares an
/// `api_key_env` (the NAME only, never the value), else `Missing` — plus one
/// row per CONFIGURED voice table (`[transcription]`/`[speech]`), which follow
/// the same precedence against their own `auth.json` entry id.
///
/// The Tavily row mirrors the daemon's `TavilyKey::discover` precedence: the
/// reserved `auth.json` entry first, then the `TAVILY_API_KEY` env var. The
/// env check reads THIS client's environment as an approximation of the
/// daemon's (the same approximation the model rows already make with
/// `api_key_env`) and checks PRESENCE only — the value is never read into the
/// projection.
///
/// Best-effort: a corrupt `auth.json` or unreadable `models.toml` degrades to
/// "no stored keys"/"no models" with a diagnostic in `warnings` (the terminal
/// is already in the alternate screen when this first runs, so nothing prints
/// to stderr). The WRITE path (`write_api_key`) still surfaces the same
/// corruption as a hard error — statuses are a view, never the authority.
fn load_key_statuses(
    paths: &RuntimePaths,
    warnings: &mut Vec<String>,
) -> (Vec<(String, KeyStatus)>, KeyStatus, Vec<VoiceKeyRow>) {
    use codypendent_runtime::auth::AuthStore;
    use codypendent_runtime::models::load_models;

    let data_dir = &paths.data_dir;
    let auth = AuthStore::load(data_dir).unwrap_or_else(|error| {
        warnings.push(format!(
            "could not read {}: {error}; /keys statuses may be incomplete",
            data_dir.join("auth.json").display()
        ));
        AuthStore::default()
    });
    let configs = load_models(&data_dir.join("models.toml")).unwrap_or_default();
    // The catalog's documented environment variable NAMES are a real step of
    // the credential precedence (`ModelRegistry::api_key_for`); the provider
    // cards already consult them, and these rows did not — so `/keys` said
    // "not set" beside a model a working `OPENAI_API_KEY` would run.
    let catalog =
        codypendent_providers::Catalog::load_with_user_overrides(&data_dir.join("providers.toml"))
            .unwrap_or_else(|_| codypendent_providers::Catalog::builtin());
    let models = configs
        .iter()
        .map(|cfg| {
            // The RUNTIME's order (`ModelRegistry::api_key_for`): both stored
            // entries are consulted before any environment variable. Naming
            // `api_key_env` second reported an unset variable as the source for
            // a model the provider-wide key was actually running.
            // EITHER stored entry outranks any environment variable, so the
            // two are one condition rather than two arms with one answer.
            let stored = auth.get(&cfg.id.0).is_some()
                || cfg
                    .provider_id
                    .as_deref()
                    .and_then(|p| auth.get(&provider_auth_id(p)))
                    .is_some();
            let status = if stored {
                KeyStatus::Stored
            } else if !cfg.api_key_env.trim().is_empty() {
                KeyStatus::Env(cfg.api_key_env.clone())
            } else if let Some(name) = cfg
                .provider_id
                .as_deref()
                .and_then(|p| catalog.get(p))
                .and_then(provider_env_in_use)
            {
                KeyStatus::Env(name)
            } else {
                KeyStatus::Missing
            };
            (cfg.id.0.clone(), status)
        })
        .collect();
    let tavily = if auth
        .get(codypendent_integrations::search::TAVILY_AUTH_ID)
        .is_some()
    {
        KeyStatus::Stored
    } else if std::env::var(codypendent_integrations::search::key::TAVILY_API_KEY_ENV)
        .is_ok_and(|value| !value.trim().is_empty())
    {
        KeyStatus::Env(codypendent_integrations::search::key::TAVILY_API_KEY_ENV.to_owned())
    } else {
        KeyStatus::Missing
    };
    // Voice rows exist only for a table that is actually configured: an absent
    // `[transcription]`/`[speech]` has no endpoint, so there is nothing a key
    // would authenticate against. A models.toml whose voice tables do not parse
    // degrades to no rows with a diagnostic, matching this function's contract
    // that statuses are a view and never the authority.
    let audio = match load_audio_models(&data_dir.join("models.toml")) {
        Ok(audio) => audio,
        Err(error) => {
            warnings.push(format!(
                "could not read the [transcription]/[speech] tables in {}: {error}; \
                 their /keys rows are hidden",
                data_dir.join("models.toml").display()
            ));
            Default::default()
        }
    };
    let voice = [
        (
            KeyTarget::Transcription,
            "Voice input (speech-to-text)",
            audio.transcription.as_ref(),
        ),
        (
            KeyTarget::Speech,
            "Voice output (text-to-speech)",
            audio.speech.as_ref(),
        ),
    ]
    .into_iter()
    .filter_map(|(target, label, config)| {
        let config = config?;
        let status = if auth.get(&key_target_auth_id(&target)).is_some() {
            KeyStatus::Stored
        } else if config.api_key_env.trim().is_empty() {
            KeyStatus::Missing
        } else {
            KeyStatus::Env(config.api_key_env.clone())
        };
        Some(VoiceKeyRow {
            target,
            label: label.to_owned(),
            // The endpoint, not the key: which model at which host, so two
            // configured voice rows are told apart at a glance. The HOST, not
            // the whole base_url — the row's detail line also carries the key
            // status, and a full URL pushes that off the panel.
            detail: format!("{} · {}", config.model, endpoint_host(&config.base_url)),
            status,
        })
    })
    .collect();
    (models, tavily, voice)
}

/// Re-read the key statuses and fold them into the TUI state (D1) — after the
/// initial seed, after every key write, and after a daemon restart. A
/// best-effort read diagnostic (e.g. a corrupt `auth.json`) surfaces as a
/// notice, exactly like the boot-time seed's diagnostics.
fn reload_key_statuses(state: &mut AppState, paths: &RuntimePaths) {
    let mut warnings = Vec::new();
    let (models, tavily, voice) = load_key_statuses(paths, &mut warnings);
    reduce(
        state,
        Action::ApiKeyStatusesLoaded {
            models,
            tavily,
            voice,
        },
    );
    for warning in warnings {
        reduce(state, Action::Issue(warning));
    }
}

/// The provider's OpenAI-compatible model-list URL: `<base_url>/models`. The
/// catalog `base_url` already carries its version segment (`…/v1`, `…/v4`, …),
/// so the list route is its sibling `/models` — never `/v1/models` (which would
/// double the version). A trailing slash is trimmed so the join is exact.
fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// One entry of a provider's `/models` response, keeping the OPTIONAL metadata
/// several OpenAI-compatible providers ship alongside the id. Every field but
/// `id` is best-effort: a provider that answers with bare ids parses exactly as
/// before, and a provider that answers with a shape this build does not know
/// simply contributes nothing extra.
///
/// The known spellings, all observed in the wild on the same endpoint the add
/// flow already calls: `context_length` (OpenRouter, Nebius `?verbose=true`),
/// `max_model_len` (vLLM-derived: DeepInfra, Novita, SambaNova),
/// `max_context_length`/`context_window` (Venice and friends), and a nested
/// `pricing.{prompt,completion}` object priced per TOKEN (OpenRouter's
/// convention) which is scaled to per-1M for display.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct DiscoveredModel {
    #[serde(default)]
    id: String,
    #[serde(default, alias = "display_name", alias = "name")]
    name: Option<String>,
    #[serde(
        default,
        alias = "context_length",
        alias = "max_model_len",
        alias = "max_context_length",
        alias = "context_window"
    )]
    context_tokens: Option<u64>,
    #[serde(default)]
    pricing: Option<DiscoveredPricing>,
}

/// A `/models` entry's optional pricing object. The values arrive as strings on
/// OpenRouter (`"0.0000004"`) and as numbers elsewhere, so both are accepted
/// and anything unparseable is simply dropped.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct DiscoveredPricing {
    #[serde(default, alias = "input")]
    prompt: Option<serde_json::Value>,
    #[serde(default, alias = "output")]
    completion: Option<serde_json::Value>,
}

/// A per-token price as USD per 1M tokens, accepting the string and number
/// spellings providers use. `None` for anything that is not a finite,
/// non-negative number — a fabricated price is worse than a blank column.
fn price_per_1m(value: Option<&serde_json::Value>) -> Option<f64> {
    let raw = match value? {
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok()?,
        other => other.as_f64()?,
    };
    (raw.is_finite() && raw >= 0.0).then_some(raw * 1_000_000.0)
}

/// Parse an OpenAI/Ollama `/models` response body (`{ "object": "list", "data":
/// [ { "id": "…" }, … ] }`) into rows: trim each id, skip blank/missing, dedup
/// preserving order, and keep any optional metadata the provider volunteered.
/// An empty result is an `Err` so the caller can fall back to the catalog
/// uniformly. A pure function over the body string — the network GET is in
/// `query_provider_models` — so it is directly unit-testable. The error strings
/// are generic and never carry a key.
fn parse_models_response(body: &str) -> Result<Vec<AddModelRow>, String> {
    #[derive(serde::Deserialize)]
    struct ModelsResponse {
        #[serde(default)]
        data: Vec<DiscoveredModel>,
    }
    let parsed: ModelsResponse = serde_json::from_str(body)
        .map_err(|_| "the provider returned an unexpected response".to_string())?;
    let mut rows: Vec<AddModelRow> = Vec::new();
    for entry in parsed.data {
        let id = entry.id.trim().to_string();
        if id.is_empty() || rows.iter().any(|row| row.id == id) {
            continue;
        }
        let pricing = entry.pricing.unwrap_or_default();
        rows.push(AddModelRow {
            id,
            name: entry
                .name
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty()),
            // A provider's own reading is normally trusted over the curated
            // catalog, but it is untrusted INPUT: `load_models` clamps it to
            // `MAX_PLAUSIBLE_CONTEXT_TOKENS` on every read regardless of writer,
            // so an over-large value here would only ever be displayed in the
            // picker and persisted into models.toml as a number nothing honors.
            // Reject it at the parse instead of showing a lie.
            context_tokens: entry
                .context_tokens
                .filter(|tokens| *tokens > 0 && *tokens <= MAX_PLAUSIBLE_CONTEXT_TOKENS),
            cost_per_1m_input_usd: price_per_1m(pricing.prompt.as_ref()),
            cost_per_1m_output_usd: price_per_1m(pricing.completion.as_ref()),
            live: true,
        });
    }
    if rows.is_empty() {
        return Err("provider returned no models".to_string());
    }
    Ok(rows)
}

/// Merge a provider's live listing with the curated catalog rows for the same
/// provider: catalog metadata fills any gap a live row left (never overwriting
/// what the provider itself said), and catalog models the listing did not name
/// are appended as unconfirmed (`live: false`) rows — the offline/no-listing
/// path. Live rows keep their listing order; catalog-only rows follow in
/// catalog order, so the provider's own ranking survives.
fn merge_catalog_rows(live: Vec<AddModelRow>, catalog: &[AddModelRow]) -> Vec<AddModelRow> {
    let mut rows = live;
    for row in &mut rows {
        let Some(known) = catalog.iter().find(|entry| entry.id == row.id) else {
            continue;
        };
        if row.name.is_none() {
            row.name.clone_from(&known.name);
        }
        if row.context_tokens.is_none() {
            row.context_tokens = known.context_tokens;
        }
        if row.cost_per_1m_input_usd.is_none() {
            row.cost_per_1m_input_usd = known.cost_per_1m_input_usd;
        }
        if row.cost_per_1m_output_usd.is_none() {
            row.cost_per_1m_output_usd = known.cost_per_1m_output_usd;
        }
    }
    for known in catalog {
        if !rows.iter().any(|row| row.id == known.id) {
            rows.push(known.clone());
        }
    }
    rows
}

/// The catalog's curated `[[model]]` rows for one provider, as pick-list rows.
/// `live: false` — these are offerable, but nothing has confirmed this account
/// can reach them.
fn catalog_rows_for(
    catalog: &codypendent_providers::Catalog,
    provider_id: &str,
) -> Vec<AddModelRow> {
    catalog
        .models()
        .filter(|model| model.provider_id == provider_id)
        .map(|model| AddModelRow {
            id: model.id.clone(),
            name: model.name.clone(),
            context_tokens: model.context_tokens,
            cost_per_1m_input_usd: model.cost_per_1m_input_usd,
            cost_per_1m_output_usd: model.cost_per_1m_output_usd,
            live: false,
        })
        .collect()
}

/// The on-disk shape of one cached provider listing
/// (`<data_dir>/model_lists/<provider>.json`). Plain data, no key material: it
/// holds exactly what the pick-list shows. `fetched_at_unix` is seconds since
/// the epoch — the same dependency-free stamp `humantime_now` uses, rather
/// than pulling a time-formatting crate into this crate's build.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedModelList {
    provider_id: String,
    fetched_at_unix: u64,
    models: Vec<CachedModelRow>,
}

/// One cached row. Mirrors [`AddModelRow`] minus `live` — everything in the
/// cache came from a live listing at `fetched_at`, and is re-marked as such
/// when it is read back.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedModelRow {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_per_1m_input_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_per_1m_output_usd: Option<f64>,
}

/// Where one provider's cached listing lives.
fn model_list_cache_path(data_dir: &Path, provider_id: &str) -> PathBuf {
    // Provider ids are catalog identifiers (`azure-openai`, `nebius`), but the
    // value reaches here from a user-editable `providers.toml`, so any path
    // separator is neutralized before it becomes a file name.
    let safe: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir.join("model_lists").join(format!("{safe}.json"))
}

/// Read a provider's cached listing, if one is on disk and parseable. Returns
/// the rows (re-marked `live`, since they were a live listing when written)
/// and a human age label for the header. A missing/corrupt cache is simply
/// `None` — the cache is an accelerator, never a source of truth.
fn read_model_list_cache(data_dir: &Path, provider_id: &str) -> Option<(Vec<AddModelRow>, String)> {
    let text = std::fs::read_to_string(model_list_cache_path(data_dir, provider_id)).ok()?;
    let cached: CachedModelList = serde_json::from_str(&text).ok()?;
    if cached.models.is_empty() {
        return None;
    }
    let rows = cached
        .models
        .into_iter()
        .map(|row| AddModelRow {
            id: row.id,
            name: row.name,
            context_tokens: row.context_tokens,
            cost_per_1m_input_usd: row.cost_per_1m_input_usd,
            cost_per_1m_output_usd: row.cost_per_1m_output_usd,
            live: true,
        })
        .collect();
    Some((rows, cache_age_label(cached.fetched_at_unix, unix_now())))
}

/// Seconds since the Unix epoch, or `0` when the clock is before it (the same
/// dependency-free stamp the crash log uses).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A human age for a cached listing ("4m ago"). A stamp from the future (a
/// clock that moved backwards) reads as "just now" rather than as a negative
/// age — the label is a hint, and `Ctrl-R` is always available.
fn cache_age_label(fetched_at_unix: u64, now_unix: u64) -> String {
    let minutes = now_unix.saturating_sub(fetched_at_unix) / 60;
    match minutes {
        m if m < 1 => "just now".to_owned(),
        m if m < 60 => format!("{m}m ago"),
        m if m < 60 * 24 => format!("{}h ago", m / 60),
        m => format!("{}d ago", m / (60 * 24)),
    }
}

/// Persist a provider's live listing for the next add (instant seed). Failures
/// are ignored: a cache that cannot be written must never break the add flow.
fn write_model_list_cache(data_dir: &Path, provider_id: &str, rows: &[AddModelRow]) {
    let path = model_list_cache_path(data_dir, provider_id);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let cached = CachedModelList {
        provider_id: provider_id.to_owned(),
        fetched_at_unix: unix_now(),
        models: rows
            .iter()
            .filter(|row| row.live)
            .map(|row| CachedModelRow {
                id: row.id.clone(),
                name: row.name.clone(),
                context_tokens: row.context_tokens,
                cost_per_1m_input_usd: row.cost_per_1m_input_usd,
                cost_per_1m_output_usd: row.cost_per_1m_output_usd,
            })
            .collect(),
    };
    if cached.models.is_empty() {
        return;
    }
    let Ok(rendered) = serde_json::to_string_pretty(&cached) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, rendered.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// GET `<base_url>/models` for the add-model flow (model-discovery), applying the
/// provider's auth header only when a non-blank `api_key` is present (a keyless
/// OpenAI-compatible endpoint sends none). Bounded at 10s so a hung endpoint
/// can't wedge the query task. Non-2xx → `Err` with the STATUS ONLY (never the
/// key); the body is parsed defensively. Every returned `reason` is key-free and
/// URL-free (send errors map to fixed strings; the auth value is marked
/// sensitive so reqwest cannot echo it in any error). This is the only I/O in
/// the model-discovery feature; it runs on a spawned task off the UI thread.
async fn query_provider_models(
    base_url: &str,
    header: &str,
    prefix: &str,
    extra_headers: &BTreeMap<String, String>,
    api_key: Option<&str>,
) -> Result<Vec<AddModelRow>, String> {
    let url = models_url(base_url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| "could not build the HTTP client".to_string())?;
    let mut request = client.get(&url);
    for (name, value) in extra_headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "the provider has an invalid extra-header name".to_string())?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| "the provider has an invalid extra-header value".to_string())?;
        request = request.header(name, value);
    }
    if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        // Mark the auth value sensitive so reqwest redacts it from any error /
        // debug (mirrors the GitHub client). The key never appears in a reason.
        match reqwest::header::HeaderValue::from_str(&format!("{prefix}{key}")) {
            Ok(mut value) => {
                value.set_sensitive(true);
                request = request.header(header, value);
            }
            Err(_) => return Err("the API key is not a valid header value".to_string()),
        }
    }
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            "request timed out".to_string()
        } else if error.is_connect() {
            "could not connect to the provider".to_string()
        } else {
            "the model-list request failed".to_string()
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let body = response
        .text()
        .await
        .map_err(|_| "could not read the response body".to_string())?;
    parse_models_response(&body)
}

/// The document a doc-editing intent operates on, when it is one the harness must
/// be subscribed to before sending (an edit needs to observe its own resulting
/// sync). A release needs no subscription.
fn doc_intent_target(intent: &Intent) -> Option<DocumentId> {
    match intent {
        Intent::AcquireDocumentLease { document_id, .. }
        | Intent::MutateDocument { document_id, .. }
        | Intent::PublishDocument { document_id, .. }
        | Intent::WatchDocument { document_id } => Some(*document_id),
        _ => None,
    }
}

/// Seed a document's client replica from its current persisted CRDT snapshot — the
/// document read path (Phase 4 STEP 4.3). Falls back to an empty replica when the
/// pool is absent or the read fails: an empty replica still converges on the first
/// full-snapshot sync, so editing degrades gracefully rather than breaking.
async fn seed_replica(pool: Option<&sqlx::SqlitePool>, document_id: DocumentId) -> DocumentReplica {
    if let Some(pool) = pool {
        match DocumentStore::new().load(pool, document_id).await {
            Ok(Some(document)) => match document.crdt.snapshot() {
                Ok(snapshot) => {
                    match DocumentReplica::from_snapshot(&snapshot, document.revision) {
                        Ok(replica) => return replica,
                        Err(error) => {
                            eprintln!("codypendent: could not seed a doc replica: {error}")
                        }
                    }
                }
                Err(error) => eprintln!("codypendent: could not read a doc snapshot: {error}"),
            },
            Ok(None) => {}
            Err(error) => eprintln!("codypendent: could not load a document to seed: {error}"),
        }
    }
    DocumentReplica::empty()
}

/// Merge one incoming [`DocumentSync`] into the document's replica and project the
/// result into a reducer action: the block-structured editor view (from the merged
/// replica) plus the review rail's pending suggestions (re-read from the store,
/// since a suggestion rides the DB, not the CRDT bytes). `None` when the merge or
/// projection fails.
async fn merge_document_sync(
    replicas: &mut HashMap<DocumentId, DocumentReplica>,
    pool: Option<&sqlx::SqlitePool>,
    sync: DocumentSync,
) -> Option<Action> {
    let document_id = sync.document_id;
    let replica = replicas
        .entry(document_id)
        .or_insert_with(DocumentReplica::empty);
    if let Err(error) = replica.merge(&sync) {
        eprintln!("codypendent: could not merge a document sync: {error}");
        return None;
    }
    let blocks: Vec<_> = match replica.blocks() {
        Ok(blocks) => blocks.iter().map(block_view).collect(),
        Err(error) => {
            eprintln!("codypendent: could not project a merged document: {error}");
            return None;
        }
    };
    let revision = format!("r{}", replica.revision());
    // Suggestions live in the DB (not the sync bytes); re-read them so a
    // just-proposed or just-resolved suggestion shows in the review rail.
    let suggestions = match pool {
        Some(pool) => SuggestionStore::new()
            .pending(pool, document_id)
            .await
            .map(|list| list.iter().map(suggestion_view).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    };
    Some(Action::DocumentSynced {
        document_id,
        revision,
        blocks,
        suggestions,
    })
}

/// Wrap a command in a fresh, self-idempotent request envelope (the command id's
/// own string is the idempotency key, so a client-side retry reuses it — same
/// contract as [`Connection::send_command`](crate::connection::Connection)).
fn command_envelope(client_id: ClientId, body: CommandBody) -> Envelope {
    let command_id = CommandId::new();
    Envelope::request(
        client_id,
        Payload::Command(Command {
            command_id,
            idempotency_key: command_id.to_string(),
            expected_revision: None,
            body,
        }),
    )
}

/// One in-flight daemon-owned UI plugin mutation.
///
/// The plugin overlay closes as soon as its intent is emitted, so the request's
/// message id is the only reliable way to distinguish a later lifecycle refusal
/// from an unrelated command rejection on the same multiplexed connection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingUiPluginCommand {
    operation: &'static str,
    plugin_id: String,
}

impl PendingUiPluginCommand {
    fn from_intent(intent: &Intent) -> Option<Self> {
        let (operation, plugin_id) = match intent {
            Intent::SmokeTestUiPlugin { plugin_id } => ("smoke test", plugin_id),
            Intent::EnableUiPlugin { plugin_id, .. } => ("enable", plugin_id),
            Intent::ApproveUiPluginUpdate { plugin_id, .. } => ("approve update", plugin_id),
            Intent::RejectUiPluginUpdate { plugin_id, .. } => ("reject update", plugin_id),
            Intent::RevokeUiPlugin { plugin_id } => ("revoke", plugin_id),
            _ => return None,
        };
        Some(Self {
            operation,
            plugin_id: plugin_id.clone(),
        })
    }

    fn rejection_message(&self, code: &str, message: &str) -> String {
        format!(
            "UI plugin {} failed for {}: {message} ({code})",
            self.operation, self.plugin_id
        )
    }
}

/// Correlation table for lifecycle mutations awaiting a daemon result.
#[derive(Debug, Default)]
struct PendingUiPluginCommands {
    by_message: HashMap<codypendent_protocol::MessageId, PendingUiPluginCommand>,
}

impl PendingUiPluginCommands {
    fn observe(
        &mut self,
        message_id: codypendent_protocol::MessageId,
        pending: PendingUiPluginCommand,
    ) {
        self.by_message.insert(message_id, pending);
    }

    fn resolve(
        &mut self,
        correlation_id: Option<codypendent_protocol::MessageId>,
    ) -> Option<PendingUiPluginCommand> {
        correlation_id.and_then(|message_id| self.by_message.remove(&message_id))
    }

    fn clear(&mut self) {
        self.by_message.clear();
    }
}

/// Owns the exact unacknowledged first-run request across transport swaps.
/// Keeping the whole envelope is deliberate: retry must preserve both reply
/// correlation and the command's daemon-side idempotency identity.
#[derive(Debug, Default)]
struct PendingStartRunCommand {
    envelope: Option<Envelope>,
}

impl PendingStartRunCommand {
    fn observe_outbound(&mut self, envelope: &Envelope) {
        debug_assert!(matches!(
            &envelope.payload,
            Payload::Command(Command {
                body: CommandBody::StartRun { .. },
                ..
            })
        ));
        self.envelope = Some(envelope.clone());
    }

    fn matches_rejection(&self, correlation_id: Option<codypendent_protocol::MessageId>) -> bool {
        self.envelope
            .as_ref()
            .is_some_and(|envelope| correlation_id == Some(envelope.message_id))
    }

    fn retry_envelope(&self, client_id: ClientId) -> Option<Envelope> {
        self.envelope.clone().map(|mut envelope| {
            // A resume token normally preserves the client id, but reconnect
            // may legitimately mint a new identity after token expiry. Command
            // and message/idempotency identities stay exact; only the transport
            // principal is rebound to the accepted handshake.
            envelope.client_id = client_id;
            envelope
        })
    }

    fn clear(&mut self) {
        self.envelope = None;
    }
}

fn remote_ui_envelope(
    client_id: ClientId,
    session_id: SessionId,
    message: codypendent_protocol::UiWireMessage,
) -> Envelope {
    let mut envelope = Envelope::request(
        client_id,
        Payload::RemoteUi {
            message: Box::new(message),
        },
    );
    envelope.session_id = Some(session_id);
    envelope
}

/// A replacement transport prepared for an in-place fresh conversation. The
/// connection is already attached, so swapping it into the event loop cannot
/// expose an unattached or half-created UI state.
struct FreshLiveSession {
    session_id: SessionId,
    catchup: Catchup,
    pending: std::collections::VecDeque<Envelope>,
    resume_token: Option<String>,
    live: LiveIo,
}

fn spawn_session_transition(
    generation: u64,
    request: SessionTransitionRequest,
    paths: RuntimePaths,
    repository: String,
    subscriptions: Vec<Subscription>,
    resume: Option<codypendent_protocol::ResumeToken>,
    tx: mpsc::Sender<ReaderSignal>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let kind = request.kind();
        let result =
            prepare_session_transition(&paths, &repository, request, subscriptions, resume)
                .await
                .map(Box::new)
                .map_err(|error| format!("{error:#}"));
        // A stale/retired receiver drops the boxed FreshLiveSession; LiveIo's
        // Drop aborts both socket tasks so losing this race cannot leak a
        // detached connection.
        let _ = tx
            .send(ReaderSignal::SessionTransition(Box::new(
                SessionTransitionCompletion {
                    generation,
                    kind,
                    result,
                },
            )))
            .await;
    })
}

async fn prepare_session_transition(
    paths: &RuntimePaths,
    repository: &str,
    request: SessionTransitionRequest,
    subscriptions: Vec<Subscription>,
    resume: Option<codypendent_protocol::ResumeToken>,
) -> anyhow::Result<PreparedSessionTransition> {
    let fresh = match &request {
        SessionTransitionRequest::New { workspace_id } => {
            create_fresh_session_live(paths, repository, *workspace_id, &subscriptions, resume)
                .await?
        }
        SessionTransitionRequest::Switch { session_id, .. } => {
            attach_session_live(paths, repository, *session_id, &subscriptions, resume).await?
        }
        SessionTransitionRequest::Fork {
            source_session_id,
            checkpoint,
            ..
        } => {
            fork_session_live(
                paths,
                repository,
                *source_session_id,
                *checkpoint,
                &subscriptions,
                resume,
            )
            .await?
        }
        SessionTransitionRequest::Reconnect {
            session_id,
            last_seen,
        } => {
            let mut failure = None;
            let mut connected = None;
            for attempt in 0_u32..5 {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(
                        250_u64.saturating_mul(1_u64 << attempt.min(4)),
                    ))
                    .await;
                }
                match reconnect_live_session(
                    paths,
                    *session_id,
                    *last_seen,
                    &subscriptions,
                    repository,
                    resume.clone(),
                )
                .await
                {
                    Ok(fresh) => {
                        connected = Some(fresh);
                        break;
                    }
                    Err(error) => failure = Some(error),
                }
            }
            connected.ok_or_else(|| {
                anyhow!(
                    "{}",
                    failure
                        .map(|error| format!("{error:#}"))
                        .unwrap_or_else(|| "the daemon did not answer".to_owned())
                )
            })?
        }
    };

    let after = request.history_after();
    let history = match snapshot_history_target(&fresh.catchup).filter(|target| *target > after) {
        Some(target) => Some(
            read_session_event_range(paths, fresh.session_id, after, target)
                .await
                .map_err(|error| format!("{error:#}")),
        ),
        None => None,
    };
    Ok(PreparedSessionTransition {
        request,
        subscriptions,
        fresh,
        history,
    })
}

/// Create a new durable session and attach the same new socket before returning.
/// The event loop keeps its old transport alive until this whole operation
/// succeeds; on success it atomically swaps transports and drops every
/// old-session forwarder with the old socket.
async fn create_fresh_session_live(
    paths: &RuntimePaths,
    repository: &str,
    workspace: WorkspaceId,
    subscriptions: &[Subscription],
    resume: Option<codypendent_protocol::ResumeToken>,
) -> anyhow::Result<FreshLiveSession> {
    let mut conn = Connection::connect(&paths.socket_path).await?;
    let hello = conn
        .handshake(
            "codypendent-tui-session-switch",
            codypendent_protocol::BUILD_ID,
            resume,
        )
        .await?;
    let title = Path::new(repository)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Codypendent".to_owned());
    let reply = conn
        .send_command(CommandBody::CreateSession {
            workspace,
            title,
            repository: Some(repository.to_owned()),
            internal: false,
            parent_session_id: None,
            parent_run_id: None,
        })
        .await?;
    let session_id = match reply.payload {
        Payload::CommandAccepted { .. } => reply
            .session_id
            .ok_or_else(|| anyhow!("daemon accepted CreateSession without a session id")),
        Payload::CommandRejected(error) => {
            bail!("CreateSession rejected: {} ({})", error.message, error.code)
        }
        other => bail!("unexpected CreateSession reply: {other:?}"),
    }?;

    let attach = conn
        .send_command(CommandBody::AttachSession {
            session_id,
            last_seen_sequence: None,
            subscriptions: subscriptions.to_vec(),
            requested_role: ClientRole::Controller,
            repository: Some(repository.to_owned()),
        })
        .await?;
    let catchup = match attach.payload {
        Payload::Catchup { catchup } => catchup,
        Payload::CommandRejected(error) => {
            bail!(
                "fresh-session attach rejected: {} ({})",
                error.message,
                error.code
            )
        }
        Payload::Error(error) => {
            bail!(
                "fresh-session attach failed: {} ({})",
                error.message,
                error.code
            )
        }
        other => bail!("unexpected fresh-session attach reply: {other:?}"),
    };
    let (live, pending) = LiveIo::start(conn);
    Ok(FreshLiveSession {
        session_id,
        catchup,
        pending,
        resume_token: hello.resume_token.map(|token| token.0),
        live,
    })
}

/// Attach a fresh socket to an *existing* durable session (the `/sessions`
/// resume path). Unlike [`create_fresh_session_live`], this issues no
/// `CreateSession`: it attaches to `session_id` directly so the resumed
/// transcript is caught up and every subsequent command targets the session the
/// socket is bound to. The event loop keeps its old transport alive until this
/// succeeds, then swaps atomically.
async fn attach_session_live(
    paths: &RuntimePaths,
    repository: &str,
    session_id: SessionId,
    subscriptions: &[Subscription],
    resume: Option<codypendent_protocol::ResumeToken>,
) -> anyhow::Result<FreshLiveSession> {
    let mut conn = Connection::connect(&paths.socket_path).await?;
    let hello = conn
        .handshake(
            "codypendent-tui-session-switch",
            codypendent_protocol::BUILD_ID,
            resume,
        )
        .await?;
    let attach = conn
        .send_command(CommandBody::AttachSession {
            session_id,
            last_seen_sequence: None,
            subscriptions: subscriptions.to_vec(),
            requested_role: ClientRole::Controller,
            repository: Some(repository.to_owned()),
        })
        .await?;
    let catchup = match attach.payload {
        Payload::Catchup { catchup } => catchup,
        Payload::CommandRejected(error) => {
            bail!(
                "resumed-session attach rejected: {} ({})",
                error.message,
                error.code
            )
        }
        Payload::Error(error) => {
            bail!(
                "resumed-session attach failed: {} ({})",
                error.message,
                error.code
            )
        }
        other => bail!("unexpected resumed-session attach reply: {other:?}"),
    };
    let (live, pending) = LiveIo::start(conn);
    Ok(FreshLiveSession {
        session_id,
        catchup,
        pending,
        resume_token: hello.resume_token.map(|token| token.0),
        live,
    })
}

async fn fork_session_live(
    paths: &RuntimePaths,
    repository: &str,
    source_session_id: SessionId,
    checkpoint: codypendent_protocol::CheckpointId,
    subscriptions: &[Subscription],
    resume: Option<codypendent_protocol::ResumeToken>,
) -> anyhow::Result<FreshLiveSession> {
    let mut conn = Connection::connect(&paths.socket_path).await?;
    let hello = conn
        .handshake(
            "codypendent-tui-session-fork",
            codypendent_protocol::BUILD_ID,
            resume,
        )
        .await?;
    let reply = conn
        .send_command(CommandBody::ForkSession {
            session_id: source_session_id,
            checkpoint,
            name: None,
        })
        .await?;
    let forked_session_id = match reply.payload {
        Payload::SessionForked { session_id, .. } => session_id,
        Payload::CommandRejected(error) => {
            bail!("ForkSession rejected: {} ({})", error.message, error.code)
        }
        Payload::Error(error) => {
            bail!("ForkSession error: {} ({})", error.message, error.code)
        }
        other => bail!("unexpected ForkSession reply: {other:?}"),
    };

    let attach = conn
        .send_command(CommandBody::AttachSession {
            session_id: forked_session_id,
            last_seen_sequence: None,
            subscriptions: subscriptions.to_vec(),
            requested_role: ClientRole::Controller,
            repository: Some(repository.to_owned()),
        })
        .await?;
    let catchup = match attach.payload {
        Payload::Catchup { catchup } => catchup,
        Payload::CommandRejected(error) => {
            bail!(
                "forked-session attach rejected: {} ({})",
                error.message,
                error.code
            )
        }
        Payload::Error(error) => {
            bail!(
                "forked-session attach failed: {} ({})",
                error.message,
                error.code
            )
        }
        other => bail!("unexpected forked-session attach reply: {other:?}"),
    };
    let (live, pending) = LiveIo::start(conn);
    Ok(FreshLiveSession {
        session_id: forked_session_id,
        catchup,
        pending,
        resume_token: hello.resume_token.map(|token| token.0),
        live,
    })
}

/// Fold an attach-time [`Catchup`] into fresh state. `Catchup::Events` replays
/// each missed event through the reducer. A caller that owns a command socket
/// should use [`fold_catchup_with_history`] for `Catchup::Snapshot`; this pure
/// fallback still projects the session/run summary when paged history cannot be
/// read. Future unknown variants remain inert.
fn fold_catchup(state: &mut AppState, catchup: Catchup) -> u64 {
    match catchup {
        Catchup::Events {
            events, through, ..
        } => {
            for event in events {
                reduce(state, Action::DaemonEvent(Box::new(event)));
            }
            through
        }
        // Too far behind for an event replay — fold the projection so a reopened
        // long-running session shows its title + active runs instead of blank.
        Catchup::Snapshot {
            through,
            projection,
        } => {
            reduce(
                state,
                Action::CatchupSnapshot {
                    title: projection.title,
                    closed: projection.closed,
                    runs: projection.active_runs,
                    pending_approvals: projection.pending_approvals,
                    pending_prompts: projection.pending_prompts,
                },
            );
            through
        }
        _ => 0,
    }
}

/// Fold a reconnect catch-up without replaying events the still-authoritative
/// old reader delivered while the replacement socket was being prepared.
fn fold_reconnect_catchup(state: &mut AppState, catchup: Catchup, already_seen: u64) -> u64 {
    match catchup {
        Catchup::Events {
            events, through, ..
        } => {
            for event in events {
                if event.sequence == 0 || event.sequence > already_seen {
                    reduce(state, Action::DaemonEvent(Box::new(event)));
                }
            }
            through.max(already_seen)
        }
        snapshot @ Catchup::Snapshot { .. } => fold_catchup(state, snapshot).max(already_seen),
        _ => already_seen,
    }
}

/// Restore a snapshot catch-up from the durable paged event log before the live
/// reader starts. A snapshot is intentionally compact and cannot carry the
/// transcript, but `ReadSessionEvents` can rebuild exactly the stable range the
/// snapshot's `through` watermark names. Failures degrade to the snapshot and a
/// persistent boot issue rather than making the entire TUI unavailable.
async fn fold_catchup_with_history(
    state: &mut AppState,
    conn: &mut Connection,
    session_id: SessionId,
    catchup: Catchup,
    boot_warnings: &BootWarnings,
) -> u64 {
    let target = match &catchup {
        Catchup::Snapshot { through, .. } => *through,
        _ => return fold_catchup(state, catchup),
    };

    let mut after = 0_u64;
    let mut history = Vec::new();
    while after < target {
        let remaining = target.saturating_sub(after);
        let limit = u32::try_from(remaining.min(500)).unwrap_or(500);
        let reply = match conn
            .send_command(CommandBody::ReadSessionEvents {
                session_id,
                after_sequence: after,
                limit,
            })
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                push_boot_warning(
                    boot_warnings,
                    format!("could not restore complete session history: {error}"),
                );
                return fold_catchup(state, catchup);
            }
        };
        let (events, through) = match reply.payload {
            Payload::SessionEventsPage {
                session_id: reply_session,
                events,
                through,
                ..
            } if reply_session == session_id => (events, through.min(target)),
            Payload::CommandRejected(error) => {
                push_boot_warning(
                    boot_warnings,
                    format!(
                        "could not restore complete session history: {} ({})",
                        error.message, error.code
                    ),
                );
                return fold_catchup(state, catchup);
            }
            other => {
                push_boot_warning(
                    boot_warnings,
                    format!("unexpected session-history reply: {other:?}"),
                );
                return fold_catchup(state, catchup);
            }
        };
        history.extend(events.into_iter().filter(|event| event.sequence <= target));
        if through <= after {
            push_boot_warning(
                boot_warnings,
                format!(
                    "session history stopped at event {after} before snapshot watermark {target}"
                ),
            );
            return fold_catchup(state, catchup);
        }
        after = through;
    }
    for event in history {
        reduce(state, Action::DaemonEvent(Box::new(event)));
    }
    target
}

/// Read an exact missing live range on an auxiliary handshaken connection.
/// The primary socket remains owned by its reader/writer tasks, so a large-gap
/// snapshot can restore every transcript event without racing their framing.
/// The sequence a catch-up's transcript must be paged up to before the session
/// can be shown whole, or `None` when the catch-up already carried its events.
///
/// Only a snapshot needs this: it is a compact projection by design and holds no
/// transcript. A `through` of zero means there is nothing behind it to page.
fn snapshot_history_target(catchup: &Catchup) -> Option<u64> {
    match catchup {
        Catchup::Snapshot { through, .. } if *through > 0 => Some(*through),
        _ => None,
    }
}

async fn read_session_event_range(
    paths: &RuntimePaths,
    session_id: SessionId,
    mut after: u64,
    target: u64,
) -> anyhow::Result<Vec<SessionEvent>> {
    let mut conn = Connection::connect(&paths.socket_path).await?;
    conn.handshake(
        "codypendent-tui-history",
        codypendent_protocol::BUILD_ID,
        None,
    )
    .await?;
    let mut history = Vec::new();
    while after < target {
        let reply = conn
            .send_command(CommandBody::ReadSessionEvents {
                session_id,
                after_sequence: after,
                limit: u32::try_from(target.saturating_sub(after).min(500)).unwrap_or(500),
            })
            .await?;
        let (events, through) = match reply.payload {
            Payload::SessionEventsPage {
                session_id: returned,
                events,
                through,
                ..
            } if returned == session_id => (events, through.min(target)),
            Payload::CommandRejected(error) => {
                bail!("{} ({})", error.message, error.code);
            }
            other => bail!("unexpected history reply: {other:?}"),
        };
        history.extend(events.into_iter().filter(|event| event.sequence <= target));
        if through <= after {
            bail!("history stopped at event {after} before {target}");
        }
        after = through;
    }
    Ok(history)
}

/// Re-establish the handshaken session socket after an unexpected EOF. The
/// attach resumes from the reducer's durable watermark and preserves every
/// document/workflow subscription accumulated by the old connection.
async fn reconnect_live_session(
    paths: &RuntimePaths,
    session_id: SessionId,
    last_seen_sequence: u64,
    subscriptions: &[Subscription],
    repository: &str,
    resume: Option<codypendent_protocol::ResumeToken>,
) -> anyhow::Result<FreshLiveSession> {
    let mut conn = Connection::connect(&paths.socket_path).await?;
    let hello = conn
        .handshake("codypendent-tui", codypendent_protocol::BUILD_ID, resume)
        .await?;
    let reply = conn
        .send_command(CommandBody::AttachSession {
            session_id,
            last_seen_sequence: Some(last_seen_sequence),
            subscriptions: subscriptions.to_vec(),
            requested_role: ClientRole::Controller,
            repository: Some(repository.to_owned()),
        })
        .await?;
    let catchup = match reply.payload {
        Payload::Catchup { catchup } => catchup,
        Payload::CommandRejected(error) => bail!("{} ({})", error.message, error.code),
        other => bail!("unexpected reconnect reply: {other:?}"),
    };
    let (live, pending) = LiveIo::start(conn);
    Ok(FreshLiveSession {
        session_id,
        catchup,
        pending,
        resume_token: hello.resume_token.map(|token| token.0),
        live,
    })
}

/// Resolve the session for `repo`, reusing the one this repo last used when it
/// still exists, otherwise creating a fresh one. Returns the session id and its
/// attach-time catch-up. This is what makes "close the TUI, reopen, the run
/// continued" work: the mapping persists across launches.
async fn resolve_or_create_session(
    conn: &mut Connection,
    store: &mut SessionStore,
    paths: &RuntimePaths,
    repo: &Path,
) -> anyhow::Result<(SessionId, WorkspaceId, Catchup)> {
    let key = repo.to_string_lossy().into_owned();

    // Try to resume the repo's remembered session.
    if let Some(stored) = store.sessions.get(&key).copied() {
        let reply = conn
            .send_command(CommandBody::AttachSession {
                session_id: stored.session_id,
                last_seen_sequence: None,
                subscriptions: default_subscriptions(),
                requested_role: ClientRole::Controller,
                // The canonical repo root, so the daemon can build its code
                // graph on open — not only on the first run.
                repository: Some(key.clone()),
            })
            .await?;
        // Only resume when the catch-up proves the session still exists. The
        // daemon can't tell an *absent* session from an empty one — it reports max
        // sequence 0 for both and replies with an empty `Catchup`, never a
        // rejection — but a real session always replays at least its
        // `SessionCreated` event (sequence 1). Accepting a zero-event catch-up
        // would open a blank TUI bound to a dead id whose every `StartRun` is then
        // rejected `session-not-found`; instead fall through and create a fresh
        // session, keeping the workspace. (issue #6 item 6)
        if let Payload::Catchup { catchup } = reply.payload {
            // A CLOSED session resumes technically (through > 0) but the event
            // loop exits the moment it folds `SessionClosed` — and with the
            // store never overwritten, every later launch would re-open the
            // closed session and instantly exit: a permanent lockout. Treat
            // closed like missing and fall through to create a fresh session.
            if catchup_proves_session_exists(&catchup) && !catchup_shows_closed(&catchup) {
                return Ok((stored.session_id, stored.workspace_id, catchup));
            }
        }
        // Rejected, or a zero-event catch-up to a session the daemon no longer has
        // (fresh data dir, GC'd, or closed): fall through and create a new one.
    }

    // Create a new session (reusing this repo's workspace id if we have one, so a
    // recreated session still belongs to the same logical workspace).
    let workspace = store
        .sessions
        .get(&key)
        .map(|s| s.workspace_id)
        .unwrap_or_default();
    let title = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| key.clone());

    let created = conn
        .send_command(CommandBody::CreateSession {
            workspace,
            title,
            // The canonical repo root, so the daemon can build its code graph
            // on open — not only on the first run.
            repository: Some(key.clone()),
            internal: false,
            parent_session_id: None,
            parent_run_id: None,
        })
        .await?;
    let session_id = match &created.payload {
        Payload::CommandAccepted { .. } => created.session_id.ok_or_else(|| {
            anyhow!("daemon accepted CreateSession but its reply carried no session_id")
        })?,
        Payload::CommandRejected(error) => {
            bail!("CreateSession rejected: {} ({})", error.message, error.code)
        }
        other => bail!("unexpected reply to CreateSession: {other:?}"),
    };

    let attach = conn
        .send_command(CommandBody::AttachSession {
            session_id,
            last_seen_sequence: None,
            subscriptions: default_subscriptions(),
            requested_role: ClientRole::Controller,
            repository: Some(key.clone()),
        })
        .await?;
    let catchup = commands::expect_catchup(attach)?;

    store.sessions.insert(
        key,
        StoredSession {
            session_id,
            workspace_id: workspace,
        },
    );
    store.save(paths); // best-effort: a persistence miss only costs the next
                       // launch a fresh session, never correctness.
    Ok((session_id, workspace, catchup))
}

/// Whether an attach-time [`Catchup`] proves its session still exists in the
/// daemon. A live session always replays at least its `SessionCreated` event, so
/// its watermark is `>= 1`; the daemon reports `0` for an absent session (it
/// cannot distinguish "gone" from "empty"). An unrecognized future variant is
/// accepted rather than needlessly discarding a resumable session — the concrete
/// failure (issue #6 item 6) is specifically the provably-empty catch-up.
/// Whether an attach-time [`Catchup`] shows the session is already CLOSED — a
/// `SessionClosed` in the replayed events, or the snapshot's `closed` flag. A
/// closed session must not be resumed from the store: the event loop exits the
/// moment it folds the close, and the remembered mapping would re-open it on
/// every later launch (a permanent lockout).
fn catchup_shows_closed(catchup: &Catchup) -> bool {
    match catchup {
        Catchup::Events { events, .. } => events
            .iter()
            .any(|event| matches!(event.body, codypendent_protocol::EventBody::SessionClosed)),
        Catchup::Snapshot { projection, .. } => projection.closed,
        _ => false,
    }
}

fn catchup_proves_session_exists(catchup: &Catchup) -> bool {
    match catchup {
        Catchup::Events { through, .. } | Catchup::Snapshot { through, .. } => *through > 0,
        _ => true,
    }
}

/// The knowledge-fabric projections the TUI reads (STEP 2.6 + Phase 4 client
/// wiring), all loaded in the one place the two worlds meet.
struct KnowledgeProjections {
    skills: Vec<SkillCard>,
    memories: Vec<MemoryCard>,
    docs: Vec<DocCard>,
    edges: Vec<GraphEdgeCard>,
    edge_total: usize,
    blackboard: Vec<BlackboardItemCard>,
}

/// Read the knowledge fabric's registry, memories, documents, and code-graph
/// edges directly from SQLite and map them into the TUI's plain projection
/// structs (STEP 2.6 + Phase 4 client wiring). This is the CLI's job precisely
/// because the TUI crate performs no I/O and never depends on
/// `codypendent-knowledge`; the mapping from the knowledge domain types to the
/// projection structs happens here and nowhere else.
///
/// The database is opened via the same helper the `index rebuild` path uses; WAL
/// mode lets this read concurrently with the running daemon. Every failure path
/// (open, list, query) is swallowed into an empty list with a stderr note, so a
/// missing or busy database only means empty browsers — never a TUI that refuses
/// to start.
async fn load_knowledge(
    paths: &RuntimePaths,
    workspace_id: WorkspaceId,
    repo: &Path,
    warnings: &mut Vec<String>,
) -> KnowledgeProjections {
    let empty = || KnowledgeProjections {
        skills: Vec::new(),
        memories: Vec::new(),
        docs: Vec::new(),
        edges: Vec::new(),
        edge_total: 0,
        blackboard: Vec::new(),
    };

    let database_path = paths.data_dir.join("codypendent.db");
    let pool = match knowledge_db::open(&database_path).await {
        Ok(pool) => pool,
        Err(error) => {
            warnings.push(format!(
                "knowledge views unavailable (opening {}: {error})",
                database_path.display()
            ));
            return empty();
        }
    };

    let skills = match Registry::new().list(&pool).await {
        Ok(items) => items.iter().map(skill_card).collect(),
        Err(error) => {
            warnings.push(format!("could not list registry items: {error}"));
            Vec::new()
        }
    };

    // Visible scopes: the System tier, this session's workspace, and THIS
    // repository — where a run's harvested memories and documents live. The
    // identity must come from `anchor_repository_id`, which resolves the Git
    // toplevel first, exactly as `codypendentd::scan::repository_id_for` does:
    // hashing the opened directory instead made every one of these lists empty
    // whenever the TUI was started from a subdirectory. The stores enforce
    // cross-scope isolation in SQL; an empty result is fine.
    let repository = crate::repo_anchor::anchor_repository_id(repo);
    let scopes = vec![
        Scope::System,
        Scope::Workspace(workspace_id),
        Scope::Repository(repository),
    ];
    let memories = match MemoryStore::new().query(&pool, &scopes, None).await {
        Ok(records) => records.iter().map(memory_card).collect(),
        Err(error) => {
            warnings.push(format!("could not query memories: {error}"));
            Vec::new()
        }
    };

    let docs = load_docs(&pool, &scopes, warnings).await;
    let (edges, edge_total, _) = load_edge_page(&pool, repository, "", 0, warnings).await;
    // Phase 5 STEP 5.3: the blackboard artifacts on the active workflow runs. The
    // workflow tables share this database (the migrations are workspace-wide), so
    // the same pool serves them; empty until a run posts artifacts.
    let blackboard = load_blackboard(&pool, warnings).await;

    pool.close().await;
    KnowledgeProjections {
        skills,
        memories,
        docs,
        edges,
        edge_total,
        blackboard,
    }
}

/// Seed the model-picker projection (MP1): every model configured in
/// `<data_dir>/models.toml` (the authoritative selectable list — STEP 1.9),
/// enriched with its measured profile from the `model_profiles` table
/// (migration 0014) when one exists, matched by `(id, base_url)` — a profile
/// row is keyed by `(model_id, endpoint)`, and `base_url` is a model's
/// endpoint. This is the CLI's job precisely because the TUI crate performs no
/// I/O and never depends on `codypendent-routing`; the mapping happens here
/// and nowhere else, exactly as [`load_knowledge`] maps the other browsers'
/// projections.
///
/// Never fails the TUI: a missing/unparsable `models.toml` degrades to an
/// empty picker (with a diagnostic in `warnings`); an unopenable database or a
/// profile-list failure degrades every model to its **id-only fallback**
/// (every badge absent) since profiles are best-effort enrichment, not the
/// selectable list itself.
async fn load_model_cards(paths: &RuntimePaths, warnings: &mut Vec<String>) -> Vec<ModelCard> {
    use codypendent_daemon::model_profiles::ModelProfileStore;
    use codypendent_runtime::models::load_models;

    let models_path = paths.data_dir.join("models.toml");
    let configs = match load_models(&models_path) {
        Ok(configs) => configs,
        Err(error) => {
            warnings.push(format!(
                "model picker unavailable (reading {}: {error})",
                models_path.display()
            ));
            return Vec::new();
        }
    };
    if configs.is_empty() {
        return Vec::new();
    }

    let database_path = paths.data_dir.join("codypendent.db");
    let pool = match knowledge_db::open(&database_path).await {
        Ok(pool) => Some(pool),
        Err(error) => {
            warnings.push(format!(
                "model profiles unavailable (opening {}: {error}); \
                 models still list, id-only",
                database_path.display()
            ));
            None
        }
    };

    let mut profiles: HashMap<(ModelId, String), codypendent_routing::ModelProfile> =
        HashMap::new();
    if let Some(pool) = &pool {
        match ModelProfileStore::new().list(pool).await {
            Ok(stored) => {
                for entry in stored {
                    profiles.insert(
                        (entry.profile.id.clone(), entry.endpoint.clone()),
                        entry.profile,
                    );
                }
            }
            Err(error) => {
                warnings.push(format!("could not list model profiles: {error}"));
            }
        }
    }
    if let Some(pool) = pool {
        pool.close().await;
    }

    let auth = codypendent_runtime::auth::AuthStore::load(&paths.data_dir).unwrap_or_default();
    let catalog = codypendent_providers::Catalog::load_with_user_overrides(
        &paths.data_dir.join("providers.toml"),
    )
    .unwrap_or_else(|error| {
        warnings.push(format!(
            "model auth catalog fell back to built-ins ({error})"
        ));
        codypendent_providers::Catalog::builtin()
    });
    let registry = codypendent_runtime::models::ModelRegistry::new(configs.clone())
        .with_auth(auth)
        .with_catalog(catalog);
    let acp_store = codypendent_integrations::acp_registry::AcpRegistryStore::new(&paths.data_dir);
    let mut cards = Vec::with_capacity(configs.len());
    for config in configs {
        let local = local_model_endpoint(&config.base_url);
        let readiness = if config.provider == "acp" {
            match acp_store.launch_spec(&config.model) {
                Ok(_) => ModelReadiness::Ready,
                Err(error) => ModelReadiness::Unavailable(error.to_string()),
            }
        } else if config.base_url.trim().is_empty() {
            ModelReadiness::Unavailable("base URL is missing".to_owned())
        } else if local {
            match registry.check_model(&config.id).await {
                Ok(()) => ModelReadiness::Ready,
                Err(error) => ModelReadiness::Unavailable(error.to_string()),
            }
        } else {
            // Hosted models are not probed during boot, but their auth and
            // protocol must resolve exactly as a real StartRun would. This
            // makes `Unverified` mean runnable-but-not-network-verified;
            // `Unavailable` is therefore an authoritative zero-runnable gate.
            match registry.credentials_resolvable(&config.id).await {
                Ok(true) => ModelReadiness::Unverified,
                Ok(false) => ModelReadiness::Unavailable("API key is not configured".to_owned()),
                Err(error) => ModelReadiness::Unavailable(error.to_string()),
            }
        };
        cards.push(model_card(config, &profiles, readiness, local));
    }
    cards
}

/// The authoritative non-secret runnable projection emitted after boot and
/// every model reload. `load_model_cards` has already resolved protocol,
/// credentials, local availability, and ACP launchability in the harness, so
/// the pure TUI only needs these ids and never re-implements I/O policy.
fn runnable_model_ids(cards: &[ModelCard]) -> Vec<ModelId> {
    cards
        .iter()
        .filter(|card| !matches!(card.readiness, ModelReadiness::Unavailable(_)))
        .map(|card| card.id.clone())
        .collect()
}

/// Fold the current authoritative harness projection into reducer state. The
/// optional id correlates an add/ACP attempt; background/boot/removal refreshes
/// update the set without completing any onboarding attempt.
fn refresh_runnable_models(state: &mut AppState, onboard_attempt: Option<ModelId>) {
    let model_ids = runnable_model_ids(&state.models);
    reduce(
        state,
        Action::RunnableModelsRefreshed {
            model_ids,
            onboard_attempt,
        },
    );
}

fn local_model_endpoint(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    ["localhost", "127.0.0.1", "0.0.0.0", "[::1]", "::1"]
        .iter()
        .any(|host| lower.contains(host))
}

/// Seed (and re-seed after create/run/delete) the `/council` browser's
/// projection: every council persisted in `<config_dir>/councils.toml`,
/// mapped into a self-contained [`codypendent_tui::state::CouncilCard`] — the
/// TUI crate performs no I/O, so the mapping happens here exactly like
/// [`load_model_cards`] and the knowledge-fabric loaders above. Never fails
/// the TUI: an unreadable/unparsable store degrades to an empty browser with
/// a diagnostic in `warnings`.
fn load_council_cards(
    paths: &RuntimePaths,
    warnings: &mut Vec<String>,
) -> Vec<codypendent_tui::state::CouncilCard> {
    match crate::council::list_definitions(paths) {
        Ok(definitions) => definitions
            .into_iter()
            .map(|definition| codypendent_tui::state::CouncilCard {
                name: definition.name,
                description: definition.description,
                chair: definition.chair,
                rounds: definition.rounds,
                evidence: definition.evidence,
                members: definition
                    .members
                    .into_iter()
                    .map(|member| (member.model, member.role))
                    .collect(),
            })
            .collect(),
        Err(error) => {
            warnings.push(format!("council browser unavailable: {error}"));
            Vec::new()
        }
    }
}

/// Format one [`crate::council::CouncilEvent`] as a single display line for the
/// off-thread council run's progress stream (rubric 6 TUI wiring) — the same
/// wording the CLI's own non-interactive `council run` prints via `eprintln!`,
/// so behavior reads identically in both surfaces.
fn council_progress_message(event: &crate::council::CouncilEvent) -> String {
    use crate::council::CouncilEvent;
    match event {
        CouncilEvent::RoundStarted {
            round,
            rounds,
            members,
        } => format!("round {round}/{rounds} — launching {members} member(s)"),
        CouncilEvent::MemberCompleted { round, role, model } => {
            format!("round {round} — {role} ({model}) completed")
        }
        CouncilEvent::MemberFailed { round, error } => {
            format!("round {round} — member failed: {error}")
        }
        CouncilEvent::ChairRuled { round } => format!("chair ruled on round {round}"),
        CouncilEvent::ReviewStarted { reviewer } => {
            format!("independent reviewer `{reviewer}` reading the synthesis")
        }
        CouncilEvent::ChairStarted { chair } => format!("asking chair `{chair}` to synthesize"),
        CouncilEvent::Warning { message } => format!("warning: {message}"),
    }
}

fn council_progress_phase(event: &crate::council::CouncilEvent) -> CouncilProgressPhase {
    use crate::council::CouncilEvent;
    match event {
        CouncilEvent::RoundStarted { .. } => CouncilProgressPhase::RoundStarted,
        CouncilEvent::MemberCompleted { .. } => CouncilProgressPhase::MemberCompleted,
        CouncilEvent::MemberFailed { .. } => CouncilProgressPhase::MemberFailed,
        CouncilEvent::ChairRuled { .. } => CouncilProgressPhase::ChairRuled,
        CouncilEvent::ReviewStarted { .. } => CouncilProgressPhase::ReviewStarted,
        CouncilEvent::ChairStarted { .. } => CouncilProgressPhase::ChairStarted,
        CouncilEvent::Warning { .. } => CouncilProgressPhase::Warning,
    }
}

/// Reduce a completed [`crate::council::CouncilRunOutcome`] to the plain,
/// dependency-free summary the TUI crate can render (it cannot name
/// `codypendent-cli`'s own `council` module types without a dependency cycle —
/// `cli` already depends on `tui`).
fn council_run_summary(
    run: crate::council::CouncilRunOutcome,
) -> codypendent_tui::state::CouncilRunSummary {
    let mut participants: Vec<String> = run
        .outcome
        .members
        .iter()
        .map(crate::council::participant_line)
        .collect();
    participants.push(crate::council::participant_line(&run.outcome.chair));
    codypendent_tui::state::CouncilRunSummary {
        result_id: run.handle.result_id.to_string(),
        council: run.outcome.council,
        status: "completed".to_owned(),
        objective: run.outcome.objective,
        started_at: run.handle.started_at,
        finished_at: run.handle.finished_at,
        repository: run.handle.repository,
        origin_session_id: run.handle.origin_session_id.map(|id| id.to_string()),
        evidence: false,
        warnings: run.warnings,
        rounds: Vec::new(),
        failure: None,
        synthesis: run.outcome.chair.response,
        participants,
        cost_line: crate::council::cost_line(&run.costs),
        report_markdown: format!(
            "{} · result {}",
            run.report_markdown.display(),
            run.handle.result_id
        ),
    }
}

fn council_stored_summary(stored: crate::council::StoredCouncilResult) -> CouncilRunSummary {
    let report = stored.report;
    let mut participants = report
        .rounds
        .iter()
        .flat_map(|round| round.members.iter())
        .map(crate::council::participant_line)
        .collect::<Vec<_>>();
    if let Some(chair) = &report.chair {
        participants.push(crate::council::participant_line(chair));
    }
    let synthesis = report
        .chair
        .as_ref()
        .map_or_else(String::new, |chair| chair.response.clone());
    let rounds = report
        .rounds
        .into_iter()
        .map(|round| CouncilRoundSummary {
            round: round.round,
            members: round
                .members
                .into_iter()
                .map(|member| CouncilMemberSummary {
                    model: member.model,
                    role: member.role,
                    session_id: member.session_id.to_string(),
                    run_id: member.run_id.to_string(),
                    response: member.response,
                    tokens: member.tokens,
                    cost_micros: member.cost_micros,
                })
                .collect(),
            failures: round.failures,
        })
        .collect();
    CouncilRunSummary {
        result_id: stored.handle.result_id.to_string(),
        council: report.council,
        status: report.status,
        objective: report.objective,
        started_at: report.started_at,
        finished_at: report.finished_at,
        repository: report.repository,
        origin_session_id: report.origin_session_id.map(|id| id.to_string()),
        evidence: report.evidence,
        warnings: report.warnings,
        rounds,
        failure: report.failure,
        synthesis,
        participants,
        cost_line: crate::council::cost_line(&report.costs),
        report_markdown: stored.handle.markdown_path.display().to_string(),
    }
}

fn load_council_result_cards(
    paths: &RuntimePaths,
    warnings: &mut Vec<String>,
) -> Vec<CouncilRunSummary> {
    let root = paths.data_dir.join("councils");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warnings.push(format!("council results unavailable: {error}"));
            return Vec::new();
        }
    };
    let mut cards = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        match crate::council::latest_result(paths, &name) {
            Ok(Some(stored)) => cards.push(council_stored_summary(stored)),
            Ok(None) => {}
            Err(error) => {
                warnings.push(format!("could not load council result `{name}`: {error:#}"))
            }
        }
    }
    cards.sort_by(|left, right| right.finished_at.cmp(&left.finished_at));
    cards
}

/// Map one configured [`ModelConfig`](codypendent_runtime::models::ModelConfig)
/// into a [`ModelCard`], enriching it with its measured profile (matched by
/// `(id, base_url)`) when `profiles` has one — an id-only fallback (every
/// badge `None`) otherwise, so an unprofiled model still selects, just without
/// badges.
fn model_card(
    config: codypendent_runtime::models::ModelConfig,
    profiles: &HashMap<(ModelId, String), codypendent_routing::ModelProfile>,
    readiness: ModelReadiness,
    local: bool,
) -> ModelCard {
    let profile = profiles.get(&(config.id.clone(), config.base_url.clone()));
    let configured_context = config.context_tokens;
    ModelCard {
        id: config.id,
        provider: config.provider,
        readiness,
        location: Some(profile.map_or_else(
            || {
                if local {
                    ModelLocationLabel::Local
                } else {
                    ModelLocationLabel::Hosted
                }
            },
            |profile| {
                if profile.is_local() {
                    ModelLocationLabel::Local
                } else {
                    ModelLocationLabel::Hosted
                }
            },
        )),
        cost_per_1k_usd: profile.map(|profile| profile.performance.cost_per_1k_tokens_usd),
        // A measured profile is the freshest source, but an explicit
        // `models.toml` value is authoritative when no profile exists. The old
        // projection discarded that configured value and showed `—` even while
        // the runtime was correctly using it.
        context_tokens: profile
            .and_then(|profile| profile.capabilities.context_tokens)
            .or(configured_context),
    }
}

/// Seed the provider-catalog projection for the `/provider` picker (Task 8):
/// the built-in ~40-provider catalog layered with the user's
/// `<data_dir>/providers.toml`, exactly as [`load_model_cards`] maps
/// `models.toml` into [`ModelCard`]s. This is the CLI's job precisely because
/// the TUI crate performs no I/O and never depends on `codypendent-providers`.
///
/// Never fails the TUI: a missing user `providers.toml` is fine (the loader
/// treats it as absent and returns the built-ins); a *malformed* one degrades
/// to the built-ins alone, with a diagnostic in `warnings`.
async fn load_provider_cards(
    paths: &RuntimePaths,
    warnings: &mut Vec<String>,
) -> Vec<ProviderCard> {
    use codypendent_integrations::acp_registry::AcpRegistryStore;
    use codypendent_providers::{AuthMethod, Catalog};

    let providers_path = paths.data_dir.join("providers.toml");
    let catalog = match Catalog::load_with_user_overrides(&providers_path) {
        Ok(catalog) => catalog,
        Err(error) => {
            warnings.push(format!("provider catalog fell back to built-ins ({error})"));
            Catalog::builtin()
        }
    };
    // How many curated models the catalog ships per provider, counted once
    // rather than re-scanning for every card.
    let mut catalog_model_counts: HashMap<&str, usize> = HashMap::new();
    for model in catalog.models() {
        *catalog_model_counts
            .entry(model.provider_id.as_str())
            .or_default() += 1;
    }
    let auth = codypendent_runtime::auth::AuthStore::load(&paths.data_dir).unwrap_or_default();
    let mut cards: Vec<_> = catalog
        .providers()
        // ACP entries come from the official live registry below. Keeping the
        // five historical built-ins as well would show stale duplicate adapters.
        .filter(|p| !matches!(p.protocol, codypendent_providers::Protocol::Acp))
        .map(|p| ProviderCard {
            id: p.id.clone(),
            name: p.name.clone(),
            protocol: protocol_label(p.protocol).to_owned(),
            auth: match p.auth.first() {
                None | Some(AuthMethod::None) => "none".to_string(),
                Some(AuthMethod::ApiKey { env, .. }) => {
                    format!("api-key: {}", env.first().map(String::as_str).unwrap_or(""))
                }
                Some(AuthMethod::Acp { command, .. }) => format!("acp: {command}"),
                Some(AuthMethod::CloudIam { variant, .. }) => format!("cloud-iam: {variant}"),
                Some(AuthMethod::OAuth { .. }) => "oauth".to_string(),
                // `AuthMethod` is `#[non_exhaustive]`: a future variant this
                // build does not understand still renders (protocol RULE 1),
                // rather than failing to compile or panicking.
                Some(_) => "unknown".to_string(),
            },
            local: p.local,
            // Adding a model from this provider needs a key iff its first auth
            // method is an API key (local/none/acp/cloud-iam/oauth skip the key
            // step). Extracted to `provider_requires_key` so this derivation has
            // an isolated unit test against the real `AuthMethod` enum, rather
            // than only being exercised indirectly through this I/O function.
            requires_key: provider_requires_key(p),
            // Whether the add-model flow can offer a live `/models` pick-list for
            // this provider (OpenAiChat + base_url + ApiKey/None), vs. the
            // free-text fallback. Extracted to `provider_can_list_models`, unit
            // tested against the real enums.
            can_list_models: provider_can_list_models(p),
            available: provider_runtime_supported(p),
            // The curated `[[model]]` rows this provider ships: what the add
            // flow can offer with no network at all.
            catalog_models: catalog_model_counts
                .get(p.id.as_str())
                .copied()
                .unwrap_or_default(),
            // A provider-wide key in `auth.json` OR a documented provider env
            // means the add flow can skip straight to the pick-list. Values
            // stay in the harness; only this boolean reaches the pure TUI.
            has_key: provider_has_resolvable_key(
                &p.id,
                &auth,
                &p.auth
                    .iter()
                    .find_map(|method| match method {
                        AuthMethod::ApiKey { env, .. } => Some(env.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
            ),
        })
        .collect();
    let acp_store = AcpRegistryStore::new(&paths.data_dir);
    let acp_registry = match acp_store.load_or_refresh().await {
        Ok(registry) => Some(registry),
        Err(error) => {
            warnings.push(format!("official ACP registry unavailable ({error})"));
            None
        }
    };
    if let Some(registry) = acp_registry {
        cards.extend(registry.agents.iter().map(|agent| {
            let distribution = if agent.distribution.npx.is_some() {
                "npx"
            } else if agent.distribution.uvx.is_some() {
                "uvx"
            } else {
                "binary"
            };
            let binary_installable = agent
                .distribution
                .binary
                .get(codypendent_integrations::acp_registry::current_platform())
                .is_some_and(|binary| binary.sha256.is_some());
            let ready = acp_store.launch_spec(&agent.id).is_ok();
            ProviderCard {
                id: agent.id.clone(),
                name: agent.name.clone(),
                protocol: "acp".to_string(),
                auth: format!("acp: {distribution} · {}", agent.version),
                local: false,
                requires_key: false,
                // An ACP agent's models come from its session-config
                // handshake, so listing them means spawning it — offered only
                // for an agent already installed and launchable. One that
                // would first have to be downloaded takes the connect-then-see
                // path rather than stalling the picker on an install.
                can_list_models: ready,
                // Verified platform binaries can be installed in the
                // background when selected. Package entries are selectable
                // only when their runner is actually present.
                available: ready || binary_installable,
                // An ACP agent owns its own model; there is nothing for the
                // catalog to curate and no provider key to hold.
                catalog_models: 0,
                has_key: false,
            }
        }));
    }
    for spec in codypendent_integrations::acp_registry::local_acp_agent_specs() {
        if let Some(card) = cards.iter_mut().find(|card| card.id == spec.registry_id) {
            // The registry describes how an adapter can be installed, while
            // this projection describes what is actually usable now. Prefer
            // the installed executable so Junie/Cursor/etc. do not remain
            // labelled as hosted previews after local discovery succeeds.
            card.name = spec.name;
            card.auth = format!("acp: local · {}", spec.version);
            card.local = true;
            card.can_list_models = true;
            card.available = true;
            continue;
        }
        cards.push(ProviderCard {
            id: spec.registry_id,
            name: spec.name,
            protocol: "acp".to_string(),
            auth: format!("acp: local · {}", spec.version),
            local: true,
            requires_key: false,
            // Locally installed, so it can be handshaken for its models.
            can_list_models: true,
            available: true,
            catalog_models: 0,
            has_key: false,
        });
    }
    // Google does not currently ship a native Antigravity ACP server. The
    // community bridge is actionable only on platforms for which Codypendent
    // carries an immutable URL+SHA-256 descriptor. The pure reducer presents a
    // host-owned risk confirmation before this can reach the install/probe
    // intent; nothing is downloaded merely by opening the catalog.
    let community = codypendent_integrations::acp_registry::community_acp_agent("antigravity-acp");
    let installable = community.as_ref().is_some_and(|agent| {
        agent
            .distribution
            .binary
            .get(codypendent_integrations::acp_registry::current_platform())
            .is_some_and(|binary| binary.sha256.is_some())
    });
    let ready = acp_store.launch_spec("antigravity-acp").is_ok();
    let antigravity = ProviderCard {
        id: "antigravity-acp".to_string(),
        name: "Google Antigravity (community bridge)".to_string(),
        protocol: "acp".to_string(),
        auth: if ready {
            "acp: local · pinned community bridge".to_string()
        } else {
            "acp: verified install · third-party ToS risk".to_string()
        },
        local: true,
        requires_key: false,
        // Selecting the card leads to an explicit confirmation and then the
        // normal ACP handshake-backed model list.
        can_list_models: ready || installable,
        available: ready || installable,
        catalog_models: 0,
        has_key: false,
    };
    if let Some(card) = cards.iter_mut().find(|card| card.id == "antigravity-acp") {
        // Keep this host-owned warning even if an upstream catalog later
        // publishes a row with the same id. It must not be possible for a
        // remote registry update to bypass the explicit community-risk gate.
        *card = antigravity;
    } else {
        cards.push(antigravity);
    }
    // Put usable providers first, with local endpoints before hosted ones. The
    // complete catalog remains searchable below them as an honest preview.
    cards.sort_by(|a, b| {
        b.available
            .cmp(&a.available)
            .then_with(|| b.local.cmp(&a.local))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    cards
}

/// Whether adding a model from `p` needs an API key: its first configured auth
/// method is `AuthMethod::ApiKey` (a local/no-auth/ACP/cloud-iam/OAuth provider,
/// or one with no auth methods at all, skips the key step). A tiny pure
/// expression — no I/O — extracted out of [`load_provider_cards`] so this
/// bool derivation is directly unit-testable against the real
/// `codypendent_providers::AuthMethod` enum, independent of that function's
/// file I/O.
fn provider_requires_key(p: &codypendent_providers::Provider) -> bool {
    use codypendent_providers::AuthMethod;
    matches!(p.auth.first(), Some(AuthMethod::ApiKey { .. }))
}

/// The part of both provider gates below that is about the ENDPOINT rather than
/// the wire protocol: a non-blank `base_url` and auth this build can actually
/// supply (an API key, or none at all). Cloud-IAM and OAuth providers fail here
/// whatever they speak. A tiny pure expression (no I/O), extracted out of
/// `load_provider_cards` so it is directly unit-testable against the real
/// `codypendent_providers` enums.
pub(crate) fn provider_endpoint_usable(p: &codypendent_providers::Provider) -> bool {
    use codypendent_providers::AuthMethod;
    p.base_url.as_deref().is_some_and(|u| !u.trim().is_empty())
        && matches!(
            p.auth.first(),
            Some(AuthMethod::ApiKey { .. } | AuthMethod::None) | None
        )
}

/// Whether adding a model from `p` can use a live `/models` list: the protocol
/// is OpenAI-compatible (`OpenAiChat`) and [`provider_endpoint_usable`]. This is
/// narrower than [`provider_runtime_supported`] on purpose, and Anthropic is
/// exactly why: the query path this gates GETs `{base_url}/models`
/// ([`models_url`]), while Anthropic's listing route is `/v1/models` against a
/// bare `https://api.anthropic.com` — the same mismatch
/// `ModelRegistry::check_model` had to special-case. Saying `false` here costs
/// Anthropic nothing: it ships 10 curated catalog rows, and `enter_add_model_flow`
/// serves those (`can_offer = can_list_models || catalog_models > 0`) instead of
/// dropping to the free-text path.
fn provider_can_list_models(p: &codypendent_providers::Provider) -> bool {
    use codypendent_providers::Protocol;
    matches!(p.protocol, Protocol::OpenAiChat) && provider_endpoint_usable(p)
}

/// The provider shapes today's runtime can EXECUTE — what decides whether
/// `/provider` opens the add flow or refuses with "catalog-only". Keeping this
/// separate from catalog visibility prevents ACP/cloud-auth cards from producing
/// an apparently valid `openai-compatible` model entry that can only fail later.
///
/// `Anthropic` belongs here alongside `OpenAiChat` because
/// `ModelRegistry::client_for` now has a real `Protocol::Anthropic` arm and
/// `provider-anthropic` is a default feature of `codypendent-runtime` — an entry
/// this flow writes (`provider = "openai-compatible"` + `provider_id =
/// "anthropic"`) resolves through `config_to_protocol_auth` to a genuine
/// native client. ACP executor protocols are still unwired here.
pub(crate) fn provider_runtime_supported(p: &codypendent_providers::Provider) -> bool {
    use codypendent_providers::Protocol;
    matches!(
        p.protocol,
        Protocol::OpenAiChat | Protocol::Anthropic | Protocol::GeminiNative
    ) && provider_endpoint_usable(p)
}

/// Why `models add <provider>` cannot serve this provider, or [`None`] when it
/// can — the single explanation behind BOTH provider enumerations and the
/// `models add` refusal, so a provider can never be offered by one surface and
/// rejected by another.
///
/// The 2026-08-13 review found `models list-providers` printing 42 rows of
/// which 6 could not be added, with nothing marking them and `models add
/// --help` naming one of them (`azure-openai`) as its worked example. The gate
/// that refuses them ([`provider_runtime_supported`]) already existed — what
/// was missing was the return path from the gate to the listing.
///
/// The reasons are ordered most-specific-first so an ACP agent is told about
/// `acp connect` rather than about its missing base URL.
pub(crate) fn provider_unusable_reason(
    p: &codypendent_providers::Provider,
) -> Option<&'static str> {
    use codypendent_providers::Protocol;
    if matches!(p.protocol, Protocol::Acp) {
        // Reachable, just not through this door: `acp connect` spawns the agent
        // and registers whatever models its session-config handshake offers.
        return Some(
            "connect it with `codypendent acp connect <id>` (an ACP agent, not an HTTP endpoint)",
        );
    }
    if !provider_endpoint_usable(p) {
        return Some(
            "no usable endpoint in the catalog — supply your own resource URL in <data_dir>/providers.toml",
        );
    }
    if !provider_runtime_supported(p) {
        return Some("this build cannot speak its wire protocol");
    }
    None
}

/// The provider picker's wire-protocol label — the same kebab-case spelling
/// `codypendent_providers::model::Protocol`'s `Serialize` impl emits (e.g.
/// `"openai-chat"`), spelled out explicitly here rather than derived from
/// `{:?}` because `Debug` prints the Rust identifier (`"OpenAiChat"`), not the
/// wire spelling. `Protocol` is `#[non_exhaustive]`, so a future variant this
/// build does not understand still renders as `"unknown"` rather than
/// failing to compile.
fn protocol_label(protocol: codypendent_providers::Protocol) -> &'static str {
    use codypendent_providers::Protocol;
    match protocol {
        Protocol::OpenAiChat => "openai-chat",
        Protocol::Anthropic => "anthropic",
        Protocol::GeminiNative => "gemini-native",
        Protocol::Acp => "acp",
        _ => "unknown",
    }
}

/// Project each visible-scope document (snapshot + pending suggestions) into a
/// [`DocCard`]. A per-document read failure collects a diagnostic and skips
/// that document; the browser degrades to what it could load rather than
/// failing.
async fn load_docs(
    pool: &sqlx::SqlitePool,
    scopes: &[Scope],
    warnings: &mut Vec<String>,
) -> Vec<DocCard> {
    let doc_store = DocumentStore::new();
    let suggestion_store = SuggestionStore::new();
    let summaries = match doc_store.list(pool, scopes).await {
        Ok(summaries) => summaries,
        Err(error) => {
            warnings.push(format!("could not list documents: {error}"));
            return Vec::new();
        }
    };

    let mut docs = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let document = match doc_store.snapshot_document(pool, summary.id).await {
            Ok(Some(document)) => document,
            Ok(None) => continue,
            Err(error) => {
                warnings.push(format!("could not load document {}: {error}", summary.id));
                continue;
            }
        };
        let suggestions = suggestion_store
            .pending(pool, summary.id)
            .await
            .unwrap_or_else(|error| {
                warnings.push(format!(
                    "could not load suggestions for {}: {error}",
                    summary.id
                ));
                Vec::new()
            });
        docs.push(doc_card(&document, &suggestions));
    }
    docs
}

/// Read one filtered graph page with endpoint names joined in SQLite. This never
/// materializes the repository's full node/edge sets: a 50k-edge checkout costs
/// one `COUNT` plus at most [`EDGE_PAGE_SIZE`] joined rows.
async fn load_edge_page(
    pool: &sqlx::SqlitePool,
    repository: RepositoryId,
    query: &str,
    requested_page: usize,
    warnings: &mut Vec<String>,
) -> (Vec<GraphEdgeCard>, usize, usize) {
    let repository = repository.to_string();
    let query = query.trim();
    let pattern = format!(
        "%{}%",
        query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let base = " FROM code_edges e \
                JOIN code_nodes f ON f.id = e.from_node \
                JOIN code_nodes t ON t.id = e.to_node \
                WHERE f.repository = ? AND t.repository = ?";
    let filter = " AND (f.qualified_name LIKE ? ESCAPE '\\' \
                         OR t.qualified_name LIKE ? ESCAPE '\\' \
                         OR e.relation LIKE ? ESCAPE '\\' \
                         OR e.evidence_kind LIKE ? ESCAPE '\\')";

    let total_result = if query.is_empty() {
        sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*){base}"))
            .bind(&repository)
            .bind(&repository)
            .fetch_one(pool)
            .await
    } else {
        sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*){base}{filter}"))
            .bind(&repository)
            .bind(&repository)
            .bind(&pattern)
            .bind(&pattern)
            .bind(&pattern)
            .bind(&pattern)
            .fetch_one(pool)
            .await
    };
    let total = match total_result {
        Ok(total) => usize::try_from(total.max(0)).unwrap_or(usize::MAX),
        Err(error) => {
            warnings.push(format!("could not count code-graph edges: {error}"));
            return (Vec::new(), 0, 0);
        }
    };
    let max_page = total.saturating_sub(1) / EDGE_PAGE_SIZE;
    let page = requested_page.min(max_page);
    let select = format!(
        "SELECT f.qualified_name AS from_name, t.qualified_name AS to_name, \
                e.relation, e.confidence, e.evidence_kind, e.evidence_artifact, e.revision \
         {base}{} \
         ORDER BY f.qualified_name COLLATE NOCASE, t.qualified_name COLLATE NOCASE, \
                  e.relation, e.id LIMIT ? OFFSET ?",
        if query.is_empty() { "" } else { filter }
    );
    let mut rows_query = sqlx::query(&select).bind(&repository).bind(&repository);
    if !query.is_empty() {
        rows_query = rows_query
            .bind(&pattern)
            .bind(&pattern)
            .bind(&pattern)
            .bind(&pattern);
    }
    rows_query = rows_query
        .bind(i64::try_from(EDGE_PAGE_SIZE).unwrap_or(i64::MAX))
        .bind(i64::try_from(page * EDGE_PAGE_SIZE).unwrap_or(i64::MAX));
    let rows = match rows_query.fetch_all(pool).await {
        Ok(rows) => rows,
        Err(error) => {
            warnings.push(format!("could not load code-graph page: {error}"));
            return (Vec::new(), total, page);
        }
    };

    let cards = rows
        .into_iter()
        .map(|row| {
            let evidence_json: Option<String> = row.get("evidence_artifact");
            let evidence = evidence_json
                .as_deref()
                .and_then(|json| serde_json::from_str::<EvidenceRef>(json).ok())
                .as_ref()
                .map_or_else(|| "(none)".to_owned(), evidence_source);
            GraphEdgeCard {
                from: row.get("from_name"),
                to: row.get("to_name"),
                relation: row.get::<String, _>("relation").replace('_', "-"),
                confidence: row.get::<f64, _>("confidence") as f32,
                evidence_kind: row.get("evidence_kind"),
                evidence,
                revision: row.get("revision"),
            }
        })
        .collect();
    (cards, total, page)
}

/// Map a governed [`RegistryItem`] into the TUI's [`SkillCard`] projection,
/// rendering each requested capability **verbatim** (STEP 2.6 "skill permissions
/// are visible").
fn skill_card(item: &RegistryItem) -> SkillCard {
    SkillCard {
        name: item.name.clone(),
        kind: registry_kind_label(item.kind).to_owned(),
        scope: scope_label(&item.scope),
        trust: trust_label(item.trust.tier).to_owned(),
        status: status_label(item.status).to_owned(),
        risk: risk_label(item.risk).to_owned(),
        description: item.description.clone(),
        permissions: item.permissions.iter().map(capability_verbatim).collect(),
    }
}

/// Map a [`MemoryRecord`] into the TUI's [`MemoryCard`] projection. `source` is a
/// human rendering of the record's evidence refs (joined when there are several),
/// which the memory browser's "open source" affordance surfaces in full.
fn memory_card(record: &MemoryRecord) -> MemoryCard {
    let source = if record.provenance.is_empty() {
        "(no evidence)".to_owned()
    } else {
        record
            .provenance
            .iter()
            .map(evidence_source)
            .collect::<Vec<_>>()
            .join("; ")
    };
    MemoryCard {
        statement: record.statement.clone(),
        class: memory_class_label(record.class).to_owned(),
        scope: scope_label(&record.scope),
        revision: record.valid_from.0.clone(),
        observed: record.observed_at.date_naive().to_string(),
        confidence: record.confidence,
        source,
    }
}

async fn load_journey(
    pool: &sqlx::SqlitePool,
    repository: &Path,
) -> Result<Vec<LearningCard>, codypendent_knowledge::LearningError> {
    let records = LearningStore::new()
        .query(
            pool,
            &LearningQuery {
                scopes: vec![
                    LearningScope::User(UserId("local".to_owned())),
                    LearningScope::Repository(crate::repo_anchor::anchor_repository_id(repository)),
                ],
                states: vec![LearningState::Proposed, LearningState::Active],
                ..LearningQuery::default()
            },
        )
        .await?;
    Ok(records.iter().map(learning_card).collect())
}

fn learning_card(record: &LearningRecord) -> LearningCard {
    let scope = match &record.scope {
        LearningScope::User(_) => "user".to_owned(),
        LearningScope::Repository(id) => format!(
            "repository {}",
            id.to_string().chars().take(8).collect::<String>()
        ),
        LearningScope::Provider(_) => "provider".to_owned(),
        LearningScope::Council(_) => "council".to_owned(),
    };
    let mut provenance = record
        .provenance
        .iter()
        .map(|source| match source {
            LearningProvenance::UserStatement { .. } => "user-confirmed",
            LearningProvenance::SuccessfulCommand { .. } => "locally verified",
            LearningProvenance::RepositoryObservation { .. } => "repository observation",
            LearningProvenance::AgentInference { .. } => "agent proposal",
            LearningProvenance::ToolOutput { .. } => "untrusted tool proposal",
            LearningProvenance::ExternalContent { .. } => "external proposal",
            LearningProvenance::CouncilResult { .. } => "council proposal",
        })
        .collect::<Vec<_>>();
    provenance.sort_unstable();
    provenance.dedup();
    LearningCard {
        id: record.id.to_string(),
        statement: record.content.summary().to_owned(),
        kind: match record.content.kind() {
            codypendent_knowledge::LearningKind::Fact => "fact".to_owned(),
            codypendent_knowledge::LearningKind::Procedure => "procedure".to_owned(),
        },
        state: match record.state {
            LearningState::Proposed => "proposed",
            LearningState::Active => "active",
            LearningState::Rejected => "rejected",
        }
        .to_owned(),
        scope,
        provenance: provenance.join(" + "),
        confidence: record.confidence,
        pinned: record.pinned,
        revision: record.revision,
    }
}

async fn mutate_learning(
    pool: &sqlx::SqlitePool,
    id: &str,
    revision: u64,
    mutation: &LearningMutation,
) -> anyhow::Result<String> {
    let id: LearningId = id.parse().context("invalid learning id")?;
    let store = LearningStore::new();
    match mutation {
        LearningMutation::Activate => match store
            .activate(
                pool,
                id,
                revision,
                Verification::UserConfirmed {
                    user: UserId("local".to_owned()),
                },
            )
            .await?
        {
            ActivationOutcome::Activated(_) => Ok("learning activated".to_owned()),
            ActivationOutcome::Conflict { .. } => {
                bail!("resolve the conflicting active learning first")
            }
        },
        LearningMutation::Reject => {
            store
                .reject(pool, id, revision, "rejected in learning journey")
                .await?;
            Ok("learning rejected".to_owned())
        }
        LearningMutation::SetPinned(pinned) => {
            store.set_pinned(pool, id, revision, *pinned).await?;
            Ok(if *pinned {
                "learning pinned"
            } else {
                "learning unpinned"
            }
            .to_owned())
        }
        LearningMutation::EditStatement(statement) => {
            let record = store.get(pool, id).await?.context("learning disappeared")?;
            let content = match record.content {
                LearningContent::Fact {
                    structured_value, ..
                } => LearningContent::Fact {
                    statement: statement.clone(),
                    structured_value,
                },
                LearningContent::Procedure(mut procedure) => {
                    procedure.summary = statement.clone();
                    LearningContent::Procedure(procedure)
                }
            };
            match store
                .edit(
                    pool,
                    id,
                    revision,
                    LearningPatch {
                        content: Some(content),
                        ..LearningPatch::default()
                    },
                )
                .await?
            {
                LearningMutationOutcome::Updated(_) => Ok("learning updated".to_owned()),
                LearningMutationOutcome::Duplicate { .. } => bail!("that learning already exists"),
                LearningMutationOutcome::Conflict { .. } => {
                    Ok("learning updated and returned to review".to_owned())
                }
            }
        }
        LearningMutation::Delete => {
            let deleted = store.delete(pool, id).await?;
            if deleted.id.is_some() {
                Ok("learning permanently deleted".to_owned())
            } else {
                bail!("learning was already deleted")
            }
        }
    }
}

/// Render one requested capability exactly as declared, e.g.
/// `"filesystem_read: $REPOSITORY"` or `"command: cargo"` — the verbatim form the
/// Skill Studio shows.
fn capability_verbatim(capability: &CapabilityRequest) -> String {
    match capability {
        CapabilityRequest::FilesystemRead(value) => format!("filesystem_read: {value}"),
        CapabilityRequest::FilesystemWrite(value) => format!("filesystem_write: {value}"),
        CapabilityRequest::Command(value) => format!("command: {value}"),
        CapabilityRequest::Network(value) => format!("network: {value}"),
        CapabilityRequest::Secret(value) => format!("secret: {value}"),
    }
}

/// A human rendering of a memory's evidence ref (what "open source" reveals).
fn evidence_source(evidence: &EvidenceRef) -> String {
    match evidence {
        EvidenceRef::EventRange {
            session_id,
            from_sequence,
            to_sequence,
        } => format!("events {from_sequence}..{to_sequence} of session {session_id}"),
        EvidenceRef::Artifact {
            artifact,
            source_path,
        } => match source_path {
            Some(path) => format!("artifact {} ({path})", artifact.id),
            None => format!("artifact {}", artifact.id),
        },
        // An agent's claim: the rationale IS the evidence, so it travels here
        // rather than being reduced to "run <uuid>". A reviewer needs to read
        // what was asserted, not be told that something was.
        EvidenceRef::AgentAssertion {
            session_id,
            run_id,
            rationale,
        } => format!("asserted by run {run_id} (session {session_id}): {rationale}"),
    }
}

/// A compact human label for a memory/registry [`Scope`]: the tier, plus a short
/// prefix of its key for the id-bearing tiers (the full UUID is noise in a card).
fn scope_label(scope: &Scope) -> String {
    match scope.key() {
        Some(key) => format!(
            "{} {}",
            scope.tier(),
            key.chars().take(8).collect::<String>()
        ),
        None => scope.tier().to_owned(),
    }
}

fn registry_kind_label(kind: RegistryItemKind) -> &'static str {
    match kind {
        RegistryItemKind::Tool => "tool",
        RegistryItemKind::Skill => "skill",
        RegistryItemKind::Plugin => "plugin",
        RegistryItemKind::Hook => "hook",
        RegistryItemKind::Command => "command",
    }
}

fn trust_label(tier: TrustTier) -> &'static str {
    match tier {
        TrustTier::Untrusted => "untrusted",
        TrustTier::Community => "community",
        TrustTier::Verified => "verified",
        TrustTier::FirstParty => "first-party",
    }
}

fn status_label(status: RegistryStatus) -> &'static str {
    match status {
        RegistryStatus::Draft => "draft",
        RegistryStatus::Active => "active",
        RegistryStatus::Modified => "modified",
        RegistryStatus::Deprecated => "deprecated",
    }
}

fn risk_label(risk: RiskClass) -> &'static str {
    match risk {
        RiskClass::Safe => "safe",
        RiskClass::Low => "low",
        RiskClass::Medium => "medium",
        RiskClass::High => "high",
    }
}

fn memory_class_label(class: MemoryClass) -> &'static str {
    match class {
        MemoryClass::Working => "working",
        MemoryClass::Episodic => "episodic",
        MemoryClass::Semantic => "semantic",
        MemoryClass::Procedural => "procedural",
        MemoryClass::Preference => "preference",
        MemoryClass::Failure => "failure",
        MemoryClass::Artifact => "artifact",
        MemoryClass::Code => "code",
    }
}

/// Map a [`KnowledgeDocument`] (plus its pending suggestions) into the TUI's
/// [`DocCard`] projection. `mode` is the collaboration mode the document's scope
/// defaults to — org-scope docs read `suggest`, the suggest-by-default the
/// engine enforces (STEP 4.3).
fn doc_card(document: &KnowledgeDocument, suggestions: &[Suggestion]) -> DocCard {
    DocCard {
        document_id: document.id,
        title: document.title.clone(),
        scope: scope_label(&document.scope),
        status: document.status.as_str().to_owned(),
        mode: collab_mode_label(CollaborationMode::default_for_scope(&document.scope)).to_owned(),
        revision: format!("r{}", document.revision),
        blocks: document.blocks.iter().map(block_view).collect(),
        suggestions: suggestions.iter().map(suggestion_view).collect(),
    }
}

/// Render one [`DocumentBlock`] into the editor rail's [`DocBlockView`]: a kind
/// label and a single-line human rendering of its content (never the raw
/// serialized block). Structured/embed blocks get a compact stand-in.
fn block_view(block: &DocumentBlock) -> DocBlockView {
    let (kind, text) = match &block.content {
        BlockContent::Heading { level, text } => (format!("heading h{level}"), text.clone()),
        BlockContent::Paragraph { text } => ("paragraph".to_owned(), text.clone()),
        BlockContent::Code { language, text } => (
            match language {
                Some(language) => format!("code {language}"),
                None => "code".to_owned(),
            },
            text.clone(),
        ),
        BlockContent::Diagram { format, .. } => {
            (format!("diagram {format}"), "(diagram)".to_owned())
        }
        BlockContent::Table { rows } => ("table".to_owned(), format!("({} rows)", rows.len())),
        BlockContent::Callout { kind, text } => (format!("callout {kind}"), text.clone()),
        BlockContent::Checklist { items } => {
            ("checklist".to_owned(), format!("({} items)", items.len()))
        }
        BlockContent::Query { query } => ("query".to_owned(), query.clone()),
        BlockContent::EmbeddedFile { path } => ("embed-file".to_owned(), path.clone()),
        BlockContent::EmbeddedSymbol { symbol } => ("embed-symbol".to_owned(), symbol.clone()),
        BlockContent::EmbeddedWorkflow { workflow } => {
            ("embed-workflow".to_owned(), workflow.clone())
        }
        BlockContent::EmbeddedSkill { skill } => ("embed-skill".to_owned(), skill.clone()),
    };
    // Collapse to one line — the editor rail renders a block per row. The raw
    // primary text rides alongside it so `e` can prefill the edit prompt with
    // exactly what the block holds (and replace exactly that many characters);
    // a structured/embed block has none, and the rail says so instead.
    DocBlockView {
        id: block.id.clone(),
        kind,
        text: text.replace('\n', " "),
        editable: block.primary_text().map(str::to_owned),
    }
}

/// Map a [`Suggestion`] into the review rail's [`DocSuggestionView`].
fn suggestion_view(suggestion: &Suggestion) -> DocSuggestionView {
    DocSuggestionView {
        id: suggestion.id.clone(),
        block_id: suggestion.block_id.clone(),
        source_revision: suggestion.source_revision,
        original: suggestion.original.clone(),
        status: suggestion_status_label(suggestion.status).to_owned(),
        author: document_author_label(&suggestion.author),
        range: format!("{}..{}", suggestion.range_start, suggestion.range_end),
        replacement: suggestion.replacement.clone(),
        rationale: suggestion.rationale.clone(),
    }
}

/// The cap on workflow nodes surfaced in the view. A pathological manifest set
/// could declare a very large graph; the read-only view shows the first
/// [`MAX_WORKFLOW_NODES`] (in discovery + topological order) and logs when it
/// truncates — never a silent cut.
const MAX_WORKFLOW_NODES: usize = 500;

/// Compile the repository's declared workflow manifests
/// (`.codypendent/workflows/*.{yaml,yml}`) and project each compiled node into a
/// [`WorkflowNodeCard`] for the workflow-graph view (Phase 5 STEP 5.2). This is
/// the CLI's job precisely because the TUI crate performs no I/O and never
/// depends on `codypendent-workflow`; the mapping from the compiled graph to the
/// projection happens here and nowhere else.
///
/// Manifests are compiled in sorted filename order for a deterministic view; an
/// unreadable or non-compiling manifest logs to stderr and is skipped, so one
/// broken file drops only its own workflow — never the others, and never the
/// TUI. Nodes keep their compiled topological order.
///
/// When `pool` is present, each workflow's LATEST durable run overlays its
/// per-node live state, MEASURED cost, and failure/block reason (Phase 5 T8 /
/// P5-D4) onto the compiled defaults; `None` (or no run yet) shows the pre-run
/// values (`pending` / `—`). Read failures degrade to the compiled view, never
/// fail the TUI.
async fn load_workflows(
    repo: &Path,
    user_dir: Option<&Path>,
    pool: Option<&sqlx::SqlitePool>,
    warnings: &mut Vec<String>,
) -> Vec<WorkflowNodeCard> {
    use codypendent_workflow::{parse_definition, WorkflowSourceRegistry, REPAIR_GITHUB_CHECK_ID};

    let repository_dir = repo.join(".codypendent").join("workflows");
    let registry = WorkflowSourceRegistry::load(user_dir, Some(&repository_dir));
    let mut ids = BTreeSet::from([REPAIR_GITHUB_CHECK_ID.to_owned()]);

    // Discover the user/repository ids while preserving useful diagnostics for
    // malformed siblings. Resolution stays with WorkflowSourceRegistry so its
    // precedence and same-version collision rules remain authoritative.
    for dir in user_dir
        .into_iter()
        .chain(std::iter::once(repository_dir.as_path()))
    {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(format!(
                    "could not read workflows in {}: {error}",
                    dir.display()
                ));
                continue;
            }
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("yaml" | "yml")
                )
            })
            .collect();
        files.sort();
        for path in files {
            match std::fs::read_to_string(&path) {
                Ok(yaml) => match parse_definition(&yaml) {
                    Ok(definition) => {
                        ids.insert(definition.id);
                    }
                    Err(error) => warnings.push(format!(
                        "skipping workflow {} (does not parse: {error})",
                        path.display()
                    )),
                },
                Err(error) => {
                    warnings.push(format!("skipping workflow {} ({error})", path.display()))
                }
            }
        }
    }

    let mut cards = Vec::new();
    for workflow_id in ids {
        let yaml = match registry.resolve(&workflow_id) {
            Ok(yaml) => yaml,
            Err(error) => {
                warnings.push(format!("skipping workflow {workflow_id} ({error})"));
                continue;
            }
        };
        match codypendent_workflow::compile_yaml(yaml) {
            Ok(compiled) => {
                let label = format!("{} v{}", compiled.id, compiled.version);
                let inputs = if compiled.inputs.is_empty() {
                    "\u{2014}".to_owned()
                } else {
                    compiled
                        .inputs
                        .iter()
                        .map(|(name, input)| {
                            format!(
                                "{}:{}{}",
                                name,
                                input.input_type,
                                if input.required { "*" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                // Overlay the latest durable run's per-node state/cost/error, when
                // one exists and a pool is available.
                let latest = match pool {
                    Some(pool) => latest_workflow_run(pool, &compiled.id).await,
                    None => None,
                };
                cards.extend(compiled.nodes.iter().map(|node| {
                    workflow_node_card(&compiled.id, &label, &inputs, latest.as_ref(), node)
                }));
            }
            Err(error) => {
                warnings.push(format!(
                    "skipping workflow {workflow_id} (does not compile: {error})"
                ));
            }
        }
    }

    if cards.len() > MAX_WORKFLOW_NODES {
        warnings.push(format!(
            "workflow view showing the first {MAX_WORKFLOW_NODES} of {} nodes",
            cards.len()
        ));
        cards.truncate(MAX_WORKFLOW_NODES);
    }
    cards
}

/// The most recent durable run's node records for `workflow_id`, keyed by node id
/// — the overlay [`load_workflows`] applies so the graph view shows a run's live
/// state, MEASURED cost, and failure/block reason. An empty map when no run exists
/// or a read fails: the view then shows the compiled defaults, never a stale one.
struct LatestWorkflowRun {
    id: String,
    phase: String,
    nodes: HashMap<String, codypendent_workflow::WorkflowNodeRecord>,
}

async fn latest_workflow_run(
    pool: &sqlx::SqlitePool,
    workflow_id: &str,
) -> Option<LatestWorkflowRun> {
    let run_id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM workflow_runs WHERE workflow_id = ? \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(workflow_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let run_id = run_id?;
    match codypendent_workflow::WorkflowStore::new()
        .snapshot(pool, &run_id)
        .await
    {
        Ok(Some(snapshot)) => Some(LatestWorkflowRun {
            id: snapshot.run.id,
            phase: snapshot.run.state.as_str().to_owned(),
            nodes: snapshot
                .nodes
                .into_iter()
                .map(|node| (node.node_id.clone(), node))
                .collect(),
        }),
        _ => None,
    }
}

/// Render a node's MEASURED cost JSON into a human string for the graph view
/// (Phase 5 T8). Only the dimensions actually measured are shown — wall time and
/// tool calls — so the column never displays a fabricated token/USD figure. An
/// empty or unrecognized cost shape renders `"—"` (nothing was measured).
fn render_node_cost(cost: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(secs) = cost
        .get("wall_time_secs")
        .and_then(serde_json::Value::as_u64)
    {
        parts.push(format!("{secs}s"));
    }
    if let Some(calls) = cost.get("tool_calls").and_then(serde_json::Value::as_u64) {
        parts.push(format!(
            "{calls} tool call{}",
            if calls == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        "\u{2014}".to_owned()
    } else {
        parts.join(" \u{b7} ")
    }
}

/// Map one [`CompiledNode`](codypendent_workflow::CompiledNode) into the view's
/// [`WorkflowNodeCard`], pre-rendering every field to a human string. `workflow`
/// is the owning workflow's `id vN` label the view groups by.
///
/// `record` is the node's durable run record when a run of this workflow exists:
/// its live state, MEASURED cost (T8), and failure/block reason (P5-D4) overlay
/// the compiled node's pre-run defaults (`pending` / `—` / `—`). This is the seam
/// that turns the forever-`—` cost column and the reasonless `failed`/`blocked`
/// node into real values.
fn workflow_node_card(
    workflow_id: &str,
    workflow: &str,
    inputs: &str,
    latest: Option<&LatestWorkflowRun>,
    node: &codypendent_workflow::CompiledNode,
) -> WorkflowNodeCard {
    use codypendent_workflow::{ApprovalPolicy, NodeAction, WorkspaceMode};

    let dash = || "\u{2014}".to_owned(); // "—"

    // Live state / measured cost / failure reason from a durable run record, else
    // the compiled node's pre-run defaults.
    let record = latest.and_then(|run| run.nodes.get(&node.id));
    let state = record.map_or_else(
        || "pending".to_owned(),
        |record| record.state.as_str().to_owned(),
    );
    let cost = record
        .and_then(|record| record.cost.as_ref())
        .map_or_else(dash, render_node_cost);
    let error = record
        .and_then(|record| record.error.clone())
        .unwrap_or_else(dash);
    let (kind, action, agent, model_policy) = match &node.action {
        NodeAction::Agent {
            role,
            model_policy,
            skill,
        } => {
            let action = match skill {
                Some(skill) => format!("agent {role} \u{b7} skill {skill}"),
                None => format!("agent {role}"),
            };
            (
                "agent".to_owned(),
                action,
                role.clone(),
                model_policy.clone().unwrap_or_else(dash),
            )
        }
        NodeAction::Tool { name } => ("tool".to_owned(), format!("tool {name}"), dash(), dash()),
    };

    let workspace = match node.workspace_mode {
        WorkspaceMode::SharedWorktree => "shared worktree",
        WorkspaceMode::IsolatedWorktree => "isolated worktree",
    }
    .to_owned();

    let approval = match node.approval {
        Some(ApprovalPolicy::BeforeWrite) => "before write".to_owned(),
        Some(ApprovalPolicy::Always) => "always".to_owned(),
        None => "none".to_owned(),
    };

    let retry = {
        let attempts = node.retry.attempts;
        let unit = if attempts == 1 { "attempt" } else { "attempts" };
        if node.retry.backoff_seconds == 0 {
            format!("{attempts} {unit}")
        } else {
            format!(
                "{attempts} {unit} \u{b7} {}s backoff",
                node.retry.backoff_seconds
            )
        }
    };

    let join = |items: &[String]| {
        if items.is_empty() {
            dash()
        } else {
            items.join(", ")
        }
    };

    WorkflowNodeCard {
        workflow_id: workflow_id.to_owned(),
        workflow: workflow.to_owned(),
        workflow_run_id: latest.map(|run| run.id.clone()),
        run_phase: latest.map_or_else(|| "not started".to_owned(), |run| run.phase.clone()),
        inputs: inputs.to_owned(),
        id: node.id.clone(),
        action,
        kind,
        state,
        agent,
        model_policy,
        workspace,
        approval,
        retry,
        depends_on: join(&node.depends_on),
        // The raw edge ids the pane lays out into ASCII lanes (rubric 5) — the
        // comma-joined string above is for the detail rail and cannot be parsed
        // back into a graph.
        depends_on_ids: node.depends_on.clone(),
        outputs: join(&node.outputs),
        cost,
        error,
    }
}

/// The cap on blackboard artifacts surfaced in the view — a long-running board
/// can accumulate many; the read-only view shows the first [`MAX_BLACKBOARD_ITEMS`]
/// (newest first, across the active runs) and logs when it truncates.
const MAX_BLACKBOARD_ITEMS: usize = 500;

/// Project the blackboard artifacts on the active workflow runs into
/// [`BlackboardItemCard`]s (Phase 5 STEP 5.3). The workflow tables share the
/// knowledge database (the migrations are workspace-wide), so the same pool
/// serves them. Runs are the daemon's non-terminal set (the boards worth
/// watching); each run's full board — live and superseded — is queried so the
/// view can dim corrected artifacts. Empty until the executor posts artifacts; a
/// query failure collects a diagnostic and skips that run rather than failing
/// the view.
async fn load_blackboard(
    pool: &sqlx::SqlitePool,
    warnings: &mut Vec<String>,
) -> Vec<BlackboardItemCard> {
    use codypendent_workflow::{BlackboardStore, WorkflowStore};

    let runs = match WorkflowStore::new().list_incomplete_runs(pool).await {
        Ok(runs) => runs,
        Err(error) => {
            warnings.push(format!("could not list workflow runs: {error}"));
            return Vec::new();
        }
    };

    let board = BlackboardStore::new();
    let mut cards = Vec::new();
    for run in runs {
        let run_label = format!("{} \u{b7} run {}", run.workflow_id, short_run_id(&run.id));
        match board.query(pool, &run.id, None, true).await {
            Ok(items) => cards.extend(
                items
                    .iter()
                    .map(|item| blackboard_item_card(&run.id, &run_label, item)),
            ),
            Err(error) => {
                warnings.push(format!(
                    "could not query the blackboard for run {}: {error}",
                    run.id
                ));
            }
        }
        if cards.len() >= MAX_BLACKBOARD_ITEMS {
            warnings.push(format!(
                "blackboard view showing the first {MAX_BLACKBOARD_ITEMS} artifacts"
            ));
            cards.truncate(MAX_BLACKBOARD_ITEMS);
            break;
        }
    }
    cards
}

/// Map a [`BlackboardItem`](codypendent_workflow::BlackboardItem) into the view's
/// [`BlackboardItemCard`], rendering its opaque JSON payload/author/evidence to
/// human strings. `run` is the owning run's label the view groups by.
fn blackboard_item_card(
    workflow_run_id: &str,
    run: &str,
    item: &codypendent_workflow::BlackboardItem,
) -> BlackboardItemCard {
    BlackboardItemCard {
        id: item.id.clone(),
        workflow_run_id: workflow_run_id.to_owned(),
        run: run.to_owned(),
        kind: item.kind.as_str().to_owned(),
        summary: summarize_json(&item.payload),
        author: summarize_author(&item.author),
        confidence: item
            .confidence
            .map_or_else(|| "\u{2014}".to_owned(), |c| format!("{c:.2}")),
        evidence: if item.evidence.is_empty() {
            "\u{2014}".to_owned()
        } else {
            format!("{} ref(s)", item.evidence.len())
        },
        revision: format!("r{}", item.revision),
        superseded: item.superseded_by.is_some(),
    }
}

/// Map the protocol's opaque blackboard view into the same card shape used by
/// the SQLite boot projection. `workflow_label` is resolved from the live
/// workflow cards when possible; the compact run id is always retained.
fn wire_blackboard_item_card(
    workflow_label: Option<&str>,
    item: &codypendent_protocol::BlackboardItemView,
) -> BlackboardItemCard {
    let run = format!(
        "{} · run {}",
        workflow_label.unwrap_or("workflow"),
        short_run_id(&item.workflow_run_id)
    );
    BlackboardItemCard {
        id: item.id.clone(),
        workflow_run_id: item.workflow_run_id.clone(),
        run,
        kind: item.kind.clone(),
        summary: summarize_json(&item.payload),
        author: summarize_author(&item.author),
        confidence: item
            .confidence
            .map_or_else(|| "\u{2014}".to_owned(), |c| format!("{c:.2}")),
        evidence: if item.evidence.is_empty() {
            "\u{2014}".to_owned()
        } else {
            format!("{} ref(s)", item.evidence.len())
        },
        revision: format!("r{}", item.revision),
        superseded: item.superseded_by.is_some(),
    }
}

/// Project one stored board item into a self-contained [`KanbanCard`] (rubric
/// 10). The board's rows ARE blackboard items — the card view just renders the
/// board-specific fields the artifact view has no place for.
fn wire_board_card(item: &codypendent_protocol::BlackboardItemView) -> KanbanCard {
    KanbanCard {
        id: item.id.clone(),
        title: summarize_json(&item.payload),
        status: item.status.clone().unwrap_or_else(|| "todo".to_owned()),
        assignee: item
            .assignee
            .clone()
            .unwrap_or_else(|| "\u{2014}".to_owned()),
        kind: item.kind.clone(),
        author: summarize_author(&item.author),
        // A card with no recorded position sorts to the top of its column rather
        // than to an arbitrary place.
        ordinal: item.ordinal.unwrap_or(0),
    }
}

/// Project a board read's items into cards, dropping any superseded revision
/// that slipped into the reply — the board shows the live card only.
fn wire_board_cards(items: &[codypendent_protocol::BlackboardItemView]) -> Vec<KanbanCard> {
    items
        .iter()
        .filter(|item| item.superseded_by.is_none())
        .map(wire_board_card)
        .collect()
}

/// The first 8 characters of a run id, for a compact run label.
fn short_run_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A one-line human summary of an opaque artifact payload: a string payload as-is;
/// an object's first human-text field (`summary`/`title`/`statement`/…) when one
/// is present; otherwise its compact JSON. Capped so a large payload cannot blow
/// out the card.
fn summarize_json(value: &serde_json::Value) -> String {
    use serde_json::Value;
    let raw = match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => {
            let field = [
                "summary",
                "title",
                "statement",
                "text",
                "description",
                "message",
            ]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str));
            match field {
                Some(text) => text.to_owned(),
                None => value.to_string(),
            }
        }
        other => other.to_string(),
    };
    truncate_chars(&raw, 200)
}

/// A compact rendering of an opaque author record: a string as-is; an object's
/// `role`/`agent` as `"agent <role>"`; otherwise its compact JSON.
fn summarize_author(value: &serde_json::Value) -> String {
    use serde_json::Value;
    let raw = match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => match map
            .get("role")
            .or_else(|| map.get("agent"))
            .and_then(Value::as_str)
        {
            Some(role) => format!("agent {role}"),
            None => value.to_string(),
        },
        other => other.to_string(),
    };
    truncate_chars(&raw, 80)
}

/// Truncate to at most `max` characters, appending an ellipsis when cut (char-safe
/// so a multi-byte boundary is never split).
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let kept: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}\u{2026}")
    }
}

fn collab_mode_label(mode: CollaborationMode) -> &'static str {
    match mode {
        CollaborationMode::Ask => "ask",
        CollaborationMode::Suggest => "suggest",
        CollaborationMode::Edit => "edit",
        CollaborationMode::CoAuthor => "co-author",
        CollaborationMode::Review => "review",
        CollaborationMode::Maintain => "maintain",
    }
}

fn suggestion_status_label(status: SuggestionStatus) -> &'static str {
    match status {
        SuggestionStatus::Pending => "pending",
        SuggestionStatus::Accepted => "accepted",
        SuggestionStatus::Rejected => "rejected",
    }
}

/// A compact label for who authored a document mutation — an agent sentence
/// names its serving model (the traceability triple's public face).
fn document_author_label(author: &DocumentAuthor) -> String {
    match author {
        DocumentAuthor::Human { .. } => "human".to_owned(),
        DocumentAuthor::Agent { model, .. } => format!("agent ({model})"),
        DocumentAuthor::Integration { integration } => format!("integration ({integration})"),
    }
}

/// The persisted repo → session mapping, so reopening the TUI in a repository
/// resumes its session instead of starting over. Stored as JSON in the data dir;
/// a corrupt or absent file reads as empty (the store is a convenience, never a
/// source of truth — the daemon's ledger is).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionStore {
    /// Canonical repository path → the session last opened there.
    sessions: HashMap<String, StoredSession>,
    /// The opaque daemon-issued resume token from the last handshake, presented
    /// on the next launch so this client keeps one identity across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume_token: Option<String>,
    /// The theme id last kept in the TUI's `/theme` picker — a built-in variant
    /// name or an installed pack id. Read at boot by `theme_select`, BELOW an
    /// explicit `--theme`/`CODYPENDENT_THEME` (an explicit override always
    /// wins) and above terminal detection. An unknown id simply falls through
    /// to detection, so a removed pack cannot wedge the TUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    theme: Option<String>,
    /// The operator explicitly chose to enter Chat without a runnable model.
    /// This is the only onboarding preference: successful completion is
    /// derived from the authoritative runnable-model projection at each boot.
    #[serde(default, skip_serializing_if = "is_false")]
    onboard_skipped: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One remembered session: its id and the workspace it belongs to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct StoredSession {
    session_id: SessionId,
    workspace_id: WorkspaceId,
}

impl SessionStore {
    fn file(paths: &RuntimePaths) -> PathBuf {
        paths.data_dir.join("tui-sessions.json")
    }

    fn load(paths: &RuntimePaths) -> Self {
        std::fs::read(Self::file(paths))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, paths: &RuntimePaths) {
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = paths.ensure_directories();
            let _ = std::fs::write(Self::file(paths), bytes);
        }
    }
}

/// Apply one reducer-owned onboarding preference intent. Returning whether it
/// was handled keeps persistence testable without constructing the socket event
/// loop and makes completion/skip share one authoritative mutation site.
fn apply_onboard_preference(store: &mut SessionStore, intent: &Intent) -> bool {
    match intent {
        Intent::SetOnboardComplete => {
            store.onboard_skipped = false;
            true
        }
        Intent::SetOnboardSkipped => {
            store.onboard_skipped = true;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{
        AgentMode, ApprovalDecision, ApprovalId, ApprovalScope, ModelId, RunId,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A snapshot catch-up needs its transcript paged in; an event one does not.
    ///
    /// Boot knew this and paged; switching or forking to a session from inside
    /// the TUI folded the snapshot alone, so a long-running session opened blank
    /// one way and complete the other.
    #[test]
    fn only_a_snapshot_catchup_needs_its_history_paged_in() {
        assert_eq!(
            snapshot_history_target(&Catchup::Snapshot {
                through: 512,
                projection: Default::default(),
            }),
            Some(512),
            "a snapshot carries no transcript; it must be paged up to `through`"
        );
        assert_eq!(
            snapshot_history_target(&Catchup::Events {
                from: 0,
                through: 512,
                events: Vec::new(),
            }),
            None,
            "an event catch-up already replayed the range"
        );
        assert_eq!(
            snapshot_history_target(&Catchup::Snapshot {
                through: 0,
                projection: Default::default(),
            }),
            None,
            "nothing precedes sequence zero, so there is nothing to page"
        );
        assert_eq!(
            snapshot_history_target(&Catchup::Unknown),
            None,
            "a future variant stays inert rather than triggering a blind read"
        );
    }

    #[test]
    fn newest_session_transition_generation_is_the_only_authority() {
        let mut transitions = SessionTransitionCoordinator::default();
        let switch_a = transitions.begin(SessionTransitionKind::Switch);
        let switch_b = transitions.begin(SessionTransitionKind::Switch);

        assert!(!transitions.finish(switch_a, SessionTransitionKind::Switch));
        assert_eq!(
            transitions.active,
            Some((switch_b, SessionTransitionKind::Switch)),
            "a late completion for A must not retire the pending B attachment"
        );
        assert!(transitions.finish(switch_b, SessionTransitionKind::Switch));
        assert!(!transitions.is_active());
    }

    #[tokio::test]
    async fn superseding_a_transition_aborts_its_background_prepare_task() {
        let mut transitions = SessionTransitionCoordinator::default();
        let first = transitions.begin(SessionTransitionKind::New);
        let task = tokio::spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        transitions.track_task(first, task);

        let _second = transitions.begin(SessionTransitionKind::Switch);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    #[test]
    fn transition_defers_intents_in_order_until_the_matching_completion() {
        let mut transitions = SessionTransitionCoordinator::default();
        let generation = transitions.begin(SessionTransitionKind::Fork);
        let first = Intent::Notify {
            message: "first".to_owned(),
        };
        let second = Intent::SearchFiles {
            query: "second".to_owned(),
        };
        assert!(transitions.defer(first.clone()).is_none());
        assert!(transitions.defer(second.clone()).is_none());

        assert!(!transitions.finish(generation, SessionTransitionKind::New));
        assert_eq!(transitions.deferred.len(), 2);
        assert!(transitions.finish(generation, SessionTransitionKind::Fork));
        assert_eq!(transitions.take_deferred(), vec![first, second]);
    }

    #[test]
    fn transport_loss_during_a_user_transition_requires_reconnect_on_failure() {
        let mut transitions = SessionTransitionCoordinator::default();
        let generation = transitions.begin(SessionTransitionKind::Switch);
        transitions.mark_transport_lost();

        assert!(transitions.transport_lost);
        assert!(transitions.finish(generation, SessionTransitionKind::Switch));
        assert!(!transitions.is_active());
        assert!(
            transitions.transport_lost,
            "finishing a failed user transition must not pretend the old socket recovered"
        );

        let reconnect = transitions.begin(SessionTransitionKind::Reconnect);
        assert_eq!(
            transitions.active,
            Some((reconnect, SessionTransitionKind::Reconnect))
        );
        assert!(transitions.finish(reconnect, SessionTransitionKind::Reconnect));
        transitions.mark_transport_ready();
        assert!(!transitions.transport_lost);
    }

    /// Editing the same document after a reconnect must not subscribe twice.
    ///
    /// Reconnect clears the replica map (the replicas reseed from the fresh
    /// stream) and keeps the subscription list (it is re-attached as-is). Code
    /// that read "no replica" as "not subscribed" therefore pushed another
    /// `Subscription::Document` for a document already being watched, once per
    /// reconnect-then-edit cycle, and every re-attach afterwards re-sent the
    /// whole accumulated list.
    #[test]
    fn a_document_is_subscribed_once_across_reconnects() {
        let document_id = DocumentId::new();
        let other = DocumentId::new();
        let mut subscriptions = default_subscriptions();

        assert!(
            subscribe_to_document(&mut subscriptions, document_id),
            "the first edit subscribes, and must re-attach to say so"
        );
        assert!(
            !subscribe_to_document(&mut subscriptions, document_id),
            "a second edit on the same document must not re-attach"
        );
        // What reconnect does: replicas are dropped, subscriptions survive.
        assert!(
            !subscribe_to_document(&mut subscriptions, document_id),
            "an edit after reconnect must not subscribe a second time"
        );
        assert!(
            subscribe_to_document(&mut subscriptions, other),
            "a different document is still its own subscription"
        );

        let documents: Vec<DocumentId> = subscriptions
            .iter()
            .filter_map(|s| match s {
                Subscription::Document { document_id } => Some(*document_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            documents,
            vec![document_id, other],
            "one subscription per document, in the order they were opened"
        );
    }

    #[test]
    fn ui_plugin_pending_commands_cover_only_lifecycle_mutations() {
        let cases = [
            (
                Intent::SmokeTestUiPlugin {
                    plugin_id: "demo".to_owned(),
                },
                "smoke test",
            ),
            (
                Intent::EnableUiPlugin {
                    plugin_id: "demo".to_owned(),
                    scope: "session".to_owned(),
                },
                "enable",
            ),
            (
                Intent::ApproveUiPluginUpdate {
                    plugin_id: "demo".to_owned(),
                    receipt: "receipt".to_owned(),
                },
                "approve update",
            ),
            (
                Intent::RejectUiPluginUpdate {
                    plugin_id: "demo".to_owned(),
                    receipt: "receipt".to_owned(),
                },
                "reject update",
            ),
            (
                Intent::RevokeUiPlugin {
                    plugin_id: "demo".to_owned(),
                },
                "revoke",
            ),
        ];

        for (intent, operation) in cases {
            assert_eq!(
                PendingUiPluginCommand::from_intent(&intent),
                Some(PendingUiPluginCommand {
                    operation,
                    plugin_id: "demo".to_owned(),
                })
            );
        }
        assert!(PendingUiPluginCommand::from_intent(&Intent::ListUiPlugins).is_none());
    }

    #[test]
    fn ui_plugin_pending_commands_resolve_only_the_correlated_reply() {
        let request = codypendent_protocol::MessageId::new();
        let unrelated = codypendent_protocol::MessageId::new();
        let mut pending = PendingUiPluginCommands::default();
        pending.observe(
            request,
            PendingUiPluginCommand {
                operation: "enable",
                plugin_id: "demo".to_owned(),
            },
        );

        assert!(pending.resolve(Some(unrelated)).is_none());
        assert_eq!(pending.by_message.len(), 1);
        let matched = pending
            .resolve(Some(request))
            .expect("the correlated lifecycle request should resolve");
        assert_eq!(
            matched.rejection_message("plugin.lifecycle-refused", "signature changed"),
            "UI plugin enable failed for demo: signature changed (plugin.lifecycle-refused)"
        );
        assert!(pending.by_message.is_empty());
    }

    #[test]
    fn splash_gate_requires_a_fresh_enter_and_keeps_quit_and_resize_responsive() {
        let enter = CrosstermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(splash_gate_decision(&enter), SplashGateDecision::Continue);

        let mut released_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        released_enter.kind = KeyEventKind::Release;
        assert_eq!(
            splash_gate_decision(&CrosstermEvent::Key(released_enter)),
            SplashGateDecision::Ignore,
            "a release event must not enter the workspace"
        );
        assert_eq!(
            splash_gate_decision(&CrosstermEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            ))),
            SplashGateDecision::Ignore
        );
        assert_eq!(
            splash_gate_decision(&CrosstermEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            ))),
            SplashGateDecision::Quit
        );
        assert_eq!(
            splash_gate_decision(&CrosstermEvent::Resize(120, 40)),
            SplashGateDecision::Redraw
        );
    }

    #[test]
    fn accessible_presentation_is_stable_and_emits_no_terminal_escapes() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::Notice("ready \u{1b}[31m safely".to_owned()),
        );
        let mut presentation = AccessiblePresentation::new(Vec::<u8>::new());
        presentation
            .draw(&state, false)
            .expect("first cooked snapshot");
        let first_len = presentation.output.len();
        presentation
            .draw(&state, false)
            .expect("unchanged state is not repeated");
        assert_eq!(presentation.output.len(), first_len);

        let output = String::from_utf8(presentation.output).expect("UTF-8 output");
        assert!(output.contains("--- accessible update ---"));
        assert!(output.contains("Notice: ready  safely"));
        assert!(!output.as_bytes().contains(&0x1b));
        assert!(
            !output.contains("[?1049h"),
            "must not enter alternate screen"
        );
        assert!(!output.contains("[?1000h"), "must not enable mouse capture");
    }

    #[test]
    fn accessible_snapshot_delta_emits_only_the_changed_window() {
        let previous = "header\nassistant: hel\ncontrols\nprompt";
        let next = "header\nassistant: hello\ncontrols\nprompt";
        assert_eq!(
            accessible_snapshot_delta(previous, next),
            "1 previous line(s) replaced.\nassistant: hello"
        );
    }

    #[test]
    fn accessible_snapshot_delta_reports_removed_lines() {
        assert_eq!(
            accessible_snapshot_delta("header\nstale\ncontrols", "header\ncontrols"),
            "1 previous line(s) replaced."
        );
    }

    #[test]
    fn accessible_script_maps_lines_without_synthetic_terminal_events() {
        let mut state = AppState::new();
        for line in ["type hello", "enter"] {
            for action in map_accessible_input(line, state.input_mode()) {
                reduce(&mut state, action);
            }
        }
        assert!(state.composer.is_empty());
        assert!(state.drain_outbox().iter().any(
            |intent| matches!(intent, Intent::StartRun { objective, .. } if objective == "hello")
        ));
    }

    #[tokio::test]
    async fn graph_loader_pages_and_filters_without_materializing_the_full_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = knowledge_db::open(&dir.path().join("graph.db"))
            .await
            .expect("knowledge db");
        let repository = RepositoryId::new();
        let repository_text = repository.to_string();
        let created_at = "2026-08-05T00:00:00Z";
        for (id, name, symbol_key) in [
            ("node-from", "crate::parser::parse", "parser-parse"),
            ("node-to", "crate::lexer::next", "lexer-next"),
        ] {
            sqlx::query(
                "INSERT INTO code_nodes \
                 (id, repository, language, package, source_path, qualified_name, kind, \
                  signature_hash, symbol_key, revision, created_at) \
                 VALUES (?, ?, 'rust', NULL, 'src/lib.rs', ?, 'function', NULL, ?, 'rev-1', ?)",
            )
            .bind(id)
            .bind(&repository_text)
            .bind(name)
            .bind(symbol_key)
            .bind(created_at)
            .execute(&pool)
            .await
            .expect("insert node");
        }
        let mut tx = pool.begin().await.expect("edge transaction");
        for index in 0..130 {
            let relation = if index == 129 {
                "calls_special"
            } else {
                "calls"
            };
            sqlx::query(
                "INSERT INTO code_edges \
                 (id, from_node, to_node, relation, confidence, evidence_kind, \
                  evidence_artifact, revision, created_at) \
                 VALUES (?, 'node-from', 'node-to', ?, 0.45, 'syntax_inferred', NULL, 'rev-1', ?)",
            )
            .bind(format!("edge-{index:03}"))
            .bind(relation)
            .bind(created_at)
            .execute(&mut *tx)
            .await
            .expect("insert edge");
        }
        tx.commit().await.expect("commit edges");

        let mut warnings = Vec::new();
        let (first, total, page) = load_edge_page(&pool, repository, "", 0, &mut warnings).await;
        assert_eq!(total, 130);
        assert_eq!(page, 0);
        assert_eq!(first.len(), EDGE_PAGE_SIZE);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        let (last, total, page) = load_edge_page(&pool, repository, "", 99, &mut warnings).await;
        assert_eq!(total, 130);
        assert_eq!(page, 1, "an oversized page clamps to the final page");
        assert_eq!(last.len(), 30);

        let (filtered, total, page) =
            load_edge_page(&pool, repository, "special", 0, &mut warnings).await;
        assert_eq!(total, 1);
        assert_eq!(page, 0);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].relation, "calls-special");
    }

    #[test]
    fn pending_start_retry_preserves_wire_and_idempotency_identity() {
        let client_id = ClientId::new();
        let session_id = SessionId::new();
        let mut envelope = command_envelope(
            client_id,
            CommandBody::StartRun {
                session_id,
                objective: "repair reconnect".to_owned(),
                mode: AgentMode::Build,
                repository: Some("/repo".to_owned()),
                model: None,
            },
        );
        envelope.session_id = Some(session_id);
        let message_id = envelope.message_id;
        let (command_id, idempotency_key) = match &envelope.payload {
            Payload::Command(command) => (command.command_id, command.idempotency_key.clone()),
            other => panic!("expected command, got {other:?}"),
        };

        let mut pending = PendingStartRunCommand::default();
        pending.observe_outbound(&envelope);
        assert!(pending.matches_rejection(Some(message_id)));
        assert!(!pending.matches_rejection(Some(codypendent_protocol::MessageId::new())));

        let replacement_client_id = ClientId::new();
        let retry = pending
            .retry_envelope(replacement_client_id)
            .expect("pending retry");
        assert_eq!(retry.message_id, message_id);
        assert_eq!(retry.client_id, replacement_client_id);
        assert_eq!(retry.session_id, Some(session_id));
        match retry.payload {
            Payload::Command(command) => {
                assert_eq!(command.command_id, command_id);
                assert_eq!(command.idempotency_key, idempotency_key);
            }
            other => panic!("expected command, got {other:?}"),
        }

        pending.clear();
        assert!(pending.retry_envelope(replacement_client_id).is_none());
    }

    /// FIX 1: resuming a session from the `/sessions` picker must ATTACH to the
    /// existing session id — never create a fresh one — so the socket the event
    /// loop keeps is bound to the same session every later command targets, and
    /// the resumed transcript is caught up. This mirrors the mock-daemon style of
    /// `workflow_it.rs`: play the daemon's side of one attach and assert the
    /// command carried the resumed id and that its catch-up was returned.
    #[tokio::test]
    async fn resume_attaches_to_the_existing_session_and_loads_its_catchup() {
        use tokio::net::{UnixListener, UnixStream};

        async fn mock_daemon(mut stream: UnixStream, expected: SessionId) {
            // 1. Handshake.
            let hello = read_envelope(&mut stream)
                .await
                .expect("read ClientHello")
                .expect("connection open");
            assert!(matches!(hello.payload, Payload::ClientHello(_)));
            write_envelope(
                &mut stream,
                &Envelope::reply_to(
                    &hello,
                    Payload::ServerHello(codypendent_protocol::ServerHello {
                        resume_token: None,
                        selected_protocol: codypendent_protocol::PROTOCOL_V1,
                        daemon_version: "mock".to_string(),
                        daemon_instance: codypendent_protocol::DaemonInstanceId::new(),
                        heartbeat_interval_ms: 15_000,
                        build_id: String::new(),
                    }),
                ),
            )
            .await
            .expect("write ServerHello");

            // 2. AttachSession must target the resumed session id — NOT a
            // freshly created one — and there must be no CreateSession first.
            let attach = read_envelope(&mut stream)
                .await
                .expect("read AttachSession")
                .expect("connection open");
            match &attach.payload {
                Payload::Command(command) => match &command.body {
                    CommandBody::AttachSession { session_id, .. } => {
                        assert_eq!(
                            *session_id, expected,
                            "resume must attach to the picked session, not a new id"
                        );
                    }
                    other => panic!("expected AttachSession, got {other:?}"),
                },
                other => panic!("expected a Command envelope, got {other:?}"),
            }
            write_envelope(
                &mut stream,
                &Envelope::reply_to(
                    &attach,
                    Payload::Catchup {
                        catchup: Catchup::Events {
                            from: 1,
                            through: 7,
                            events: vec![ev(7)],
                        },
                    },
                ),
            )
            .await
            .expect("write Catchup");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind mock socket");
        let existing = SessionId::new();
        let server = tokio::spawn(async move {
            let (stream, _addr) = listener.accept().await.expect("accept");
            mock_daemon(stream, existing).await;
        });

        let mut paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        paths.socket_path = socket_path;

        let fresh = attach_session_live(&paths, "/repo", existing, &[], None)
            .await
            .expect("attach to the existing session");

        assert_eq!(
            fresh.session_id, existing,
            "the live socket is bound to the resumed session"
        );
        match fresh.catchup {
            Catchup::Events { events, .. } => {
                assert_eq!(seqs(&events), vec![7], "the resumed transcript is loaded");
            }
            other => panic!("expected an event catch-up, got {other:?}"),
        }
        server.await.expect("mock server task");
    }

    #[test]
    fn intents_map_to_the_matching_command_bodies() {
        let session_id = SessionId::new();
        let run_id = RunId::new();
        let repository = "/repo/one";

        assert_eq!(
            intent_to_command(
                Intent::StartRun {
                    objective: "diagnose".into(),
                    mode: AgentMode::Build,
                    // A pinned model (STEP MP2) must flow onto the command.
                    model: Some(ModelId("hosted-gpt".into())),
                },
                session_id,
                repository,
            ),
            CommandBody::StartRun {
                session_id,
                objective: "diagnose".into(),
                mode: AgentMode::Build,
                repository: Some(repository.to_owned()),
                model: Some(ModelId("hosted-gpt".into())),
            }
        );

        let approval_id = ApprovalId::new();
        assert_eq!(
            intent_to_command(
                Intent::ResolveApproval {
                    approval_id,
                    decision: ApprovalDecision::Approve,
                    scope: ApprovalScope::Once,
                },
                session_id,
                repository,
            ),
            CommandBody::ResolveApproval {
                approval_id,
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Once,
            }
        );

        // Task 5 (continuous-session plan): a follow-up after a terminal run
        // maps to `SubmitUserInput`, not `StartRun` — the daemon seeds it from
        // the session's prior turns instead of starting cold. Mirrors
        // `StartRun`'s `session_id` binding above; the operator's current model
        // pin threads through so a mid-conversation switch reaches the wire
        // (repository is still not on this shape).
        assert_eq!(
            intent_to_command(
                Intent::SubmitUserInput {
                    text: "also add tests".into(),
                    mode: AgentMode::Build,
                    model: Some(codypendent_protocol::ModelId("pinned-model-x".into())),
                },
                session_id,
                repository,
            ),
            CommandBody::SubmitUserInput {
                session_id,
                text: "also add tests".into(),
                mode: AgentMode::Build,
                model: Some(codypendent_protocol::ModelId("pinned-model-x".into())),
                envelope: None,
            }
        );
        // And an unpinned follow-up carries no model on the wire (inherit).
        assert_eq!(
            intent_to_command(
                Intent::SubmitUserInput {
                    text: "keep going".into(),
                    mode: AgentMode::Build,
                    model: None,
                },
                session_id,
                repository,
            ),
            CommandBody::SubmitUserInput {
                session_id,
                text: "keep going".into(),
                mode: AgentMode::Build,
                model: None,
                envelope: None,
            }
        );

        assert_eq!(
            intent_to_command(Intent::PauseRun { run_id }, session_id, repository),
            CommandBody::PauseRun { run_id }
        );
        assert_eq!(
            intent_to_command(Intent::ResumeRun { run_id }, session_id, repository),
            CommandBody::ResumeRun { run_id }
        );
        assert_eq!(
            intent_to_command(Intent::CancelRun { run_id }, session_id, repository),
            CommandBody::CancelRun { run_id }
        );
        assert_eq!(
            intent_to_command(
                Intent::QueueSteering {
                    run_id,
                    text: "focus on the failing test".into(),
                },
                session_id,
                repository,
            ),
            CommandBody::QueueSteering {
                run_id,
                text: "focus on the failing test".into(),
            }
        );

        // Phase 4 STEP 4.3 document-editing intents lower to their commands.
        let document_id = codypendent_protocol::DocumentId::new();
        assert_eq!(
            intent_to_command(
                Intent::AcquireDocumentLease {
                    document_id,
                    block_id: Some("b1".into()),
                },
                session_id,
                repository,
            ),
            CommandBody::AcquireDocumentLease {
                lease: DocumentEditLease {
                    document_id,
                    block_id: Some("b1".into()),
                },
                ttl_seconds: None,
            }
        );
        assert_eq!(
            intent_to_command(
                Intent::ReleaseDocumentLease {
                    lease_id: "lease-1".into(),
                },
                session_id,
                repository,
            ),
            CommandBody::ReleaseDocumentLease {
                lease_id: "lease-1".into(),
            }
        );
        let mutation = codypendent_protocol::DocumentMutation::AcceptSuggestion {
            suggestion_id: "s1".into(),
        };
        assert_eq!(
            intent_to_command(
                Intent::MutateDocument {
                    document_id,
                    mutation: mutation.clone(),
                },
                session_id,
                repository,
            ),
            CommandBody::MutateDocument {
                document_id,
                mutation,
            }
        );

        // Only edit-bearing intents drive a subscription; a release does not.
        assert_eq!(
            doc_intent_target(&Intent::MutateDocument {
                document_id,
                mutation: codypendent_protocol::DocumentMutation::RejectSuggestion {
                    suggestion_id: "s1".into(),
                },
            }),
            Some(document_id)
        );
        assert_eq!(
            doc_intent_target(&Intent::ReleaseDocumentLease {
                lease_id: "lease-1".into(),
            }),
            None
        );
    }

    /// Every board-bound command carries the CHECKOUT, not the directory the
    /// TUI was opened in.
    ///
    /// The board's channel id is `board:{path}`, so this path IS the board's
    /// identity: opened from `repo/src`, the unanchored spelling minted a
    /// second board — `repo/` showed 6 cards, `repo/src` showed 0, and a card
    /// created from `src/` was invisible from the root forever, silently.
    /// Revert `board_repository` to `repository.to_owned()` and both
    /// assertions below fail with the subdirectory path.
    #[test]
    fn board_commands_anchor_to_the_checkout_not_the_opened_directory() {
        let repo = tempfile::tempdir().expect("tempdir");
        let status = std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["init", "--quiet"])
            .status()
            .expect("git init");
        assert!(status.success());
        let nested = repo.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("mkdir -p");

        let toplevel = crate::repo_anchor::anchor_repository_path(repo.path())
            .to_string_lossy()
            .into_owned();
        let opened_in = nested.to_string_lossy().into_owned();
        assert_ne!(
            opened_in, toplevel,
            "the test is vacuous unless the opened directory differs from the checkout"
        );

        let session_id = SessionId::new();
        match intent_to_command(
            Intent::CreateBoardCard {
                title: "from a subdirectory".to_owned(),
            },
            session_id,
            &opened_in,
        ) {
            CommandBody::PostBlackboardItem {
                scope: BlackboardScope::RepositoryBoard { repository },
                ..
            } => assert_eq!(repository, toplevel),
            other => panic!("expected a RepositoryBoard post, got {other:?}"),
        }

        match intent_to_command(
            Intent::MoveBoardCard {
                item_id: "item-1".to_owned(),
                status: "doing".to_owned(),
            },
            session_id,
            &opened_in,
        ) {
            CommandBody::UpdateBlackboardItem {
                scope: codypendent_protocol::BlackboardScope::RepositoryBoard { repository },
                ..
            } => assert_eq!(repository, toplevel),
            other => panic!("expected a RepositoryBoard update, got {other:?}"),
        }

        // And the subscription's channel id, which is derived from the same
        // string — a read that anchored while the subscribe did not would
        // load the right board and then never see a live update on it.
        assert_eq!(
            codypendent_protocol::board_scope_id(&board_repository(&opened_in)),
            codypendent_protocol::board_scope_id(&toplevel)
        );
    }

    /// MP1: `model_card` maps a configured model to its measured profile when
    /// one exists at the SAME endpoint (a profile is keyed by
    /// `(model_id, endpoint)`). Without one, configured endpoint locality and
    /// context still render honestly while measured cost remains absent.
    #[test]
    fn model_card_matches_a_profile_by_id_and_endpoint_or_falls_back_id_only() {
        use codypendent_routing::{
            EditProtocol, ModelCapabilities, ModelExecutionProfile, ModelLocation,
            ModelPerformance, ModelProfile, SchemaRepairPolicy, StructuredOutputSupport,
            ToolCallSupport,
        };
        use std::collections::BTreeMap;

        let hosted = codypendent_runtime::models::ModelConfig {
            id: ModelId("hosted-default".into()),
            provider: "openai-compatible".to_owned(),
            base_url: "https://api.openai.com/v1".to_owned(),
            model: "gpt-5.1-codex".to_owned(),
            api_key_env: "OPENAI_API_KEY".to_owned(),
            context_tokens: None,
            provider_id: None,
        };
        // Same id, but the profile below is measured against a DIFFERENT
        // endpoint — must not match (proves the lookup keys on the pair, not
        // just the id).
        let same_id_other_endpoint = codypendent_runtime::models::ModelConfig {
            id: ModelId("hosted-default".into()),
            provider: "openai-compatible".to_owned(),
            base_url: "https://other.example.com/v1".to_owned(),
            model: "gpt-5.1-codex".to_owned(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: None,
        };
        let unprofiled = codypendent_runtime::models::ModelConfig {
            id: ModelId("local-default".into()),
            provider: "openai-compatible".to_owned(),
            base_url: "http://localhost:11434/v1".to_owned(),
            model: "qwen2.5-coder:14b".to_owned(),
            api_key_env: String::new(),
            context_tokens: None,
            provider_id: None,
        };

        let profile = ModelProfile {
            id: ModelId("hosted-default".into()),
            location: ModelLocation::Hosted,
            capabilities: ModelCapabilities {
                streaming: true,
                tools: ToolCallSupport::Parallel,
                parallel_tools: true,
                structured_output: StructuredOutputSupport::Strict,
                vision: false,
                audio_input: false,
                embeddings: false,
                prompt_caching: true,
                reasoning_controls: false,
                context_tokens: Some(200_000),
                output_tokens: Some(8_192),
            },
            performance: ModelPerformance {
                reliability: 0.9,
                cost_per_1k_tokens_usd: 0.03,
                latency_ms_p50: 500.0,
                task_class_success: BTreeMap::new(),
                failure_patterns: vec![],
            },
            execution: ModelExecutionProfile {
                preferred_tool_count: 8,
                edit_protocol: EditProtocol::StructuredPatch,
                context_layout: "system-context-history".to_owned(),
                reasoning_budget: None,
                schema_repair: SchemaRepairPolicy::Reprompt,
            },
            bench: None,
        };
        let mut profiles = HashMap::new();
        profiles.insert(
            (
                ModelId("hosted-default".into()),
                "https://api.openai.com/v1".to_owned(),
            ),
            profile,
        );

        let hosted_card = model_card(hosted, &profiles, ModelReadiness::Unverified, false);
        assert_eq!(hosted_card.id, ModelId("hosted-default".into()));
        assert_eq!(hosted_card.provider, "openai-compatible");
        assert_eq!(hosted_card.location, Some(ModelLocationLabel::Hosted));
        assert!(
            (hosted_card.cost_per_1k_usd.unwrap() - 0.03).abs() < 1e-9,
            "cost should round-trip: {:?}",
            hosted_card.cost_per_1k_usd
        );
        assert_eq!(hosted_card.context_tokens, Some(200_000));

        let other_endpoint_card = model_card(
            same_id_other_endpoint,
            &profiles,
            ModelReadiness::Unverified,
            false,
        );
        assert_eq!(
            other_endpoint_card.location,
            Some(ModelLocationLabel::Hosted),
            "a mismatched profile must not erase configured endpoint locality"
        );

        let unprofiled_card = model_card(unprofiled, &profiles, ModelReadiness::Ready, true);
        assert_eq!(unprofiled_card.id, ModelId("local-default".into()));
        assert_eq!(
            unprofiled_card.location,
            Some(ModelLocationLabel::Local),
            "a local configured endpoint remains visible without a profile"
        );
        assert!(unprofiled_card.cost_per_1k_usd.is_none());
        assert!(unprofiled_card.context_tokens.is_none());
    }

    #[test]
    fn command_envelope_is_self_idempotent() {
        let client_id = ClientId::new();
        let envelope = command_envelope(
            client_id,
            CommandBody::PauseRun {
                run_id: RunId::new(),
            },
        );
        match envelope.payload {
            Payload::Command(command) => {
                // A client-side retry reuses the command id, so the idempotency
                // key must be that same id — the duplicate-delivery contract.
                assert_eq!(command.idempotency_key, command.command_id.to_string());
                assert!(command.expected_revision.is_none());
            }
            other => panic!("expected a Command payload, got {other:?}"),
        }
    }

    #[test]
    fn session_store_round_trips_through_the_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
        paths.ensure_directories().unwrap();

        let mut store = SessionStore::default();
        let stored = StoredSession {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
        };
        store.sessions.insert("/repo/one".into(), stored);
        store.onboard_skipped = true;
        store.save(&paths);

        let loaded = SessionStore::load(&paths);
        let got = loaded.sessions.get("/repo/one").expect("entry persisted");
        assert_eq!(got.session_id, stored.session_id);
        assert_eq!(got.workspace_id, stored.workspace_id);
        assert!(loaded.onboard_skipped);
    }

    #[test]
    fn onboarding_preferences_have_one_authoritative_skip_semantics() {
        let mut store = SessionStore::default();
        assert!(apply_onboard_preference(
            &mut store,
            &Intent::SetOnboardSkipped
        ));
        assert!(store.onboard_skipped);

        assert!(apply_onboard_preference(
            &mut store,
            &Intent::SetOnboardComplete
        ));
        assert!(
            !store.onboard_skipped,
            "a verified completion clears a prior skip"
        );
        assert!(!apply_onboard_preference(
            &mut store,
            &Intent::SetTheme {
                id: "dark".to_owned()
            }
        ));
    }

    #[test]
    fn post_boot_gate_is_shared_and_uses_runnable_projection_not_model_count() {
        use codypendent_tui::state::OnboardStep;

        let mut state = AppState::new();
        state.models.push(ModelCard {
            id: ModelId("configured/but-unrunnable".to_owned()),
            provider: "openai-compatible".to_owned(),
            readiness: ModelReadiness::Unavailable("missing API key".to_owned()),
            location: Some(ModelLocationLabel::Hosted),
            cost_per_1k_usd: None,
            context_tokens: None,
        });
        let store = SessionStore::default();

        assert_eq!(splash_ready_stage("repo", 0), "set up a model to continue");
        assert!(apply_post_boot_onboard_gate(&mut state, &store));
        assert!(matches!(
            state.overlay,
            codypendent_tui::Overlay::Onboard {
                step: OnboardStep::Triage { .. }
            }
        ));

        let mut skipped_state = AppState::new();
        let skipped_store = SessionStore {
            onboard_skipped: true,
            ..SessionStore::default()
        };
        assert!(!apply_post_boot_onboard_gate(
            &mut skipped_state,
            &skipped_store
        ));
        assert_eq!(skipped_state.overlay, codypendent_tui::Overlay::None);

        let mut ready_state = AppState::new();
        ready_state
            .runnable_models
            .push(ModelId("ready/model".to_owned()));
        assert_eq!(splash_ready_stage("repo", 1), "repo is ready");
        assert!(!apply_post_boot_onboard_gate(&mut ready_state, &store));
    }

    #[test]
    fn provider_request_generation_rejects_stale_model_and_acp_terminals() {
        let mut requests = ProviderRequestGenerations::default();
        let stale_acp = requests.begin("claude-acp");
        let latest_acp = requests.begin("claude-acp");
        assert!(!requests.is_current("claude-acp", stale_acp));
        assert!(requests.is_current("claude-acp", latest_acp));

        let moved_to_codex = requests.begin("codex-acp");
        assert!(requests.is_current("codex-acp", moved_to_codex));
        assert!(
            !requests.is_current("claude-acp", latest_acp),
            "moving to another provider invalidates a late ACP completion"
        );
    }

    #[test]
    fn a_missing_store_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
        assert!(SessionStore::load(&paths).sessions.is_empty());
    }

    /// Crash-logger tests (crash investigation follow-up). `format_crash_entry`
    /// is pure (no I/O), so a test can assert its shape with a synthetic
    /// message/location/backtrace without ever touching a real
    /// `PanicHookInfo` — deliberately, since fabricating one outside an actual
    /// panic is not practical, and this fn is what the panic hook's formatting
    /// reduces to (see `append_crash_log`).
    #[test]
    fn format_crash_entry_includes_message_and_location() {
        let entry = format_crash_entry(
            "index out of bounds: the len is 3 but the index is 5",
            Some("crates/tui/src/render.rs:42:9"),
            "0: codypendent_cli::tui::install_crash_hook\n1: <backtrace omitted>",
        );
        assert!(!entry.is_empty());
        assert!(entry.contains("index out of bounds: the len is 3 but the index is 5"));
        assert!(entry.contains("crates/tui/src/render.rs:42:9"));
        assert!(entry.contains("<backtrace omitted>"));
    }

    #[test]
    fn format_crash_entry_falls_back_when_location_is_absent() {
        let entry = format_crash_entry("boom", None, "<no backtrace captured>");
        assert!(!entry.is_empty());
        assert!(entry.contains("boom"));
    }

    #[test]
    fn write_crash_entry_creates_the_log_and_appends_a_nonempty_entry() {
        let tmp = tempfile::tempdir().unwrap();
        // The parent `logs/` dir does not exist yet — `write_crash_entry` must
        // create it (mirroring `<data_dir>/logs/tui-crash.log`), never fail
        // just because nothing has been logged there before.
        let path = tmp.path().join("logs").join("tui-crash.log");
        assert!(!path.exists());

        let entry = format_crash_entry("boom", Some("src/main.rs:1:1"), "<bt>");
        write_crash_entry(&path, &entry).expect("write_crash_entry must create the dir + file");

        let contents = std::fs::read_to_string(&path).expect("the crash log must now exist");
        assert!(!contents.is_empty());
        assert!(contents.contains("boom"));
        assert!(contents.contains("src/main.rs:1:1"));
    }

    #[test]
    fn write_crash_entry_appends_rather_than_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("logs").join("tui-crash.log");

        write_crash_entry(&path, "first entry\n").unwrap();
        write_crash_entry(&path, "second entry\n").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("first entry") && contents.contains("second entry"),
            "a second crash must not erase the first crash's entry, got:\n{contents}"
        );
    }

    #[test]
    fn a_zero_event_catchup_is_treated_as_a_missing_session() {
        // The helper keys off the watermark, not the event vec. An absent session
        // watermarks at 0 and must not be resumed (issue #6 item 6); a live one
        // replays at least its SessionCreated event, so its watermark is >= 1.
        assert!(!catchup_proves_session_exists(&Catchup::Events {
            from: 1,
            through: 0,
            events: vec![],
        }));
        assert!(catchup_proves_session_exists(&Catchup::Events {
            from: 1,
            through: 3,
            events: vec![],
        }));
        // A forward-compat variant we can't inspect is accepted rather than
        // discarding a possibly-resumable session against a newer daemon.
        assert!(catchup_proves_session_exists(&Catchup::Unknown));
    }

    /// A manifest exercising both action kinds and every rendered node field: an
    /// agent step with a skill, an isolated worktree, and a before-write
    /// approval; a tool step with a multi-attempt retry and a dependency.
    const TEST_MANIFEST: &str = "\
schema_version: 1
id: test-workflow
version: 1
orchestration_reason: independent-review
budget:
  maximum_cost_usd: 5.0
  maximum_agents: 2
steps:
  - id: patch
    agent:
      role: implementer
      model_policy: coding
    skill: code.repair
    workspace:
      mode: isolated-worktree
    approval: before-write
    outputs: [proposed_patch]
  - id: verify
    depends_on: [patch]
    tool: repository.test
    retry:
      attempts: 2
      backoff_seconds: 5
    outputs: [test_result]
";

    #[test]
    fn workflow_node_card_renders_agent_and_tool_nodes() {
        let compiled = codypendent_workflow::compile_yaml(TEST_MANIFEST).expect("compiles");
        let label = format!("{} v{}", compiled.id, compiled.version);
        let cards: Vec<_> = compiled
            .nodes
            .iter()
            .map(|node| workflow_node_card(&compiled.id, &label, "—", None, node))
            .collect();

        let patch = cards.iter().find(|c| c.id == "patch").expect("patch node");
        assert_eq!(patch.workflow, "test-workflow v1");
        assert_eq!(patch.kind, "agent");
        assert_eq!(patch.agent, "implementer");
        assert_eq!(patch.model_policy, "coding");
        assert!(
            patch.action.contains("skill code.repair"),
            "{}",
            patch.action
        );
        assert_eq!(patch.workspace, "isolated worktree");
        assert_eq!(patch.approval, "before write");
        // A compiled-but-not-yet-run node (no record) is pending with no cost/error.
        assert_eq!(patch.state, "pending");
        assert_eq!(patch.cost, "\u{2014}");
        assert_eq!(patch.error, "\u{2014}");
        assert_eq!(patch.depends_on, "\u{2014}"); // no dependencies
        assert_eq!(patch.outputs, "proposed_patch");

        let verify = cards
            .iter()
            .find(|c| c.id == "verify")
            .expect("verify node");
        assert_eq!(verify.kind, "tool");
        assert_eq!(verify.agent, "\u{2014}");
        assert_eq!(verify.model_policy, "\u{2014}");
        assert_eq!(verify.action, "tool repository.test");
        assert_eq!(verify.workspace, "shared worktree");
        assert_eq!(verify.approval, "none");
        assert_eq!(verify.retry, "2 attempts \u{b7} 5s backoff");
        assert_eq!(verify.depends_on, "patch");
    }

    /// The T8 seam: a durable node record overlays the compiled defaults — the
    /// graph view renders the node's live state, MEASURED cost (only measured
    /// dimensions), and failure/block reason (P5-D4). This is what turns the
    /// forever-`—` cost column and the reasonless block into real values.
    #[test]
    fn workflow_node_card_renders_a_durable_records_cost_state_and_error() {
        use codypendent_workflow::{NodeState, WorkflowNodeRecord};
        let compiled = codypendent_workflow::compile_yaml(TEST_MANIFEST).expect("compiles");
        let label = format!("{} v{}", compiled.id, compiled.version);
        let node = compiled.node("verify").expect("verify node");

        // A blocked record carrying a measured cost + a budget-block reason.
        let record = WorkflowNodeRecord {
            node_id: "verify".to_owned(),
            state: NodeState::Blocked,
            agent_run_id: None,
            attempt: 1,
            topo_order: node.topo_order,
            cost: Some(serde_json::json!({ "wall_time_secs": 12, "tool_calls": 3 })),
            error: Some("workflow.budget-exceeded: node budget for `tool_calls`".to_owned()),
        };
        let latest = LatestWorkflowRun {
            id: "workflow-run-1".to_owned(),
            phase: "running".to_owned(),
            nodes: HashMap::from([("verify".to_owned(), record)]),
        };
        let card = workflow_node_card(&compiled.id, &label, "—", Some(&latest), node);
        assert_eq!(card.state, "blocked");
        assert_eq!(card.cost, "12s \u{b7} 3 tool calls");
        assert!(card.error.contains("budget"), "error: {}", card.error);

        // A cost with only a tool-call count renders just that (singular form),
        // never a fabricated wall-time or token/USD figure.
        assert_eq!(
            render_node_cost(&serde_json::json!({ "tool_calls": 1 })),
            "1 tool call"
        );
        // An unrecognized / empty cost shape renders "—" (nothing was measured).
        assert_eq!(render_node_cost(&serde_json::json!({})), "\u{2014}");
    }

    #[tokio::test]
    async fn load_workflows_compiles_manifests_and_skips_the_uncompilable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // A fresh install still exposes the embedded repair workflow.
        let built_in = load_workflows(repo, None, None, &mut Vec::new()).await;
        assert!(built_in
            .iter()
            .any(|card| card.workflow_id == "repair-github-check"));

        let dir = repo.join(".codypendent").join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("repair.yaml"), TEST_MANIFEST).unwrap();
        // A manifest that parses but fails to compile (no steps) is skipped, not
        // fatal — the good workflow still loads.
        std::fs::write(
            dir.join("broken.yaml"),
            "schema_version: 1\nid: broken\nversion: 1\nsteps: []\n",
        )
        .unwrap();
        // A non-manifest file is ignored by extension.
        std::fs::write(dir.join("notes.txt"), "ignore me").unwrap();

        let mut warnings = Vec::new();
        let cards = load_workflows(repo, None, None, &mut warnings).await;
        assert_eq!(
            cards.len(),
            7,
            "five built-in nodes plus both nodes of the good manifest"
        );
        let test_cards: Vec<_> = cards
            .iter()
            .filter(|card| card.workflow_id == "test-workflow")
            .collect();
        assert!(test_cards.iter().all(|c| c.workflow == "test-workflow v1"));
        // Nodes keep their compiled topological order.
        assert_eq!(test_cards[0].id, "patch");
        assert_eq!(test_cards[1].id, "verify");
        // The skipped manifest left a diagnostic, not a stderr print.
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(
            warnings[0].contains("broken") && warnings[0].contains("does not compile"),
            "the diagnostic names the skipped manifest: {warnings:?}"
        );
    }

    #[test]
    fn blackboard_item_card_renders_opaque_payload_and_provenance() {
        use codypendent_workflow::{BlackboardItem, BlackboardKind};
        use serde_json::json;

        let item = BlackboardItem {
            id: "0192-abc".to_owned(),
            kind: BlackboardKind::Finding,
            payload: json!({ "summary": "off-by-one in paginate()", "detail": "…" }),
            author: json!({ "role": "investigator", "run": "r1" }),
            confidence: Some(0.85),
            evidence: vec![json!({ "artifact": "a1" }), json!({ "artifact": "a2" })],
            revision: 1,
            superseded_by: None,
            board: Default::default(),
        };
        let card = blackboard_item_card(
            "workflow-run-1",
            "repair-github-check \u{b7} run 0192abcd",
            &item,
        );
        assert_eq!(card.kind, "finding");
        assert_eq!(card.summary, "off-by-one in paginate()");
        assert_eq!(card.author, "agent investigator");
        assert_eq!(card.confidence, "0.85");
        assert_eq!(card.evidence, "2 ref(s)");
        assert_eq!(card.revision, "r1");
        assert!(!card.superseded);
    }

    #[test]
    fn json_summaries_fall_back_gracefully() {
        use codypendent_workflow::{BlackboardItem, BlackboardKind};
        use serde_json::json;

        // A string payload is used verbatim; an object without a known text field
        // falls back to compact JSON rather than panicking.
        assert_eq!(summarize_json(&json!("plain text")), "plain text");
        assert!(summarize_json(&json!({ "x": 1 })).contains("\"x\""));
        assert!(summarize_author(&json!({ "who": "?" })).contains("who"));

        // A superseded hypothesis with no confidence or evidence renders em dashes.
        let item = BlackboardItem {
            id: "1".to_owned(),
            kind: BlackboardKind::Hypothesis,
            payload: json!("a guess"),
            author: json!("someone"),
            confidence: None,
            evidence: vec![],
            revision: 3,
            superseded_by: Some("2".to_owned()),
            board: Default::default(),
        };
        let card = blackboard_item_card("workflow-run-1", "run", &item);
        assert_eq!(card.summary, "a guess");
        assert_eq!(card.author, "someone");
        assert_eq!(card.confidence, "\u{2014}");
        assert_eq!(card.evidence, "\u{2014}");
        assert_eq!(card.revision, "r3");
        assert!(card.superseded);
    }

    // ----------------------------------------------------------------------
    // GapTracker — the reconnect / gap-repair state machine (C6 + FP-2).
    //
    // This is the code that keeps a lagged client from losing an event the
    // daemon dropped from its live fan-out — worst case an `ApprovalRequested`.
    // It had zero tests; these drive the pure decision unit directly.
    // ----------------------------------------------------------------------

    use codypendent_protocol::{Actor, EventBody, ProposedAction, Risk, RiskLevel};

    /// A benign live event at `sequence` (body content is irrelevant to the
    /// tracker, which orders purely by sequence).
    fn ev(sequence: u64) -> SessionEvent {
        SessionEvent {
            sequence,
            occurred_at: chrono::Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: EventBody::NoteAppended {
                text: format!("event {sequence}"),
                run_id: None,
            },
        }
    }

    #[test]
    fn reconnect_catchup_drops_events_delivered_by_old_reader_during_prepare() {
        let mut state = AppState::default();
        let already_live = ApprovalId::new();
        let newly_replayed = ApprovalId::new();
        reduce(
            &mut state,
            Action::DaemonEvent(Box::new(approval_ev(5, already_live))),
        );
        let watermark = fold_reconnect_catchup(
            &mut state,
            Catchup::Events {
                from: 3,
                through: 6,
                events: vec![
                    approval_ev(4, ApprovalId::new()),
                    approval_ev(5, already_live),
                    approval_ev(6, newly_replayed),
                ],
            },
            5,
        );

        assert_eq!(watermark, 6);
        let approvals = state
            .pending_approvals
            .iter()
            .map(|approval| approval.approval_id)
            .collect::<Vec<_>>();
        assert_eq!(approvals, vec![already_live, newly_replayed]);
    }

    /// An `ApprovalRequested` event at `sequence` carrying `approval_id` — the
    /// event whose loss under lag the whole repair path exists to prevent.
    fn approval_ev(sequence: u64, approval_id: ApprovalId) -> SessionEvent {
        SessionEvent {
            sequence,
            occurred_at: chrono::Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: EventBody::ApprovalRequested {
                approval_id,
                action: ProposedAction::ReadFiles {
                    paths: vec!["src/lib.rs".to_owned()],
                },
                risk: Risk {
                    level: RiskLevel::Low,
                    reasons: Vec::new(),
                },
                pattern: None,
            },
        }
    }

    fn seqs(events: &[SessionEvent]) -> Vec<u64> {
        events.iter().map(|e| e.sequence).collect()
    }

    /// An in-order event is applied and advances the watermark; a duplicate of
    /// an already-folded event is ignored (catch-up + live overlap).
    #[test]
    fn in_order_events_apply_and_duplicates_are_ignored() {
        let mut t = GapTracker::new(5);
        let now = Instant::now();

        match t.on_event(ev(6), now) {
            GapAction::Apply(e) => assert_eq!(e.sequence, 6),
            other => panic!("expected Apply(6), got {other:?}"),
        }
        assert_eq!(t.last_seen, 6);
        // A stale re-delivery of an already-folded sequence folds nothing.
        assert!(matches!(t.on_event(ev(4), now), GapAction::Ignore));
        assert!(matches!(t.on_event(ev(6), now), GapAction::Ignore));
        assert_eq!(t.last_seen, 6);
    }

    /// Behaviour 1: a detected gap re-attaches from `last_seen` — the watermark
    /// BEFORE the gap-revealing event, never the gap event's own sequence.
    #[test]
    fn detected_gap_reattaches_from_last_seen_not_the_gap_event() {
        let mut t = GapTracker::new(5);
        let now = Instant::now();

        // Expected next is 6; 8 arrives → a 6..=7 gap.
        match t.on_event(ev(8), now) {
            GapAction::Reattach { last_seen_sequence } => {
                assert_eq!(
                    last_seen_sequence, 5,
                    "must replay from the pre-gap watermark, not the gap event"
                );
            }
            other => panic!("expected Reattach, got {other:?}"),
        }
        // The watermark did NOT advance to the gap event (the C6 invariant), and
        // the gap event is held for after the replay.
        assert_eq!(t.last_seen, 5);
        assert_eq!(seqs(&t.gap_buffer), vec![8]);
    }

    /// Behaviour 2 + 3: events that arrive live during the repair are buffered,
    /// then applied after the catch-up in ascending order, deduped against the
    /// watermark — none dropped (the original C6 bug dropped them), none
    /// duplicated.
    #[test]
    fn buffered_events_apply_in_order_after_repair_without_loss_or_dup() {
        let mut t = GapTracker::new(5);
        let now = Instant::now();

        // Gap at 8 opens the repair.
        assert!(matches!(t.on_event(ev(8), now), GapAction::Reattach { .. }));
        // More live events during the repair, out of order and with a duplicate.
        assert!(matches!(t.on_event(ev(10), now), GapAction::Ignore));
        assert!(matches!(t.on_event(ev(9), now), GapAction::Ignore));
        assert!(matches!(t.on_event(ev(8), now), GapAction::Ignore)); // dup of buffered
                                                                      // A stale re-delivery below the watermark is ignored, not buffered.
        assert!(matches!(t.on_event(ev(3), now), GapAction::Ignore));

        // The daemon replayed 6,7 (the harness folds those); catch-up through=7.
        let drain = t.on_catchup(7, now);
        assert_eq!(
            seqs(&drain.apply),
            vec![8, 9, 10],
            "buffered events apply in order, deduped, none lost"
        );
        assert!(drain.reattach.is_none());
        assert_eq!(t.last_seen, 10);
    }

    /// Behaviour 4 (the safety property): an `ApprovalRequested` that fell in
    /// the gap is applied after the repair, never dropped.
    #[test]
    fn an_approval_in_the_gap_is_never_lost() {
        let approval_id = ApprovalId::new();
        let mut t = GapTracker::new(5);
        let now = Instant::now();

        // The gap-revealing event IS the approval (it raced ahead of its span).
        assert!(matches!(
            t.on_event(approval_ev(8, approval_id), now),
            GapAction::Reattach { .. }
        ));
        let drain = t.on_catchup(7, now);
        assert_eq!(drain.apply.len(), 1, "the approval must survive the repair");
        match &drain.apply[0].body {
            EventBody::ApprovalRequested {
                approval_id: got, ..
            } => assert_eq!(*got, approval_id),
            other => panic!("the approval was lost or corrupted: {other:?}"),
        }
    }

    /// The C6 fix summary "buffers mid-repair events and re-repairs": if the
    /// catch-up did not reach the buffered tail (more loss occurred while
    /// repairing), the tracker applies what it can and asks to repair again,
    /// keeping the tail — still no loss, still in order.
    #[test]
    fn a_hole_in_the_buffered_tail_triggers_another_repair() {
        let mut t = GapTracker::new(5);
        let now = Instant::now();

        assert!(matches!(t.on_event(ev(8), now), GapAction::Reattach { .. }));
        // Further loss: 12 arrives with 9..=11 still missing.
        assert!(matches!(t.on_event(ev(12), now), GapAction::Ignore));

        // First catch-up only reached 7. Buffer is [8, 12].
        let drain = t.on_catchup(7, now);
        assert_eq!(
            seqs(&drain.apply),
            vec![8],
            "8 folds, 12 is still out of order"
        );
        assert_eq!(
            drain.reattach,
            Some(8),
            "re-repair from 8, keeping the tail"
        );
        assert_eq!(seqs(&t.gap_buffer), vec![12]);

        // Second catch-up fills 9,10,11 → through=11; 12 now folds.
        let drain2 = t.on_catchup(11, now);
        assert_eq!(seqs(&drain2.apply), vec![12]);
        assert!(drain2.reattach.is_none());
        assert_eq!(t.last_seen, 12);
    }

    /// FP-2a: the buffer is bounded. Once it fills during a repair, the next
    /// event drops the incremental replay and re-attaches afresh from the
    /// watermark — failing toward a fresh catch-up, never toward unbounded
    /// memory. The ledger re-delivers the whole span, so nothing is lost.
    #[test]
    fn a_full_gap_buffer_reattaches_fresh_instead_of_growing() {
        let mut t = GapTracker::new(5);
        let now = Instant::now();

        assert!(matches!(t.on_event(ev(8), now), GapAction::Reattach { .. }));

        // Feed distinct later sequences until the buffer overflows. The range is
        // generous enough (cap + slack) that the overflow must occur within it.
        let mut overflowed = false;
        for seq in 9..=(9 + MAX_GAP_BUFFER as u64 + 5) {
            match t.on_event(ev(seq), now) {
                GapAction::Ignore => {}
                GapAction::Reattach { last_seen_sequence } => {
                    assert_eq!(last_seen_sequence, 5, "overflow replays from the watermark");
                    overflowed = true;
                    break;
                }
                other => panic!("unexpected action during buffering: {other:?}"),
            }
        }
        assert!(
            overflowed,
            "the buffer must overflow into a fresh re-attach, not grow past the cap"
        );
        assert!(t.gap_buffer.len() <= MAX_GAP_BUFFER);
        assert!(
            t.gap_buffer.is_empty(),
            "the stale buffer is dropped on overflow"
        );
        assert!(t.repairing, "still awaiting the fresh catch-up");
    }

    /// FP-2b: a repair whose catch-up reply never arrives is abandoned once the
    /// deadline passes — `on_tick` asks for a fresh re-attach from the
    /// watermark instead of wedging the client in `repairing` forever.
    #[test]
    fn a_stalled_repair_times_out_into_a_fresh_reattach() {
        let mut t = GapTracker::new(5);
        let t0 = Instant::now();

        assert!(matches!(t.on_event(ev(8), t0), GapAction::Reattach { .. }));
        // Before the deadline: nothing.
        assert!(t.on_tick(t0 + Duration::from_millis(1)).is_none());
        assert!(t
            .on_tick(t0 + REPAIR_TIMEOUT - Duration::from_millis(1))
            .is_none());
        // Past the deadline: re-attach from the watermark, dropping the stale
        // buffer.
        assert_eq!(
            t.on_tick(t0 + REPAIR_TIMEOUT + Duration::from_millis(1)),
            Some(5)
        );
        assert!(t.gap_buffer.is_empty());

        // A tracker that is not repairing never fires a timeout.
        let mut idle = GapTracker::new(5);
        assert!(idle
            .on_tick(Instant::now() + Duration::from_secs(3600))
            .is_none());
    }

    /// FP-2c: sequence 0 is a sentinel (the daemon numbers events 1-based), so
    /// it is folded straight through in any state — including mid-repair, where
    /// it was previously buffered and then silently discarded — and it never
    /// moves the watermark or disturbs the gap buffer.
    #[test]
    fn sentinel_sequence_zero_is_applied_in_any_state() {
        let now = Instant::now();

        // Idle: applied, watermark untouched.
        let mut t = GapTracker::new(5);
        match t.on_event(ev(0), now) {
            GapAction::Apply(e) => assert_eq!(e.sequence, 0),
            other => panic!("expected Apply(0), got {other:?}"),
        }
        assert_eq!(t.last_seen, 5);

        // Mid-repair: STILL applied immediately (not buffered/discarded), and
        // the real gap buffer is left intact.
        let mut t = GapTracker::new(5);
        assert!(matches!(t.on_event(ev(8), now), GapAction::Reattach { .. }));
        match t.on_event(ev(0), now) {
            GapAction::Apply(e) => assert_eq!(e.sequence, 0),
            other => panic!("a sentinel must fold through mid-repair, got {other:?}"),
        }
        assert_eq!(seqs(&t.gap_buffer), vec![8]);
        assert_eq!(t.last_seen, 5);
    }

    /// A zero attach-watermark (an empty catch-up baseline) accepts the first
    /// live event without gap detection — there is no baseline to gap against.
    #[test]
    fn a_zero_watermark_seeds_from_the_first_live_event() {
        let mut t = GapTracker::new(0);
        let now = Instant::now();
        match t.on_event(ev(42), now) {
            GapAction::Apply(e) => assert_eq!(e.sequence, 42),
            other => panic!("expected Apply(42), got {other:?}"),
        }
        assert_eq!(t.last_seen, 42);
        // And now a real gap past that seed is detected.
        assert!(matches!(
            t.on_event(ev(50), now),
            GapAction::Reattach { .. }
        ));
    }

    // -- write_add_model (add-a-usable-model-from-the-TUI, Task 3) ------------

    #[test]
    fn write_add_model_appends_an_entry_that_round_trips_through_load_models() {
        use codypendent_runtime::models::load_models;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        // "groq" is a built-in catalog provider (hosted, api-key).
        write_add_model(
            &paths,
            "groq/llama",
            "groq",
            "llama-3.1-8b",
            Some("sk-secret"),
            None,
        )
        .expect("write_add_model");

        let configs = load_models(&paths.data_dir.join("models.toml")).expect("parse");
        let entry = configs
            .iter()
            .find(|c| c.id.0 == "groq/llama")
            .expect("the entry is present");
        assert_eq!(entry.provider, "openai-compatible");
        assert_eq!(entry.model, "llama-3.1-8b");
        assert!(
            entry.base_url.contains("groq"),
            "base_url comes from the catalog: {}",
            entry.base_url
        );
        assert_eq!(
            entry.api_key_env, "",
            "the key lives in auth.json, not api_key_env"
        );

        // The key landed in auth.json, per model AND provider-wide (so the
        // next model from this provider needs no second paste).
        let auth =
            codypendent_runtime::auth::AuthStore::load(&paths.data_dir).expect("auth.json loads");
        assert_eq!(auth.get("groq/llama"), Some("sk-secret"));
        assert_eq!(auth.get(&provider_auth_id("groq")), Some("sk-secret"));
    }

    /// The HIGH-severity auth-flatten regression, from the add side: the entry
    /// must record which catalog provider it came from, so the runtime sends
    /// that provider's real auth header (`api-key` for Azure OpenAI) instead
    /// of a hardcoded bearer. Without this the add "succeeds" and every run
    /// 401s.
    #[test]
    fn write_add_model_records_the_provider_so_azure_auth_survives() {
        use codypendent_providers::{AuthMethod, Catalog};
        use codypendent_runtime::models::load_models;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.data_dir).expect("create data dir");

        // The built-in `azure-openai` entry declares the non-bearer header but
        // deliberately carries no base_url (it is per-resource), so a real user
        // sets theirs in `providers.toml` — exactly what this fixture does. The
        // built-in's header is asserted so this fails loudly if it ever changes.
        let builtin = Catalog::builtin();
        assert!(
            builtin
                .get("azure-openai")
                .expect("azure-openai is a built-in provider")
                .auth
                .iter()
                .any(|method| matches!(
                    method,
                    AuthMethod::ApiKey { header, .. } if header == "api-key"
                )),
            "the azure-openai catalog entry must declare the `api-key` header"
        );
        std::fs::write(
            paths.data_dir.join("providers.toml"),
            br#"
[[provider]]
id = "azure-openai"
name = "Azure OpenAI (my resource)"
protocol = "openai-chat"
base_url = "https://my-resource.openai.azure.com/openai/v1/"
[[provider.auth]]
kind = "api_key"
env = ["AZURE_OPENAI_API_KEY"]
header = "api-key"
prefix = ""
"#,
        )
        .expect("seed providers.toml");

        write_add_model(
            &paths,
            "azure-openai/gpt-5.1",
            "azure-openai",
            "gpt-5.1",
            Some("azure-secret"),
            None,
        )
        .expect("write_add_model");

        let configs = load_models(&paths.data_dir.join("models.toml")).expect("parse");
        let entry = configs
            .iter()
            .find(|c| c.id.0 == "azure-openai/gpt-5.1")
            .expect("the entry is present");
        assert_eq!(
            entry.provider_id.as_deref(),
            Some("azure-openai"),
            "the provider id is what the runtime resolves the `api-key` header from"
        );
        assert_eq!(
            entry.base_url, "https://my-resource.openai.azure.com/openai/v1",
            "the catalog's trailing slash is normalized on persist"
        );
    }

    /// A catalog base URL written with a trailing slash would otherwise reach
    /// the chat client as `…/v1//chat/completions`.
    #[test]
    fn normalize_base_url_trims_trailing_slashes() {
        assert_eq!(
            normalize_base_url("https://api.tokenfactory.nebius.com/v1/"),
            "https://api.tokenfactory.nebius.com/v1"
        );
        assert_eq!(
            normalize_base_url("  http://localhost:11434/v1  "),
            "http://localhost:11434/v1"
        );
    }

    /// The context window the picker showed is what gets persisted, so the
    /// context gauge and the `num_ctx` hint work from the first run.
    #[test]
    fn write_add_model_persists_the_context_window_it_was_given() {
        use codypendent_runtime::models::load_models;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        write_add_model(
            &paths,
            "groq/llama",
            "groq",
            "llama-3.1-8b",
            None,
            Some(131_072),
        )
        .expect("write");
        let configs = load_models(&paths.data_dir.join("models.toml")).expect("parse");
        assert_eq!(configs[0].context_tokens, Some(131_072));
    }

    /// The merge is what makes a catalog-only provider usable and a bare live
    /// listing informative: live rows keep their order and gain the catalog's
    /// metadata, catalog-only models follow as unconfirmed rows, and anything
    /// the provider itself stated wins over the catalog.
    #[test]
    fn merge_catalog_rows_enriches_live_rows_and_appends_catalog_only_ones() {
        let live = vec![
            AddModelRow::live("llama-3.1-8b"),
            AddModelRow {
                id: "llama-3.3-70b".to_owned(),
                name: None,
                context_tokens: Some(999),
                cost_per_1m_input_usd: None,
                cost_per_1m_output_usd: None,
                live: true,
            },
        ];
        let catalog = vec![
            AddModelRow {
                id: "llama-3.1-8b".to_owned(),
                name: Some("Llama 3.1 8B".to_owned()),
                context_tokens: Some(128_000),
                cost_per_1m_input_usd: Some(0.05),
                cost_per_1m_output_usd: Some(0.08),
                live: false,
            },
            AddModelRow {
                id: "llama-3.3-70b".to_owned(),
                name: Some("Llama 3.3 70B".to_owned()),
                context_tokens: Some(128_000),
                cost_per_1m_input_usd: None,
                cost_per_1m_output_usd: None,
                live: false,
            },
            AddModelRow {
                id: "not-listed-today".to_owned(),
                name: Some("Retired?".to_owned()),
                context_tokens: Some(8_192),
                cost_per_1m_input_usd: None,
                cost_per_1m_output_usd: None,
                live: false,
            },
        ];
        let merged = merge_catalog_rows(live, &catalog);
        assert_eq!(
            merged.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["llama-3.1-8b", "llama-3.3-70b", "not-listed-today"],
            "live order first, catalog-only rows appended"
        );
        assert_eq!(merged[0].name.as_deref(), Some("Llama 3.1 8B"));
        assert_eq!(merged[0].context_tokens, Some(128_000));
        assert!(merged[0].live);
        assert_eq!(
            merged[1].context_tokens,
            Some(999),
            "what the provider itself said is never overwritten by the catalog"
        );
        assert!(
            !merged[2].live,
            "a model the provider did not list stays marked unconfirmed"
        );
    }

    /// The discovery cache: a live listing is written, read back as live rows,
    /// and labelled with its age — the instant seed the next add gets.
    #[test]
    fn model_list_cache_round_trips_with_an_age_label() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_path_buf();
        let rows = vec![
            AddModelRow {
                id: "llama-3.1-8b".to_owned(),
                name: Some("Llama 3.1 8B".to_owned()),
                context_tokens: Some(128_000),
                cost_per_1m_input_usd: Some(0.05),
                cost_per_1m_output_usd: None,
                live: true,
            },
            // Catalog-only rows are not cached: the cache records what the
            // provider actually served.
            AddModelRow {
                id: "catalog-only".to_owned(),
                name: None,
                context_tokens: None,
                cost_per_1m_input_usd: None,
                cost_per_1m_output_usd: None,
                live: false,
            },
        ];
        write_model_list_cache(&data_dir, "groq", &rows);
        assert!(model_list_cache_path(&data_dir, "groq").exists());

        let (cached, age) = read_model_list_cache(&data_dir, "groq").expect("a cache was written");
        assert_eq!(cached.len(), 1, "only the live rows are cached");
        assert_eq!(cached[0].id, "llama-3.1-8b");
        assert_eq!(cached[0].context_tokens, Some(128_000));
        assert!(cached[0].live, "cached rows were live when written");
        assert_eq!(age, "just now");

        // A provider with no cache is simply `None` — never an error.
        assert!(read_model_list_cache(&data_dir, "nebius").is_none());
    }

    /// A provider id from a user-editable `providers.toml` must never escape
    /// the cache directory.
    #[test]
    fn model_list_cache_path_neutralizes_path_separators() {
        let path = model_list_cache_path(Path::new("/data"), "../../etc/passwd");
        assert_eq!(
            path,
            Path::new("/data/model_lists/______etc_passwd.json"),
            "no path traversal survives into the cache file name"
        );
    }

    #[test]
    fn cache_age_label_reads_in_human_units() {
        let now = 10_000_000;
        assert_eq!(cache_age_label(now, now), "just now");
        assert_eq!(cache_age_label(now - 240, now), "4m ago");
        assert_eq!(cache_age_label(now - 7_200, now), "2h ago");
        assert_eq!(cache_age_label(now - 172_800, now), "2d ago");
        assert_eq!(
            cache_age_label(now + 600, now),
            "just now",
            "a clock that moved backwards never reads as a negative age"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_add_model_stores_the_key_at_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        write_add_model(
            &paths,
            "groq/llama",
            "groq",
            "llama-3.1-8b",
            Some("sk-secret"),
            None,
        )
        .expect("write");
        let meta = std::fs::metadata(paths.data_dir.join("auth.json")).expect("metadata");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn write_remove_model_is_exact_and_preserves_comments_and_unrelated_settings() {
        use codypendent_runtime::auth::AuthStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::write(
            paths.data_dir.join("models.toml"),
            r#"# removed with the profile below
[[model]]
id = "remove/me"
provider = "openai-compatible"
base_url = "https://example.test/v1"
model = "old"

# keep this model comment
[[model]]
id = "keep/me"
provider = "openai-compatible"
base_url = "https://example.test/v1"
model = "new"

# keep this operator note
[voice]
play_command = ["mpv", "-"]

[retrieval]
mcp_top_k = 7

[future_settings]
enabled = true
"#,
        )
        .expect("write models");
        let mut auth = AuthStore::default();
        auth.set("remove/me", "secret-to-remove");
        auth.set("keep/me", "secret-to-keep");
        auth.set("provider/acme", "provider-wide-stays");
        auth.save(&paths.data_dir).expect("write auth");

        write_remove_model(&paths, "remove/me").expect("remove exact model");

        let raw = std::fs::read_to_string(paths.data_dir.join("models.toml")).expect("read models");
        assert!(!raw.contains("remove/me"));
        assert!(raw.contains("keep/me"));
        assert!(raw.contains("# keep this operator note"));
        assert!(raw.contains("# keep this model comment"));
        assert!(raw.contains("[voice]"));
        assert!(raw.contains("[retrieval]"));
        assert!(raw.contains("[future_settings]"));

        let auth = AuthStore::load(&paths.data_dir).expect("read auth");
        assert_eq!(auth.get("remove/me"), None);
        assert_eq!(auth.get("keep/me"), Some("secret-to-keep"));
        assert_eq!(auth.get("provider/acme"), Some("provider-wide-stays"));
    }

    #[test]
    fn write_remove_model_rejects_an_unknown_id_without_changing_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        let original = r#"# untouched
[[model]]
id = "keep/me"
provider = "acp"
model = "keep-agent"
"#;
        std::fs::write(paths.data_dir.join("models.toml"), original).expect("write models");

        let error = write_remove_model(&paths, "missing").expect_err("unknown id is rejected");
        assert!(error.to_string().contains("not configured"));
        assert_eq!(
            std::fs::read_to_string(paths.data_dir.join("models.toml")).expect("read models"),
            original
        );
    }

    /// A hand-edited `models.toml` can list the same id twice. `auth.remove` is
    /// unconditional, so removing only the FIRST match would leave a listed
    /// model with no credential. Every match goes, and an emptied array is
    /// dropped rather than left as `model = []`.
    #[test]
    fn write_remove_model_removes_every_duplicate_id_and_drops_the_emptied_array() {
        use codypendent_runtime::auth::AuthStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::write(
            paths.data_dir.join("models.toml"),
            r#"[[model]]
id = "dup/me"
provider = "openai-compatible"
base_url = "https://example.test/v1"
model = "first"

[[model]]
id = "dup/me"
provider = "openai-compatible"
base_url = "https://example.test/v1"
model = "second"

[retrieval]
mcp_top_k = 7
"#,
        )
        .expect("write models");
        let mut auth = AuthStore::default();
        auth.set("dup/me", "secret-to-remove");
        auth.save(&paths.data_dir).expect("write auth");

        write_remove_model(&paths, "dup/me").expect("remove every duplicate");

        let raw = std::fs::read_to_string(paths.data_dir.join("models.toml")).expect("read models");
        assert!(!raw.contains("dup/me"), "no duplicate may survive: {raw}");
        assert!(
            !raw.contains("[[model]]") && !raw.contains("model = ["),
            "an emptied model array is removed, not left behind: {raw}"
        );
        assert!(raw.contains("[retrieval]"), "unrelated tables survive");
        let auth = AuthStore::load(&paths.data_dir).expect("read auth");
        assert_eq!(auth.get("dup/me"), None);
    }

    /// The removal is all-or-nothing across BOTH files. When the key store
    /// cannot be written, `models.toml` must still list the model (its temp
    /// discarded, never renamed into place) and `auth.json` must still hold the
    /// key — never the half-applied state where the entry is gone but the
    /// secret remains. `AuthStore::save` is forced to fail by pre-creating its
    /// temp path as a directory.
    #[test]
    fn write_remove_model_leaves_both_files_untouched_when_the_key_store_cannot_be_saved() {
        use codypendent_runtime::auth::AuthStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        let original = r#"[[model]]
id = "remove/me"
provider = "openai-compatible"
base_url = "https://example.test/v1"
model = "old"
"#;
        std::fs::write(paths.data_dir.join("models.toml"), original).expect("write models");
        let mut auth = AuthStore::default();
        auth.set("remove/me", "secret-to-remove");
        auth.save(&paths.data_dir).expect("write auth");

        let auth_tmp = paths
            .data_dir
            .join(format!(".auth-{}.json.tmp", std::process::id()));
        std::fs::create_dir(&auth_tmp).expect("block the auth temp path");

        let error =
            write_remove_model(&paths, "remove/me").expect_err("the key store write must fail");
        assert!(
            error.to_string().contains("auth.json"),
            "the failure names the key store: {error}"
        );

        assert_eq!(
            std::fs::read_to_string(paths.data_dir.join("models.toml")).expect("read models"),
            original,
            "models.toml is unchanged when the key store write fails"
        );
        let auth = AuthStore::load(&paths.data_dir).expect("read auth");
        assert_eq!(
            auth.get("remove/me"),
            Some("secret-to-remove"),
            "the credential is unchanged when the key store write fails"
        );
        let models_tmp = paths
            .data_dir
            .join(format!(".models-{}.toml.tmp", std::process::id()));
        assert!(
            !models_tmp.exists(),
            "the models temp is cleaned up on every failure path"
        );
    }

    #[test]
    fn write_add_model_for_a_local_provider_writes_no_key() {
        use codypendent_runtime::models::load_models;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        // "ollama" is a built-in LOCAL provider (auth none) — no key entered.
        write_add_model(
            &paths,
            "ollama/qwen",
            "ollama",
            "qwen2.5-coder:14b",
            None,
            None,
        )
        .expect("write");

        let configs = load_models(&paths.data_dir.join("models.toml")).expect("parse");
        assert!(configs.iter().any(|c| c.id.0 == "ollama/qwen"));
        assert!(
            !paths.data_dir.join("auth.json").exists(),
            "a local add writes no auth.json"
        );
    }

    /// Hard requirement: an empty (or whitespace-only) key must never reach
    /// `AuthStore::set` — storing `set(id, "")` would silently shadow a valid
    /// `api_key_env` into "no key" at resolution time (SDD ledger M1). A blank
    /// key is treated exactly like `None`: no `auth.json` at all.
    #[test]
    fn write_add_model_treats_a_blank_key_as_no_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        write_add_model(
            &paths,
            "groq/llama",
            "groq",
            "llama-3.1-8b",
            Some("   "),
            None,
        )
        .expect("write");

        assert!(
            !paths.data_dir.join("auth.json").exists(),
            "a blank/whitespace-only key must never be written to auth.json"
        );
    }

    #[test]
    fn write_add_model_updates_a_duplicate_display_id_in_place() {
        use codypendent_runtime::models::load_models;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        write_add_model(
            &paths,
            "groq/llama",
            "groq",
            "llama-3.1-8b",
            Some("k1"),
            None,
        )
        .expect("write 1");
        write_add_model(
            &paths,
            "groq/llama",
            "groq",
            "llama-3.3-70b",
            Some("k2"),
            None,
        )
        .expect("write 2");

        let configs = load_models(&paths.data_dir.join("models.toml")).expect("parse");
        let matching: Vec<_> = configs.iter().filter(|c| c.id.0 == "groq/llama").collect();
        assert_eq!(
            matching.len(),
            1,
            "a duplicate display id updates in place, never dupes"
        );
        assert_eq!(
            matching[0].model, "llama-3.3-70b",
            "the entry took the new model"
        );
        assert_eq!(
            codypendent_runtime::auth::AuthStore::load(&paths.data_dir)
                .expect("auth.json loads")
                .get("groq/llama"),
            Some("k2"),
            "the key updated too"
        );
    }

    /// Hard requirement: appending a new model must not clobber an existing
    /// `[[model]]` entry already in `models.toml` — the write goes through the
    /// real loader (parse → dedupe-by-id → serialize), not a blind text append.
    #[test]
    fn write_add_model_preserves_an_existing_entry_when_adding_another() {
        use codypendent_runtime::models::load_models;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        write_add_model(
            &paths,
            "groq/llama",
            "groq",
            "llama-3.1-8b",
            Some("k1"),
            None,
        )
        .expect("write first model");
        write_add_model(
            &paths,
            "ollama/qwen",
            "ollama",
            "qwen2.5-coder:14b",
            None,
            None,
        )
        .expect("write second model");

        let configs = load_models(&paths.data_dir.join("models.toml")).expect("parse");
        assert_eq!(configs.len(), 2, "both entries survive: {configs:?}");
        assert!(configs.iter().any(|c| c.id.0 == "groq/llama"));
        assert!(configs.iter().any(|c| c.id.0 == "ollama/qwen"));
        // The first model's own key is untouched by the second (keyless) add.
        assert_eq!(
            codypendent_runtime::auth::AuthStore::load(&paths.data_dir)
                .expect("auth.json loads")
                .get("groq/llama"),
            Some("k1")
        );
    }

    #[test]
    fn write_add_model_preserves_non_model_configuration_tables() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::write(
            paths.data_dir.join("models.toml"),
            r#"
[voice]
push_to_talk = "F4"

[transcription]
provider = "local"

[embedding]
model = "embed-v1"

[retrieval]
limit = 12
"#,
        )
        .expect("seed settings");

        write_add_model(&paths, "ollama/test", "ollama", "test", None, Some(8192))
            .expect("add model");

        let raw = std::fs::read_to_string(paths.data_dir.join("models.toml")).expect("read models");
        let parsed: toml::Value = raw.parse().expect("valid toml");
        assert_eq!(parsed["voice"]["push_to_talk"].as_str(), Some("F4"));
        assert_eq!(parsed["transcription"]["provider"].as_str(), Some("local"));
        assert_eq!(parsed["embedding"]["model"].as_str(), Some("embed-v1"));
        assert_eq!(parsed["retrieval"]["limit"].as_integer(), Some(12));
        assert_eq!(parsed["model"].as_array().map(Vec::len), Some(1));
    }

    /// Hard requirement: a blank/whitespace-only display id is rejected with a
    /// user-visible error and writes nothing — neither `models.toml` nor
    /// `auth.json` is created.
    #[test]
    fn write_add_model_rejects_a_blank_display_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        let error = write_add_model(
            &paths,
            "   ",
            "groq",
            "llama-3.1-8b",
            Some("sk-secret"),
            None,
        )
        .expect_err("a blank display id must be rejected");
        assert!(
            !error.to_string().is_empty(),
            "the error must carry a user-visible message"
        );
        assert!(
            !paths.data_dir.join("models.toml").exists(),
            "a rejected add writes no models.toml"
        );
        assert!(
            !paths.data_dir.join("auth.json").exists(),
            "a rejected add writes no auth.json"
        );
    }

    /// Hard requirement (M3, all-or-nothing): when a non-blank key is entered,
    /// a pre-existing but CORRUPT `auth.json` must abort the whole add before
    /// anything is written — never leaving a keyless `models.toml` entry
    /// behind. This is why `write_add_model` loads `auth.json` (fallible)
    /// BEFORE writing `models.toml` — see its doc comment.
    #[test]
    fn write_add_model_is_all_or_nothing_when_auth_json_is_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        std::fs::create_dir_all(&paths.data_dir).expect("create data dir");
        std::fs::write(paths.data_dir.join("auth.json"), b"{ not json")
            .expect("seed corrupt auth.json");

        let error = write_add_model(
            &paths,
            "groq/llama",
            "groq",
            "llama-3.1-8b",
            Some("sk-secret"),
            None,
        )
        .expect_err("a corrupt pre-existing auth.json must abort the whole add");
        assert!(
            !error.to_string().is_empty(),
            "the error must carry a user-visible message"
        );
        assert!(
            !paths.data_dir.join("models.toml").exists(),
            "a corrupt auth.json must abort BEFORE models.toml is written (all-or-nothing)"
        );
    }

    // -- write_api_key / load_key_statuses (D1, `/keys`) ----------------------

    #[test]
    fn write_api_key_sets_replaces_and_round_trips_at_mode_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        // An unrelated entry must survive a set (load-before-write).
        write_api_key(
            &paths,
            &KeyTarget::Model("openai/gpt".to_owned()),
            Some("sk-other"),
        )
        .expect("set unrelated");
        write_api_key(
            &paths,
            &KeyTarget::Model("groq/llama".to_owned()),
            Some("sk-first"),
        )
        .expect("set");
        write_api_key(
            &paths,
            &KeyTarget::Model("groq/llama".to_owned()),
            Some("sk-second"),
        )
        .expect("replace");

        let auth =
            codypendent_runtime::auth::AuthStore::load(&paths.data_dir).expect("auth.json loads");
        assert_eq!(auth.get("groq/llama"), Some("sk-second"), "replace wins");
        assert_eq!(
            auth.get("openai/gpt"),
            Some("sk-other"),
            "the unrelated entry survived"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(paths.data_dir.join("auth.json")).expect("metadata");
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn write_api_key_remove_deletes_the_entry_and_an_absent_remove_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        // Removing an absent entry creates no auth.json at all.
        write_api_key(&paths, &KeyTarget::Model("groq/llama".to_owned()), None)
            .expect("absent remove is a no-op");
        assert!(
            !paths.data_dir.join("auth.json").exists(),
            "a no-op remove must not create an empty auth.json"
        );

        write_api_key(
            &paths,
            &KeyTarget::Model("groq/llama".to_owned()),
            Some("sk-secret"),
        )
        .expect("set");
        write_api_key(&paths, &KeyTarget::Model("groq/llama".to_owned()), None).expect("remove");
        let auth =
            codypendent_runtime::auth::AuthStore::load(&paths.data_dir).expect("auth.json loads");
        assert_eq!(auth.get("groq/llama"), None, "the entry is gone");
    }

    #[test]
    fn write_api_key_maps_the_tavily_target_to_the_reserved_id_the_daemon_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());

        write_api_key(&paths, &KeyTarget::Tavily, Some("tvly-saved")).expect("set the Tavily key");
        let auth =
            codypendent_runtime::auth::AuthStore::load(&paths.data_dir).expect("auth.json loads");
        assert_eq!(
            auth.get(codypendent_integrations::search::TAVILY_AUTH_ID),
            Some("tvly-saved"),
            "the Tavily target lands under the reserved id"
        );

        // And the daemon's own discovery (auth.json first, env second) reads
        // exactly this slot — the file wins regardless of the env, so this
        // assertion never touches the process environment.
        let key = codypendent_integrations::search::TavilyKey::discover(&paths.data_dir)
            .expect("the daemon's discovery reads the saved key");
        assert_eq!(key.expose(), "tvly-saved");

        write_api_key(&paths, &KeyTarget::Tavily, None).expect("remove");
        let auth =
            codypendent_runtime::auth::AuthStore::load(&paths.data_dir).expect("auth.json loads");
        assert_eq!(
            auth.get(codypendent_integrations::search::TAVILY_AUTH_ID),
            None,
            "the reserved entry removes too"
        );
    }

    #[test]
    fn write_api_key_rejects_a_blank_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        write_api_key(
            &paths,
            &KeyTarget::Model("groq/llama".to_owned()),
            Some("   "),
        )
        .expect_err("a blank key is rejected (the M1 shadow guard)");
        assert!(
            !paths.data_dir.join("auth.json").exists(),
            "a rejected write creates no auth.json"
        );
    }

    #[test]
    fn write_api_key_aborts_on_a_corrupt_auth_json_without_touching_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.data_dir).expect("create data dir");
        std::fs::write(paths.data_dir.join("auth.json"), b"{ not json")
            .expect("seed corrupt auth.json");

        write_api_key(
            &paths,
            &KeyTarget::Model("groq/llama".to_owned()),
            Some("sk-secret"),
        )
        .expect_err("a corrupt pre-existing auth.json must abort (load-before-write)");
        write_api_key(&paths, &KeyTarget::Model("groq/llama".to_owned()), None)
            .expect_err("a remove aborts too");
        assert_eq!(
            std::fs::read(paths.data_dir.join("auth.json")).expect("read"),
            b"{ not json",
            "the corrupt file is left untouched, never silently replaced"
        );
    }

    #[test]
    fn load_key_statuses_reflects_stored_env_and_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.data_dir).expect("create data dir");
        std::fs::write(
            paths.data_dir.join("models.toml"),
            r#"
[[model]]
id = "groq/llama"
provider = "openai-compatible"
base_url = "https://api.groq.com/openai/v1"
model = "llama-3.1-8b"

[[model]]
id = "openai/gpt"
provider = "openai-compatible"
base_url = "https://api.openai.com/v1"
model = "gpt-5.1-codex"
api_key_env = "OPENAI_API_KEY"

[[model]]
id = "ollama/qwen"
provider = "openai-compatible"
base_url = "http://localhost:11434/v1"
model = "qwen2.5-coder:14b"
"#,
        )
        .expect("write models.toml");

        // Nothing stored yet: env-declared shows the NAME, the rest Missing.
        let (models, _, _) = load_key_statuses(&paths, &mut Vec::new());
        assert_eq!(
            models,
            vec![
                ("groq/llama".to_owned(), KeyStatus::Missing),
                (
                    "openai/gpt".to_owned(),
                    KeyStatus::Env("OPENAI_API_KEY".to_owned())
                ),
                ("ollama/qwen".to_owned(), KeyStatus::Missing),
            ]
        );

        // A stored key beats the env declaration. (The Tavily row lives in
        // `load_key_statuses_tavily_row_mirrors_the_daemon_discovery_precedence`
        // — the ONE test allowed to touch `TAVILY_API_KEY`.)
        write_api_key(
            &paths,
            &KeyTarget::Model("openai/gpt".to_owned()),
            Some("sk-x"),
        )
        .expect("set model key");
        let (models, _, _) = load_key_statuses(&paths, &mut Vec::new());
        assert_eq!(
            models,
            vec![
                ("groq/llama".to_owned(), KeyStatus::Missing),
                ("openai/gpt".to_owned(), KeyStatus::Stored),
                ("ollama/qwen".to_owned(), KeyStatus::Missing),
            ]
        );
    }

    /// `models.toml` with a `[transcription]` table pointing at `base_url`.
    fn seed_transcription_models_toml(paths: &RuntimePaths, base_url: &str, api_key_env: &str) {
        std::fs::create_dir_all(&paths.data_dir).expect("create data dir");
        std::fs::write(
            paths.data_dir.join("models.toml"),
            format!(
                r#"
[[model]]
id = "groq/llama"
provider = "openai-compatible"
base_url = "https://api.groq.com/openai/v1"
model = "llama-3.1-8b"

[transcription]
base_url = "{base_url}"
model = "whisper-large-v3-turbo"
api_key_env = "{api_key_env}"
"#
            ),
        )
        .expect("seed models.toml");
    }

    /// Audio review F3, the whole point of the voice `/keys` rows: the key the
    /// operator types into `/keys` must land under the entry the runtime's
    /// `audio_api_key` actually reads. Nothing downstream of `write_api_key`
    /// validates the entry NAME, so a wrong string here would save happily and
    /// read back as absent — which is why this asserts on the wire a real
    /// `AudioTranscriber` sends, not on the string itself.
    #[tokio::test]
    async fn a_transcription_key_saved_through_keys_is_the_one_the_transcriber_sends() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/audio/transcriptions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "text": "hello" })),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        // No `api_key_env` — the ONLY way a key can reach the endpoint here is
        // the `auth.json` entry `/keys` writes.
        seed_transcription_models_toml(&paths, &server.uri(), "");

        write_api_key(&paths, &KeyTarget::Transcription, Some("sk-stt-from-keys"))
            .expect("save the transcription key");

        let audio = load_audio_models(&paths.data_dir.join("models.toml")).expect("audio tables");
        let auth =
            codypendent_runtime::auth::AuthStore::load(&paths.data_dir).expect("load auth.json");
        let transcriber = codypendent_runtime::models::AudioTranscriber::new(&audio, auth)
            .expect("[transcription] is configured");
        let text = transcriber
            .transcribe(b"fake-wav", "clip.wav", "audio/wav")
            .await
            .expect("transcribe");
        assert_eq!(text, "hello");

        let requests = server.received_requests().await.expect("recorded requests");
        let sent = requests
            .first()
            .expect("the transcriber called the endpoint")
            .headers
            .get("authorization")
            .expect("the resolved key was sent")
            .to_str()
            .expect("ascii header");
        assert_eq!(
            sent, "Bearer sk-stt-from-keys",
            "the /keys write must land under the auth.json entry `audio_api_key` reads"
        );
    }

    /// The projection half: a configured voice table produces a `/keys` row
    /// with the same Stored/Env/Missing precedence a model row gets, and an
    /// UNconfigured one produces none (there is no endpoint to authenticate).
    #[test]
    fn load_key_statuses_projects_a_row_only_for_a_configured_voice_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        seed_transcription_models_toml(&paths, "https://api.groq.com/openai/v1", "GROQ_API_KEY");

        let (_, _, voice) = load_key_statuses(&paths, &mut Vec::new());
        assert_eq!(voice.len(), 1, "[speech] is absent, so it gets no row");
        assert_eq!(voice[0].target, KeyTarget::Transcription);
        assert_eq!(
            voice[0].detail, "whisper-large-v3-turbo · api.groq.com",
            "the row identifies the endpoint compactly, with no key material"
        );
        assert_eq!(
            voice[0].status,
            KeyStatus::Env("GROQ_API_KEY".to_owned()),
            "with no auth.json entry the declared env NAME is shown"
        );

        write_api_key(&paths, &KeyTarget::Transcription, Some("sk-stt")).expect("save");
        let (_, _, voice) = load_key_statuses(&paths, &mut Vec::new());
        assert_eq!(
            voice[0].status,
            KeyStatus::Stored,
            "a saved key outranks the env NAME, exactly as a model row does"
        );

        write_api_key(&paths, &KeyTarget::Transcription, None).expect("remove");
        let (_, _, voice) = load_key_statuses(&paths, &mut Vec::new());
        assert_eq!(voice[0].status, KeyStatus::Env("GROQ_API_KEY".to_owned()));
    }

    #[test]
    fn load_key_statuses_has_no_voice_rows_when_models_toml_configures_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.data_dir).expect("create data dir");
        std::fs::write(
            paths.data_dir.join("models.toml"),
            "[[model]]\nid = \"groq/llama\"\nprovider = \"openai-compatible\"\nbase_url = \
             \"https://api.groq.com/openai/v1\"\nmodel = \"llama-3.1-8b\"\n",
        )
        .expect("seed models.toml");
        let (_, _, voice) = load_key_statuses(&paths, &mut Vec::new());
        assert!(
            voice.is_empty(),
            "voice is opt-in; an unconfigured table must not offer a credential row"
        );
    }

    #[test]
    fn load_key_statuses_tavily_row_mirrors_the_daemon_discovery_precedence() {
        // Every `TAVILY_API_KEY`-touching case lives in this ONE test: the
        // process environment is global mutable state, so two tests racing
        // `set_var`/`remove_var` on the SAME variable would flake (the
        // `search/key.rs` convention). No other test may set the variable or
        // assert on the Tavily row.
        use codypendent_integrations::search::key::TAVILY_API_KEY_ENV;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.data_dir).expect("create data dir");

        // 1. Neither source → Missing.
        std::env::remove_var(TAVILY_API_KEY_ENV);
        let (_, tavily, _) = load_key_statuses(&paths, &mut Vec::new());
        assert_eq!(tavily, KeyStatus::Missing);

        // 2. Env set (no auth.json entry) → Env(NAME) — the variable NAME
        //    only; the value is never read into the projection.
        std::env::set_var(TAVILY_API_KEY_ENV, "tvly-env-key");
        let (_, tavily, _) = load_key_statuses(&paths, &mut Vec::new());
        assert_eq!(tavily, KeyStatus::Env(TAVILY_API_KEY_ENV.to_owned()));

        // 3. A blank env value counts as absent (exactly like `discover`).
        std::env::set_var(TAVILY_API_KEY_ENV, "   ");
        let (_, tavily, _) = load_key_statuses(&paths, &mut Vec::new());
        assert_eq!(tavily, KeyStatus::Missing);

        // 4. A stored entry beats the env — the file wins, exactly like
        //    `TavilyKey::discover`.
        std::env::set_var(TAVILY_API_KEY_ENV, "tvly-env-key");
        write_api_key(&paths, &KeyTarget::Tavily, Some("tvly-stored")).expect("set tavily");
        let (_, tavily, _) = load_key_statuses(&paths, &mut Vec::new());
        assert_eq!(tavily, KeyStatus::Stored);

        std::env::remove_var(TAVILY_API_KEY_ENV);
    }

    #[test]
    fn tavily_key_set_and_remove_apply_without_a_daemon_restart() {
        use codypendent_tui::Overlay;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        let mut state = AppState::new();

        // The daemon's reloading adapter resolves immediately before each call.
        apply_set_api_key(&mut state, &paths, &KeyTarget::Tavily, "tvly-saved");
        assert_eq!(state.overlay, Overlay::None);
        assert_eq!(state.tavily_key_status, KeyStatus::Stored);
        assert!(state.notice.as_ref().unwrap().0.contains("ready"));

        // Removal is equally immediate (an environment fallback may still be
        // reported separately by the status projection).
        apply_remove_api_key(&mut state, &paths, &KeyTarget::Tavily);
        assert_eq!(state.overlay, Overlay::None);
        // Not `Missing` exactly: the sibling test above owns `TAVILY_API_KEY`
        // and sets it while it runs, and the projection reads the process
        // environment, so a concurrent run can legitimately see `Env(_)`
        // here. What removal must guarantee is that the STORED key is gone.
        assert!(
            matches!(
                state.tavily_key_status,
                KeyStatus::Missing | KeyStatus::Env(_)
            ),
            "stored key must be gone after removal: {:?}",
            state.tavily_key_status
        );
        let notice = state
            .notice
            .as_ref()
            .map(|(text, _)| text.as_str())
            .unwrap_or("");
        assert!(
            notice.contains("disabled"),
            "immediate removal notice: {notice}"
        );
    }

    #[tokio::test]
    async fn apply_add_model_refires_the_key_status_projection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        let mut state = AppState::new();

        // "groq" is a built-in catalog provider (hosted, api-key).
        apply_add_model(
            &mut state,
            &paths,
            "groq/llama",
            "groq",
            "llama-3.1-8b",
            Some("sk-secret"),
            None,
        )
        .await;

        // The picker re-seeded…
        assert!(
            state.models.iter().any(|card| card.id.0 == "groq/llama"),
            "the model picker re-seeded"
        );
        // …and the /keys projection re-fired with it — a model added WITH a
        // key shows `Stored` immediately, without a TUI restart.
        assert_eq!(
            state.key_status,
            vec![("groq/llama".to_owned(), KeyStatus::Stored)]
        );
        assert_eq!(
            state.notice.as_ref().map(|(text, _)| text.as_str()),
            Some("added model groq/llama")
        );
    }

    #[tokio::test]
    async fn onboarding_completes_only_after_added_profile_reloads_as_runnable() {
        use codypendent_tui::state::{OnboardFlow, OnboardProviderClass, OnboardStep};

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        let id = ModelId("groq/onboard-model".to_owned());
        let mut state = AppState::new();
        state.onboard_flow = Some(OnboardFlow {
            class: OnboardProviderClass::Hosted,
            provider_id: Some("groq".to_owned()),
            awaiting_model: Some(id.clone()),
            error: None,
        });
        state.overlay = codypendent_tui::Overlay::Onboard {
            step: OnboardStep::Validating {
                model_id: id.clone(),
            },
        };

        apply_add_model(
            &mut state,
            &paths,
            &id.0,
            "groq",
            "llama-3.3-70b-versatile",
            Some("sk-secret"),
            None,
        )
        .await;

        assert!(state.runnable_models.contains(&id));
        assert_eq!(state.pending_model, Some(id));
        assert!(state.onboard_flow.is_none());
        assert_eq!(state.overlay, codypendent_tui::Overlay::None);
        assert!(state
            .drain_outbox()
            .iter()
            .any(|intent| matches!(intent, Intent::SetOnboardComplete)));
    }

    #[tokio::test]
    async fn failed_onboarding_add_stays_inside_onboarding() {
        use codypendent_tui::state::{OnboardFlow, OnboardProviderClass, OnboardStep};

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::write(paths.data_dir.join("auth.json"), b"not-json")
            .expect("corrupt auth fixture");
        let id = ModelId("groq/onboard-model".to_owned());
        let mut state = AppState::new();
        state.onboard_flow = Some(OnboardFlow {
            class: OnboardProviderClass::Hosted,
            provider_id: Some("groq".to_owned()),
            awaiting_model: Some(id.clone()),
            error: None,
        });
        state.overlay = codypendent_tui::Overlay::Onboard {
            step: OnboardStep::Validating {
                model_id: id.clone(),
            },
        };

        apply_add_model(
            &mut state,
            &paths,
            &id.0,
            "groq",
            "llama-3.3-70b-versatile",
            Some("sk-secret"),
            None,
        )
        .await;

        assert!(!matches!(state.overlay, codypendent_tui::Overlay::None));
        assert!(state.pending_model.is_none());
        assert!(!paths.data_dir.join("models.toml").exists());
        assert!(!state
            .drain_outbox()
            .iter()
            .any(|intent| matches!(intent, Intent::SetOnboardComplete)));
    }

    #[test]
    fn boot_warnings_survive_the_watch_channel_and_notice_post_boot() {
        // What `warn_stage` does: the stage channel keeps only the LATEST
        // value — the very next stage overwrites a `Reconciling` warning —
        // while the shared vec keeps every one.
        let (stage_tx, stage_rx) = watch::channel(SplashStage::StartingDaemon);
        let warnings: BootWarnings = BootWarnings::default();
        let warn_stage = |message: &str| {
            push_boot_warning(&warnings, message.to_owned());
            let _ = stage_tx.send(SplashStage::Reconciling(message.to_owned()));
        };
        warn_stage("daemon build mismatch; continuing on the running build");
        warn_stage("restart refused: 2 active run(s)");
        let _ = stage_tx.send(SplashStage::RestoringSession);

        // The channel retained only the overwrite…
        assert_eq!(stage_rx.borrow().text(), "restoring session…");
        // …but the vec kept both warnings, and draining persists both in the
        // diagnostics centre instead of losing all but the final transient.
        let mut state = AppState::new();
        drain_boot_warnings(&mut state, &warnings);
        assert!(
            warnings.lock().expect("poisoned").is_empty(),
            "drained exactly once"
        );
        assert_eq!(state.issues.len(), 2);
        assert!(state
            .issues
            .iter()
            .any(|issue| issue == "daemon build mismatch; continuing on the running build"));
        assert!(state
            .issues
            .iter()
            .any(|issue| issue == "restart refused: 2 active run(s)"));
    }

    // -- provider_requires_key (Task 8 add-model key step derivation) --------

    /// A minimal `Provider` for exercising `provider_requires_key` directly —
    /// every field but `auth` is irrelevant to that derivation.
    fn provider_with_auth(
        auth: Vec<codypendent_providers::AuthMethod>,
    ) -> codypendent_providers::Provider {
        codypendent_providers::Provider {
            id: "test-provider".to_string(),
            name: "Test Provider".to_string(),
            protocol: codypendent_providers::Protocol::OpenAiChat,
            base_url: None,
            auth,
            extra_headers: Default::default(),
            query_params: Default::default(),
            local: false,
        }
    }

    #[test]
    fn provider_requires_key_is_true_when_first_auth_is_api_key() {
        use codypendent_providers::AuthMethod;
        let p = provider_with_auth(vec![AuthMethod::ApiKey {
            env: vec!["GROQ_API_KEY".to_string()],
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
        }]);
        assert!(provider_requires_key(&p));
    }

    #[test]
    fn provider_requires_key_is_false_when_first_auth_is_none() {
        use codypendent_providers::AuthMethod;
        let p = provider_with_auth(vec![AuthMethod::None]);
        assert!(!provider_requires_key(&p));
    }

    #[test]
    fn provider_requires_key_is_false_for_acp_cloud_iam_and_oauth() {
        use codypendent_providers::AuthMethod;

        let acp = provider_with_auth(vec![AuthMethod::Acp {
            command: "gemini".to_string(),
            args: vec!["--acp".to_string()],
            env: Default::default(),
        }]);
        assert!(!provider_requires_key(&acp));

        let cloud_iam = provider_with_auth(vec![AuthMethod::CloudIam {
            variant: "aws_sigv4".to_string(),
            env: Default::default(),
            scopes: vec![],
        }]);
        assert!(!provider_requires_key(&cloud_iam));

        let oauth = provider_with_auth(vec![AuthMethod::OAuth {
            authorize_url: "https://example.com/authorize".to_string(),
            token_url: "https://example.com/token".to_string(),
            client_id: "client".to_string(),
            scopes: vec![],
            pkce: true,
        }]);
        assert!(!provider_requires_key(&oauth));
    }

    #[test]
    fn provider_requires_key_is_false_for_an_empty_auth_list() {
        let p = provider_with_auth(vec![]);
        assert!(!provider_requires_key(&p));
    }

    #[test]
    fn provider_key_resolution_prefers_auth_then_non_blank_environment() {
        use codypendent_runtime::auth::AuthStore;

        const ENV: &str = "CODYPENDENT_TEST_PROVIDER_CARD_KEY_43ca";
        std::env::set_var(ENV, "from-environment");
        let env_names = vec![ENV.to_owned()];

        let empty = AuthStore::default();
        assert_eq!(
            resolve_provider_api_key("test", &empty, &env_names).as_deref(),
            Some("from-environment")
        );
        assert!(provider_has_resolvable_key("test", &empty, &env_names));

        let mut stored = AuthStore::default();
        stored.set(provider_auth_id("test"), "from-auth-json");
        assert_eq!(
            resolve_provider_api_key("test", &stored, &env_names).as_deref(),
            Some("from-auth-json")
        );

        let mut blank = AuthStore::default();
        blank.set(provider_auth_id("test"), "   ");
        assert_eq!(
            resolve_provider_api_key("test", &blank, &env_names).as_deref(),
            Some("from-environment"),
            "a blank stored value must not shadow a usable environment key"
        );
        std::env::remove_var(ENV);
    }

    /// One-request server for asserting discovery headers without involving a
    /// real provider or ever printing a credential.
    async fn capture_model_list_request() -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 8192];
            let n = stream.read(&mut request).await.unwrap();
            let body = r#"{"data":[{"id":"listed-model"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        (format!("http://{address}/v1"), task)
    }

    #[tokio::test]
    async fn provider_model_query_sends_catalog_auth_and_extra_headers() {
        let (base_url, server) = capture_model_list_request().await;
        let extra_headers =
            BTreeMap::from([("x-provider-version".to_owned(), "2026-08-12".to_owned())]);
        let rows = query_provider_models(
            &base_url,
            "api-key",
            "",
            &extra_headers,
            Some("discovery-secret"),
        )
        .await
        .expect("model list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "listed-model");

        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("api-key: discovery-secret"));
        assert!(request.contains("x-provider-version: 2026-08-12"));
        assert!(!request.contains("authorization:"));
    }

    #[tokio::test]
    async fn hosted_model_projection_is_unavailable_until_catalog_env_resolves() {
        const ENV: &str = "CODYPENDENT_TEST_RUNNABLE_MODEL_KEY_846f";
        std::env::remove_var(ENV);
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::write(
            paths.data_dir.join("providers.toml"),
            format!(
                r#"
[[provider]]
id = "runnable-test"
name = "Runnable test"
protocol = "openai-chat"
base_url = "https://example.invalid/v1"
[[provider.auth]]
kind = "api_key"
env = ["{ENV}"]
header = "Authorization"
prefix = "Bearer "
"#
            ),
        )
        .expect("provider config");
        std::fs::write(
            paths.data_dir.join("models.toml"),
            r#"
[[model]]
id = "runnable-test/model"
provider = "openai-compatible"
provider_id = "runnable-test"
base_url = "https://example.invalid/v1"
model = "model"
api_key_env = ""
"#,
        )
        .expect("model config");

        let mut warnings = Vec::new();
        let cards = load_model_cards(&paths, &mut warnings).await;
        assert_eq!(cards.len(), 1);
        assert!(matches!(cards[0].readiness, ModelReadiness::Unavailable(_)));
        assert!(runnable_model_ids(&cards).is_empty());

        std::env::set_var(ENV, "resolved-secret");
        let cards = load_model_cards(&paths, &mut warnings).await;
        assert_eq!(cards[0].readiness, ModelReadiness::Unverified);
        assert_eq!(
            runnable_model_ids(&cards),
            vec![ModelId("runnable-test/model".to_owned())]
        );
        std::env::remove_var(ENV);
    }

    // -- provider_can_list_models (model-discovery gate) ----------------------

    /// A `Provider` with an explicit protocol + base_url, for exercising
    /// `provider_can_list_models` (which reads all three of protocol, base_url,
    /// and the first auth method).
    fn provider_listable(
        protocol: codypendent_providers::Protocol,
        base_url: Option<&str>,
        auth: Vec<codypendent_providers::AuthMethod>,
    ) -> codypendent_providers::Provider {
        codypendent_providers::Provider {
            id: "test-provider".to_string(),
            name: "Test Provider".to_string(),
            protocol,
            base_url: base_url.map(str::to_string),
            auth,
            extra_headers: Default::default(),
            query_params: Default::default(),
            local: false,
        }
    }

    #[test]
    fn can_list_models_true_for_openai_chat_with_base_url_and_api_key() {
        use codypendent_providers::{AuthMethod, Protocol};
        let p = provider_listable(
            Protocol::OpenAiChat,
            Some("https://api.groq.com/openai/v1"),
            vec![AuthMethod::ApiKey {
                env: vec!["GROQ_API_KEY".to_string()],
                header: "Authorization".to_string(),
                prefix: "Bearer ".to_string(),
            }],
        );
        assert!(provider_can_list_models(&p));
    }

    #[test]
    fn can_list_models_true_for_openai_chat_with_base_url_and_no_auth() {
        use codypendent_providers::{AuthMethod, Protocol};
        let p = provider_listable(
            Protocol::OpenAiChat,
            Some("http://localhost:11434/v1"),
            vec![AuthMethod::None],
        );
        assert!(provider_can_list_models(&p));
    }

    #[test]
    fn can_list_models_true_for_openai_chat_with_base_url_and_empty_auth() {
        use codypendent_providers::Protocol;
        let p = provider_listable(
            Protocol::OpenAiChat,
            Some("http://localhost:1234/v1"),
            vec![],
        );
        assert!(provider_can_list_models(&p));
    }

    #[test]
    fn can_list_models_false_without_a_base_url() {
        use codypendent_providers::{AuthMethod, Protocol};
        let p = provider_listable(
            Protocol::OpenAiChat,
            None,
            vec![AuthMethod::ApiKey {
                env: vec!["OPENAI_API_KEY".to_string()],
                header: "Authorization".to_string(),
                prefix: "Bearer ".to_string(),
            }],
        );
        assert!(!provider_can_list_models(&p));
    }

    #[test]
    fn can_list_models_false_for_a_blank_base_url() {
        use codypendent_providers::{AuthMethod, Protocol};
        let p = provider_listable(Protocol::OpenAiChat, Some("   "), vec![AuthMethod::None]);
        assert!(!provider_can_list_models(&p));
    }

    #[test]
    fn can_list_models_false_for_non_openai_chat_protocols() {
        use codypendent_providers::{AuthMethod, Protocol};
        for protocol in [Protocol::Anthropic, Protocol::GeminiNative, Protocol::Acp] {
            let p = provider_listable(
                protocol,
                Some("https://api.anthropic.com"),
                vec![AuthMethod::ApiKey {
                    env: vec!["ANTHROPIC_API_KEY".to_string()],
                    header: "x-api-key".to_string(),
                    prefix: "".to_string(),
                }],
            );
            assert!(
                !provider_can_list_models(&p),
                "protocol {protocol:?} must not list"
            );
        }
    }

    #[test]
    fn can_list_models_false_for_cloud_iam_and_oauth() {
        use codypendent_providers::{AuthMethod, Protocol};
        let cloud_iam = provider_listable(
            Protocol::OpenAiChat,
            Some("https://bedrock.example/v1"),
            vec![AuthMethod::CloudIam {
                variant: "aws_sigv4".to_string(),
                env: Default::default(),
                scopes: vec![],
            }],
        );
        assert!(!provider_can_list_models(&cloud_iam));

        let oauth = provider_listable(
            Protocol::OpenAiChat,
            Some("https://oauth.example/v1"),
            vec![AuthMethod::OAuth {
                authorize_url: "https://example.com/authorize".to_string(),
                token_url: "https://example.com/token".to_string(),
                client_id: "client".to_string(),
                scopes: vec![],
                pkce: true,
            }],
        );
        assert!(!provider_can_list_models(&oauth));
    }

    // -- provider_runtime_supported (the /provider "catalog-only" gate) -------

    /// ACP-models review F3: `/provider` refused Anthropic as "catalog-only"
    /// because the runtime gate was a thin delegate to the LISTING gate, which
    /// requires `Protocol::OpenAiChat`. `ModelRegistry::client_for` now has a
    /// real `Protocol::Anthropic` arm (`provider-anthropic` is a default
    /// feature), so that refusal hid the catalog's 10 priced Anthropic rows
    /// behind a message that was no longer true.
    #[test]
    fn runtime_supported_accepts_anthropic_even_though_it_cannot_list() {
        use codypendent_providers::{AuthMethod, Protocol};
        let anthropic = provider_listable(
            Protocol::Anthropic,
            Some("https://api.anthropic.com"),
            vec![AuthMethod::ApiKey {
                env: vec!["ANTHROPIC_API_KEY".to_string()],
                header: "x-api-key".to_string(),
                prefix: String::new(),
            }],
        );
        assert!(
            provider_runtime_supported(&anthropic),
            "the add flow must open for Anthropic"
        );
        assert!(
            !provider_can_list_models(&anthropic),
            "but it must NOT be sent at `{{base_url}}/models`, which 404s for \
             Anthropic — its curated rows answer instead"
        );
    }

    /// The gate must stay closed for what is genuinely unwired, or the
    /// "catalog-only" message would be replaced by a later, worse failure.
    #[test]
    fn runtime_supported_still_refuses_unwired_protocols_and_unusable_endpoints() {
        use codypendent_providers::{AuthMethod, Protocol};
        let api_key = || {
            vec![AuthMethod::ApiKey {
                env: vec!["KEY".to_string()],
                header: "Authorization".to_string(),
                prefix: "Bearer ".to_string(),
            }]
        };
        let p = provider_listable(Protocol::Acp, Some("https://example.com/v1"), api_key());
        assert!(!provider_runtime_supported(&p), "ACP has no ChatClient arm");
        assert!(
            !provider_runtime_supported(&provider_listable(Protocol::Anthropic, None, api_key())),
            "no base_url means nothing to execute against"
        );
        assert!(
            !provider_runtime_supported(&provider_listable(
                Protocol::Anthropic,
                Some("https://api.anthropic.com"),
                vec![AuthMethod::CloudIam {
                    variant: "aws_sigv4".to_string(),
                    env: Default::default(),
                    scopes: vec![],
                }],
            )),
            "cloud-IAM auth is not something this build can supply"
        );
        assert!(
            !provider_runtime_supported(&provider_listable(
                Protocol::GeminiNative,
                Some("https://generativelanguage.googleapis.com/v1beta"),
                vec![AuthMethod::OAuth {
                    authorize_url: "https://accounts.example/authorize".to_string(),
                    token_url: "https://accounts.example/token".to_string(),
                    client_id: "public-client".to_string(),
                    scopes: vec!["models".to_string()],
                    pkce: true,
                }],
            )),
            "OAuth native auth needs a production token-provider injection"
        );
    }

    // -- ACP model discovery (the picker's agent-model plumbing) --------------

    #[test]
    fn acp_profile_ids_mirror_the_coordinate_they_store() {
        assert_eq!(acp_profile_id("demo-acp", None), "acp/demo-acp");
        assert_eq!(
            acp_profile_id("demo-acp", Some("agent-model-1")),
            "acp/demo-acp#agent-model-1"
        );
    }

    #[test]
    fn an_acp_probe_pins_only_a_model_that_was_asked_for() {
        let probe = AcpProbe {
            registry_id: "demo-acp".to_string(),
            version: "1.2.3".to_string(),
            models: vec!["agent-model-1".to_string()],
        };
        assert_eq!(probe.coordinate(None), "demo-acp@1.2.3");
        assert_eq!(
            probe.coordinate(Some("agent-model-1")),
            "demo-acp@1.2.3#agent-model-1"
        );
        // A profile id and its coordinate must agree on the pin, so the
        // executor asks the agent for the model the picker showed.
        let coordinate = probe.coordinate(Some("agent-model-1"));
        assert_eq!(
            codypendent_integrations::acp_registry::agent_model_from_coordinate(&coordinate),
            Some("agent-model-1")
        );
        assert_eq!(
            codypendent_integrations::acp_registry::agent_id_from_coordinate(&coordinate),
            "demo-acp",
            "a pinned coordinate must still resolve to the same launchable agent"
        );
    }

    // -- models_url + parse_models_response (model-discovery, pure) -----------

    #[test]
    fn models_url_appends_models_without_doubling_the_version() {
        // The base_url already carries its version segment; the list route is its
        // sibling `/models`, never `/v1/models`.
        assert_eq!(
            models_url("https://api.groq.com/openai/v1"),
            "https://api.groq.com/openai/v1/models"
        );
        assert_eq!(
            models_url("http://localhost:11434/v1"),
            "http://localhost:11434/v1/models"
        );
        // A non-`/v1` base (z.ai) must not be forced to `/v1`.
        assert_eq!(
            models_url("https://api.z.ai/api/paas/v4"),
            "https://api.z.ai/api/paas/v4/models"
        );
        // A trailing slash is trimmed so the join is exact.
        assert_eq!(
            models_url("http://localhost:1234/v1/"),
            "http://localhost:1234/v1/models"
        );
    }

    /// Ids of the parsed rows, in order — most parse tests only care about
    /// which models came back, not the optional metadata.
    fn parsed_ids(body: &str) -> Vec<String> {
        parse_models_response(body)
            .expect("parse")
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    #[test]
    fn parse_models_response_extracts_ids_from_the_openai_shape() {
        let body = r#"{"object":"list","data":[{"id":"llama-3.1-8b"},{"id":"llama-3.3-70b"}]}"#;
        assert_eq!(parsed_ids(body), vec!["llama-3.1-8b", "llama-3.3-70b"]);
    }

    #[test]
    fn parse_models_response_skips_blank_and_dedups_preserving_order() {
        let body = r#"{"data":[{"id":"a"},{"id":"  "},{"id":""},{"id":"a"},{"id":"b"}]}"#;
        assert_eq!(parsed_ids(body), vec!["a", "b"]);
    }

    /// A bare-id response (Ollama, Groq, most providers) parses to rows with
    /// every optional column empty — the metadata is never required.
    #[test]
    fn parse_models_response_leaves_metadata_empty_when_the_provider_sends_none() {
        let body = r#"{"data":[{"id":"qwen2.5-coder:14b"}]}"#;
        let rows = parse_models_response(body).expect("parse");
        assert_eq!(rows[0].id, "qwen2.5-coder:14b");
        assert!(rows[0].name.is_none());
        assert!(rows[0].context_tokens.is_none());
        assert!(rows[0].cost_per_1m_input_usd.is_none());
        assert!(rows[0].live, "a listed model is a live row");
    }

    /// The richer shapes several OpenAI-compatible providers already return on
    /// the same endpoint: OpenRouter's `context_length` + per-TOKEN string
    /// prices (scaled to per-1M here), and the vLLM-derived `max_model_len`
    /// numeric spelling.
    #[test]
    fn parse_models_response_keeps_context_and_pricing_when_present() {
        let body = r#"{"data":[
            {"id":"meta-llama/llama-3.3-70b","name":"Llama 3.3 70B",
             "context_length":131072,
             "pricing":{"prompt":"0.00000013","completion":"0.0000004"}},
            {"id":"deepseek-v3","max_model_len":163840,
             "pricing":{"input":0.0000005,"output":0.0000015}}
        ]}"#;
        let rows = parse_models_response(body).expect("parse");
        assert_eq!(rows[0].name.as_deref(), Some("Llama 3.3 70B"));
        assert_eq!(rows[0].context_tokens, Some(131_072));
        // Per-token prices are scaled to the per-1M column the picker shows.
        assert!((rows[0].cost_per_1m_input_usd.expect("input price") - 0.13).abs() < 1e-9);
        assert!((rows[0].cost_per_1m_output_usd.expect("output price") - 0.4).abs() < 1e-9);
        assert_eq!(rows[1].context_tokens, Some(163_840));
        assert!((rows[1].cost_per_1m_input_usd.expect("input price") - 0.5).abs() < 1e-9);
    }

    /// ACP-models review F4: `context_tokens` is not display-only — the picker's
    /// number is what `write_add_model` persists, and from there it becomes the
    /// Ollama `num_ctx` hint and the footer's context-usage denominator. A
    /// misconfigured or hostile gateway reporting an absurd `context_length` got
    /// carried straight through. `load_models` now clamps on every read, so the
    /// picker must not DISPLAY a number the loader is going to cap anyway.
    #[test]
    fn parse_models_response_drops_an_implausible_context_length() {
        let body = format!(
            r#"{{"data":[
                {{"id":"honest","context_length":131072}},
                {{"id":"absurd","context_length":9223372036854775807}},
                {{"id":"at-the-ceiling","context_length":{ceiling}}}
            ]}}"#,
            ceiling = MAX_PLAUSIBLE_CONTEXT_TOKENS
        );
        let rows = parse_models_response(&body).expect("parse");
        assert_eq!(rows[0].context_tokens, Some(131_072));
        assert!(
            rows[1].context_tokens.is_none(),
            "an implausible reading is dropped, not shown and not persisted"
        );
        assert_eq!(
            rows[2].context_tokens,
            Some(MAX_PLAUSIBLE_CONTEXT_TOKENS),
            "the ceiling itself is still a legitimate reading"
        );
        assert_eq!(
            rows[1].id, "absurd",
            "the row itself survives — only the bad column is dropped"
        );
    }

    /// A price this build cannot make sense of is dropped, not guessed: a
    /// fabricated number in a cost column is worse than a blank one.
    #[test]
    fn parse_models_response_drops_unparseable_prices() {
        let body = r#"{"data":[{"id":"m","pricing":{"prompt":"free","completion":null}}]}"#;
        let rows = parse_models_response(body).expect("parse");
        assert!(rows[0].cost_per_1m_input_usd.is_none());
        assert!(rows[0].cost_per_1m_output_usd.is_none());
    }

    #[test]
    fn parse_models_response_errors_on_an_empty_list() {
        let body = r#"{"object":"list","data":[]}"#;
        let err = parse_models_response(body).expect_err("empty list must be an error");
        assert!(err.contains("no models"), "reason: {err}");
    }

    #[test]
    fn parse_models_response_errors_when_the_data_key_is_absent() {
        // `data` is `#[serde(default)]`, so a body missing the key entirely is
        // still valid JSON (unlike a malformed body) — it must fall through to
        // the same "no models" error as an explicit empty list, not panic or
        // silently succeed with an empty `Vec`.
        let body = r#"{"object":"list"}"#;
        let err = parse_models_response(body).expect_err("missing data key must be an error");
        assert!(err.contains("no models"), "reason: {err}");
    }

    #[test]
    fn parse_models_response_errors_on_a_malformed_body() {
        assert!(parse_models_response("not json at all").is_err());
        assert!(parse_models_response("").is_err());
    }
}
