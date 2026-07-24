# Continuous Session Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A follow-up message continues the same conversation — the model receives the prior turns as context, the repo isn't re-mapped every message, and the whole session renders as one continuous scroll.

**Architecture:** Shape B — wire the dormant `SubmitUserInput` command. The daemon reconstructs prior turns from the per-session event ledger (Hybrid: verbatim recent + chronicle-compacted older) and seeds the continuation run's transcript; context assembly is skipped on continuations; the TUI renders all of a session's runs as one conversation. Additive protocol only; run boundaries preserved.

**Tech Stack:** Rust; `crates/{protocol,daemon,codypendentd,runtime,tui,cli}`; the append-only session `events` ledger; `TurnItem` transcript model.

## Global Constraints

- Additive protocol only: reuse existing `CommandBody::SubmitUserInput { session_id, text, mode }`; any new run field is `#[serde(default)]`; `RunContext.prior`/`RunLaunch.prior` are server-internal (not wire fields). No wire-format break. Regenerate golden vectors only if a wire type changes shape.
- Preserve run boundaries: a continuation is a normal bounded run (budgets, worktree lifecycle, cancel, chronicle all keep working) that happens to be seeded with history.
- Preserve the T1/T7 cost-honesty invariant: seeded-history tokens count as measured; nothing fabricated.
- Pure-reducer TUI (no I/O, no new dep); `F2` workspace layout keeps working.
- Clippy runs on Linux CI — gate any macOS-only test helper.
- NEVER edit/stage `README.md`, `docs/cli-and-tui-user-guide.md`, `docs/docs/*`, `ROADMAP.md`, `.superpowers/`. Stage only changed files by explicit path.
- Commit trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Gate each task: `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-features`.

## File structure / order

T1 ledger→transcript projection (daemon, pure+testable) → T2 seed carrier (runtime `RunContext.prior` + daemon `RunLaunch.prior`) → T3 wire `apply_submit_input` to launch a seeded continuation → T4 conditional context assembly → T5 TUI continuous conversation + `SubmitUserInput` intent + new-conversation action. T1→T4 are daemon/runtime (sequential-ish, shared executor); T5 is TUI.

---

## Task 1: Ledger → transcript projection (Hybrid)

A pure function turning a session's persisted events into a seed `Vec<TurnItem>`, verbatim for the last N runs and chronicle-compacted for older ones.

**Files:**
- Create/Modify: `crates/codypendentd/src/executor.rs` (or a new `session_history.rs` module in codypendentd) — the projection fn.
- Test: same file `#[cfg(test)]`.

**Interfaces:**
- Consumes: `codypendent_daemon::ledger::load_events(pool, session_id) -> Vec<SessionEvent>` (used today by `harvest_memories`, `executor.rs:597`); `EventBody::{RunStarted, ModelStreamDelta, ToolCompleted, SteeringApplied, RunCompleted}` (`protocol/src/events.rs`); `codypendent_runtime::agent::TurnItem::{Objective, Assistant, ToolResult, Steering}` (`runtime/src/agent.rs:100`); `RunCompleted.chronicle`.
- Produces: `pub(crate) fn session_transcript(events: &[SessionEvent], verbatim_runs: usize) -> Vec<TurnItem>`.

- [ ] **Step 1: Write the failing test** — a 2-run session projects verbatim; an older 3rd run (beyond the window) compacts to its chronicle.

```rust
#[test]
fn session_transcript_is_verbatim_recent_and_compacted_older() {
    // Build events for 3 runs in one session:
    //  run A (oldest): RunStarted{objective:"first"}, ModelStreamDelta "A-reply", RunCompleted{chronicle:"A did X"}
    //  run B:          RunStarted{objective:"second"}, ModelStreamDelta "B-reply", RunCompleted{...}
    //  run C (newest): RunStarted{objective:"third"},  ModelStreamDelta "C-reply", RunCompleted{...}
    let events = /* helper building the SessionEvents above */;
    let ts = session_transcript(&events, /*verbatim_runs*/ 2);
    // Older run A compacted to a single summary TurnItem carrying its chronicle text.
    assert!(ts.iter().any(|t| matches!(t, TurnItem::Assistant(s) if s.contains("A did X"))));
    // Recent runs B & C verbatim: their objectives appear as Objective turns and replies as Assistant.
    assert!(ts.iter().any(|t| matches!(t, TurnItem::Objective(o) if o == "second")));
    assert!(ts.iter().any(|t| matches!(t, TurnItem::Assistant(s) if s == "C-reply")));
    // Order preserved: A(summary) before B before C.
}
```

- [ ] **Step 2: Run — fails** (`cargo test -p codypendent-codypendentd session_transcript_is_verbatim` — fn undefined).

- [ ] **Step 3: Implement `session_transcript`** — group events by `run_id` (in ledger order); for runs within the last `verbatim_runs`, emit `Objective` (from `RunStarted.objective`), coalesced `Assistant` (concatenate that run's `ModelStreamDelta.text` in order), `ToolResult` summaries (from `ToolCompleted`), and `Steering`; for older runs, emit one compacted `Assistant`/summary `TurnItem` from `RunCompleted.chronicle` (fallback: objective + final assistant text when the chronicle is thin). Coalescing mirrors the TUI's `ModelStreamDelta` fold (`reduce.rs:264`). Keep it pure (events in → Vec out).

- [ ] **Step 4: Run — passes.**

- [ ] **Step 5: Gate + commit** (`feat(codypendentd): reconstruct a session transcript from the event ledger`).

---

## Task 2: Seed carrier — `prior` on `RunContext` + `RunLaunch`

Thread an optional prior-transcript through the launch types so the runtime can seed from it. Behavior-neutral until T3/T5 populate it.

**Files:**
- Modify: `crates/runtime/src/agent.rs` (`RunContext` ~476-511 gains `prior: Vec<TurnItem>`, default empty)
- Modify: `crates/daemon/src/executor.rs` (`RunLaunch` ~30-50 gains `prior: Vec<TurnItem>`, default empty; plumb into `RunContext`)
- Test: constructors compile; default empty preserves today's behavior.

**Interfaces:**
- Produces: `RunContext.prior: Vec<TurnItem>` and `RunLaunch.prior: Vec<TurnItem>` (both default `Vec::new()`), server-internal.
- Consumes: existing `RunContext`/`RunLaunch` constructors and every call site.

- [ ] **Step 1: Write the failing test** — a `RunContext` built with a non-empty `prior` exposes it (a simple field-access/getter test), and the default is empty.

- [ ] **Step 2: Run — fails** (no `prior` field).

- [ ] **Step 3: Add the field** to both structs (`#[default]`/`Vec::new()`), thread `RunLaunch.prior → RunContext.prior` where the daemon builds the run context, and update all constructor call sites to default empty. (Grep both structs' construction sites; add `prior: Vec::new()` / `..Default::default()`.)

- [ ] **Step 4: Run — passes; full suite green** (behavior unchanged — every existing run passes empty).

- [ ] **Step 5: Gate + commit** (`feat(runtime,daemon): carry an optional prior transcript on the run launch`).

---

## Task 3: Wire `apply_submit_input` to launch a seeded continuation

Turn the stub into a real launch: `SubmitUserInput` mints a continuation run whose `prior` is the reconstructed session transcript.

**Files:**
- Modify: `crates/daemon/src/commands.rs` (`apply_submit_input` ~485-517)
- Modify: `crates/codypendentd/src/executor.rs` (build `RunLaunch.prior = session_transcript(load_events(session_id), N)` on the continuation path)
- Test: `crates/daemon/tests/server_it.rs` (or codypendentd IT) — a `SubmitUserInput` on a session with a prior terminal run launches a run whose context carries the prior transcript.

**Interfaces:**
- Consumes: T1 `session_transcript`, T2 `RunLaunch.prior`, existing `apply_start_run` launch machinery (`commands.rs:443-483`), `ledger::load_events`.
- Produces: a launched continuation run seeded with history.

- [ ] **Step 1: Write the failing test** — submit input to a session that already has one completed run; assert a new run starts AND its launch/context `prior` contains the prior run's objective+reply (drive via the existing IT harness that scripts a daemon + a scripted model, asserting the model's transcript or an emitted event reflects the seeding).

- [ ] **Step 2: Run — fails** (today `apply_submit_input` only records a `NoteAppended`, launches nothing).

- [ ] **Step 3: Implement** — `apply_submit_input` mints a `RunId` + appends `RunStarted` (objective = the input `text`) like `apply_start_run`, flagged as a continuation, and the executor's launch path sets `RunLaunch.prior = session_transcript(&load_events(session_id), VERBATIM_RUNS)`. Add `const VERBATIM_RUNS: usize = 3;`. Keep the role/state gates `apply_start_run` uses.

- [ ] **Step 4: Run — passes.**

- [ ] **Step 5: Gate + commit** (`feat(daemon): SubmitUserInput launches a seeded continuation run`).

---

## Task 4: Seed the runtime transcript + conditional context

The runtime seeds from `prior`; the daemon skips the full repo re-map on a continuation.

**Files:**
- Modify: `crates/runtime/src/agent.rs:847` (seed from `prior` + objective)
- Modify: `crates/codypendentd/src/executor.rs:673` (`emit_context` conditional on first-run-of-session vs continuation)
- Test: runtime — a seeded `RunContext` yields a transcript beginning with the prior turns then the objective; codypendentd — a continuation does NOT emit the `=== CONTEXT` manifest, the first run does.

**Interfaces:**
- Consumes: T2 `RunContext.prior`; existing `emit_context`/`assemble_context` (`executor.rs:562`).

- [ ] **Step 1: Write the failing tests** — (a) runtime: build a `RunContext` with `prior = [Objective("p"), Assistant("pa")]` and objective `"q"`; assert the loop's initial transcript is `[Objective("p"), Assistant("pa"), Objective("q")]`. (b) codypendentd: a run flagged continuation does not produce a `NoteAppended` whose text starts `=== CONTEXT`.

- [ ] **Step 2: Run — fails.**

- [ ] **Step 3: Implement** — `agent.rs:847`: `let mut transcript = run.prior.clone(); transcript.push(TurnItem::Objective(run.objective.clone()));` (prior empty ⇒ identical to today). `executor.rs:673`: only call `emit_context` when the run is NOT a continuation (first run of the session), or emit a short "context carried from the conversation" marker instead of the full manifest.

- [ ] **Step 4: Run — passes; full suite green.**

- [ ] **Step 5: Gate + commit** (`feat(runtime,codypendentd): seed the transcript from prior turns; skip repo re-map on continuations`).

---

## Task 5: TUI — continuous conversation + SubmitUserInput + new-conversation action

The composer follow-up continues the session; the view renders all session runs as one scroll; a palette command starts a fresh conversation.

**Files:**
- Modify: `crates/tui/src/action.rs` (add `Intent::SubmitUserInput { session_id?, text }` — session id supplied by the harness like StartRun)
- Modify: `crates/tui/src/reduce.rs` (~1083-1099: terminal-run follow-up → `SubmitUserInput`, not `StartRun`; add a `NewConversation` palette command)
- Modify: `crates/tui/src/render.rs` (`render_conversation` ~218: walk ALL session runs' transcripts, not just `selected_run()`)
- Modify: `crates/cli/src/tui.rs` (`intent_to_command` ~795: map `Intent::SubmitUserInput` → `CommandBody::SubmitUserInput`)
- Test: `reduce.rs` + `render.rs` + `cli/tui.rs` mapping test.

**Interfaces:**
- Consumes: existing `Intent`/`intent_to_command`, `AppState.runs`, the PR#25 turn rendering.
- Produces: `Intent::SubmitUserInput`; a continuous-conversation render.

- [ ] **Step 1: Write the failing reduce test** — with a terminal selected run, submitting composer text pushes `Intent::SubmitUserInput { text }` (not `StartRun`); with an active run it still steers.

```rust
#[test]
fn a_follow_up_after_a_run_completes_continues_the_conversation() {
    let mut s = AppState::new();
    // start + complete a run, select it (terminal) …
    type_and_submit(&mut s, "follow up");
    assert!(matches!(s.outbox.last(), Some(Intent::SubmitUserInput { text, .. }) if text == "follow up"));
}
```

- [ ] **Step 2: Run — fails** (today it pushes `StartRun`).

- [ ] **Step 3: Implement the reduce + intent + mapping** — add `Intent::SubmitUserInput`; in `submit_prompt`'s terminal-run branch push it instead of `StartRun`; `intent_to_command` maps it to `CommandBody::SubmitUserInput { session_id, text, mode }`. Add a `PaletteCommand::NewConversation` that starts a fresh session/run (emits `StartRun` for a new objective, or a new-session intent) so the user can begin a clean conversation.

- [ ] **Step 4: Render test + implement continuous view** — assert that with two runs in the session, `render_to_string` shows BOTH runs' user turns + replies in one scroll (the first turn does not disappear). Change `render_conversation` to iterate all of the session's runs in order, applying the PR#25 per-turn rendering across them. Keep `F2` workspace layout working.

- [ ] **Step 5: Run tests — pass; full suite green.**

- [ ] **Step 6: Gate + commit** (`feat(tui): continuous conversation view + SubmitUserInput follow-ups + new-conversation`).

---

## After all tasks

- Whole-branch review (adversarial: does a follow-up truly seed the model; is context correctly skipped; run boundaries/budgets intact; TUI shows all turns; no protocol break; the Hybrid compaction bounds tokens). Then push (CodeHalwell → restore synextra) and open a PR to `main` (base retargets when PR #25 merges), left for the user.
- Rebuild + relaunch the TUI to see continuous conversations; the daemon must also be rebuilt+restarted (this changes daemon behavior). Offer both.
