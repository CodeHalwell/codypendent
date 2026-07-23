# Streaming Chat Responses Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the chat transcript stream the model's reply token-by-token, with a live "working" status and cadence polish, by turning on the streaming path that already exists end-to-end.

**Architecture:** Approach A — a `DeltaSink` the agent loop owns and passes into `ModelDriver::next_step`. The driver pushes text chunks to the sink as the model generates (via the framework's `get_streaming_response`); the loop turns each chunk into the existing `EventBody::ModelStreamDelta`. The daemon already forwards it and the TUI already coalesces it. The TUI gains a derived activity status and a streaming caret.

**Tech Stack:** Rust; `agent-framework-core`/`agent-framework-openai` (feature `provider-openai`); ratatui TUI (pure reducer); `tokio`; `async_trait`.

## Global Constraints

- No protocol change: `EventBody::ModelStreamDelta { run_id, text }` is unchanged; no new event type.
- Preserve the T1/T7 honesty invariant: `StepOutcome.usage` is measured-or-`None`, never fabricated; cost pricing stays in the daemon node path (untouched here).
- Pure-reducer discipline in `crates/tui`: no I/O; status is derived reducer state, never fetched; `tui` gains no new crate dependency.
- Clippy runs on Linux CI (`cargo clippy --workspace --all-targets --all-features -- -D warnings`) — gate any macOS-only test helper with the same `#[cfg]` as its sole caller, or it is dead code on Linux.
- NEVER edit/stage `README.md` or `docs/cli-and-tui-user-guide.md` (foreign) or anything under `.superpowers/`. Stage only changed files by explicit path; never `git add -A`.
- Commit trailer on every commit: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Full gate green per task: `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-features`.

---

## File structure

- `crates/runtime/src/agent.rs` — Tasks 1 & 2. Add `DeltaSink`; change `next_step` signature; loop emits per-chunk deltas via the sink; `FrameworkModelDriver` streams; `ScriptedDriver` emits scripted chunks.
- `crates/tui/src/state.rs`, `crates/tui/src/reduce.rs` — Task 3. `RunActivity` status field + reducer transitions.
- `crates/tui/src/render.rs` — Tasks 3 & 4. Status row; streaming caret; auto-scroll follow.

No new files; all changes extend existing focused files following their patterns.

---

## Task 1: `DeltaSink` seam — route streamed text through a sink (behavior-equivalent)

Establish the sink and thread it through `next_step`, with the loop emitting `ModelStreamDelta` via the sink and `ScriptedDriver` pushing its `Say` text through it. Net behavior is identical to today (one delta per `Say`), but text now flows through the sink instead of a post-hoc emission — the seam every later task builds on.

**Files:**
- Modify: `crates/runtime/src/agent.rs` (trait `ModelDriver`, `ScriptedDriver`, `FrameworkModelDriver` signature only, the loop's `next_step` call site + the `ModelStep::Say` handling)
- Test: `crates/runtime/src/agent.rs` `#[cfg(test)] mod tests`; `crates/runtime/tests/agent_it.rs` (signature updates)

**Interfaces:**
- Produces: `pub trait DeltaSink: Send { fn on_text(&mut self, chunk: &str); }`
- Produces: `ModelDriver::next_step(&self, transcript: &[TurnItem], sink: &mut dyn DeltaSink) -> anyhow::Result<StepOutcome>` (signature changed — the `sink` param is new)
- Produces: `ScriptedDriver` unchanged public API; internally its `next_step` calls `sink.on_text(&text)` for a `Say` step before returning it.
- Consumes: existing `StepOutcome { step: ModelStep, usage: Option<ModelUsage> }`, `ModelStep::Say(String)`, `EventBody::ModelStreamDelta { run_id, text }`.

- [ ] **Step 1: Write the failing test** (in `agent.rs` tests) — a scripted `Say` run emits exactly one `ModelStreamDelta` carrying the text, routed through the sink.

```rust
#[tokio::test]
async fn a_say_step_streams_its_text_as_a_delta_through_the_sink() {
    // Build a runtime with a ScriptedDriver that says "Hello, world.", run it,
    // and assert the emitted events contain exactly one ModelStreamDelta with
    // that text (the run's model output routed through the DeltaSink).
    let driver = ScriptedDriver::new(vec![
        ModelStep::Say("Hello, world.".to_string()),
        ModelStep::Finish { summary: "done".to_string() },
    ]);
    let (runtime, mut events) = test_runtime_with_driver(driver); // existing test harness helper
    runtime.run_to_completion(test_launch("obj")).await.unwrap();
    let deltas: Vec<String> = drain_deltas(&mut events); // helper: collect ModelStreamDelta.text in order
    assert_eq!(deltas, vec!["Hello, world.".to_string()]);
}
```

(If `test_runtime_with_driver`/`drain_deltas` do not exist, add thin local test helpers next to the existing agent tests that build the runtime and filter emitted `EventBody::ModelStreamDelta`. Reuse whatever harness the existing agent tests use to run a `ScriptedDriver` and capture events.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p codypendent-runtime a_say_step_streams_its_text_as_a_delta_through_the_sink`
Expected: FAIL to compile — `next_step` takes no `sink` argument yet.

- [ ] **Step 3: Add the `DeltaSink` trait and a no-op + loop sink**

```rust
/// Receives natural-language text chunks as the model generates them, so the
/// agent loop can emit a `ModelStreamDelta` per chunk. Text flows through the
/// sink DURING generation; the driver still returns the assembled `StepOutcome`.
pub trait DeltaSink: Send {
    fn on_text(&mut self, chunk: &str);
}

/// A sink that discards chunks — for drivers/tests that do not stream.
pub struct NullDeltaSink;
impl DeltaSink for NullDeltaSink {
    fn on_text(&mut self, _chunk: &str) {}
}
```

- [ ] **Step 4: Change the `next_step` signature on the trait and every impl**

Trait:
```rust
async fn next_step(
    &self,
    transcript: &[TurnItem],
    sink: &mut dyn DeltaSink,
) -> anyhow::Result<StepOutcome>;
```

`ScriptedDriver::next_step` — push a `Say`'s text through the sink, then return the step:
```rust
async fn next_step(
    &self,
    _transcript: &[TurnItem],
    sink: &mut dyn DeltaSink,
) -> anyhow::Result<StepOutcome> {
    let step = { self.steps.lock().expect("scripted driver mutex poisoned").pop_front() }
        .unwrap_or(ModelStep::Finish { summary: "scripted run complete".to_string() });
    if let ModelStep::Say(text) = &step {
        sink.on_text(text);
    }
    Ok(StepOutcome { step, usage: self.usage.clone() })
}
```

`FrameworkModelDriver::next_step` — add the `sink: &mut dyn DeltaSink` parameter to the signature only (keep the existing `get_response` body for now; do not use the sink yet — Task 2 rewrites this body).

- [ ] **Step 5: Update the loop to own a sink that emits `ModelStreamDelta`, and stop the post-hoc `Say` emission**

At the loop's `next_step` call site (`agent.rs` ~line 830), build a sink that captures the events channel + run id and emits a delta per chunk, then pass it in. Because the sink now emits the text, REMOVE the old `ModelStep::Say(text) => { emit ModelStreamDelta … }` emission (the `Say` arm keeps any transcript bookkeeping but must NOT emit a second delta). Sink impl (a local struct in the loop module):

```rust
struct EmittingSink<'a> { runtime: &'a FrameworkAgentRuntime, session_id: SessionId, run_actor: Actor, run_id: RunId, emitted: bool }
impl DeltaSink for EmittingSink<'_> {
    fn on_text(&mut self, chunk: &str) {
        if chunk.is_empty() { return; }
        self.emitted = true;
        self.runtime.emit_blocking(self.session_id, self.run_actor.clone(),
            EventBody::ModelStreamDelta { run_id: self.run_id, text: chunk.to_string() });
    }
}
```

Use the loop's existing emit path. If the loop's `emit` is `async`, buffer chunks on the sink and drain-emit them immediately after `next_step` returns, preserving order (simplest correct option given `on_text` is sync); document the choice in a comment. Add the `TurnItem::Assistant` transcript update for the full `Say` text exactly as today (accumulate the streamed text or use the returned `Say` text).

- [ ] **Step 6: Update call sites in `crates/runtime/tests/agent_it.rs`** — any direct `next_step(&transcript)` call gains `, &mut NullDeltaSink` (or a collecting sink where the test inspects chunks). Show each changed call with the argument added.

- [ ] **Step 7: Run the test + full suite**

Run: `cargo test -p codypendent-runtime`
Expected: PASS (the new test + all existing agent tests, unchanged behavior).

- [ ] **Step 8: Gate + commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git add crates/runtime/src/agent.rs crates/runtime/tests/agent_it.rs
git commit -m "feat(runtime): route model text through a DeltaSink seam (streaming groundwork)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 2: `FrameworkModelDriver` streams via `get_streaming_response`

Replace the driver's blocking `get_response` with `get_streaming_response`, forwarding each update's text delta to the sink and assembling the final `StepOutcome` (text + tool calls + usage) from the updates.

**Files:**
- Modify: `crates/runtime/src/agent.rs` (`FrameworkModelDriver::next_step` body, under `#[cfg(feature = "provider-openai")]`; add a pure `updates_to_step` helper)
- Test: `crates/runtime/src/agent.rs` tests (pure helper unit tests)

**Interfaces:**
- Consumes: `agent_framework_core::client::ChatClient::get_streaming_response(messages, options) -> Result<ChatStream>` where `ChatStream = Pin<Box<dyn Stream<Item = Result<ChatResponseUpdate>> + Send>>`; `ChatResponseUpdate` (`agent_framework_core::types`, defined in `types/response.rs:310`); the existing `to_messages`/`tool_definitions`/response→`ModelStep` mapping already in this file.
- Produces: unchanged `StepOutcome`.

**VERIFY-FIRST (spec open question):** before writing code, read `types/response.rs` around lines 300–540 to confirm (a) the `ChatResponseUpdate` text accessor (e.g. a `text()` method or a `contents`/`UsageContent` field), and (b) the framework's update→response assembler (there is an assembly path near `into_chat_update`/the tests at 640-647 — e.g. a `ChatResponse::from_updates`-style coalescer). Use the framework's assembler if present; otherwise hand-accumulate text + tool calls + usage. Mirror the EXISTING non-streaming code in this file that maps a `ChatResponse` to `ModelStep`/usage — reuse it on the assembled response so the tool-call and usage handling are identical to today.

- [ ] **Step 1: Write the failing test** — a pure helper that folds a vec of updates into (streamed-text chunks, final `ModelStep`, `Option<ModelUsage>`), proving text is chunked and usage assembled.

```rust
#[test]
fn updates_fold_into_streamed_chunks_and_a_final_step_with_usage() {
    // Two text updates then a usage-bearing final update assemble into:
    // - chunks ["Hel", "lo"] pushed in order,
    // - ModelStep::Say("Hello"),
    // - Some(ModelUsage { prompt_tokens: 3, completion_tokens: 2, cost_micros: None }).
    let updates = vec![
        text_update("Hel"),
        text_update("lo"),
        usage_update(3, 2), // helper building a ChatResponseUpdate carrying usage
    ];
    let mut chunks = Vec::new();
    let (step, usage) = updates_to_step(updates, |c| chunks.push(c.to_string()));
    assert_eq!(chunks, vec!["Hel".to_string(), "lo".to_string()]);
    assert!(matches!(step, ModelStep::Say(t) if t == "Hello"));
    assert_eq!(usage, Some(ModelUsage { prompt_tokens: 3, completion_tokens: 2, cost_micros: None }));
}
```

(Adjust `text_update`/`usage_update` to the real `ChatResponseUpdate` constructors confirmed in VERIFY-FIRST. `updates_to_step` is a pure fn: `fn updates_to_step(updates: Vec<ChatResponseUpdate>, mut on_text: impl FnMut(&str)) -> (ModelStep, Option<ModelUsage>)`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p codypendent-runtime --features provider-openai updates_fold_into_streamed_chunks`
Expected: FAIL — `updates_to_step` not defined.

- [ ] **Step 3: Implement `updates_to_step` and call it from `next_step`**

Write `updates_to_step` (pure: iterate updates, call `on_text` for each text delta, accumulate full text + tool calls + usage, map to `ModelStep` exactly as the existing `ChatResponse`→`ModelStep` code does). Then rewrite `FrameworkModelDriver::next_step`:

```rust
async fn next_step(&self, transcript: &[TurnItem], sink: &mut dyn DeltaSink)
    -> anyhow::Result<StepOutcome> {
    use futures::StreamExt;
    let mut options = ChatOptions::new();
    options.tools = Self::tool_definitions();
    let mut stream = self.client
        .get_streaming_response(Self::to_messages(transcript), options).await
        .map_err(|e| anyhow::anyhow!("model stream failed: {e}"))?;
    let mut updates = Vec::new();
    while let Some(update) = stream.next().await {
        updates.push(update.map_err(|e| anyhow::anyhow!("model stream error: {e}"))?);
    }
    let (step, usage) = updates_to_step(updates, |chunk| sink.on_text(chunk));
    Ok(StepOutcome { step, usage })
}
```

(If streaming text should reach the sink AS each update arrives rather than after collecting — preferred for true liveness — call `sink.on_text(...)` inside the `while` loop as each text delta arrives, and accumulate the non-text parts for a final assembly pass. Choose the incremental form; the collected form above is the fallback if assembly needs the whole vec. Keep `updates_to_step` pure for the test by having the incremental path delegate text extraction to the same helper logic.)

- [ ] **Step 4: Run the test**

Run: `cargo test -p codypendent-runtime --features provider-openai updates_fold_into_streamed_chunks`
Expected: PASS.

- [ ] **Step 5: Stream-error handling (no new test — reuses the tested failure path)**

Confirm (by reading the loop) that a `next_step` returning `Err` already fails the run cleanly with the error as the reason — this path is already covered by the existing "driver error fails the run" agent test. The `while … stream.next()` loop's `?` turns a mid-stream error into exactly that `Err`, so the stream-error case maps onto the existing tested failure path; text chunks already pushed to the sink before the error remain in the transcript by construction (they were emitted as they arrived). Add a one-line comment at the `?` documenting this. Do NOT fabricate usage on error — `updates_to_step` is never reached, so `StepOutcome`/usage is simply not produced (the `Err` returns first).

- [ ] **Step 6: Gate + commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git add crates/runtime/src/agent.rs
git commit -m "feat(runtime): stream provider tokens via get_streaming_response

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 3: TUI live activity status (`RunActivity`)

Add a derived per-run activity status the reducer maintains and the renderer shows as a dim row, so there is never an unexplained pause.

**Files:**
- Modify: `crates/tui/src/state.rs` (add `RunActivity` enum + `activity` field on `RunView`)
- Modify: `crates/tui/src/reduce.rs` (set/clear `activity` when folding run-state, delta, and tool events)
- Modify: `crates/tui/src/render.rs` (render the status row)
- Test: `crates/tui/src/reduce.rs` and `render.rs` `#[cfg(test)]` tests

**Interfaces:**
- Produces: `pub enum RunActivity { Idle, Thinking, Streaming, RunningTool(String) }` (default `Idle`); `RunView.activity: RunActivity`.
- Consumes: existing folds for `EventBody::RunStateChanged`, `ModelStreamDelta`, tool lifecycle events, and terminal (`RunCompleted`/terminal `RunStateChanged`).

- [ ] **Step 1: Write the failing reducer test**

```rust
#[test]
fn run_activity_tracks_thinking_streaming_tool_and_idle() {
    let mut s = AppState::new();
    let run_id = RunId::new();
    reduce(&mut s, system_ev(EventBody::RunStarted { run_id, objective: "o".into(), mode: AgentMode::Build }));
    reduce(&mut s, system_ev(EventBody::RunStateChanged { run_id, state: RunState::Running }));
    assert_eq!(s.run(run_id).activity, RunActivity::Thinking);
    reduce(&mut s, ev(agent_actor(run_id), EventBody::ModelStreamDelta { run_id, text: "hi".into() }));
    assert_eq!(s.run(run_id).activity, RunActivity::Streaming);
    reduce(&mut s, system_ev(EventBody::RunCompleted { run_id, disposition: completed("done") }));
    assert_eq!(s.run(run_id).activity, RunActivity::Idle);
}
```

(Use the test's existing `RunState`/disposition/`s.run(..)` accessors; match their real names.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p codypendent-tui run_activity_tracks`
Expected: FAIL — `RunActivity`/`activity` not defined.

- [ ] **Step 3: Add the enum + field + reducer transitions**

```rust
// state.rs
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RunActivity { #[default] Idle, Thinking, Streaming, RunningTool(String) }
// on RunView: pub activity: RunActivity,
```

In `reduce.rs`, when folding into the run: `RunStateChanged{Preparing|Running}` ⇒ `Thinking`; first/any `ModelStreamDelta` ⇒ `Streaming`; a tool card entering `Running` ⇒ `RunningTool(tool_name)`; that tool `Completed` ⇒ back to `Thinking`; `RunCompleted`/terminal state ⇒ `Idle`. Set the field on the matching `RunView`.

- [ ] **Step 4: Add the render test + render the row**

```rust
#[test]
fn a_thinking_run_shows_a_working_status_row() {
    let mut s = AppState::new();
    // ... start a run, set Running (Thinking), no deltas ...
    let out = render_to_string(&s, 80, 20);
    assert!(out.contains("working"));
}
```

Render (`render.rs`, in the transcript/footer area for the active run): a dim row — `Thinking ⇒ "working…"`, `RunningTool(n) ⇒ "running {n}…"`, `Streaming`/`Idle ⇒ no row`. Reuse the theme's muted style.

- [ ] **Step 5: Run tests**

Run: `cargo test -p codypendent-tui run_activity_tracks a_thinking_run_shows_a_working_status_row`
Expected: PASS.

- [ ] **Step 6: Gate + commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git add crates/tui/src/state.rs crates/tui/src/reduce.rs crates/tui/src/render.rs
git commit -m "feat(tui): live run-activity status (thinking/streaming/running-tool)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 4: TUI streaming caret + cadence + auto-scroll follow

Render a caret on the in-progress model cell while streaming, at the existing frame cadence, and keep auto-scroll following the growing text.

**Files:**
- Modify: `crates/tui/src/render.rs` (caret on the streaming cell; auto-scroll follow of the growing cell)
- Test: `crates/tui/src/render.rs` `#[cfg(test)]` tests

**Interfaces:**
- Consumes: `RunView.activity` (Task 3) — the cell is streaming when `activity == Streaming`; the model transcript cell built from coalesced `ModelStreamDelta`s (`reduce.rs:264`); the existing auto-scroll/follow logic.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_streaming_cell_shows_a_caret_then_drops_it_on_completion() {
    let mut s = AppState::new();
    let run_id = RunId::new();
    // start run, Running, one ModelStreamDelta "partial" => Streaming
    // ... (mirror the reduce test setup) ...
    let mid = render_to_string(&s, 80, 20);
    assert!(mid.contains("partial"));
    assert!(mid.contains('▋'), "streaming cell shows a caret");
    reduce(&mut s, system_ev(EventBody::RunCompleted { run_id, disposition: completed("partial") }));
    let done = render_to_string(&s, 80, 20);
    assert!(!done.contains('▋'), "caret is gone once the run completes");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p codypendent-tui a_streaming_cell_shows_a_caret`
Expected: FAIL — no caret rendered.

- [ ] **Step 3: Implement the caret + follow**

In `render.rs` where the model/assistant transcript cell renders: when the owning run's `activity == RunActivity::Streaming`, append a `▋` caret after the accumulated text (styled muted). When not streaming, render text as today. Ensure the auto-scroll "follow latest" path treats the growing streaming cell as the bottom so the caret stays visible while following (verify against the existing auto-scroll code; adjust the measured bottom to include the streaming cell).

- [ ] **Step 4: Run tests**

Run: `cargo test -p codypendent-tui a_streaming_cell_shows_a_caret`
Expected: PASS.

- [ ] **Step 5: Full gate + commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git add crates/tui/src/render.rs
git commit -m "feat(tui): streaming caret + auto-scroll follow for live responses

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## After all tasks

- Whole-branch review (adversarial, per subagent-driven-development), then push (CodeHalwell account, restore synextra) and open a PR to `main`, left for the user's review.
- **Prerequisite to observe the feature:** rebuild and restart the daemon (the running Jul-17 process predates even the single-delta emission), then run against Ollama and confirm the reply streams.
