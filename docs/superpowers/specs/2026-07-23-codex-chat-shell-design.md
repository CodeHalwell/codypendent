# Codex-style chat shell — design

**Date:** 2026-07-23 · **Status:** approved (pre-implementation) · **Branch:** `claude/codex-chat-shell`

## Problem

The TUI transcript is cramped and repeats itself. A single reply renders **three
times** — the streamed `Model` cell, the `Completed { disposition }` marker
("run completed: <summary>"), and the daemon's "remembered: … completed: <summary>"
`Note` — with the context-manifest `Note` stacked on top. The user's own message
isn't shown as a turn at all. The result reads like an event log, not a
conversation.

Goal: reshape the primary view into a **Codex-style chat shell** — clean
conversational turns, one reply per turn, coding progress as compact collapsible
cards, and backstage material (context/evidence, memory writes) folded out of the
way — while keeping the existing `F2` workspace layout as the alternate view.

## Approved layout

```
┌ codypendent ───────────────────────────── gemma · Build · $0.00 ┐
│                                                                  │
│  › Hello                                                         │
│                                                                  │
│  ⏺ codypendent                                                   │
│    I am ready to assist. Please let me know what you'd like      │
│    me to do with the repository.                                 │
│                                                                  │
│  ⋯ context · 131 lines · memory updated                          │
│                                                                  │
└ › message the agent…            ⏎ send · / commands · ⌃C quit ──┘
```

Coding turn — tool activity as compact, collapsible cards inside the turn:

```
│  › add a round-trip test for the parser                          │
│                                                                  │
│  ⏺ codypendent                                                   │
│    I'll add one — let me check the parser first.                 │
│    ▸ ⏺ read   parser.rs                                          │
│    ▸ ⏺ ran    cargo test                       ✓ 12 passed       │
│    ▸ ❖ patch  parser_test.rs        +18 −0     ⟳ review          │
│    Added a round-trip test; all green. ▌                         │
```

## Goals

1. **Turns.** The user's input renders as a `›` user turn; the agent's activity
   renders as one `⏺ codypendent` assistant turn (header + streamed prose + inline
   tool/patch cards), grouped so a back-and-forth reads as a conversation.
2. **One reply, no echoes.** The assistant's text appears once. The `Completed`
   marker no longer repeats the summary on success; the "remembered:" memory note
   and the context-manifest note are removed from the conversation flow.
3. **Backstage fold.** Context/evidence and memory writes collapse into a single
   dim, expandable line per turn (`⋯ context · N lines · memory updated`) — still
   present (they are security-relevant evidence), just out of the way.
4. **Compact progress.** Tool and patch activity render as compact collapsible
   cards (`⏺ ran cargo test ✓`, `❖ patch … review`) inside the turn.
5. **Chrome.** A header (model · mode · cost) and the persistent composer footer
   with hints. The `F2` workspace layout is unchanged and remains the alternate.

## Non-goals

- **No protocol change.** User turns are derived from existing `RunStarted.objective`
  and steering events; the backstage fold and turn grouping are client view state.
- No change to what the daemon emits (the "remembered"/context notes still arrive as
  `NoteAppended`; the client re-homes them). Reducing daemon-side note emission is a
  separate future option.
- The alternate `F2` workspace layout keeps its current rendering.
- Composer *editor* features (multiline, history, `@`-mentions) are out of scope —
  the footer composer is styled, not rewritten (those are separate roadmap items).

## Architecture

Almost entirely in `crates/tui` (pure reducer; no I/O; no new crate dep). Three
layers:

1. **State (`state.rs`)** — additions, not a rewrite of the typed-cell model:
   - `TranscriptEntry::User { text }` — a user turn (the objective and each steering
     input). Keeps the existing variants (`Model`, `Tool`, `Patch`, `Steering`,
     `Budget`, `Completed`, `Note`, `Unsupported`).
   - A new `TranscriptEntry::Backstage { context_lines: Option<usize>, memory_updates: usize, raw: Vec<String>, expanded: bool }`
     — context-manifest and "remembered:" notes fold into this single per-turn entry
     (counts for the dim line; `raw` holds the folded note bodies for expansion)
     instead of living as visible `Note` cells. The reducer updates the turn's
     existing `Backstage` entry (or pushes one) as such notes arrive.
   - Turn grouping is a **render-time** concern (a helper that walks the transcript
     and groups a `User` turn + the following agent activity up to the next `User`
     or terminal); no new persistent grouping state.

2. **Reducer (`reduce.rs`)** — routing, not new event handling:
   - `RunStarted { objective }` ⇒ push a `User { text: objective }` turn (today the
     objective is only the run title).
   - `SteeringApplied` (a user follow-up) ⇒ push a `User { text }` turn.
   - A `NoteAppended` whose text is a context manifest (`=== CONTEXT: EVIDENCE`) or a
     `remembered:` memory note ⇒ fold into the turn's **Backstage** summary instead
     of a visible `Note` cell. Any *other* note still renders as a `Note` cell
     (unchanged) — this classification is by the note's own text prefix, matching how
     the daemon labels them.
   - `RunCompleted`: keep the `Completed` entry for state, but rendering demotes it
     (see below). A **failed** disposition still surfaces its reason (not redundant).

3. **Render (`render.rs`)** — the redesign:
   - Turn-based layout: `›` user turns; a `⏺ codypendent` assistant header once per
     agent turn; streamed prose; inline compact tool/patch cards; the dim Backstage
     line; blank-line turn separation.
   - `Completed` on success renders **nothing** — never the repeated summary (the
     turn already ended with the assistant's prose). On failure, it renders the
     failure reason (the one case it must stay visible).
   - Header row: `model · mode · cost` (from the run's `RunView`/status projection);
     composer footer keeps the existing contextual hints.
   - The streaming caret and `RunActivity` status (already shipped) sit inside the
     assistant turn.
   - The `F2` workspace layout path is untouched.

## Data flow

`RunStarted.objective` → reducer pushes `User` turn → render as `›`. Agent events
(`ModelStreamDelta`, tool lifecycle, `PatchProposed`) fold into the current agent
turn (existing cells). Context/`remembered` `NoteAppended` → reducer folds into the
turn's Backstage summary → render as one dim `⋯` line (expand shows raw). `RunCompleted`
→ demoted marker (or failure reason). Everything renders through the same
`render_to_string`-testable path.

## Error handling / edge cases

- **Failed run:** the `Completed` marker renders the failure reason (e.g. "no model
  configured") — this is the one case the marker must stay visible, since it is not a
  duplicate of any reply.
- **Forward-compat:** `Unsupported` cells and unknown note shapes still render (never
  crash); an unclassified note stays a normal `Note` cell.
- **Empty agent turn** (a run that fails before any prose): the user turn + the
  failure marker render; no empty `⏺` header with nothing under it.
- **Backstage with nothing folded:** no dim line rendered (only appears when there is
  context and/or memory to fold).

## Testing

- **reduce:** `RunStarted` pushes a `User` turn with the objective; a steering input
  pushes a `User` turn; a context-manifest note and a `remembered:` note fold into the
  Backstage summary (counts correct) and do NOT create visible `Note` cells; an
  ordinary note still becomes a `Note` cell.
- **render** (`render_to_string`, contains-asserts as the crate already uses): a
  completed turn shows the reply exactly once (assert the summary text count == 1,
  no "run completed:" echo, no "remembered:" line); the `› <objective>` user turn
  renders; the dim `⋯ context …` line renders and expands; a failed run shows its
  reason; a coding turn shows compact tool cards; the header shows model/mode.
- **F2 alternate** still renders (existing tests stay green).
- All existing `codypendent-tui` tests remain green (adjust ones that asserted the
  old echo/notes layout, keeping them meaningful).

## Constraints

- Pure reducer; no I/O; `tui` gains no new crate dependency.
- **No protocol change** — everything derives from existing events + client view state.
- Clippy runs on Linux CI — gate any macOS-only test helper.
- Foreign files (`README.md`, `docs/cli-and-tui-user-guide.md`) never touched.
- The `F2` workspace layout must keep working unchanged.

## Open questions / risks

- **Turn grouping heuristic:** grouping "agent activity until the next user turn /
  terminal" must handle interleaved steering mid-run (a steering user-turn inside an
  agent turn) — the plan will pin the exact boundary rule with a test.
- **Glyph fallback:** `⏺ › ❖ ⋯ ▸ ▌` must degrade on the 16-color / monochrome themes
  (reuse the existing theme's ASCII fallbacks where a glyph is unavailable).
