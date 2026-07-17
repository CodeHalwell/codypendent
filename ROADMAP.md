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
| **3** | GitHub & IDE awareness — PR flows, editor extensions, shared session | 🟡 |
| **4** | Docs Studio & code intelligence — CRDT docs, semantic index | ⬜ |
| **5** | Workflows & multi-agent orchestration | ⬜ |
| **6** | Plugins & multimodal — MCP/WASM plugins, voice/image, themes | ⬜ |
| **7** | Intelligent routing & learning — model router, graders, canary | ⬜ |

> **You are here:** Phases 0–2 are complete. The system is now editable and
> knowledgeable: a governed registry with hybrid retrieval (recall@8 = 1.0 on the
> eval set, unsafe items filtered), an always-on memory fabric with provenance and
> absolute cross-repository isolation, and a tree-sitter code graph + repository
> map. Phase 3 (GitHub & IDE awareness) is **in progress**: the backend
> foundation has landed — a new `codypendent-integrations` crate with an
> idempotent, approval-gated GitHub client, replay-safe webhook ingestion, and
> the IDE bridge contract with normative source-provenance labels. The editor
> extensions (VS Code/Cursor, Zed ACP) and full session handoff are the remaining
> slice.

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
- [x] A run started from the TUI reaches a terminal state (verified via PTY smoke test)
- [x] Patch is reviewable and attributable (change-set + artifact provenance)
- [x] Worktree cleanup protects unmerged work (safety patch before force-remove)
- [x] `Explore` mode cannot write; status line; JSONL/TUI observe the same events

**Follow-ups tracked into later phases (not blocking the slice):**

- [ ] Bind a dedicated per-run worktree in the executor (module exists; the loop
      currently runs in the repo root — full binding lands with Phase 5 parallel worktrees)
- [ ] Catch-up `Snapshot` rendering in the TUI (currently folds `Events`; a very
      long session falls back to live-tail)
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

## Phase 3 — GitHub & IDE awareness 🟡

New `codypendent-integrations` crate; protocol `ide` module + `ProposedAction::GitHubMutation`; migration `0004`→`0005` (webhook delivery idempotency).

- [x] **3.1** GitHub personal-mode client — `GitHubApi` trait + `reqwest` client (get PR, check-runs, job logs, review comments, draft PR, update PR, check-run summary); opaque `GitHubToken` broker (`gh auth token`/`GITHUB_TOKEN`, redacted, never serialized); hidden-marker idempotency (list-before-create); `eval_github_mutation` policy gate (network-scoped to `api.github.com:443`, always approval-gated); wiremock tests
- [ ] **3.2** `/fix-ci` failed-check workflow (hard-coded Phase 3 flow; wires the client into the agent loop) — *not started*
- [x] **3.3** Webhook ingestion — `X-Hub-Signature-256` HMAC verify **before** parse; normalize → internal events; `X-GitHub-Delivery` GUID replay dedup (migration `0005`); optional loopback listener wired into `codypendentd` (default off); policy-off ⇒ no workflow trigger
- [x] **3.4** IDE bridge contract (`IdeBridge`) + source provenance — protocol `IdeContextUpdate`/`DirtyBufferDigest`/edit-request types + `SourceProvenance` labels (`committed@<rev>` | `filesystem` | `unsaved-ide-buffer` | `generated-patch` | `agent-worktree`); dirty-buffer-over-filesystem resolution; deterministic debounce. *Live-path wiring into the model read path + TUI trace render is deferred with 3.5.*
- [ ] **3.5** VS Code / Cursor extension (side panel, approvals, IDE context, diff view) — *not started*
- [ ] **3.6** Zed via ACP adapter — *not started*
- [ ] **3.7** Session handoff polish (`ClientPresenceChanged`, same run in TUI + IDE) — *not started*

**Exit:** same run visible in TUI + IDE; unsaved-buffer provenance shown; PR
actions idempotent + approval-gated; webhook replay safe.

**Verified so far:** GitHub writes are idempotent (repeated key → one object) and
approval-gated (`GitHubMutation` → `RequireApproval`, network-denied by default);
token never enters `Debug`/serialization; replayed webhook delivery (same GUID)
produces no second event and a forged signature is rejected before parsing. The
IDE/TUI-visible-same-run and `/fix-ci` end-to-end criteria await the editor
extensions and the `/fix-ci` workflow.

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
