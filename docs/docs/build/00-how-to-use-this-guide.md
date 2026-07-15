# End-to-End Build Guide — How to Use This Guide

This guide tells an implementation agent **exactly** how to build Codypendent from an empty directory to the full product, phase by phase. It is written for an executor with **no prior knowledge of this project** and deliberately leaves as little to judgement as possible. If you are a human engineer, the same instructions work; you are simply allowed to be smarter between the lines.

Read this page completely before opening any phase chapter.

## 1. What you are building

Codypendent is a local-first agentic developer environment: a persistent Rust daemon (`codypendentd`) that owns sessions, agent runs, knowledge, and policy; plus disposable clients (CLI, Ratatui TUI, IDE extensions) that attach to it. The complete design lives in the numbered manual chapters (`docs/docs/00`–`20`) and is summarized as one narrative in [The Codypendent Story](../21-the-codypendent-story.md). **Read the story first.** When a phase chapter cites a manual chapter, that chapter is the authoritative specification for the step you are executing.

## 2. The phase sequence

Execute the phases strictly in order. Each phase produces a usable vertical slice with exit criteria; a phase is DONE only when every item in its exit checklist passes.

| Order | File | Builds |
|---|---|---|
| 1 | [Phase 0 — Workspace Bootstrap](10-phase-0-workspace-bootstrap.md) | Cargo workspace, protocol crate, daemon skeleton, SQLite ledger seed, CLI lifecycle commands, CI |
| 2 | [Phase 1 — Persistent Agent Slice](11-phase-1-persistent-agent-slice.md) | Event-sourced sessions/runs, full protocol server, artifact store, tools, approvals, worktrees, model integration, TUI, JSONL, recovery |
| 3 | [Phase 2 — Skills and Knowledge](12-phase-2-skills-and-knowledge.md) | Registry, skill packages, semantic retrieval, memory fabric, provenance, basic code graph |
| 4 | [Phase 3 — GitHub and IDE](13-phase-3-github-and-ide.md) | GitHub read/draft-PR workflows, webhooks, VS Code extension, Zed ACP adapter, session handoff |
| 5 | [Phase 4 — Docs Studio and Code Intelligence](14-phase-4-docs-studio-and-code-intelligence.md) | CRDT benchmark and documents, Git publication, symbol links, staleness engine, Rust/Python/TS semantic indexing |
| 6 | [Phase 5 — Workflows and Multi-Agent](15-phase-5-workflows-and-multi-agent.md) | Declarative workflows, durable checkpoints, delegation, blackboard, parallel worktrees, budgets |
| 7 | [Phase 6 — Plugins and Multimodal](16-phase-6-plugins-and-multimodal.md) | MCP plugin manager, WASM SDK, native sandbox, permission UI, voice/image input, themes, setup agent |
| 8 | [Phase 7 — Routing and Learning](17-phase-7-routing-and-learning.md) | Task classifier, cost router, local benchmarks, graders, shadow/canary promotion, rollback |
| 9 | [Master Acceptance Checklist](99-master-acceptance-checklist.md) | Full-system verification and release gates |

Phase 0 contains **complete, verified file contents** — every file compiles, every test passes, and the exit-criteria commands were executed successfully before the guide was written. Later phases specify modules, schemas, behaviours, and tests explicitly but leave routine code expansion to you; their rules section tells you exactly which behaviours are non-negotiable.

## 3. The rules (read twice)

1. **Execute steps in order.** Never skip a step, never reorder steps, never start a phase before the previous phase's exit checklist passes.
2. **Do not invent alternative designs.** If you believe a step is wrong, first re-read the step, then the cited manual chapter. If they agree with each other, they are right and you are wrong. Only if the step contradicts its cited chapter do you stop and report the contradiction.
3. **Copy literal file contents exactly** where a step says CREATE FILE with a code block. Do not reformat, rename, "improve", or add comments.
4. **Literal code is normative for names and behaviour.** If a literal block fails to compile in your environment (for example, a dependency released a breaking change), make the minimal fix that preserves every public name, field, message string, and behaviour — then record what you changed in the commit message.
5. **Never change dependency versions** given in a step, except as the minimal fix under rule 4.
6. **Run every RUN command from the repository root** unless the step says otherwise.
7. **Verify every CHECKPOINT before continuing.** A checkpoint that fails means you stop, fix within the current step's scope, and re-run. After three failed attempts, stop and report the exact command, output, and the step number — do not push past a red checkpoint.
8. **Commit exactly where the guide says COMMIT**, with the given message. Do not batch multiple COMMIT points into one commit. Never commit with failing `fmt`/`clippy`/`test`.
9. **Never delete the database, data directory, or repository to "start fresh"** unless a step explicitly says so.
10. **Never weaken a security rule to make something work**: no disabling policy checks, no widening capability grants, no bypassing approvals, no `unsafe_code`, no turning off `-D warnings`.
11. **Respect the non-goals.** Do not build: a plugin marketplace, multi-tenancy, a graph database, desktop computer-use, autonomous self-promotion of learned artifacts, or default multi-agent swarming. If a step seems to require one of these, you misread it.
12. **Secrets** (API keys) come only from environment variables or the OS keychain; never write one into a file, a commit, or a log.

### Step notation

Every phase chapter uses this vocabulary:

- `STEP N.M — title` — one atomic unit of work; do it fully before N.M+1.
- **CREATE FILE `path`** — create the file with exactly the fenced content that follows.
- **EDIT FILE `path`** — apply the described change to an existing file.
- **RUN** — execute the command from the repository root.
- **EXPECT** — what the previous RUN must print/produce. Treat mismatches as failure.
- **CHECKPOINT** — a block of RUN/EXPECT pairs gating progress (rule 7).
- **COMMIT `"message"`** — stage everything and commit with this message.
- **RULES** — numbered behavioural requirements (MUST-level) for code the step asks you to write without literal content.
- **TESTS** — tests you must write and make pass in that step.

## 4. Environment prerequisites

Phase 0–7 target **Linux and macOS**. (Windows support arrives with the named-pipe transport; it is tracked in phase notes, not required for exit criteria.)

Verify before starting:

```bash
git --version          # need ≥ 2.40 (worktree porcelain v2 features)
rustc --version        # need ≥ 1.82 (agent-framework MSRV); stable channel
cargo --version
rg --version           # ripgrep, used by the search tool in Phase 1
sqlite3 --version      # optional, for inspecting the database while debugging
```

Install Rust via rustup if missing (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`), then `rustup component add rustfmt clippy`. Network access to crates.io is required for builds.

Key facts you must not rediscover the hard way:

- **The framework dependency is real and pinned**: crate `agent-framework-core` version `0.1.1` on crates.io (repository: `https://github.com/CodeHalwell/agent-framework-rs`), MSRV Rust 1.82, umbrella crate `agent-framework` with per-provider features (`openai` is the default). Never enable the umbrella `full` feature (ADR-009).
- **Unix socket paths are limited to ~104–108 bytes.** The daemon resolves its socket into `$XDG_RUNTIME_DIR` when available and validates length up front; tests must use short temp paths. Phase 0 handles this for you — do not "simplify" it away.
- **SQLite runs in WAL mode** with migrations embedded from `migrations/` at the repo root; migrations are append-only — never edit a committed migration.

## 5. Source-of-truth precedence

When documents disagree, resolve in this order (highest wins):

1. The phase chapter you are executing (it encodes the reconciliations).
2. The manual chapter it cites (`docs/docs/01`–`20`) and the example manifests in `docs/specs/`.
3. [The Codypendent Story](../21-the-codypendent-story.md).
4. Everything else (`PROJECT_SCAFFOLD.md`, `TIMELINE.md`, stub outlines) — historical context only.

One pre-resolved conflict you will meet: the scaffold document lists five crates, the manual lists nine directories. The build guide creates crates **only when a phase needs them** (Phase 0: `protocol`, `daemon`, `cli`, `test-support`; Phase 1 adds `runtime`, `tui`; Phase 2 adds `knowledge`; Phase 3 adds `integrations` and `extensions/`; Phase 6 adds `sandbox`). Package names are `codypendent-*`; directory names are the short form (`crates/protocol`); binaries are `codypendent` and `codypendentd`.

## 6. Failure protocol

When a RUN fails or an EXPECT mismatches:

1. Re-read the current step from its beginning. Most failures are a skipped sub-step.
2. Confirm prerequisites: correct directory, previous CHECKPOINT green, toolchain versions.
3. Diagnose from the actual error text. Fix only within the current step's file list.
4. If the failure is environmental (network, disk), retry up to 3 times with backoff.
5. After three genuine attempts: STOP. Report step number, exact command, full error output, and what you tried. Do not improvise around a failing gate.

If the daemon misbehaves at runtime, its log is at `<data_dir>/logs/daemon.log` (default data dir `~/.local/share/codypendent`, override with `CODYPENDENT_DATA_DIR`). Read it before theorizing.

## 7. Definition of done (every phase)

A phase is complete when, in this order:

1. Every step's CHECKPOINT is green.
2. `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace` all pass from the repo root.
3. Every item in the phase's **Exit checklist** is verified and checked off.
4. All COMMIT points are committed; the working tree is clean.

Then, and only then, open the next phase chapter.
