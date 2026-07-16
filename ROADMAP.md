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
| **2** | Skills & knowledge — registry, retrieval, memory | ⬜ |
| **3** | GitHub & IDE awareness — PR flows, editor extensions, shared session | ⬜ |
| **4** | Docs Studio & code intelligence — CRDT docs, semantic index | ⬜ |
| **5** | Workflows & multi-agent orchestration | ⬜ |
| **6** | Plugins & multimodal — MCP/WASM plugins, voice/image, themes | ⬜ |
| **7** | Intelligent routing & learning — model router, graders, canary | ⬜ |

> **You are here:** Phase 1 is complete end-to-end — you can drive a run from the
> TUI or headlessly, approvals park and resolve, disconnect/reconnect continues
> the run, and kill-9 recovers it. Phase 2 is the next slice.

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

## Phase 2 — Skills & knowledge ⬜

- [ ] Skill registry + scope hierarchy; package loader; Skill Studio
- [ ] Hybrid BM25/dense/exact retrieval; memory observer + curator; provenance UI
- [ ] Basic code symbol graph; framework-compatible registry skill provider

**Exit:** top-k selection beats full-tool injection on an eval set; skill
permissions visible; every retrieved memory opens its source; stale indexes rebuild.

## Phase 3 — GitHub & IDE awareness ⬜

- [ ] GitHub read + draft-PR workflows; GitHub App option
- [ ] VS Code/Cursor extension; Zed ACP adapter; IDE context (active file, selection, diagnostics, dirty buffers)
- [ ] Shared session handoff (same run in TUI and IDE)

**Exit:** same run visible in TUI + IDE; unsaved-buffer provenance shown; PR
actions idempotent + approval-gated; webhook replay safe.

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
