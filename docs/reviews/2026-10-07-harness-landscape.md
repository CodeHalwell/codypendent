# Coding-agent harnesses: the state of the art, October 2026

> **Provenance.** Compiled on 2026-10-07 by a research pass over primary documentation,
> engineering posts, source-code analyses and 2025–2026 papers (about ninety sources, listed at
> the end). It was not independently re-verified line by line. Every number carries a date;
> **"reported"** marks a claim found in only one secondary source and **"unverified"** one that
> could not be confirmed. The GitHub repositories themselves could not be read in that pass, so
> source-level statements come from published analyses and README fetches. Spot-check anything
> you intend to quote. How Codypendent compares is in
> [the review, section 5](2026-10-07-review.md#5-how-codypendent-compares-with-the-field).

A *harness* is everything around the model: the loop, the tools, how edits are applied, context
management, permissions and sandboxing, sub-agent orchestration, and the instruction and memory
files. The "Dive into Claude Code" paper estimates that about 1.6% of such a harness is decision
logic and about 98.4% operational infrastructure; that ratio describes the whole category.

---

## 1. Claude Code (Anthropic)

**Architecture.** A single Node/TypeScript process running a single-threaded loop: prompt + history → model → either text (done) or tool calls → execute → feed `tool_result`s back → repeat until a text-only response. The same binary is bundled by the Agent SDK (TypeScript/Python) and drives the desktop app, VS Code extension, claude.ai/code cloud sessions (Anthropic-managed VMs) and an ACP adapter for Zed. Sessions are append-only JSONL transcripts under `~/.claude/projects/`; resume and fork replay them, compaction writes boundary markers rather than deleting lines, and session-scoped permissions are deliberately *not* restored on resume.

**Tools and edits.** Built-ins: `Read`, `Edit`, `Write`, `Glob`, `Grep`, `Bash`, `WebSearch`, `WebFetch`, `ToolSearch` (deferred tool loading), `Agent`, `Skill`, `AskUserQuestion`, `TaskCreate/TaskUpdate`, `EnterWorktree/ExitWorktree`, `NotebookEdit`, `Monitor`; the 2026 source analysis counts up to 54 tools (19 unconditional, 35 feature-gated), with deny-ruled tools removed before the model sees them. `Edit` is exact string replacement: an `old_string` that matches zero or more than one location is rejected and the model is told to supply more surrounding lines. Read-only tools (and MCP tools annotated `readOnlyHint`) run concurrently; state-changing tools serialize. Tool output is capped at 25,000 tokens by default. Before every edit the file is snapshotted (`~/.claude/file-history/<session>/`), giving `Esc Esc` rewind independent of git.

**Context management.** A five-layer compaction pipeline runs before each model call: (1) per-tool-result budget caps, (2) "snip" trimming of old history, (3) cache-aware "microcompact", (4) "context collapse" (a read-time projection that leaves the transcript intact), (5) model-written auto-compact; in short, clear older tool outputs first, then summarize. Compaction honors a "Compact Instructions" section in CLAUDE.md, fires `PreCompact`/`PostCompact` hooks, and stops after repeated thrashing when one output refills the window. CLAUDE.md is re-injected on every request (so it survives compaction) and is prompt-cached. MCP tool schemas are deferred behind `ToolSearch` by default; skills load only their descriptions until invoked; auto-memory loads at most the first 200 lines / 25 KB of `MEMORY.md`.

**Permissions and sandbox.** Modes: `default` (manual), `acceptEdits`, `plan`, `auto`, `dontAsk`, `bypassPermissions`, plus an internal `bubble` mode for subagent escalation. Deny rules win in every mode. `auto` (the default starting mode since v2.1.283) sends actions to a separate classifier model (optionally server-side) that trusts the working directory and its remotes at session start and blocks, for example, `git reset --hard`, un-named remotes, IaC destroys, secret exfiltration and disabling security tests. The motivating finding: users approve ~93% of prompts, so prompt-only safety fails. Independently, a Bash sandbox (macOS Seatbelt; Linux/WSL2 bubblewrap + socat) restricts writes to the working directory plus a per-user temp dir, protects `.git/hooks`, `.gitconfig`, shell rc files, and routes all egress through a local proxy with an allow-list that starts empty; blocked commands can be retried unsandboxed via an explicit `dangerouslyDisableSandbox` escape hatch that admins can disable. File tools, MCP servers and hooks run *outside* the sandbox. Worktree isolation adds four static checks that refuse edits or git commands aimed at the main checkout.

**Sub-agents and parallelism.** `Agent` spawns subagents with their own context, returning only a summary; built-in types include Explore, Plan, General-purpose and Verification, with custom ones in `.claude/agents/*.md`. Isolation can be in-process (default), a temporary git worktree (`isolation: worktree`), or remote; subagents run sync or background, and a `/subtask` fork inherits the parent conversation. Above that sit agent view, experimental agent teams (shared task list, `SendMessage`), `/batch` (5–30 worktree-isolated subagents) and scripted workflows. Subagent spend counts toward the budget cap; the paper measures agent teams at roughly 7× the tokens of a normal session.

**Memory and hooks.** CLAUDE.md hierarchy: managed (`/etc/claude-code/`) → user (`~/.claude/`) → project (`CLAUDE.md`, `.claude/rules/*.md`) → `CLAUDE.local.md`, with `@include` imports and lazy loading of nested files; delivered as user context, not system prompt. `AGENTS.md` is read in its place. 27 hook events (`PreToolUse`, `PermissionRequest`, `SubagentStop`, `PreCompact`, `WorktreeCreate`…) with command, prompt, HTTP and agent handlers.

**Benchmarks.** Anthropic reports model scores with its own scaffold rather than entering Claude Code on public leaderboards; the best Claude entries on Terminal-Bench 2.0 (Opus 4.6, ~79.8%) use third-party scaffolds (§10).

**Stated lessons** (from the Claude Code paper and Anthropic's engineering posts): deny-first because humans habituate; defense-in-depth layers must fail independently (a >50-subcommand parse fallback once skipped per-subcommand deny checks); minimal scaffolding plus deterministic infrastructure; context is the binding constraint, hence just-in-time retrieval, 1–2k-token subagent summaries and structured note-taking.

## 2. OpenAI Codex CLI (Rust rewrite)

**Architecture.** A Cargo workspace (`cli`, `tui` on ratatui, `core`, `exec`, `app-server`, `linux-sandbox`, `protocol`). Core and TUI talk over a *queue pair*: a submission channel of `Op`s (`UserInput`, `Cancel`, `Shutdown`…) and an event channel of `EventMsg`s, so the UI never blocks on inference. The loop is three nested loops: `submission_loop` → per-turn loop (assemble prompt, call the Responses API) → inner tool loop. Threads persist as JSONL rollouts under `~/.codex/sessions/` plus a state DB. Since 2026 the public surface is `codex app-server`, a stateful JSON-RPC 2.0 process (JSONL over stdio; WebSocket experimental) with a thread/turn/item model (`thread/start|resume|fork`, `turn/start|interrupt`) and *server-to-client* requests for `execCommandApproval` and `applyPatchApproval`. It powers the VS Code extension, desktop app, SDKs, and `codex mcp-server`.

**Tools and edits.** Sandboxed `shell`/exec (with persistent PTY sessions), `apply_patch` (a custom patch format with `*** Update File:` hunks anchored on context lines rather than line numbers), MCP tools, web search and image viewing. A patch that fails to apply is returned as an error for the model to re-emit.

**Context.** Reactive compaction at a configurable token limit (reported ~80% of the window), `/compact`, and, from GPT-5.2-Codex (Jan 2026), native model-side compaction (reported). Skills follow progressive disclosure (~8 KB of metadata at start).

**Sandbox and approvals.** `SandboxPolicy`: `read-only`, `workspace-write` (cwd, `/tmp`, configured writable roots; `.git` and `.codex` re-applied read-only), `danger-full-access`. macOS: dynamically generated Seatbelt profiles plus process hardening (`PT_DENY_ATTACH`, no core dumps, `DYLD_*` stripped). Linux: bubblewrap is now the default (`--ro-bind / /`, bind-mounted writable roots, `--unshare-user/pid/net`, in-process seccomp network filter, `/dev/null` masking of `**/*.env` globs); legacy Landlock is rejected for filesystem-restricted policies because it cannot isolate the app-server's Unix sockets. Windows: restricted tokens and two local sandbox accounts. Network is off by default; "managed proxy mode" tunnels through a domain-allow-listing MITM proxy. Approval policy is orthogonal: `on-request` (default; ask only to escalate), `never`, `on-failure` (ask before retrying outside the sandbox); `untrusted` is deprecated and `--full-auto` was removed in v0.147 (Aug 2026). The `approvals_reviewer` can be `user` or `auto_review`, the "Guardian" subagent (GA March 2026) that sees the transcript and the diff/command and approves low/medium-risk actions; OpenAI reports it catches 96.1% of malicious behaviour while cutting interruptions ~200×. Project-trust gates (after CVE-2025-61260) stop untrusted repos from loading `.codex` config. Six hooks (`SessionStart` … `Stop`) run outside the sandbox.

**Sub-agents.** Multi-agent "V2" (2026): subagents are TOML definitions (`.codex/agents/*.toml`), each a separate thread inheriting the parent's sandbox policy, with configurable models, reasoning levels and concurrency. Guardian itself is a reviewer subagent.

**Memory.** `AGENTS.md` (Codex originated it, Aug 2025; nested files, closest wins), `config.toml`, and `.agents/skills/*/SKILL.md`.

**Benchmarks.** Codex/GPT-5.5 tops Terminal-Bench 2.0 at 82.0% (Oct 2026); GPT-5.3-Codex was 85.0% on SWE-bench Verified and ~57% on SWE-bench Pro (April 2026 snapshot).

**Lessons** ("Harness engineering", OpenAI, Feb 2026): AGENTS.md is a map, not an encyclopedia; put knowledge in a `docs/` tree the agent discovers progressively; enforce architecture with custom linters whose error messages tell the agent how to fix the violation; give agents the same feedback loops humans use (tests, logs, browser screenshots); run background "garbage-collection" agents to pay down entropy. Reported outcome: ~1M lines and ~1,500 merged PRs in five months by a 3–7 person team. Official framing: the sandbox defines what is *possible*, the approval policy defines when Codex must *ask*.

## 3. Aider

**Architecture.** Single Python process, chat loop, git-native (each accepted edit is auto-committed with an attributed message). Different "coders" implement different edit formats, selected per model.

**Edits.** Formats: `whole` (full file), `diff` (SEARCH/REPLACE blocks in merge-conflict syntax, ~10× fewer tokens than whole-file), `diff-fenced` (path inside the fence, for Gemini), `udiff`/`udiff-simple` (simplified unified diff, created because GPT-4 Turbo replaced code with "...placeholder" comments), and `editor-diff`/`editor-whole` for architect mode. Blocks that fail to match are reported back with the current file so the model re-emits them; the matcher tolerates whitespace differences (per the `editblock` coder implementation). Aider switched from its original "edit block" to search/replace because it produced fewer malformed blocks.

**Context: the repo map.** Tree-sitter parses every file into definition/reference *tags*; files become graph nodes with edges weighted by cross-references; PageRank, personalized toward files already in the chat and identifiers the user mentioned, ranks tags; the top-ranked signatures are packed into a budget (`--map-tokens`, default 1k, expanded when no files are in the chat; the implementation binary-searches the tag count to fit). The map is sent with every request.

**Sandbox.** None; commands run locally with confirmation.

**Architect/editor split** (Sept 2024): a reasoning model describes the change, a second model converts it into a valid edit format, because "the model has to split its attention between solving the coding problem and conforming to the edit format." Pairings hit 85.0% on Aider's benchmark (o1-preview + DeepSeek) vs 80.5% for Sonnet alone. No sub-agents.

**Lint/test loop.** Every edited file is linted automatically (`--lint-cmd`, disable with `--no-auto-lint`); with `--auto-test`/`--test-cmd` the suite runs after each edit and non-zero exits are fed back for repair.

**Memory/benchmarks.** Conventions files via `--read`; an AGENTS.md consumer; measured on its own polyglot benchmark and present, but not near the top, on Terminal-Bench 2.0.

## 4. OpenHands (ex-OpenDevin)

**Architecture.** V0 was a controller process plus a per-session Docker runtime exchanging typed action/observation events on an event stream. The V1 Software Agent SDK (paper, Nov 2025) keeps event sourcing (immutable `ActionEvent`/`ObservationEvent` log; `ConversationState` is the single mutable object, persisted as `base_state.json` plus per-event files for replay) but runs in-process by default: `Conversation(path)` gives a `LocalConversation`; a `RemoteWorkspace` transparently yields a `RemoteConversation` that POSTs the serialized agent to an agent-server and streams events over WebSocket. Official images bundle VS Code Web, VNC desktop and Chromium.

**Tools.** Terminal, file editor (string-replacement style), task tracker, browser; MCP schemas auto-convert to Action/Observation models. Tools cross process boundaries as JSON specs resolved to executors at runtime.

**Context.** A `Condenser` replaces forgotten events with a `CondensationEvent` applied at read time (the full log is kept). The default `LLMSummarizingCondenser(max_size, keep_first)` triggers on event count and reports up to 2× lower API cost with no accuracy loss; the V0 codebase also shipped observation-masking and pipeline condensers (the paper mentions ~10 strategies).

**Security.** `SecurityAnalyzer` rates each call low/medium/high/unknown (an LLM analyzer appends a `security_risk` field) and a `ConfirmationPolicy` (default `ConfirmRisky` at high) pauses the agent in `WAITING_FOR_CONFIRMATION`. Docker sandboxing is opt-in in V1. A `SecretRegistry` injects secrets as env vars at execution time and masks them in outputs.

**Sub-agents.** A delegation tool runs child conversations in blocking parallel, implemented entirely as a user-level tool.

**Memory.** `AGENTS.md`/`repo.md`, `.openhands/skills/` (also `.cursorrules`), keyword-triggered microagents.

**Benchmarks** (SDK paper, Nov 2025): SWE-bench Verified 72.8% (Claude Sonnet 4.5), 68.8% (GPT-5 high), 65.2% (Qwen3-Coder-480B); GAIA 67.9%. Higher 2026 numbers circulate but I could not verify them.

**Lessons.** "Sandboxing should be opt-in, not universal" (V0's mandatory sandbox caused state divergence and resource contention); immutable components, one mutable state; V0's 140+ config fields across 15 classes were a liability; separate SDK from apps and benchmarks.

## 5. SWE-agent and mini-SWE-agent (Princeton/Stanford)

**ACI design.** The original finding (NeurIPS 2024): a raw shell underperforms a small purpose-built interface — a 100-line windowed file viewer (`open/goto/scroll`), a search trio (`find_file/search_file/search_dir`), a line-range `edit` that runs flake8 and auto-rolls back on syntax errors, and `submit`. Tools ship as *bundles* (a folder of `bin/` executables, `config.yaml`, `install.sh`), and a `state` command runs after every action to feed prompt templates (open file, cwd). Later bundles added `str_replace_editor` and `filemap`. Execution is via SWE-ReX (Docker, Modal, etc.); one YAML file configures everything; "history processors" truncate old observations (the taxonomy paper classifies it as rule-based keep-earliest-and-latest).

**mini-SWE-agent** (v2, 2026) is the recommended successor: ~100 lines for the agent class, bash only with no tool-calling API, a strictly linear history, and each action run via `subprocess.run` with no persistent shell, so swapping in `docker exec`/podman/bubblewrap is trivial. README claims >74% on SWE-bench Verified and adoption by Meta, NVIDIA and Nebius; it backs the SWE-bench "bash-only" leaderboard. The taxonomy paper notes it has *no* context management.

**Lesson stated by the authors:** a year of model progress made most specialised tools unnecessary; put the model, not the scaffold, at the centre, and keep actions stateless and the history linear for debugging and RL.

## 6. Cline, Roo Code, Kilo Code

**Cline** (VS Code, now also a runtime): Plan/Act modes; classic tools `execute_command`, `read_file`, `write_to_file`, `replace_in_file` (SEARCH/REPLACE), `search_files`, `list_code_definition_names` (tree-sitter), `browser_action`, `use_mcp_tool`, `new_task`. In May 2026 Cline extracted its harness as `@cline/sdk`, a TypeScript runtime whose tools are `read_files`, `search_codebase`, `run_commands`, `fetch_web_content`, `apply_patch` (default for GPT/Codex models), `editor` (others), `skills`, `ask_question`, `submit_and_exit`, routed per model. Checkpoints are a *shadow git repository* committed after every tool use (restore files, task, or both). The taxonomy paper cites Cline as the one agent where the *model* decides when to compact.

**Roo Code** (Cline fork): custom modes (Architect/Code/Debug/Ask), an orchestrator mode delegating subtasks, `apply_diff` with multi-block SEARCH/REPLACE and a configurable fuzzy threshold (exact by default; 80–99% enables Levenshtein matching), and context condensing at a configurable threshold (default 100%) that deliberately uses the active model because another model mis-summarises tool-call history. The repository was archived on 15 May 2026 after the team pivoted to Roomote, a Slack-based cloud agent.

**Kilo Code** (Roo fork) keeps the modes and adds an Agent Manager that runs parallel sessions in separate git worktrees.

## 7. Cursor

Per Cursor's production write-up (via ByteByteGo): a **router** ("Auto") picks Composer or a frontier model; **Composer** is an RL-trained MoE model trained on edit trajectories, with search-and-replace deliberately over-trained; **>10 tools** (codebase search, read/write, apply edit, terminal); a semantic **retrieval** index; a ReAct **orchestrator**. Edits use "Fast Apply": the reasoning model emits lazy snippets (`// ... existing code ...`) and a second model merges them by speculative decoding at ~1,000 tokens/s, checked in a shadow workspace with LSP diagnostics. Cloud agents run in isolated VMs (network blocked, writes limited to workspace and `/tmp`) on custom scheduling built for hundreds of thousands of concurrent RL environments. Compaction keeps failing test names, error types and key stack frames. Cursor 2.0 (Oct 2025) and 3 (2026) are agent-first UIs with worktree-isolated parallel agents; rules live in `.cursor/rules`, AGENTS.md supported. Lessons: tool use must be trained in, not prompted; speed shapes usability; adoption beats benchmarks.

## 8. Gemini CLI (Google)

TypeScript monorepo: `packages/cli` (Ink UI) and `packages/core` (Gemini client, prompt assembly, `ToolRegistry`, execution). Tools: ls, read/read-many, glob, grep, edit, write, shell, web fetch/search and memory, each implementing `shouldConfirmExecute()`; MCP tools are prefixed `server__tool`. Sandboxing is opt-in: macOS Seatbelt profiles (`permissive-open` default, `permissive-closed`, `permissive-proxied`, `restrictive-open/closed`) or Docker/Podman. `GEMINI.md` is discovered hierarchically (global → project → subdirectories). Checkpointing snapshots the project (git) and conversation before destructive tools. Chat compression triggers at ~70% of the window (reported) and writes a structured XML state snapshot; the taxonomy paper singles out Gemini CLI for *verifying* summaries with a probe turn and for classifier-chain model routing. Subagents and an ACP server (Gemini CLI co-designed ACP) are built in.

## 9. The rest: Amp, Goose, Devin, Jules, Copilot, Zed, Warp, Codebuff, Factory, Kiro, OpenCode

- **Amp (Sourcegraph).** Threads are persistent, shareable units; per-thread cloud machines ("Orbs"). Subagents are role- and model-specialised: Search (Gemini 3 Flash), Librarian (Claude Sonnet 4.6, searches GitHub), Task subagents, and the Oracle (GPT-5.4 high) as a second opinion. Amp dropped auto-compaction for **Handoff** (14 Nov 2025): `/handoff` generates a goal-oriented prompt plus the relevant files into a *new* thread, citing OpenAI's internal finding that recursive summaries degrade accuracy ("you should basically never use compaction"). Amp co-authored AGENTS.md.
- **Goose (Block).** Rust workspace; all extensions are MCP servers (70+); a multi-stage tool-inspection pipeline including an LLM "adversary inspector"; permission modes autonomous (default), approve, smart-approve, chat-only; subagents spawned by prompt or YAML recipe, sequential by default, parallel on request, summary-only return optional, available only in autonomous mode. Context strategies: summarise, truncate or prompt (reported). Goose, MCP and AGENTS.md were donated to the Linux Foundation's Agentic AI Foundation (Dec 2025).
- **Devin (Cognition).** Cloud VM with shell, editor and browser; Devin 2.0 (Apr 2025) centred on parallel cloud Devins, interactive planning, Devin Search and an auto-updating repo wiki; declarative Blueprints/snapshots and VPC deployment; Windsurf became "Devin Desktop" (June 2026); in-house SWE-1.7 (July 2026) and SWE-2 (Sept 2026) models (the "Kimi K3 base" claim is unverified).
- **Jules (Google).** Clones the repo into a cloud VM, installs dependencies, produces a plan the user approves before any edit, reads root `AGENTS.md`, opens PRs; CLI/API.
- **GitHub Copilot coding agent.** Ephemeral GitHub Actions-powered environment with an egress firewall (configurable allow-list), `copilot-setup-steps.yml` for dependencies, MCP servers, one PR per task, 59-minute cap; reads `copilot-instructions.md`, `*.instructions.md`, `AGENTS.md`, `CLAUDE.md` and `GEMINI.md` from the head branch. Agent HQ runs Claude/Codex/Copilot agents side by side; a Sept 2026 update runs agents inside the repo's dev container (reported).
- **Zed.** Rust/GPUI editor; created the **Agent Client Protocol** with Google's Gemini CLI team (Aug 2025): JSON-RPC over stdio between editor and agent subprocess with typed diffs, permission prompts and terminals; Claude Code, Codex, Goose, Factory Droid, Kiro and JetBrains/Neovim followed.
- **Warp.** Rust GPU terminal with Agent/Dispatch modes and mixed-model routing; **Oz** (Feb 2026) is a cloud control plane running Warp's agent alongside Claude Code, Codex and Gemini CLI. Its "75.6% SWE-bench Verified" claim is vendor marketing (Aug 2025); no verified Terminal-Bench 2.0 entry found.
- **Codebuff.** Open-source multi-agent harness: a base agent spawns file-picker/code-searcher agents, an editor, a reviewer and a test-running "commander"; agents are TypeScript generator functions with unlimited nesting over `read_files`, `str_replace`, `code_search`, terminal and spawn tools. Self-reported 61% vs Claude Code's 53% on a 175-task eval.
- **Factory Droid.** Specialised droids (Code, Knowledge, Reliability, Product) over one CLI/desktop/ACP harness with `droid exec` headless mode and configurable autonomy levels; Droid/GPT-5.3-Codex sits at 77.3% on Terminal-Bench 2.0. Its docs were unreachable during this survey.
- **Kiro (AWS).** One harness across IDE (VS Code fork), CLI, web and mobile; spec-driven development (`requirements.md` in EARS notation, `design.md`, `tasks.md`), `.kiro/steering` files, hooks on file/tool/task events, Autopilot vs Supervised modes, checkpoints, compaction, sub-agents, on-demand "Powers", MCP and ACP.
- **OpenCode.** TypeScript client/server (OpenAPI) with TUI, Tauri desktop and web clients; LSP diagnostics after edits; a nine-stage fallback matcher for search/replace (exact → trimmed → whitespace/indent-normalised → Levenshtein) plus post-edit formatters; permissions allow most actions but deny `.env` reads and ask on `doom_loop` (repeated identical calls) and external directories.

## 10. Benchmarks (with dates and caveats)

- **Terminal-Bench 2.0** (Nov 2025; 89 tasks, 5 trials each, Harbor harness, scores the whole agent). Top entries, Oct 2026 (codesota mirror): Codex/GPT-5.5 82.0; ForgeCode/GPT-5.4 81.8; TongAgents/Gemini 3.1 Pro 80.2; ForgeCode/Claude Opus 4.6 79.8; Droid/GPT-5.3-Codex 77.3. The same model moves several points across scaffolds. Terminal-Bench 3.0 (July 2026, 74 tasks) and 4.0 (Aug 2026, 66 tasks) have since replaced it.
- **SWE-bench Verified** is widely treated as saturated. Aggregator llm-stats (7 Oct 2026) lists vendor-reported 95.0% (Claude Fable 5), 93.9% (Claude Mythos Preview), 92.2% (Fireworks Ember-1); harnesses are vendor-internal and not comparable. OpenHands' 72.8% (Sonnet 4.5, Nov 2025) and mini-SWE-agent's >74% are the best harness-explicit numbers I could verify.
- **SWE-bench Pro** (Scale, 1,865 tasks, contamination-resistant). On the SEAL standardised scaffold (250-turn cap, 731 public tasks) early-2026 leaders were Opus 4.5 45.9%, Sonnet 4.5 43.6%, Gemini 3 Pro 43.3%; an April 2026 snapshot put GPT-5.3-Codex at ~57%. Later aggregator claims of 80–90% (Aug–Oct 2026) appear to refer to a revised "Pro V2"/"Pro Verified" set and are unverified.

## 11. Research and engineering write-ups that shaped 2025–2026 harness design

- **Anthropic:** "Building effective agents" (Dec 2024: workflows vs agents, poka-yoke ACI design); "Multi-agent research system" (June 2025: orchestrator-workers, +90.2% over single Opus 4, ~15× chat tokens, token use explains 80% of variance); "Writing tools for agents" (Sept 2025: consolidate, namespace, return high-signal fields, 25k-token truncation default); "Effective context engineering" (Sept 2025: just-in-time retrieval, clear tool results first, `NOTES.md` memory, 1–2k-token subagent summaries).
- **OpenAI:** "Harness engineering" (11 Feb 2026) — see §2.
- **"Dive into Claude Code"** (arXiv 2604.14228, Apr 2026) — source-level account of permission modes, the classifier, compaction and hooks; and **"Inside the Scaffold"** (arXiv 2604.03515, Apr 2026): 13 open-source agents characterised on 12 dimensions; five composable loop primitives (ReAct, generate-test-repair, plan-execute, multi-attempt retry, tree search; 11/13 agents compose several); seven compaction strategies; convergence on tool categories, edit formats and isolation, divergence on context, state and model routing.
- **Edit formats:** "To Diff or Not to Diff" (2604.27296): structure-aware diffs plus an adaptive diff-vs-whole-file policy match whole-file accuracy at >30% lower cost; search/replace breaks when source contains its delimiters. "Diffs vs Whole Files" (2609.05779): for small models direct generation wins on every metric; diffs win only for short, local edits. Practitioner consensus: search/replace beats unified diff on success and recoverability; whitespace mismatch is the dominant failure, hence normalised/fuzzy fallbacks.
- **Compaction:** CliffCompaction (2609.26779): truncate/drop but never paraphrase and discard prior summaries; up to 50% cheaper, +10 pp on Terminal-Bench via a scaffold-agnostic proxy. SelfCompact (2606.23525): a model-invoked compaction tool with a when-to-fire rubric, 30–70% cheaper than threshold triggers. CompactionRL (2607.05378): training compaction as a skill lifted GLM-4.5-Air from 59.8% to 66.8% on SWE-bench Verified. Slipstream: run the compactor in a parallel fork and validate the summary against the agent's next steps (+8.8 pp on a 9B model). A June 2026 study found compaction erases safety constraints (~30% policy violations afterwards) — reported.
- **Harness as artefact:** 2026 papers (Natural-Language Agent Harnesses, Agentic Harness Engineering, MemoHarness, Harness-R1, Harness Handbook) treat the harness as something to observe, evolve or learn from failure trajectories.
- **MCP:** 97M monthly SDK downloads (Mar 2026), ~16k public servers, Agentic AI Foundation stewardship; the 2026-07-28 revision adds paginated tool discovery and non-blocking startup. Cost: seven mounted servers ≈ 67k tokens of schemas before any work; deferred loading with tool search removes ~85%.
- **AGENTS.md:** 60k+ repositories; read natively by Codex, Copilot, Cursor, Gemini CLI, Jules, Devin, Aider, Zed, Amp, Factory, Goose, OpenCode, Warp, Kiro and (in place of CLAUDE.md) Claude Code: the de-facto cross-tool standard, with CLAUDE.md/GEMINI.md/`.cursor/rules` persisting as richer native formats.

---

## Cross-cutting patterns the best harnesses share

1. **One plain loop, no planner DSL.** Model → tool calls → results → repeat until a text-only turn (Claude Code, Codex, Gemini CLI, mini-SWE-agent, OpenHands); planning is a mode or prompt, not a separate engine.
2. **Exact-match string replacement as the primary edit tool**, with uniqueness checks or fuzzy fallbacks and failures returned to the model (Claude Code, OpenHands, SWE-agent, Aider, Cline/Roo, OpenCode). Unified diffs lost; context-anchored patches (Codex `apply_patch`) and model-merged "fast apply" (Cursor) are the working alternatives.
3. **Fast verification feedback after every edit**: linters, LSP diagnostics, tests (SWE-agent lint-and-rollback, Aider auto-lint/test, OpenCode LSP, Cursor shadow workspace, OpenAI's self-explaining linters).
4. **Tool results are the context budget's main enemy**, so outputs are capped (Claude Code 25k tokens), cleared first during compaction (Claude Code), masked (OpenHands) or paginated by design (Anthropic tool guidance).
5. **Layered, graduated compaction** instead of one summariser: Claude Code's five layers; OpenHands' read-time condensation events; Gemini CLI's verified summaries; Amp's refusal to summarise at all in favour of handoff. Everyone keeps the raw transcript on disk.
6. **Instruction files re-injected every turn and loaded hierarchically** (CLAUDE.md, AGENTS.md, GEMINI.md, `.kiro/steering`), with nested files resolved by proximity and loaded lazily.
7. **Progressive disclosure of capability**: deferred MCP schemas and tool search (Claude Code), skills loaded by description (Claude Code, Codex, Kiro), `docs/` maps (OpenAI).
8. **OS-level sandboxes with an allow-listed egress proxy** (Codex Seatbelt/bubblewrap/seccomp; Claude Code Seatbelt/bubblewrap + proxy; Gemini CLI Seatbelt profiles; Copilot firewall; Cursor VMs), always with an explicit, auditable "retry unsandboxed" escalation.
9. **A second model as the approver**: Claude Code's auto-mode classifier, Codex's Guardian/auto_review, OpenHands' LLM security analyzer, Goose's inspector stack — motivated by approval fatigue (93% approve rate).
10. **Protected paths and trust gates**: `.git/hooks`, `.codex`, shell rc files, `.env` masked or read-only; repo trust prompts before loading project config (Codex, Claude Code).
11. **Sub-agents as context firewalls** that return summaries, not transcripts (Claude Code, Codex, Amp, Goose, OpenHands, Codebuff), increasingly with role- and model-specialisation (Amp's Oracle/Librarian, Factory's droids).
12. **Git worktrees as the unit of parallel isolation** (Claude Code `--worktree`/`/batch`, Kilo Agent Manager, Cursor parallel agents, Codex worktree loaders).
13. **Checkpoints outside git**: pre-edit file snapshots (Claude Code), shadow repos (Cline), git+conversation snapshots (Gemini CLI, Kiro).
14. **Everything is an append-only event log** (Claude Code JSONL, Codex rollouts, OpenHands event sourcing, mini-SWE-agent linear history) enabling resume, fork, replay and RL data.
15. **A protocol boundary between core and UI** so one engine serves terminal, IDE, web and other agents: Codex app-server JSON-RPC, OpenHands agent-server, OpenCode OpenAPI, Claude Agent SDK, and ACP across editors.

## Emerging practices in the last 12 months (Oct 2025 – Oct 2026)

- **Classifier-in-the-loop autonomy became the default**: Claude Code auto mode and Codex Guardian (GA Mar 2026) replace most human prompts; `--full-auto`-style flags gave way to reviewer policies.
- **Harness engineering as a discipline**: OpenAI's post, awesome lists, and papers that evolve or learn harnesses from failure trajectories; AGENTS.md stewardship moved to the Linux Foundation.
- **Handoff and no-paraphrase compaction** challenged summarisation: Amp's handoff, CliffCompaction's truncate-only rule, SelfCompact's model-chosen timing, and model-native compaction (GPT-5.2-Codex, CompactionRL).
- **Multi-model, multi-harness orchestration**: Amp's per-role models, Warp Oz and GitHub Agent HQ running Claude Code/Codex/Gemini side by side, Goose and Claude Code hosting other agents as subagents or MCP servers.
- **Agent-to-agent plumbing**: Claude Code agent teams, cross-session messaging and dynamic workflows; Codex multi-agent V2; ACP for editor interop; MCP 2026-07-28.
- **Harnesses extracted as embeddable runtimes**: Claude Agent SDK, Codex app-server + SDKs, `@cline/sdk` (May 2026), OpenHands SDK, Codebuff SDK.
- **Consolidation**: Roo Code archived (May 2026), Windsurf folded into Devin Desktop (June 2026), SWE-agent superseded by mini-SWE-agent; benchmark churn (Terminal-Bench 3.0/4.0, SWE-bench Pro revisions) as Verified saturated.

---

## Sources

- Claude Code agent loop (SDK docs): https://code.claude.com/docs/en/agent-sdk/agent-loop
- How Claude Code works: https://code.claude.com/docs/en/how-claude-code-works
- Claude Code permission modes / auto mode: https://code.claude.com/docs/en/permission-modes
- Claude Code sandboxing: https://code.claude.com/docs/en/sandboxing
- Claude Code worktrees: https://code.claude.com/docs/en/worktrees
- Claude Code parallel agents: https://code.claude.com/docs/en/agents
- Dive into Claude Code (arXiv 2604.14228): https://arxiv.org/html/2604.14228v1
- Anthropic, Building effective agents: https://www.anthropic.com/engineering/building-effective-agents
- Anthropic, Multi-agent research system: https://www.anthropic.com/engineering/multi-agent-research-system
- Anthropic, Writing tools for agents: https://www.anthropic.com/engineering/writing-tools-for-agents
- Anthropic, Effective context engineering: https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
- OpenAI, Harness engineering (Feb 2026): https://openai.com/index/harness-engineering/ (summary mirror: https://madplay.github.io/en/post/harness-engineering)
- Codex linux-sandbox README: https://github.com/openai/codex/blob/main/codex-rs/linux-sandbox/README.md
- Codex protocol crate README: https://github.com/openai/codex/blob/main/codex-rs/protocol/README.md
- Codex app-server README: https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md
- Codex sandboxing (official): https://learn.chatgpt.com/codex/sandboxing
- Codex approvals & security (official): https://learn.chatgpt.com/codex/agent-approvals-security
- Codex app-server developer guide (gist): https://gist.github.com/oneryalcin/ee2c27e2d8aa040da8fbe7eebcc2ecea
- Codex internals: queue-pair, Guardian, 3-OS sandbox: https://codex.danielvaughan.com/2026/04/10/codex-cli-internals-queue-pair-guardian-sandbox/
- Codex v0.147 release notes analysis: https://codex.danielvaughan.com/2026/08/07/codex-cli-v0147-release-approve-for-me-mcp-2026-07-28-project-trust-plugin-search-secrets-redaction/
- Codex compaction and research round-up: https://codex.danielvaughan.com/2026/07/28/next-generation-context-compaction-slipstream-selfcompact-compactionrl-codex-cli-long-horizon-sessions/
- Simon Willison, Codex sandbox investigation: https://simonwillison.net/2025/Nov/9/codex-sandbox-investigation/
- Aider repo map: https://aider.chat/docs/repomap.html and https://aider.chat/2023/10/22/repomap.html
- Aider edit formats: https://aider.chat/docs/more/edit-formats.html
- Aider architect/editor: https://aider.chat/2024/09/26/architect.html
- Aider lint/test: https://aider.chat/docs/usage/lint-test.html
- OpenHands SDK paper (arXiv 2511.03690): https://arxiv.org/html/2511.03690v1
- OpenHands condenser docs: https://docs.openhands.dev/sdk/guides/context-condenser
- SWE-agent: https://github.com/swe-agent/swe-agent and tool bundles https://github.com/SWE-agent/SWE-agent/blob/main/docs/config/tools.md
- mini-SWE-agent: https://github.com/SWE-agent/mini-swe-agent
- Cline tools guide: https://docs.cline.bot/exploring-clines-tools/cline-tools-guide
- Cline checkpoints: https://docs.cline.bot/features/checkpoints
- Cline SDK (May 2026): https://www.buildfastwithai.com/blogs/cline-sdk-review-agent-runtime-2026
- Roo Code context condensing: https://roocodeinc.github.io/Roo-Code/features/intelligent-context-condensing
- Roo Code shutdown: https://kilo.ai/compare/roo-code-shutdown-roomote
- Cursor agent in production (ByteByteGo): https://blog.bytebytego.com/p/how-cursor-shipped-its-coding-agent
- How coding agents edit files: https://kondasamy.com/blog/2026/how-ai-coding-agents-edit-code/
- Gemini CLI architecture: https://google-gemini.github.io/gemini-cli/docs/architecture.html
- Gemini CLI sandbox: https://google-gemini.github.io/gemini-cli/docs/cli/sandbox.html
- Gemini CLI tools API: https://google-gemini.github.io/gemini-cli/docs/core/tools-api.html
- Gemini CLI core: https://geminicli.com/docs/core/
- Amp handoff (Tessl): https://tessl.io/blog/amp-retires-compaction-for-a-cleaner-handoff-in-the-coding-agent-context-race
- Amp manual: https://ampcode.com/manual
- Goose subagents: https://goose-docs.ai/docs/guides/context-engineering/subagents/
- Goose architecture overview: https://wuu73.org/aiguide/coding-agents-goose/
- Devin docs: https://docs.devin.ai/ ; Devin 2026 overview: https://apidog.com/de/blog/whats-new-in-devin-2026/
- Jules docs: https://jules.google/docs
- Copilot coding agent: https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent ; firewall: https://docs.github.com/en/copilot/how-tos/use-copilot-agents/coding-agent/customize-the-agent-firewall
- Zed ACP announcement: https://zed.dev/blog/bring-your-own-agent-to-zed ; ACP: https://agentclientprotocol.com/overview/introduction
- Warp Oz / Terminal-Bench: https://www.warp.dev/blog/category/product
- Codebuff: https://www.mintlify.com/CodebuffAI/codebuff/concepts/architecture ; https://news.codebuff.com/p/codebuff-goes-open-source-beats-claude
- Factory Droid: https://innfactory.ai/en/ai-harness/droid/ ; https://zed.dev/acp/agent/factory-droid
- Kiro docs: https://kiro.dev/docs/
- OpenCode architecture: https://forums.basehub.com/anomalyco/opencode/8 ; https://www.morphllm.com/comparisons/opencode-vs-codex
- Inside the Scaffold taxonomy (arXiv 2604.03515): https://arxiv.org/html/2604.03515
- To Diff or Not to Diff (arXiv 2604.27296): https://arxiv.org/abs/2604.27296
- Diffs vs Whole Files (arXiv 2609.05779): https://arxiv.org/abs/2609.05779
- CliffCompaction (arXiv 2609.26779): https://arxiv.org/abs/2609.26779
- Self-Compacting Agents (arXiv 2606.23525): https://arxiv.org/abs/2606.23525
- CompactionRL (arXiv 2607.05378): https://arxiv.org/pdf/2607.05378
- Harness papers: https://arxiv.org/pdf/2603.25723 ; https://arxiv.org/pdf/2604.25850 ; https://arxiv.org/pdf/2607.14159 ; https://arxiv.org/pdf/2608.02276 ; https://arxiv.org/pdf/2607.13285
- Terminal-Bench 2.0 leaderboard mirror: https://www.codesota.com/benchmark/terminal-bench-2 ; TB 3.0/4.0: https://www.tbench.ai/news/terminal-bench-3-0 , https://www.tbench.ai/news/terminal-bench-4-0
- SWE-bench Verified aggregator: https://llm-stats.com/benchmarks/swe-bench-verified ; SWE-bench scores (Apr 2026): https://codeant.ai/blogs/swe-bench-scores ; SWE-bench Pro: https://www.morphllm.com/swe-bench-pro
- AGENTS.md: https://agents.md/
- MCP adoption and tool-schema cost: https://algeriatech.news/mcp-97-million-installs-2026/ ; https://mcpplaygroundonline.com/blog/mcp-context-bloat-tool-search ; https://radar.firstaimovers.com/agentic-coding-agents-july-2026-momentum-mcp-spec-revision
