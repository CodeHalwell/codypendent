# Streaming chat responses — design

**Date:** 2026-07-23 · **Status:** approved (pre-implementation) · **Branch:** `claude/streaming-chat`

## Problem

The chat transcript currently reads like a batch job: after the user sends, nothing
appears until the model's whole reply is ready, which then lands at once. It should
feel alive — the response typing out as it's generated, with a visible "working" state
so there's never a dead pause.

The streaming *pipeline* already exists end-to-end and is unused only at the source:

- `EventBody::ModelStreamDelta { run_id, text }` — a real protocol event
  (`crates/protocol/src/events.rs:94`, round-trip tested).
- The agent loop emits it (`crates/runtime/src/agent.rs:866`) when a step yields
  `ModelStep::Say(text)`.
- The daemon forwards it (`crates/daemon/src/server.rs:1872`); the TUI coalesces and
  renders it (`crates/tui/src/reduce.rs:264`, `crates/tui/src/render.rs:2524`); the CLI
  (`crates/cli/src/stream.rs:180`) and ACP (`crates/cli/src/acp.rs:165`) consume it.

**The gap:** `FrameworkModelDriver::next_step` calls the framework's blocking
`get_response` (`crates/runtime/src/agent.rs:~2289`) and returns the *complete* reply,
which the loop emits as a **single** `ModelStreamDelta` carrying the full text. The
framework also exposes `get_streaming_response` → `ChatStream`
(`agent_framework_core::client`, a `Stream<Item = Result<ChatResponseUpdate>>`), which
is not used. Ollama's OpenAI-compatible endpoint supports SSE streaming, so a local
model will stream once we ask for it.

## Goals

1. The model's prose streams token-by-token into the transcript as generated.
2. A live "working…" status so there's never an unexplained pause (before the first
   token, and between steps / during tool execution).
3. Cadence polish: a streaming caret on the in-progress cell, smooth rendering of
   bursty chunks, and auto-scroll that follows the streaming text.

## Non-goals

- No new provider support; `openai-compatible` only (unchanged).
- No protocol change — `ModelStreamDelta` already carries what we need.
- No change to tool-execution semantics, the approval flow, or the T1/T7 usage/cost
  accounting (usage still arrives once, in the final chunk).
- Mid-run *model switching* and other picker work are out of scope.

## Approach A — a delta sink on `next_step` (chosen)

Keep the loop as the sole event-emitter (as it is today); give the driver a way to push
text chunks *out* to the loop while it streams, and still return the assembled
`StepOutcome` at the end.

Rejected alternatives: **B** — `next_step` returns a stream and the loop assembles the
final step itself (moves framework-owned assembly into our loop; more churn; complicates
the scripted test driver). **C** — the driver emits `ModelStreamDelta` directly (couples
the runtime driver to the daemon event system and breaks the "driver returns data, loop
emits events" boundary).

## Design

### Part 1 — Runtime streaming (`crates/runtime/src/agent.rs`)

**`DeltaSink` seam.** Introduce a minimal sink the loop owns and passes into the driver:

```rust
/// Receives natural-language text chunks as the model generates them, so the loop
/// can emit a `ModelStreamDelta` per chunk. A no-op sink preserves today's behavior.
pub trait DeltaSink: Send {
    fn on_text(&mut self, chunk: &str);
}
```

**Trait change.** `next_step` gains a sink parameter:

```rust
async fn next_step(&self, transcript: &[TurnItem], sink: &mut dyn DeltaSink)
    -> anyhow::Result<StepOutcome>;
```

- **`FrameworkModelDriver`** calls `get_streaming_response`; for each
  `ChatResponseUpdate`, it forwards any text delta to `sink.on_text(chunk)` and
  accumulates updates into the final `ChatResponse`-equivalent (full text + tool calls +
  usage). It returns the same `StepOutcome { step, usage }` it does today — so the loop's
  downstream handling (tool dispatch, `Finish`, usage) is unchanged. Text arrives via the
  sink *during* generation; tool-call and usage assembly complete when the stream ends.
- **`ScriptedDriver`** calls `sink.on_text(...)` with scripted chunks (default: emit the
  `Say` text as one or more chunks) so tests exercise the streaming path deterministically.

**Loop change.** Where the loop calls `next_step`, it passes a sink whose `on_text`
emits `EventBody::ModelStreamDelta { run_id, text: chunk }`. The existing single-delta
emission for `ModelStep::Say` (agent.rs:862-866) is removed/subsumed: text now flows via
the sink as it streams, so a `Say` step no longer needs a post-hoc delta. (A `Say` step
whose text was fully streamed must not be double-emitted — see Error handling.)

**Usage / honesty (T1/T7) unchanged.** Usage is read from the final assembled response
(the last chunk) into `StepOutcome.usage`; `None` when the stream reports none. No
fabricated values; cost pricing stays downstream in the daemon node path.

### Part 2 — Live status (`crates/tui`, synthesized; no protocol change)

The TUI derives a transient status line for the active run from signals it already folds:

| Condition | Status shown |
|---|---|
| Run `Preparing`/`Running`, no stream started, no tool in flight | `working…` (i.e. "thinking") |
| A `ModelStreamDelta` is actively arriving | (no status — the streaming text itself is the signal) |
| A tool card is `Running` | `running <tool>…` (e.g. `running cargo test…`) |
| Run reached a terminal state | status cleared |

Implementation: a reducer-maintained `status: RunActivity` field on the active `RunView`
(an enum — `Thinking` / `Streaming` / `RunningTool(name)` / `Idle`), set and cleared by the
reducer as it folds `RunStateChanged` (→ `Thinking` on Preparing/Running), `ModelStreamDelta`
(first delta → `Streaming`), tool lifecycle events (Running → `RunningTool`, Completed →
back to `Thinking`), and terminal events (→ `Idle`, status row hidden). Render reads this one
field into a dim status row beneath the in-progress cell. No new protocol event — YAGNI; all
the driving signals are already folded. Keeps the pure-reducer discipline (no I/O in the TUI
crate); the field is derived state, never fetched.

### Part 3 — Cadence polish (`crates/tui/src/render.rs`)

- **Streaming caret.** While a run's model cell is mid-stream (deltas arriving, not yet
  `Completed`), render a caret (`▋`) at the end of the accumulated text; drop it on
  completion.
- **Smooth cadence.** Deltas already coalesce into the cell (reduce.rs:264). Render the
  accumulated text at the TUI's existing frame cadence rather than forcing a redraw per
  chunk, so Ollama's bursty chunks read as steady typing.
- **Auto-scroll-follow.** The existing auto-scroll (follows latest; PgUp to leave follow)
  must track the growing streamed cell so the caret stays in view while following.

## Components & interfaces

- `DeltaSink` (runtime) — 1-method trait; a loop-owned impl emits `ModelStreamDelta`; a
  no-op/collecting impl for tests. Single responsibility: carry text chunks out.
- `ModelDriver::next_step(transcript, sink)` — the one signature change; all impls
  (`FrameworkModelDriver`, `ScriptedDriver`, any mocks) and the single loop call site
  update. Same ripple shape as the T7 `StepOutcome` change (contained to `agent.rs`).
- TUI status field — computed/reducer-maintained; consumed only by render.
- TUI streaming caret + cadence — render-only, keyed off cell stream-in-progress state.

## Data flow

model chunk (Ollama SSE) → framework `ChatStream` update → `FrameworkModelDriver`
(`sink.on_text(chunk)` + accumulate) → loop sink → `EventBody::ModelStreamDelta` →
daemon forward → TUI `reduce` (coalesce into the run's model cell; set streaming flag) →
`render` (accumulated text + caret at frame cadence; status row; auto-scroll follows).
At stream end: driver returns `StepOutcome` → loop dispatches tools / `Finish` / records
usage exactly as today; TUI clears the caret/status on the terminal event.

## Error handling

- **Stream error mid-generation:** the driver returns the same `Err` shape `get_response`
  would; the loop fails the step cleanly (as today). Any text already emitted via the
  sink stays in the transcript (partial), and the run's failure reason is recorded —
  consistent with fail-closed behavior. Usage is `None` on a broken stream.
- **No double-emit:** because text now streams via the sink, the loop must NOT also emit
  a full-text delta for a `Say` step. Guard: the sink is the only text-delta source; the
  `Say` branch stops emitting its own delta.
- **Empty stream / no text (pure tool-call step):** no deltas emitted; behaves as today
  (tool card appears, no model prose).

## Testing

- **Runtime:** `ScriptedDriver` emits scripted chunks → the loop emits one
  `ModelStreamDelta` per chunk (assert order + coalesced text equals the full message);
  a `Say`-only step is not double-emitted; usage still flows into `StepOutcome`; a stream
  error fails the step with partial text preserved.
- **TUI reduce:** multiple `ModelStreamDelta`s coalesce into one growing cell (extends the
  existing `model_stream_deltas_coalesce` test); the streaming flag sets on first delta and
  clears on terminal; status transitions (working → streaming → tool → cleared).
- **TUI render:** `render_to_string` shows the caret mid-stream and not after completion;
  the `working…` status row appears when Running-with-no-stream; auto-scroll follows.
- **End-to-end (manual/gated):** against the rebuilt daemon + Ollama, a run's reply
  visibly streams. (Requires the daemon restart — see Prerequisite.)
- All existing tests stay green (`cargo fmt`/`clippy --workspace --all-targets
  --all-features -D warnings`/`cargo test --workspace --all-features`).

## Constraints

- No protocol change (`ModelStreamDelta` unchanged); additive Rust changes otherwise.
- Preserve the T1/T7 honesty invariant (usage measured-or-`None`, never fabricated).
- Pure-reducer discipline in the TUI (no I/O); status is derived, not fetched.
- Clippy runs on Linux CI — gate any macOS-only test helper.
- Foreign files (`README.md`, `docs/cli-and-tui-user-guide.md`) never touched.

## Prerequisite (to observe the feature)

The running daemon is the Jul-17 build and predates even the single-delta emission;
streaming will not appear until the daemon is rebuilt from this branch and restarted.
This is an operational step, not part of the code change.

## Open questions / risks

- **Framework update shape:** confirm `ChatResponseUpdate` cleanly separates text deltas
  from tool-call deltas and that the framework offers (or we write) a small helper to
  assemble updates into the final response + usage. If assembly is awkward, the driver
  hand-accumulates — still within Approach A.
- **Caret redraw cost:** frame-cadence rendering should bound redraws; verify no busy-loop
  when a stream stalls (the caret can blink on the existing frame timer, not per chunk).
