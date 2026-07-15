# Phase 1 — Persistent Coding-Agent Slice

> **Objective:** the user story from the roadmap, working end to end:
>
> *"Open a repository, ask an agent to diagnose a failing test, approve commands, inspect a patch, rerun tests, close the TUI, reconnect, and continue."*
>
> **Specification chapters:** [Roadmap Phase 1](../15-roadmap.md), [Daemon and Client Protocol](../03-daemon-client-protocol.md), [Agent Runtime and Workflows](../04-agent-runtime-and-workflows.md), [Models, Routing, and Compaction](../09-model-routing-and-compaction.md), [Security and Governance](../11-security-and-governance.md), [`agent-framework-rs` Integration](../12-agent-framework-rs-integration.md), [Core Data Contracts](../14-core-data-contracts.md), [Interaction and Autonomy Model](../20-interaction-and-autonomy-model.md), [Testing Strategy](../16-testing-strategy.md).
>
> **Exit criteria (from the roadmap):** client disconnect does not stop the run; duplicate command delivery does not duplicate an effect; daemon restart recovers or cleanly marks the run; the patch is reviewable and attributable; worktree cleanup protects unmerged work. Plus the Phase 1 competitive overlay: Explore/Plan/Build/Review modes, status line, JSONL, chronicle v0, change-set review v0, safe-point steering.

Unlike Phase 0, this chapter specifies **modules, schemas, behaviours, and tests** rather than full literal file bodies. Where a Rust snippet appears, its names and semantics are normative (guide rule 4); routine bodies are yours to write. Work top to bottom; each STEP ends with tests that must pass before the next STEP.

## New crates in this phase

**EDIT FILE `Cargo.toml`** — add to `members`: `"crates/runtime"`, `"crates/tui"`. Add workspace dependencies:

```toml
codypendent-runtime = { path = "crates/runtime" }
codypendent-tui = { path = "crates/tui" }
agent-framework-openai = "0.1.1"
agent-framework-anthropic = "0.1.1"
ratatui = "0.29"
crossterm = "0.28"
sha2 = "0.10"
hex = "0.4"
async-trait = "0.1"
toml = "0.8"
```

- `codypendent-runtime` — agent runs, tools, approvals bridge, model integration, context, compaction. This crate (and only this crate) depends on the framework crates, behind features: in the runtime crate's manifest declare `agent-framework-openai = { workspace = true, optional = true }` and `agent-framework-anthropic = { workspace = true, optional = true }`, with features `provider-openai = ["dep:agent-framework-openai"]` (in `default`) and `provider-anthropic = ["dep:agent-framework-anthropic"]` (off). Every `dep:` feature target must be a declared optional dependency — that is why both crates appear in the workspace list above even though only OpenAI is enabled (ADR-009: selected crates, never the umbrella `full`).
- `codypendent-tui` — rendering, input, layout, components, themes. Depended on by `codypendent-cli`; contains **no** direct database or network code — it speaks only protocol types.

## STEP 1.1 — Schema: migration 0002

**CREATE FILE `migrations/0002_phase1.sql`** with exactly these tables (column types shown are normative; add `CREATE INDEX` statements where noted):

```sql
CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    objective TEXT NOT NULL,
    state TEXT NOT NULL,              -- RunState as string
    mode TEXT NOT NULL,               -- AgentMode as string
    model_policy TEXT NOT NULL,
    workspace_lease_id TEXT,
    budget_json TEXT NOT NULL,
    started_at TEXT,
    ended_at TEXT
);
CREATE INDEX idx_runs_session ON runs(session_id);

CREATE TABLE commands (
    id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    session_id TEXT,
    client_id TEXT NOT NULL,
    body TEXT NOT NULL,               -- CommandBody JSON
    status TEXT NOT NULL,             -- received | applied | rejected
    result_json TEXT,
    received_at TEXT NOT NULL,
    applied_at TEXT
);

CREATE TABLE pending_effects (
    id TEXT PRIMARY KEY,
    command_id TEXT NOT NULL REFERENCES commands(id),
    kind TEXT NOT NULL,               -- e.g. shell, git-commit, file-write
    intent_json TEXT NOT NULL,
    state TEXT NOT NULL,              -- intended | performed | reconciled | abandoned
    created_at TEXT NOT NULL,
    resolved_at TEXT
);

CREATE TABLE approvals (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    action_json TEXT NOT NULL,        -- ProposedAction
    risk_json TEXT NOT NULL,
    capabilities_json TEXT NOT NULL,
    state TEXT NOT NULL,              -- pending | approved | rejected | expired
    scope TEXT NOT NULL,              -- once | run | pattern | repository
    resolved_by TEXT,
    requested_at TEXT NOT NULL,
    resolved_at TEXT,
    expires_at TEXT
);

CREATE TABLE artifacts (
    id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    media_type TEXT NOT NULL,
    byte_length INTEGER NOT NULL,
    classification TEXT NOT NULL,     -- DataClassification
    created_at TEXT NOT NULL,
    provenance_json TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_artifacts_hash ON artifacts(sha256, media_type);

CREATE TABLE workspace_leases (
    id TEXT PRIMARY KEY,
    repository_path TEXT NOT NULL,
    worktree_path TEXT NOT NULL UNIQUE,
    branch TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    owner_run_id TEXT NOT NULL REFERENCES runs(id),
    mode TEXT NOT NULL,               -- write | read
    state TEXT NOT NULL,              -- active | released | orphaned
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT
);
```

**RULES**

1. Migrations are append-only; `0001_init.sql` is untouched.
2. Every JSON column stores one serde-serialized protocol/runtime type; never ad-hoc JSON.

**TESTS** — extend the migration test implicitly: `cargo test` still green (migrations apply on a fresh DB).

**COMMIT** `"phase1: schema for runs, commands, approvals, artifacts, leases"`

## STEP 1.2 — Protocol expansion

Extend `codypendent-protocol` (additive only — Phase 0 payloads keep working; protocol stays 1.x):

1. `ClientCapabilities` exactly as in [Chapter 02](../02-system-architecture.md) (rich_text, image_display, audio_capture, editor_mutations, diff_view, mouse, unicode, true_color — all `bool`).
2. `ClientHello { client_name, client_version, supported_protocols, capabilities, resume_token }` and `ServerHello { selected_protocol, daemon_version, daemon_instance, heartbeat_interval_ms }`; handshake is the first exchange on every connection. Add `ClientRole` (`Observer | Contributor | Controller | Approver`) and `Subscription` enum (`SessionSummary | RunTrace{run_id} | AgentActivity | RepositoryStatus | BudgetState`) from [Chapter 03](../03-daemon-client-protocol.md).
3. `Command { command_id, idempotency_key: String, expected_revision: Option<u64>, body: CommandBody }` with `CommandBody`: `CreateSession{workspace, title}`, `AttachSession{session_id, last_seen_sequence, subscriptions, requested_role}`, `SubmitUserInput{session_id, text, mode}`, `StartRun{session_id, objective, mode}`, `ResolveApproval{approval_id, decision, scope}`, `CancelRun{run_id}`, `PauseRun{run_id}`, `ResumeRun{run_id}`, `QueueSteering{run_id, text}`.
4. `Catchup` enum exactly per Chapter 03: `Events{from, through, events}` or `Snapshot{through, projection}`. Rule: if the client is ≤ 500 events behind, send `Events`; otherwise `Snapshot`.
5. `EventBody` additions: `RunStarted{run_id, objective, mode}`, `RunStateChanged{run_id, state}`, `ModelStreamDelta{run_id, text}`, `ToolProposed{run_id, approval_id, action}`, `ToolStarted{run_id, tool, args_digest}`, `ToolCompleted{run_id, tool, outcome, artifact}`, `PatchProposed{run_id, changeset_id, artifact}`, `ApprovalRequested{approval_id, action, risk}`, `ApprovalResolved{approval_id, decision}`, `SteeringQueued{run_id}`, `SteeringApplied{run_id}`, `BudgetWarning{run_id, dimension, used, limit}`, `RunCompleted{run_id, disposition, chronicle}`.
6. `ArtifactRef { id, media_type, byte_length, sha256, sensitivity }` ([Chapter 03](../03-daemon-client-protocol.md)); events embed `ArtifactRef`, never inline bulk content.
7. Structured error grows to the full [Chapter 14](../14-core-data-contracts.md) shape: `CodypendentError { code, message, retryable, user_action, details, correlation_id }`.

**RULES**

1. Unknown enum variants must not crash a client: give every protocol enum `#[non_exhaustive]` and handle `_` arms in consumers.
2. Events are only ever produced by the daemon **after** persistence (see STEP 1.3).
3. Clients send **semantic** input (whole text submissions, approval decisions) — never keystrokes.

**TESTS** — round-trip serde tests for every new payload; a fixture-corpus test that deserializes `crates/test-support/fixtures/events-basic.jsonl` (Phase 0 bytes) still passes — old events must parse forever.

**COMMIT** `"phase1: protocol v1.1 payloads, subscriptions, catchup, artifact refs"`

## STEP 1.3 — Command handling and the crash-consistent write path

In `codypendent-daemon`, create `commands.rs` implementing the six-step sequence from [Chapter 03](../03-daemon-client-protocol.md). This is the single most important algorithm in the product; implement it exactly:

```text
1. validate the command (schema, session exists, role allows it)
2. BEGIN TRANSACTION:
     insert into commands (status = received)
     insert any pending_effects rows describing intended external effects
     append resulting ledger events
     update projections' backing rows (runs, approvals, ...)
     set commands.status = applied
   COMMIT
3. perform the external side effect (if any), outside the transaction
4. persist the outcome (update pending_effects → performed, append outcome event)
5. publish events to subscribed clients (in-memory broadcast)
```

**RULES**

1. **Idempotency:** before step 1, look up `idempotency_key`. If present with `status = applied`, return the recorded `result_json` without re-executing anything. If present with `status = received` (crash mid-apply), resume reconciliation, not re-execution. This is the exit criterion "duplicate command delivery does not duplicate an effect".
2. **Persist before publish** (CONTRIBUTING principle): no client may observe an event that is not yet committed.
3. Event `sequence` is allocated inside the same transaction that appends the event.
4. On daemon startup, scan `pending_effects` in state `intended`/`performed`-without-outcome and reconcile: check the real world (did the file get written? does the commit exist?), then mark `reconciled` or `abandoned` and emit a reconciliation event.
5. Every command handler returns `CodypendentError` codes, never panics, on bad input.

**TESTS**

- `duplicate_command_is_idempotent`: submit the same `StartRun` command envelope twice → exactly one run row, both replies carry the same result.
- `crash_between_persist_and_effect`: simulate by inserting a command + pending effect, then run recovery → effect reconciled, no duplicate.
- Property test (per [Chapter 16](../16-testing-strategy.md)): replaying the event ledger produces identical projections.

**COMMIT** `"phase1: idempotent command pipeline with crash-consistent write path"`

## STEP 1.4 — Content-addressed artifact store

In `codypendent-daemon`, create `artifacts.rs`:

```rust
pub struct ArtifactStore { root: PathBuf }  // <data_dir>/artifacts

impl ArtifactStore {
    /// Write: stream to `<root>/tmp/<uuid>`, hash with SHA-256 while writing,
    /// then rename to `<root>/sha256/<first two hex chars>/<full hex>`.
    /// Insert the artifacts row in the same call. Returns the ArtifactRef.
    pub async fn put(&self, pool, media_type, classification, bytes) -> Result<ArtifactRef>;
    pub async fn open(&self, id) -> Result<tokio::fs::File>;
    pub async fn verify(&self, id) -> Result<bool>; // re-hash equals stored hash
}
```

**RULES**

1. Same content + same media type ⇒ same stored file (dedup via the unique index); a second `put` returns the existing ref.
2. Rename-into-place makes writes atomic; a crash leaves only `tmp/` garbage, which startup sweeps.
3. Nothing above 64 KiB is ever embedded in an event; store it and reference it.

**TESTS** — put/open round-trip; hash verification; dedup; tmp-sweep on startup.

## STEP 1.5 — Policy engine and capabilities (MVP)

In `codypendent-daemon`, create `policy/` implementing the [Chapter 11](../11-security-and-governance.md) model, scoped down to what Phase 1 tools need:

1. `Capability` enum: `FileRead(PathScope)`, `FileWrite(PathScope)`, `CommandExecute(CommandScope)`, `NetworkConnect(NetworkScope)`, `GitCommit`, `GitPush`.
2. `PathScope` = list of canonicalized root directories. **Canonicalize before every check** (resolve `..` and symlinks with `std::fs::canonicalize` on the nearest existing ancestor); a path is in scope only if its canonical form starts with a canonical root. Deny paths matching the policy's `deny` list even inside allowed roots.
3. Policy loading: `[repo]/.codypendent/policy.toml` merged over `<config_dir>/codypendent/policy.toml` merged over built-in defaults, using the exact key shapes of [`specs/policy.toml`](../../specs/policy.toml) (`[filesystem] read/write/deny`, `[shell] allowed_programs/maximum_seconds`, `[network] allow/default`, `[git] commit/push`). Variables `$REPOSITORY` and `$WORKTREE` expand at evaluation time.
4. Merge rule (invariant): narrower scopes may **restrict** further or set preferences; they may never widen a higher scope's security restriction. Deny wins over allow. Unknown keys are an error, not a warning.
5. `PolicyDecision { decision: Allow | Deny | RequireApproval, reasons, capability_grant, policy_version }` per [Chapter 14](../14-core-data-contracts.md).
6. Built-in defaults (used when no policy file exists): read = repository; write = worktree only; shell = `["cargo", "git", "rg", "rustfmt"]` with approval; network = deny; git commit/push = approval.

**TESTS** (all from the Chapter 16 security list): path traversal (`../../etc/passwd` rejected), symlink escape (symlink inside worktree pointing outside → rejected), deny-precedence (`.git` under an allowed root still denied), lower-scope-cannot-widen property test.

**COMMIT** `"phase1: policy engine, capability grants, path canonicalization"`

## STEP 1.6 — Approval broker

`approvals.rs` in the daemon: approvals are **workflow states**, not UI modals ([Chapter 04](../04-agent-runtime-and-workflows.md)).

1. `request(run_id, action, risk, capabilities) -> ApprovalId`: persists the row (state `pending`), appends `ApprovalRequested`, and parks the awaiting task on a `tokio::sync::oneshot`/watch keyed by `ApprovalId`.
2. `resolve(approval_id, decision, scope, resolved_by)`: transactional update + `ApprovalResolved` event + wake the waiter. Scope `run` records a pattern so subsequent identical proposals in this run auto-approve; scope `once` does not.
3. Expiry: a background task expires `pending` approvals past `expires_at` (state `expired` behaves as rejection).
4. **Recovery:** on daemon restart, `pending` approvals are re-emitted to newly attached clients; waiting runs resume waiting (state machine `WaitingForApproval` — nothing is lost).

**TESTS** — approve/reject round-trip; run-scoped auto-approval; restart with a pending approval leaves the run in `WaitingForApproval` and re-surfaces the request.

## STEP 1.7 — The tool layer

Create `codypendent-runtime` with `tools/` implementing four tools. Each tool declares its required capabilities; the middleware (STEP 1.10) enforces policy → approval → grant before execution and converts big outputs to artifacts after.

| Tool | Signature (conceptual) | Required capability | Notes |
|---|---|---|---|
| `workspace.read_file` | `(path, range?) → excerpt` | `FileRead` | Max 200 lines per call unless range given; returns line-numbered text |
| `workspace.search` | `(pattern, glob?) → matches` | `FileRead` | Shell out to `rg --json -n`; cap 200 matches; parse into typed matches |
| `shell.run` | `CommandRequest → {exit_code, stdout_ref, stderr_ref, salient}` | `CommandExecute` | See rules below |
| `git.diff` / `git.apply_patch` | worktree diff / apply | `FileWrite` + `CommandExecute(git)` | `apply_patch` runs `git apply --check` first; refuses on failure |

**RULES for `shell.run`** (straight from [Chapter 11](../11-security-and-governance.md)):

1. Input is the structured `CommandRequest { program, args, cwd, environment, timeout }` — never an unparsed shell string. (A `shell_interpreter` escape hatch exists but always requires explicit approval.)
2. `program` must be in the policy's allowlist; `cwd` must be inside the grant's `PathScope`; environment starts **empty** plus explicitly allowed bindings; no inherited secrets.
3. Enforce timeout (kill the process group), output caps (1 MiB in memory, overflow streamed to artifacts), and record exit code + duration.
4. Output goes through **observation compaction (Level 1)** ([Chapter 09](../09-model-routing-and-compaction.md)): the model sees command, exit code, salient lines (first/last 40 + error-matching lines), and the artifact reference — never 10 MB of raw log.

**TESTS** — allowlist rejection; timeout kill; env isolation (a canary env var of the daemon must not appear in `env` output); output-cap artifact spill; `read_file` refusing an out-of-scope path.

**COMMIT** `"phase1: file, search, shell, git tools under capability enforcement"`

## STEP 1.8 — Worktree manager

`worktrees.rs` in the daemon, per [Chapter 04](../04-agent-runtime-and-workflows.md) and [ADR-006](../17-architecture-decisions.md):

1. `allocate(repository, run_id) -> WorkspaceLease`: creates branch `codypendent/run-<short-run-id>` at current HEAD and `git worktree add <repo>/../codypendent-worktrees/<repo-name>/run-<short-id> <branch>` (worktrees live **outside** the repository working tree; nested paths are rejected). Persists the lease.
2. `release(lease_id, policy)`: before deletion — reconcile with `git worktree list --porcelain`; detect unmerged commits (`git log <base>..<branch>`) and dirty files; if either exists, export a patch artifact and mark the lease `released` **without deleting** unless an explicit `force` override is supplied. This is the exit criterion "worktree cleanup protects unmerged work".
3. `reconcile_on_startup()`: compare lease rows against `git worktree list --porcelain`; mark rows without directories `orphaned`; adopt directories without rows into `orphaned` for manual cleanup; never auto-delete on startup.
4. One writable lease per worktree; a second writer request fails with a structured error.

**TESTS** (Chapter 16 worktree list): unmerged-commit protection; dirty-file preservation (patch artifact produced); stale-record reconciliation; simultaneous allocation gets distinct worktrees; nested-path rejection.

**COMMIT** `"phase1: worktree manager with protective cleanup and reconciliation"`

## STEP 1.9 — Model providers

In `codypendent-runtime`, create `models.rs`:

1. Provider configuration lives in `<config_dir>/codypendent/models.toml`:

```toml
[[model]]
id = "hosted-default"            # ModelId
provider = "openai-compatible"
base_url = "https://api.openai.com/v1"
model = "gpt-5.1-codex"
api_key_env = "OPENAI_API_KEY"   # env var NAME; value never stored

[[model]]
id = "local-default"
provider = "openai-compatible"
base_url = "http://localhost:11434/v1"   # Ollama's OpenAI-compatible endpoint
model = "qwen2.5-coder:14b"
api_key_env = ""                  # local endpoints may need none
```

2. Build framework clients from this config via `agent-framework-openai` (one code path serves both the hosted and the local/OpenAI-compatible provider — that satisfies the roadmap's "one hosted + one local" with one adapter). Consult the crate's docs.rs for exact constructor names; required behaviour: streaming chat with tool calls.
3. `ModelPolicy` (Phase 1): an ordered candidate list per mode (`Build` → try `hosted-default`, fall back to `local-default` on connection failure). Record which model served each run in the run row and every `ModelStreamDelta`'s trace metadata. Full utility routing arrives in Phase 7 — do **not** build it now.
4. API keys are read from the named env var at call time; never persisted, never logged, never placed in model context ([Chapter 11](../11-security-and-governance.md)).

**TESTS** — config parse; missing-env-var produces a structured error naming the variable; fallback on connect-refused (use a closed port).

## STEP 1.10 — The agent loop

The heart of the phase: `codypendent-runtime/src/agent.rs` implementing the [Chapter 12](../12-agent-framework-rs-integration.md) adapter:

```rust
pub struct FrameworkAgentRuntime {
    models: ModelRegistry,
    tools: ToolRegistry,
    artifacts: ArtifactStore,
    approvals: ApprovalBroker,
    policy: PolicyEngine,
    ledger: LedgerHandle,
}
```

A run executes the Level 1 deterministic loop from [Chapter 04](../04-agent-runtime-and-workflows.md) — `Inspect → Plan → Modify → Test → Review → Present` — as explicit nodes around a framework agent:

1. Resolve model profile from the run's `ModelPolicy`; construct the framework `ChatClient` and agent with the Phase 1 tool set.
2. Wrap tool execution in middleware that: (a) converts the framework tool call into a `ProposedAction`; (b) evaluates policy; (c) requests approval when required (parking the run in `WaitingForApproval`); (d) executes under the minted capability grant; (e) converts outputs to artifacts + observation compaction; (f) appends `ToolStarted`/`ToolCompleted` events.
3. Translate framework stream events into ledger events (`ModelStreamDelta`, batched per ~250ms) — **persist, then publish**.
4. Persist `RunState` transitions before exposing them (`Queued → Preparing → Running → … → Completed|Failed|Cancelled`, exactly the [Chapter 04](../04-agent-runtime-and-workflows.md) enum, including `WaitingForApproval`, `WaitingForUserInput`, `Paused`, `Recovering`).
5. Cancellation: every model call, tool, and child process gets a token; cancel = stop new work → signal active ops → kill children after 5s grace → persist partial artifacts → mark unresolved effects for reconciliation.
6. **Steering:** `QueueSteering` text is injected as a user message at the next *safe point* — defined in Phase 1 as: between workflow nodes, or immediately after a completed tool call. Emit `SteeringApplied`.
7. **ChangeSet v0:** when the run's worktree has a diff at `Review`, store the ordered per-file patches as artifacts and a `changesets` JSON blob (persist within the run row or a new table — either is acceptable if serde-typed), emit `PatchProposed`. Accept/reject arrives via command; accepted patches are applied to the user's repository only through `git.apply_patch` under approval.
8. **Chronicle v0:** at terminal state, fold the run's ledger events into the [Chapter 20](../20-interaction-and-autonomy-model.md) `SessionChronicle` shape (objective, findings, actions, changes, verification, costs, unresolved) and store it as a JSON artifact referenced from `RunCompleted`.
9. **Message compaction:** enable the framework's token-budget compaction for the request list; daemon-side episode compaction is Phase 2+.

**RULES**

1. The daemon (not the model, not the client) is the only component that executes tools — invariant 2.
2. A client disconnect must have **zero** effect on this loop — no client handles are held by the run task (exit criterion 1). Publishing to zero subscribers is normal.
3. Every model request records: model id, request hash, token usage, latency, cost estimate (trace groundwork for [Chapter 13](../13-observability-evaluation-learning.md)).

**Modes (overlay):** implement `AgentMode` presets for `Ask`, `Explore`, `Plan`, `Build`, `Review` as policy bundles ([Chapter 20](../20-interaction-and-autonomy-model.md) table): Ask/Explore deny writes (Explore gets read-only tools), Plan may write plan artifacts only, Build gets the worktree write scope, Review gets read + comment. Enforce in policy evaluation, not just prompts: an `Explore` run proposing `git.apply_patch` is **denied by policy**, regardless of what the model says.

**TESTS**

- End-to-end fixture run against a **mock model server** (an OpenAI-compatible HTTP stub in `test-support` that scripts: propose `shell.run cargo test` → receive output → propose patch → finish). Assert the full event sequence, approval flow, artifact creation, and chronicle.
- `client_disconnect_does_not_stop_run`: start a run, drop the client connection, assert the run reaches `Completed` and all events are in the ledger.
- `explore_mode_cannot_write` (Chapter 16 interaction tests): patch proposal in Explore → policy denial event, run continues.
- Steering applied at a safe point, visible in event order.

**COMMIT** `"phase1: framework agent loop with approvals, modes, changesets, chronicle"`

## STEP 1.11 — Protocol server: attach, resume, subscriptions, heartbeat

Extend the Phase 0 server into the full session server:

1. Handshake first (`ClientHello`/`ServerHello`, heartbeat interval 15s; drop clients silent for 3 intervals).
2. `AttachSession` returns `Catchup` (events vs snapshot per the ≤500 rule) and registers subscriptions; multiple clients per session; roles enforced (an `Observer` submitting `StartRun` gets `protocol.role-denied`).
3. Event fan-out: a `tokio::sync::broadcast` per session, filtered by each client's subscriptions; slow clients fall back to re-attach (never block the ledger on a slow consumer).
4. Resume tokens: opaque, signed with the per-user daemon secret (random 32 bytes in `<data_dir>/daemon.secret`, mode 0600, created on first boot), carrying client_id + last sequence, expiring after 24h.

**TESTS** — reconnect-and-resume from a sequence (receives exactly the missed events, in order); snapshot path (>500 behind); role enforcement; two clients observing one run receive identical event sequences.

## STEP 1.12 — The Ratatui TUI

`codypendent-tui`, wired into `codypendent` (the CLI binary): running `codypendent` with no subcommand opens the TUI attached to the current repository's session (creating it if needed, auto-starting the daemon if needed).

Architecture **RULES** ([Chapter 10](../10-ide-github-and-inputs.md)):

1. Strict unidirectional loop: input events → `Action` enum → reducer updates `AppState` → render. **Widgets never perform I/O**; a dedicated task owns the protocol connection and translates daemon events into `Action`s.
2. No blocking operations on the render thread (Chapter 16 TUI tests).
3. Every mouse interaction has a keyboard equivalent (CONTRIBUTING).
4. Layout (Phase 1): left pane session/run list; center transcript (streaming deltas, tool cards with expandable output, patch summaries); right pane approvals + run details; bottom **status line** showing: mode, run state, model, context %, cost so far, worktree name, pending-approval count ([Chapter 20](../20-interaction-and-autonomy-model.md) projections).
5. Approval modal: `a` approve once, `A` approve for run, `r` reject; shows action, risk, requested capabilities verbatim from the event.
6. Keys: `Tab` cycle panes, `Enter` open/expand, `n` new run prompt, `p` pause, `c` cancel (with confirm), `s` steering input, `q` detach (never kills the run — say so in the UI), `?` help overlay.
7. Themes: define the semantic `Theme` token struct (surface/text/status/syntax/diff/agent) with a built-in dark theme; no hard-coded colors in widgets.

**TESTS** — reducer unit tests (event → state); snapshot tests for transcript and approval rendering (use `ratatui::backend::TestBackend`); keyboard/mouse equivalence table test.

**COMMIT** `"phase1: ratatui client with transcript, approvals, status line"`

## STEP 1.13 — Headless JSONL client

Two CLI additions ([Chapter 20](../20-interaction-and-autonomy-model.md), [ADR-015](../17-architecture-decisions.md)):

```bash
codypendent run --objective "diagnose the failing test" [--mode build] [--repo PATH] --jsonl
codypendent attach <SESSION_ID> --events jsonl
```

`run` creates/attaches a session, starts a run, and streams **every session event as one JSON envelope per line** to stdout until the run terminates; exit code 0 on `Completed`, 2 on `Failed`, 130 on cancel. `attach` replays from `--from-sequence N` (default: live tail). The JSONL stream and the TUI consume the same events — no privileged side channel.

**TESTS** — JSONL output parses line-by-line as envelopes; equivalence test: TUI action log and JSONL stream for the same fixture run contain the same event sequence (Chapter 16 interaction test "JSONL and TUI observe equivalent events").

## STEP 1.14 — Recovery and the failure matrix

Startup recovery (`recovery.rs` in the daemon):

1. Runs in live states (`Running`, `Preparing`, `WaitingForApproval`, `WaitingForUserInput`, `Paused`) at boot → transition to `Recovering`, then: re-park approvals; reconcile `pending_effects`; if the run's in-memory continuation is unrecoverable (Phase 1 has no mid-node checkpoint), end it as `Failed{reason: "daemon restart"}` **with** chronicle and artifacts intact — "recovers or cleanly marks the run" is the exit criterion; silent disappearance is the only forbidden outcome.
2. Worktree reconciliation (STEP 1.8) and artifact tmp-sweep run before the socket opens.

**TESTS** — the [Chapter 16](../16-testing-strategy.md) injection matrix, automated with `kill -9` of a daemon child process in integration tests, at minimum these five points: after command persistence / before external effect / after effect before outcome persistence / during model stream / during shell execution. For each: restart the daemon, assert the documented state (no duplicate effect, run `Recovering → Failed` or resumed, ledger consistent, pending effect reconciled).

**COMMIT** `"phase1: startup recovery, effect reconciliation, failure-injection tests"`

## Exit checklist

Roadmap criteria:

- [ ] Start a run from the TUI, close the TUI, reconnect: the run continued and the transcript catches up (client disconnect does not stop the run).
- [ ] Duplicate command delivery (same idempotency key) produces one effect and one result.
- [ ] `kill -9` the daemon mid-run; restart: the run is `Recovering → Failed` or resumed, with chronicle and artifacts intact; no duplicated external effect.
- [ ] A proposed patch is reviewable in the TUI as a change set, attributable to run + model, and applies only via approval.
- [ ] Releasing a worktree with unmerged commits or dirty files preserves them (patch artifact + retained directory without `force`).

Overlay criteria:

- [ ] Ask/Explore/Plan/Build/Review modes exist and are policy-enforced (Explore cannot write — test green).
- [ ] Status line shows mode, run state, model, context %, cost, worktree, approvals.
- [ ] `codypendent run --jsonl` emits the event stream; exit codes as specified.
- [ ] Chronicle v0 artifact produced for every terminal run.
- [ ] Steering text queued mid-run applies at a safe point, in event order.

Hygiene:

- [ ] `fmt` / `clippy -D warnings` / `test` all green; all COMMIT points committed; tree clean.
