# Codypendent — Build Roadmap & Progress Tracker

A single, scannable view of where the build is. Phases are **usable vertical
slices**, not isolated subsystems — each one ends with something you can run.

**Legend:** ✅ done & verified · 🟡 in progress · ⬜ not started

For the full narrative and exit criteria see
[`docs/docs/15-roadmap.md`](docs/docs/15-roadmap.md); for step-by-step build
plans see the [End-to-End Build Guide](docs/docs/build/00-how-to-use-this-guide.md);
the release gate is the
[Master Acceptance Checklist](docs/docs/build/99-master-acceptance-checklist.md).

---

## At a glance

| Phase | Slice | Status |
|------:|-------|:------:|
| **0** | Workspace bootstrap — daemon lifecycle, protocol, ledger, CI | ✅ |
| **1** | Persistent coding-agent slice — sessions/runs, tools, approvals, TUI, JSONL | ✅ |
| **2** | Skills & knowledge — registry, retrieval, memory, code graph | ✅ |
| **3** | GitHub & IDE awareness — PR flows, editor extensions, shared session | ✅ |
| **4** | Docs Studio & code intelligence — CRDT docs, semantic index | ⬜ |
| **5** | Workflows & multi-agent orchestration | ⬜ |
| **6** | Plugins & multimodal — MCP/WASM plugins, voice/image, themes | ⬜ |
| **7** | Intelligent routing & learning — model router, graders, canary | ⬜ |

> **You are here:** Phases 0–3 are complete. Beyond the editable, knowledgeable
> core (governed registry, hybrid retrieval, memory fabric, code graph), the
> runtime now reaches real developer surfaces: an idempotent, approval-gated
> GitHub client wired into the agent loop (with the `/fix-ci` repair flow),
> replay-safe webhook ingestion, source-provenance labeling of unsaved editor
> buffers, a VS Code/Cursor extension, a Zed ACP adapter, and session handoff with
> presence. Phase 4 (Docs Studio & richer code intelligence) is the next slice.

---

## Phase 0 — Workspace bootstrap ✅

Daemon starts, persists an instance database, and replays a fixture event log.

- [x] Cargo workspace + pinned `agent-framework-rs` (0.3–0.8)
- [x] Domain IDs & event contracts; migration `0001_init` (0.4–0.5)
- [x] `codypendentd` daemon: db, instance, ledger, replay, socket server (0.6)
- [x] `codypendent` CLI: `daemon start` / `status --json` / `stop` (0.7)
- [x] Test support + fixture event log; integration tests (0.8–0.9)
- [x] CI (fmt, clippy, test); full verification & exit criteria (0.10–0.12)

**Exit:** `daemon start/status/stop` work; restart preserves `instance_id`,
increments `boot_count`; fixture log replays deterministically. ✅

## Phase 1 — Persistent coding-agent slice ✅

> *Open a repo, ask an agent to diagnose a failing test, approve commands,
> inspect a patch, rerun tests, close the TUI, reconnect, and continue.*

- [x] **1.1** Schema migration `0002` (runs, commands, effects, approvals, artifacts, leases)
- [x] **1.2** Protocol v1.1 (handshake, catchup, artifact refs, unknown-variant tolerance)
- [x] **1.3** Command handling — crash-consistent 6-step write path + idempotency
- [x] **1.4** Content-addressed artifact store (SHA-256 dedup)
- [x] **1.5** Policy engine & capabilities (path canonicalization, deny-wins)
- [x] **1.6** Approval broker (park in `WaitingForApproval`, durable, live-published)
- [x] **1.7** Tool layer (file, search, shell, git) with policy/approval middleware
- [x] **1.8** Worktree manager (allocation, stale-lease reconciliation, unmerged-work rescue)
- [x] **1.9** Model providers (hosted + OpenAI-compatible, behind features)
- [x] **1.10** The agent loop (`FrameworkAgentRuntime`, run-state machine, chronicle)
- [x] **1.11** Protocol server — attach, resume, subscriptions, heartbeat
- [x] **1.12** Ratatui TUI **+ interactive harness wired into `codypendent`**
- [x] **1.13** Headless JSONL client (`run --jsonl`, `attach --events jsonl`)
- [x] **1.14** Recovery & the failure matrix (kill-9 → run recovered/failed)
- [x] **Wiring** agent loop ↔ daemon via a `RunExecutor` seam (`codypendentd` assembly crate)

**Exit criteria**

- [x] Client disconnect does not stop the run (verified: TUI reconnect resumes the session)
- [x] Duplicate command delivery does not duplicate an effect (idempotency keys)
- [x] Daemon restart recovers or cleanly marks the run (kill-9 integration test)
- [x] A run started from the TUI reaches a terminal state (driven to a terminal `RunState`; the JSONL client asserts the terminal exit code in `crates/cli/tests/jsonl_it.rs`)
- [x] Patch is reviewable and attributable (change-set + artifact provenance)
- [x] Worktree cleanup protects unmerged work (safety patch before force-remove)
- [x] `Explore` mode cannot write; status line; JSONL/TUI observe the same events

**Follow-ups tracked into later phases (not blocking the slice):**

- [ ] Bind a dedicated per-run worktree in the executor (module exists; the loop
      currently runs in the repo root — full binding lands with Phase 5 parallel worktrees)
- [x] Catch-up `Snapshot` rendering in the TUI (folds a `Snapshot` into title +
      run stubs; test `catchup_snapshot_seeds_title_and_run_stubs`)
- [ ] Surface `CommandRejected` in the TUI as a transient notice

## Phase 2 — Skills & knowledge ✅

New `codypendent-knowledge` crate; migration `0003`; the mandatory index-outbox.

- [x] **2.1** Schema `0003` + crate foundation (registry/memory/code-graph/outbox tables, shared types)
- [x] **2.2** Scoped registry + `skill.toml` package loader (strict keys, content-hash change detection) + built-in tools + `rust.fix-ci` reference skill
- [x] **2.3** Hybrid retrieval (dense + BM25 + exact + history) with hard security filters, rerank, dependency closure, budget disclosure
- [x] **2.4** Memory observer + curator pipeline + provenance + SQL-level scoped retrieval + supersession
- [x] **2.5** Tree-sitter code graph (nodes/edges + evidence) + repository map v1
- [x] **2.6** Skill Studio + memory browser in the TUI (permissions verbatim, provenance card)
- [x] Daemon registers built-in tools on startup; `codypendent index rebuild`; run-lifecycle context manifest + memory-on-completion

**Exit criteria**

- [x] Retrieval eval: **recall@8 = 1.0** (≥ 0.8 gate), 100% unsafe-item exclusion, disclosed top-k (254 tok) fits a budget the full-injection baseline (4580 tok) blows through
- [x] `rust.fix-ci` loads, is retrieved for "the CI test is failing", and its permissions render verbatim in the Studio
- [x] Memory never leaks across repositories (SQL scope filter; leak test green)
- [x] `codypendent index rebuild` after deleting `<data_dir>/index/` restores identical results
- [x] Every retrieved memory opens its source (provenance card + open-source affordance)
- [x] Agent context includes repository map + retrieved cards + cited memories (emitted into the run trace); a run's events are curated into provenance-bearing memories
- [x] `fmt` / `clippy` / `test` green; commits made; tree clean

## Phase 3 — GitHub & IDE awareness ✅

New `codypendent-integrations` crate; protocol `ide` module + `ProposedAction::GitHubMutation` + `UpdateIdeContext`/`ClientPresenceChanged`; migrations `0005` (webhook delivery idempotency) and `0006` (IDE context); `extensions/vscode/`.

- [x] **3.1** GitHub personal-mode client — `GitHubApi` trait + `reqwest` client (get PR, check-runs, job logs, review comments, draft PR, update PR, check-run summary); opaque `GitHubToken` broker (`gh auth token`/`GITHUB_TOKEN`, redacted, never serialized); hidden-marker idempotency (list-before-create); `eval_github_mutation` policy gate (network-scoped to `api.github.com:443`, always approval-gated); wiremock tests
- [x] **3.2** GitHub in the agent loop + `/fix-ci` — five `github.*` tools wired into the runtime (get PR, list check-runs as network reads; create-draft-PR, update-PR, check-run-summary as approval-gated `GitHubMutation`s), the client injected from the personal-mode token at daemon startup, the policy admitting `api.github.com:443` only when configured, `/fix-ci` registered as a built-in `Command` (in the Skill Studio) with a hard-coded objective template. End-to-end tested: the /fix-ci sequence (read check → test → update PR → post summary) with each write parking for a durable approval before it happens; rejected/denied writes never call GitHub. *(The declarative workflow engine that replaces the prompt-encoded sequence is Phase 5.)*
- [x] **3.3** Webhook ingestion — `X-Hub-Signature-256` HMAC verify **before** parse; normalize → internal events; `X-GitHub-Delivery` GUID replay dedup (migration `0005`); optional loopback listener wired into `codypendentd` (default off); policy-off ⇒ no workflow trigger
- [x] **3.4** IDE bridge + source-provenance live-path — protocol `IdeContextUpdate`/`DirtyBufferDigest`/edit-request types + `SourceProvenance`; `UpdateIdeContext` command stored as a projection (migration `0006`); the run read path labels an excerpt whose disk bytes diverge from an unsaved editor buffer `unsaved-ide-buffer` in the trace; `IdeBridge` trait; deterministic debounce
- [x] **3.5** VS Code / Cursor extension — `extensions/vscode/` (TypeScript, esbuild): frame codec + discovery mirroring the Rust protocol, a `DaemonClient` attaching as `Approver` with reconnect-resume, a side-panel webview, approval notifications → `ResolveApproval`, debounced `IdeContextUpdate` push, `vscode.diff`; 27 vitest tests + typecheck + lint green; Cursor compat note
- [x] **3.6** Zed via ACP adapter — minimal ACP over stdio JSON-RPC (initialize/session·new/prompt/cancel + permission requests) decoupled behind an `AcpBackend`; `codypendent acp` CLI subcommand; round-trip + cancellation tests
- [x] **3.7** Session handoff + presence — `ClientPresenceChanged` event; the server publishes presence on attach/detach; `codypendent open <session> --in <ide>` hands a session to an editor as a contributor without restarting the run

**Exit:** same run visible in TUI + IDE; unsaved-buffer provenance shown; PR
actions idempotent + approval-gated; webhook replay safe.

**Verified:** GitHub writes are idempotent and approval-gated end-to-end through
the agent loop; the token never enters `Debug`/serialization/logs; a read of a
diverging unsaved buffer is labeled `unsaved-ide-buffer` in the trace; a replayed
webhook (same GUID) produces no second event and a forged signature is rejected
before parsing; a second client attaching emits a `ClientPresenceChanged` the
first observes; the ACP handshake/prompt/cancel round-trips over stdio; the VS
Code extension's codec/discovery/reconnect pass 27 vitest tests. `fmt` / `clippy
--all-features -D warnings` / `test --workspace` green; `extensions/vscode`
typecheck/lint/test green.

## Phase 4 — Docs Studio & richer code intelligence ⬜

- [ ] CRDT benchmark + choice; collaborative documents; Git publication
- [ ] Document ↔ code symbol links; Rust semantic index; Python/TS adapters; staleness workflows

**Exit:** concurrent edits merge; document snapshot reproducible; symbol changes
flag affected docs; graph edges expose evidence + revision.

## Phase 5 — Workflow & multi-agent orchestration ⬜

- [ ] Declarative workflows; durable checkpoint storage; supervisor/specialist delegation; blackboard
- [ ] Parallel worktrees; budgets; pause/resume/retry-from-node; independent review agent

**Exit:** multi-agent edits never share writable worktrees; workflow resumes
after restart; node-level cost/provenance visible; single-agent baseline selectable.

## Phase 6 — Plugin & multimodal ecosystem ⬜

- [ ] MCP plugin manager; WASM component SDK; native process sandbox; plugin permission UI
- [ ] Voice input; image/screenshot input; themes + theme packs; agentic setup assistant

**Exit:** plugin cannot access undeclared path/network; permission-expansion on
update requires approval; original audio/image artifacts linked; setup assistant proposes, never silently changes.

## Phase 7 — Intelligent routing & learning ⬜

- [ ] Task classifier; cost/quality router; local-model benchmark harness; route cascading
- [ ] Trace graders; skill/prompt experiments; shadow + canary promotion; rollback UI

**Exit:** routing meets quality threshold at lower cost than static
strongest-model; no learned artifact self-promotes; regressions covered; every
promotion attributable and reversible.

---

## Every-release hygiene (any phase)

- [x] `cargo fmt --all -- --check` clean
- [x] `cargo clippy --workspace --all-targets` clean
- [x] `cargo test --workspace` green
- [ ] `cargo deny check` / `cargo audit` clean or with dated exceptions
- [x] CI green on the release commit; working tree clean; migrations unchanged since first commit
