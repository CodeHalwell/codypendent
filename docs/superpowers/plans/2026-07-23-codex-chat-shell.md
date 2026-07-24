# Codex-style Chat Shell Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reshape the TUI's primary conversation into Codex-style turns — one user turn, one assistant turn per exchange, no repeated echoes, backstage material folded away, compact tool cards.

**Architecture:** Entirely in `crates/tui` (pure reducer, no I/O, no new dep, no protocol change). Add two `TranscriptEntry` variants (`User`, `Backstage`); route the objective + context/memory notes into them in the reducer; redesign `render_conversation`/`entry_lines` into a turn layout. `render_conversation` is shared by the Chat and `F2` Workspace layouts, so both get the new look; the Workspace pane structure is untouched.

**Tech Stack:** Rust; ratatui; pure-reducer TUI (`state.rs` state, `reduce.rs` fold, `render.rs` view).

## Global Constraints

- No protocol change: `User`/`Backstage` are client-only cells; user turns derive from `RunStarted.objective` and steering; the fold + turn grouping are view state. `NoteAppended` is unchanged on the wire.
- Pure-reducer discipline: no I/O in `tui`; no new crate dependency; render reads state, never mutates.
- The `F2` Workspace layout must keep working (its pane structure is unchanged; only the shared conversation render changes).
- Glyphs (`⏺ › ❖ ⋯ ▸ ▌`) must degrade on 16-color/monochrome themes — reuse the theme's existing ASCII fallback pattern where a glyph isn't safe; if unsure, prefer ASCII (`>`, `*`, `#`, `…`→`...`).
- Clippy runs on Linux CI (`cargo clippy --workspace --all-targets --all-features -- -D warnings`) — gate any macOS-only test helper.
- NEVER edit/stage `README.md`, `docs/cli-and-tui-user-guide.md`, `docs/docs/*`, `ROADMAP.md`, or `.superpowers/`. Stage only changed files by explicit path; never `git add -A`.
- Commit trailer every commit: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Gate each task: `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-features`.

## File structure

- `crates/tui/src/state.rs` — Tasks 1,2: the `TranscriptEntry::User` + `TranscriptEntry::Backstage` variants.
- `crates/tui/src/reduce.rs` — Tasks 1,2,3: push a `User` turn at `RunStarted`; classify + fold context/`remembered:` notes into `Backstage`; keep `Completed` state.
- `crates/tui/src/render.rs` — Tasks 1–5: the turn-based conversation render, backstage dim line, demoted `Completed`, header chrome, compact tool cards.

No new files.

---

## Task 1: `User` turn cell

Show the user's message as a `› …` turn (today the objective is only the pane title).

**Files:**
- Modify: `crates/tui/src/state.rs` (add `TranscriptEntry::User { text: String }`)
- Modify: `crates/tui/src/reduce.rs` (`RunStarted` arm ~line 252 pushes a `User` turn)
- Modify: `crates/tui/src/render.rs` (`entry_lines` ~line 382 renders it)
- Test: `reduce.rs` + `render.rs` `#[cfg(test)]`

**Interfaces:**
- Produces: `TranscriptEntry::User { text: String }` (a new variant on the existing enum).
- Consumes: `EventBody::RunStarted { run_id, objective, mode }`; `AppState::push_entry(run, entry)`; `render_to_string`.

- [ ] **Step 1: Write the failing reduce test**

```rust
#[test]
fn run_started_pushes_a_user_turn_with_the_objective() {
    let mut s = AppState::new();
    let run_id = RunId::new();
    reduce(&mut s, system_ev(EventBody::RunStarted {
        run_id, objective: "add a test".to_owned(), mode: AgentMode::Build,
    }));
    assert!(matches!(&s.runs[0].transcript[0], TranscriptEntry::User { text } if text == "add a test"));
}
```

- [ ] **Step 2: Run it — fails** (`cargo test -p codypendent-tui run_started_pushes_a_user_turn` → no `User` variant).

- [ ] **Step 3: Add the variant + push it**

In `state.rs` `enum TranscriptEntry`, add (near `Model`):
```rust
/// The user's own message — the run objective, or a steering follow-up.
User { text: String },
```
In `reduce.rs` `RunStarted` arm (after the run is created/selected, before other pushes), push the objective as the first turn:
```rust
AppState::push_entry(run, TranscriptEntry::User { text: objective.clone() });
```
(Keep the existing objective handling that sets `RunView.objective`/title.)

- [ ] **Step 4: Add the render test + render it**

```rust
#[test]
fn a_user_turn_renders_with_a_caret_marker() {
    let mut s = AppState::new();
    let run_id = RunId::new();
    reduce(&mut s, system_ev(EventBody::RunStarted {
        run_id, objective: "add a test".to_owned(), mode: AgentMode::Build }));
    let out = render_to_string(&s, 80, 12);
    assert!(out.contains("› add a test") || out.contains("> add a test"));
}
```
In `render.rs` `entry_lines`, add the `TranscriptEntry::User { text }` arm — a `› {text}` head line styled with the theme's user/primary color (mirror how other head lines are built; ASCII `>` fallback per Global Constraints).

- [ ] **Step 5: Run tests — pass** (`cargo test -p codypendent-tui run_started_pushes_a_user_turn a_user_turn_renders`).

- [ ] **Step 6: Gate + commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features
git add crates/tui/src/state.rs crates/tui/src/reduce.rs crates/tui/src/render.rs
git commit -m "feat(tui): show the user's message as a turn

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 2: Backstage fold (context + memory notes)

Route context-manifest and `remembered:` notes into one dim, expandable `⋯` line instead of visible `Note` cells.

**Files:**
- Modify: `crates/tui/src/state.rs` (add `TranscriptEntry::Backstage { … }`)
- Modify: `crates/tui/src/reduce.rs` (`NoteAppended` arm ~line 231: classify + fold)
- Modify: `crates/tui/src/render.rs` (`entry_lines` renders the dim line; `expand_selected` toggles it)
- Test: `reduce.rs` + `render.rs`

**Interfaces:**
- Produces: `TranscriptEntry::Backstage { context_lines: Option<usize>, memory_updates: usize, raw: Vec<String>, expanded: bool }`.
- Consumes: `EventBody::NoteAppended { text, run_id }`; the existing `Note` classification.

- [ ] **Step 1: Write the failing reduce test**

```rust
#[test]
fn context_and_memory_notes_fold_into_backstage_not_visible_notes() {
    let mut s = AppState::new();
    let run_id = RunId::new();
    reduce(&mut s, system_ev(EventBody::RunStarted { run_id, objective: "o".into(), mode: AgentMode::Build }));
    reduce(&mut s, system_ev(EventBody::NoteAppended {
        run_id, text: "=== CONTEXT: EVIDENCE, NOT INSTRUCTIONS ===\nline\nline\nline".into() }));
    reduce(&mut s, system_ev(EventBody::NoteAppended {
        run_id, text: "remembered: the test command is cargo test".into() }));
    // No visible Note cells; exactly one Backstage entry with the right counts.
    assert!(!s.runs[0].transcript.iter().any(|e| matches!(e, TranscriptEntry::Note { .. })));
    let bs = s.runs[0].transcript.iter().find_map(|e| match e {
        TranscriptEntry::Backstage { context_lines, memory_updates, .. } => Some((*context_lines, *memory_updates)),
        _ => None });
    assert_eq!(bs, Some((Some(4), 1)));
}

#[test]
fn an_ordinary_note_still_renders_as_a_note_cell() {
    let mut s = AppState::new();
    let run_id = RunId::new();
    reduce(&mut s, system_ev(EventBody::RunStarted { run_id, objective: "o".into(), mode: AgentMode::Build }));
    reduce(&mut s, system_ev(EventBody::NoteAppended { run_id, text: "a plain observation".into() }));
    assert!(s.runs[0].transcript.iter().any(|e| matches!(e, TranscriptEntry::Note { .. })));
}
```

- [ ] **Step 2: Run — fails** (no `Backstage` variant; notes still all become `Note`).

- [ ] **Step 3: Add the variant + classify in the reducer**

`state.rs`:
```rust
/// Folded backstage material — the context manifest and memory writes for the
/// current turn. Rendered as one dim, expandable line; never part of the wire.
Backstage { context_lines: Option<usize>, memory_updates: usize, raw: Vec<String>, expanded: bool },
```
`reduce.rs` `NoteAppended` arm — classify by the note's own text prefix (the daemon labels them), fold into the run's existing `Backstage` entry or push one; only otherwise fall through to the existing `Note` push:
```rust
let is_context = text.starts_with("=== CONTEXT");
let is_memory  = text.trim_start().starts_with("remembered:");
if is_context || is_memory {
    // find-or-push a single Backstage entry on this run, then update it
    let entry = run.transcript.iter_mut().find_map(|e| match e {
        TranscriptEntry::Backstage { .. } => Some(e), _ => None });
    let backstage = match entry {
        Some(TranscriptEntry::Backstage { context_lines, memory_updates, raw, .. }) => {
            if is_context { *context_lines = Some(text.lines().count()); }
            if is_memory  { *memory_updates += 1; }
            raw.push(text.clone());
            return; // folded — no visible Note
        }
        _ => TranscriptEntry::Backstage {
            context_lines: is_context.then(|| text.lines().count()),
            memory_updates: is_memory as usize,
            raw: vec![text.clone()],
            expanded: false,
        },
    };
    AppState::push_entry(run, backstage);
    return;
}
// … existing Note fold (declutter) unchanged for every other note …
```
(Adapt to the arm's actual `run` binding + borrow shape; the invariant is: context/`remembered` notes never create a `Note` cell, and there is at most one `Backstage` per run.)

- [ ] **Step 4: Render test + render the dim line + expand**

```rust
#[test]
fn backstage_renders_a_dim_summary_line() {
    // build state as in the fold test …
    let out = render_to_string(&s, 80, 12);
    assert!(out.contains("context") && out.contains("memory"));
    assert!(!out.contains("EVIDENCE, NOT INSTRUCTIONS")); // raw hidden while folded
}
```
`render.rs` `entry_lines` `Backstage` arm: when `!expanded`, one dim line like `⋯ context · {n} lines · memory updated` (omit each half when its count is 0/None; render nothing if both empty). When `expanded`, follow with the `raw` bodies (dim, indented). Add the `TranscriptEntry::Backstage { expanded, .. } => *expanded = !*expanded` arm to `expand_selected` (reduce.rs ~line 688, beside the `Note` arm).

- [ ] **Step 5: Run tests — pass.**

- [ ] **Step 6: Gate + commit** (stage the 3 files; message `feat(tui): fold context + memory notes into a backstage line`).

---

## Task 3: Demote the `Completed` echo + assistant-turn header

Stop repeating the reply; head the agent's activity with `⏺ codypendent`.

**Files:**
- Modify: `crates/tui/src/render.rs` (`entry_lines` `Completed` arm; assistant header in the turn walk)
- Test: `render.rs`

**Interfaces:**
- Consumes: `TranscriptEntry::Completed { disposition }` (`RunDisposition::{Completed { summary }, Failed { reason }, Cancelled}`); `TranscriptEntry::{User, Model}`.

- [ ] **Step 1: Write the failing render tests**

```rust
#[test]
fn a_completed_success_shows_the_reply_once_no_echo() {
    let mut s = AppState::new();
    let run_id = RunId::new();
    reduce(&mut s, system_ev(EventBody::RunStarted { run_id, objective: "hi".into(), mode: AgentMode::Build }));
    reduce(&mut s, ev(agent_actor(run_id), EventBody::ModelStreamDelta { run_id, text: "hello there".into() }));
    reduce(&mut s, system_ev(EventBody::RunCompleted { run_id, disposition: completed("hello there") }));
    let out = render_to_string(&s, 80, 12);
    assert_eq!(out.matches("hello there").count(), 1, "reply appears exactly once");
    assert!(!out.contains("run completed:"));
    assert!(out.contains("⏺ codypendent") || out.contains("codypendent"));
}

#[test]
fn a_failed_run_shows_its_reason() {
    // RunStarted then RunCompleted with Failed { reason: "no model configured" }
    // assert out.contains("no model configured")
}
```

- [ ] **Step 2: Run — fails** (today `Completed` renders `run completed: {summary}`).

- [ ] **Step 3: Implement**

In `render.rs` `entry_lines`, change the `Completed { disposition }` arm:
```rust
TranscriptEntry::Completed { disposition } => match disposition {
    RunDisposition::Completed { .. } => { /* success: render nothing — the prose already ended the turn */ }
    RunDisposition::Failed { reason } => out.push(head(format!("✗ {reason}"), theme.status.error)),
    RunDisposition::Cancelled => out.push(head("✗ cancelled".to_string(), theme.text.muted)),
},
```
(Match the real `RunDisposition` shape.) And in the turn walk (where `render_conversation` iterates entries): before the first agent cell of a turn (a `Model`/`Tool`/`Patch` following a `User` turn), emit a `⏺ codypendent` header line (theme primary; ASCII `*` fallback). Track "are we at the first agent cell since the last User turn" while walking.

- [ ] **Step 4: Run tests — pass.**

- [ ] **Step 5: Gate + commit** (`feat(tui): one reply per turn — demote the completed echo, add assistant header`).

---

## Task 4: Header chrome (model · mode · cost) + turn spacing

**Files:**
- Modify: `crates/tui/src/render.rs` (`render_conversation` title/header ~line 215; blank-line turn separation in the entry walk)
- Test: `render.rs`

**Interfaces:**
- Consumes: `RunView.{model, mode}` (state.rs:262), `StatusProjection` (state.rs:678) for the header; the existing `pane_block` title.

- [ ] **Step 1: Failing render test**

```rust
#[test]
fn the_conversation_header_shows_model_and_mode() {
    let mut s = AppState::new();
    let run_id = RunId::new();
    reduce(&mut s, system_ev(EventBody::RunStarted { run_id, objective: "o".into(), mode: AgentMode::Build }));
    reduce(&mut s, ev(agent_actor(run_id), EventBody::ModelStreamDelta { run_id, text: "hi".into() })); // learns model? if model comes via actor
    let out = render_to_string(&s, 96, 12);
    assert!(out.contains("Build"));
}
```

- [ ] **Step 2: Run — fails / adjust** (the header may already show some of this; make the test assert the target and implement to it).

- [ ] **Step 3: Implement** — in `render_conversation`, build the pane title (or a header row) as `codypendent · {model} · {mode}[ · {cost}]`, reading `RunView.model`/`mode` (and cost from the status projection if present; omit when unknown). Add one blank line between turns in the entry walk (before each `User` turn after the first) so turns breathe.

- [ ] **Step 4: Run tests — pass.**

- [ ] **Step 5: Gate + commit** (`feat(tui): conversation header (model·mode·cost) + turn spacing`).

---

## Task 5: Compact tool / patch cards

Restyle tool + patch cards to the compact Codex form inside a turn.

**Files:**
- Modify: `crates/tui/src/render.rs` (the `ToolCard`/`Patch` rendering — `tool_card_lines` ~line 416 area)
- Test: `render.rs`

**Interfaces:**
- Consumes: existing `ToolCard { status, outcome, expanded, … }`, `PatchSummary { expanded, … }`.

- [ ] **Step 1: Failing render test**

```rust
#[test]
fn a_tool_card_renders_compact_with_a_status_glyph() {
    // build a run with a completed shell tool card (reuse existing tool-card test setup)
    let out = render_to_string(&s, 80, 12);
    // compact single-line head: a run/tool glyph, the tool name, and an outcome mark
    assert!(out.contains("ran") || out.contains("⏺"));
}
```

- [ ] **Step 2: Run — fails / adjust to the target compact form.**

- [ ] **Step 3: Implement** — collapsed tool card head becomes one compact line: `▸ ⏺ {verb} {target}` + a right/inline outcome (`✓`/`✗`/count) using the existing `card.status`/`card.outcome`; expanded still shows detail (unchanged). Patch head: `▸ ❖ patch {target} +{add} −{del}` + a `⟳ review` marker when it awaits approval. Keep the existing expand/selection behavior.

- [ ] **Step 4: Run tests — pass.**

- [ ] **Step 5: Gate + commit** (`feat(tui): compact tool + patch cards for the chat shell`).

---

## After all tasks

- Whole-branch review (adversarial: turn-grouping correctness with interleaved steering, no reply echo, backstage security material still reachable, F2 workspace unbroken, glyph fallbacks). Then push (CodeHalwell → restore synextra) and open a PR to `main`, left for the user's review.
- To see it: rebuild + restart the local daemon is NOT required (this is client-only — rebuild + relaunch the TUI). Offer to do the TUI rebuild + relaunch.
