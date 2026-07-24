# Continuous conversation session — design

**Date:** 2026-07-24 · **Status:** approved (pre-implementation) · **Branch:** `claude/continuous-session` (stacked on `claude/codex-chat-shell` / PR #25)

## Problem

Every message the user sends starts a **new, isolated run**: the TUI composer, when
no run is live, emits `Intent::StartRun` (`reduce.rs:1083-1099`), the daemon mints a
fresh `RunId` (`apply_start_run`, `commands.rs:443-483`), and the runtime seeds the
agent transcript from **only that message's objective** (`agent.rs:847`). The chat view
renders only `selected_run()` (`render.rs:218`), so a follow-up makes the previous turn
**disappear** from view (it's a different run, reachable only via Ctrl-↑/↓). And because
context is assembled per run (`executor.rs:673`, keyed on the objective), the full repo
map is re-emitted on **every** message ("context · 133 lines" each time).

The model never sees the prior conversation, so it cannot resolve "it" / "that file" —
this is not a conversation, it's a series of amnesiac one-shots.

Goal: a **true continuous session** — a follow-up continues the same conversation, the
model receives the prior turns as context, the repo isn't re-mapped every message, and
the whole conversation renders as one scroll.

## What already exists (from the architecture exploration)

- **A session already spans many runs** (DB: `sessions` 1-row, `runs` many with
  `session_id` FK; per-session append-only `events` ledger stores every turn of every
  run). The raw history is on disk; it is simply never re-read.
- **`CommandBody::SubmitUserInput { session_id, text, mode }`** already exists on the
  wire (`command.rs:47-51`), role-gated and golden-vectored — the **reserved but dormant
  "continue the conversation" seam**. Its handler (`apply_submit_input`, `commands.rs:485`)
  is a Phase-1 stub that only records a note and launches nothing; **no client sends it**.
- Steering is already true multi-turn *within one live run* (`agent.rs:1125` appends
  `TurnItem::Steering`).

So the store and protocol are already conversation-shaped; the missing pieces are:
seed the transcript from prior turns, route a follow-up to a continue path, and stop
re-mapping context every turn.

## Approach — Shape B: wire `SubmitUserInput` (chosen)

Each turn stays a bounded run (so budgets, worktree lifecycle, cancel, and the
per-run chronicle all keep working — matching how Codex/Claude Code treat a turn as a
bounded step), but a follow-up run is **seeded with the prior conversation**.

Rejected — **Shape A (one never-ending run, follow-ups steer in):** smallest diff but
breaks "a run is a bounded unit of work" — `MAX_STEPS`/wall-clock budgets, the terminal
chronicle step, worktree release, and cancel all key off a run boundary; a run that
never ends needs reset/checkpoint semantics for each. Not worth the blast radius.

## History fidelity — Hybrid (verbatim recent + compacted older)

The follow-up's seed transcript is built from the session ledger as:

- **Verbatim** for the last **N** turns (default N configurable, e.g. 3): the user
  objective, the assistant's final text, and tool-result summaries, as real `TurnItem`s
  (`Objective` / `Assistant` / `ToolResult` / `Steering`).
- **Compacted** for older turns: each prior run's structured **chronicle**
  (`RunCompleted.chronicle`, the system's intended compaction substrate) rendered as a
  single condensed `Assistant`/summary `TurnItem`, so token cost stays bounded as the
  conversation grows.

Rationale: verbatim recent turns are what let the model resolve conversational
references; chronicle-compacted older turns bound the token budget (which T1/T7 now
measures). The N threshold is the one knob.

## Architecture

### 1. Protocol (`crates/protocol`) — minimal, additive
- No new command: reuse the existing `SubmitUserInput { session_id, text, mode }`.
- `RunContext`/`RunLaunch` gain an **internal** `prior: Vec<TurnItem>` (server-side only,
  loaded from the ledger) — not a wire field, so **no wire-format change**.
- A run gains a `continues: Option<RunId>` / `turn_index` provenance field (additive,
  `#[serde(default)]`) so a continuation is attributable to its session predecessor.

### 2. Daemon (`crates/daemon`, `crates/codypendentd`)
- `apply_submit_input` (`commands.rs:485`) stops being a stub: it **mints and launches a
  run** like `apply_start_run`, but flagged as a **continuation** of the session.
- New **ledger→transcript projection**: `load_events(session_id)` →
  `Vec<TurnItem>` (`RunStarted`/objective→`Objective`, coalesced `ModelStreamDelta`→
  `Assistant`, `ToolCompleted`→`ToolResult` summary, `SteeringApplied`→`Steering`), with
  the Hybrid rule (verbatim last N runs; chronicle for older). Reuses existing
  `ledger::load_events` + `RunCompleted.chronicle`.
- **Context assembly becomes conditional** (`executor.rs:673`): the **first** run of a
  session assembles the full context manifest; a **continuation skips the full repo
  re-map** (the repo map is already in the seeded history / unchanged) — optionally
  emitting only a short "context unchanged" marker or an incremental delta. This removes
  the "context · 133 lines every message" waste.

### 3. Runtime (`crates/runtime/src/agent.rs`)
- Seed the transcript from `RunContext.prior` + the new objective, instead of
  `vec![TurnItem::Objective(objective)]` (`agent.rs:847`). Everything downstream (the
  loop, streaming, budgets) is unchanged.

### 4. TUI (`crates/tui`)
- The composer's follow-up path: when the selected run is **terminal** (not active),
  emit `Intent::SubmitUserInput { session_id, text }` instead of `Intent::StartRun`
  (`reduce.rs:1083-1099`). (A live run still steers, unchanged.)
- **Render the session's runs as one continuous conversation**: `render_conversation`
  walks **all** of the session's runs' transcripts in order (not just `selected_run()`),
  so every turn stays visible as one scroll. The Codex turn rendering (PR #25: `User`
  turns, `⏺ codypendent` headers, Backstage fold, compact cards) applies per turn across
  the whole session. The run counter `[n/n]` is replaced by (or augments) a single
  conversation. `F2` workspace layout unchanged.
- `intent_to_command` (`cli/tui.rs`) maps the new `Intent::SubmitUserInput` to the
  existing `CommandBody::SubmitUserInput`.

## Data flow

First message → `StartRun` (unchanged) → full context assembled → run to terminal →
chronicle persisted. Follow-up (selected run terminal) → `Intent::SubmitUserInput` →
`CommandBody::SubmitUserInput` → daemon launches a **continuation** run → daemon
reconstructs `prior: Vec<TurnItem>` from the session ledger (Hybrid) → `RunContext.prior`
→ runtime seeds the transcript with prior + new objective → model answers with full
conversational context → **no full repo re-map** → the TUI, rendering all session runs,
shows it as the next turn in one continuous scroll.

## Error handling / edge cases

- **Empty/short history:** the first run has no prior — behaves exactly as today.
- **A prior run that failed** (no chronicle / partial): its available turns
  (objective + any assistant text) are included; a failed run contributes what it has,
  never a fabricated summary.
- **Token growth:** bounded by the Hybrid compaction (verbatim window + chronicles);
  the existing budget accounting (T1/T7) measures it, and a run that would exceed its
  budget blocks as today.
- **Ledger projection fidelity:** `ModelStreamDelta` is fragmentary — coalesce per run
  before emitting one `Assistant` turn (the TUI already coalesces; reuse that logic).
  Tool outputs are artifact-referenced/compacted — use the summary, not raw bytes.
- **Backward compat:** an old client that still sends `StartRun` for every message keeps
  working (each is a fresh run, as today) — the continuation path is additive.

## Testing

- **daemon:** `apply_submit_input` launches a continuation run (not just a note); the
  ledger→transcript projection produces the expected `TurnItem`s for a 2-run session
  (verbatim recent) and compacts an older run to its chronicle (Hybrid threshold).
- **runtime:** a `RunContext.prior` seeds the transcript; the agent loop sees prior turns
  (assert the transcript the driver receives contains the prior objective + assistant).
- **context:** a continuation run does NOT re-emit the full `=== CONTEXT` manifest; the
  first run does.
- **tui:** a terminal-run follow-up emits `SubmitUserInput`, not `StartRun`; the
  conversation renders **all** session runs' turns in one scroll (the prior turn does not
  disappear); a live run still steers.
- All existing tests green; golden vectors updated only if a `RunStarted`/run field is
  added (additive).

## Constraints

- Additive protocol only (reuse `SubmitUserInput`; `prior` is server-internal; any new
  run field is `#[serde(default)]`). No wire-format break.
- Preserve run boundaries (budgets, worktree lifecycle, cancel, chronicle) — a
  continuation is a normal bounded run that happens to be seeded.
- Preserve the T1/T7 cost-honesty invariant (seeded history counts toward measured
  tokens; nothing fabricated).
- Pure-reducer TUI; `F2` workspace layout preserved.
- Foreign files never touched.

## Open questions / risks

- **Verbatim window N:** default value + whether it's user-configurable. Plan pins a
  default (3) and a config knob; tune later.
- **Chronicle availability:** confirm `RunCompleted.chronicle` is rich enough to stand
  in for a compacted turn; if thin, fall back to the run's objective + final assistant
  text.
- **Context "incremental" vs "skip":** a continuation skips the full repo map, but a
  long session across code changes may want a refreshed map — a later refinement
  (re-map on demand / on detected repo change), not v1.
- **New-conversation affordance:** with continuity, the user needs a "start a fresh
  conversation" action (new session) — a small TUI addition (e.g. a palette command);
  scope in the plan.
