# Phase 3 — GitHub and IDE Awareness

> **Objective:** connect the runtime to real developer surfaces: GitHub read/draft-PR workflows with idempotent, approval-gated writes; a VS Code/Cursor extension; a Zed ACP adapter; and backend-owned session handoff between TUI and IDE.
>
> **Specification chapters:** [Roadmap Phase 3](../15-roadmap.md), [IDE, GitHub, and Multimodal Integration](../10-ide-github-and-inputs.md), [Daemon and Client Protocol](../03-daemon-client-protocol.md), [Security and Governance](../11-security-and-governance.md). Example manifests: [`specs/command.toml`](../../specs/command.toml), [`specs/workflow.yaml`](../../specs/workflow.yaml), [`specs/plugin.toml`](../../specs/plugin.toml) (the GitHub integration's capability profile).
>
> **Exit criteria (from the roadmap):** the same run is visible in TUI and IDE; unsaved-buffer provenance is displayed; PR actions are idempotent and approval-gated; webhook delivery replay is safe.

## New crate and directory

**EDIT FILE `Cargo.toml`** — add member `"crates/integrations"` (`codypendent-integrations`): GitHub client, webhook normalization, ACP adapter, IDE bridge contract. Create `extensions/vscode/` (TypeScript, outside the Cargo workspace) and `extensions/zed/` (thin config; most of Zed arrives via ACP).

## STEP 3.1 — GitHub personal mode (read + draft PR)

1. Authentication: personal mode uses the user's existing credentials — run `gh auth token` if `gh` exists, else read `GITHUB_TOKEN` ([Chapter 10](../10-ide-github-and-inputs.md) personal mode). The token is a **secret**: brokered to the GitHub client, never into model context, never logged, never stored in the DB.
2. Implement a typed client (use `octocrab = "0.44"` or plain `reqwest` — either is acceptable; wrap it behind `trait GitHubApi` so the org-mode App can implement the same trait) covering: get PR, list checks + Actions runs, download job logs (→ artifacts), list/create review comments, create draft PR, update PR branch/description, create check-run summaries.
3. **Every write is a `ProposedAction::GitHubMutation`** through policy + approval + `pending_effects`: the six-step write path from Phase 1 STEP 1.3 applies unchanged. Idempotency: before creating a PR/comment, query for an existing object created with the same idempotency key (embed the key as a hidden HTML comment in bodies) — a retried command finds it and returns it (exit criterion 3).
4. Rate limiting and retry with backoff live in the client, under the run's budget clock.

**TESTS** — mock GitHub server (in `test-support`): draft-PR creation is idempotent under duplicate command delivery; write without approval is refused; token never appears in any event/artifact (scan test).

**COMMIT** `"phase3: github personal-mode client with idempotent approval-gated writes"`

## STEP 3.2 — The failed-check workflow (first real command)

Wire the [`specs/command.toml`](../../specs/command.toml) `/fix-ci` command to a hard-coded Phase 3 workflow (declarative YAML engine arrives in Phase 5) implementing the [Chapter 10](../10-ide-github-and-inputs.md) PR flow: select PR/check → retrieve metadata + logs → isolated worktree on the PR branch → investigate (Explore) → propose patch (Build) → run tests → present change set → on accept: commit/push/update PR + check summary, each write approval-gated. Register `/fix-ci` in the registry as a command item; the TUI command palette (`:` key) lists registry commands.

**TESTS** — end-to-end against the mock server + mock model: from failing-check fixture to updated-PR state, asserting every GitHub write has a matching approval and pending-effect record.

## STEP 3.3 — Webhook ingestion (org-mode groundwork)

Even in personal mode, implement the ingestion path now ([Chapter 10](../10-ide-github-and-inputs.md)):

1. `codypendentd` gains an optional localhost HTTP listener (`webhooks.enabled = false` by default in config) accepting GitHub webhook posts (for use with `gh webhook forward` or an org-mode App later).
2. Verify `X-Hub-Signature-256` (HMAC with a configured secret) **before parsing**; reject on mismatch.
3. Normalize deliveries into internal events; the `X-GitHub-Delivery` GUID is the idempotency key — a replayed delivery is acknowledged but produces no second internal event (exit criterion 4).
4. Events update GitHub projections (PR status in the TUI); they may **trigger** workflows only when policy explicitly allows (default: never).

**TESTS** — forged-signature rejection (Chapter 16 security list); delivery replay idempotency; policy-off means no workflow trigger.

**COMMIT** `"phase3: webhook verification, normalization, replay-safe ingestion"`

## STEP 3.4 — IDE bridge contract and dirty-buffer provenance

In `codypendent-protocol`, add the IDE context types ([Chapter 03](../03-daemon-client-protocol.md)): `IdeContextUpdate { active_file, selection, open_files, dirty_buffers: Vec<DirtyBufferDigest>, diagnostics_revision }` (debounced ≥ 300ms client-side), plus daemon→IDE requests: `ApplyEdit`, `RevealLocation`, `ShowDiff`. In `codypendent-integrations`, define `trait IdeBridge` exactly as [Chapter 10](../10-ide-github-and-inputs.md) (workspace_state, open_documents, active_selection, diagnostics, apply_edit, reveal_location, show_diff).

**Source provenance is normative:** every file excerpt entering model context is labeled with its origin — `committed@<rev>` | `filesystem` | `unsaved-ide-buffer` | `generated-patch` | `agent-worktree` — carried through the context manifest and **rendered in the TUI/IDE trace view** (exit criterion 2). Dirty buffers send digests; full contents transfer only when the daemon requests them and policy authorizes.

**TESTS** — context provider prefers dirty-buffer content over filesystem when digests differ, and the manifest records `unsaved-ide-buffer`; debounce collapses bursts.

## STEP 3.5 — VS Code / Cursor extension

`extensions/vscode/` (TypeScript, esbuild, `vscode` engine ≥ 1.90): connects to the daemon socket (`net.createConnection` on the discovery path — reimplement discovery's resolution order in TS; it is 30 lines), speaks the JSON frame protocol, attaches as `Approver` (it both starts runs and
resolves the approvals it surfaces, so it needs the approval-resolving role — a
superset of `Contributor`).

Deliverables ([Chapter 10](../10-ide-github-and-inputs.md)): side panel webview rendering session transcript + run state from projections; approval prompts as native notifications with Approve/Reject; selection/active-file/diagnostics context pushed as `IdeContextUpdate`; diff display via `vscode.diff` for change sets; commands `codypendent.openSession`, `codypendent.approve`, `codypendent.startRun`. Cursor: same artifact, add a compatibility note + smoke-test checklist in the extension README.

**RULES** — the extension holds **no state** beyond its connection and last-seen sequence; kill/reload must be recoverable via attach-resume. It never executes tools locally (invariant 2).

**TESTS** — extension unit tests for frame codec + reconnect logic (vitest); a scripted handoff test: start run in TUI, attach VS Code, both render the same `RunStateChanged` sequence (drive the extension's client class headlessly against a live daemon).

**COMMIT** `"phase3: vscode/cursor extension with side panel, approvals, ide context"`

## STEP 3.6 — Zed via ACP

Expose the daemon as an ACP agent ([ADR-002](../17-architecture-decisions.md): ACP is an adapter, not the internal protocol): `codypendent-integrations/src/acp.rs` implements the Agent Client Protocol server side (stdio JSON-RPC per the ACP spec; pin the `agent-client-protocol` crate if published, else implement the minimal method set: initialize, new session, prompt, cancel, permission requests), translating ACP turns into `SubmitUserInput`/`StartRun` commands and daemon events into ACP updates. Approval requests map to ACP permission requests. Add `codypendent acp` CLI subcommand that Zed's `agent_servers` config points at. Session identity: an ACP session binds to a Codypendent session; reconnect resumes it.

**TESTS** — ACP handshake + prompt round-trip over stdio against a live daemon with the mock model; cancellation propagates.

## STEP 3.7 — Session handoff polish

The [Chapter 10](../10-ide-github-and-inputs.md) handoff sequence must work exactly: TUI session → `codypendent open --in vscode` (or the extension's "open current repo session") → daemon attaches the IDE as contributor → IDE reveals relevant files and the active change-set diff → TUI stays attached (observer/controller) → the run never restarts. Add presence events (`ClientPresenceChanged`) so each client shows who else is attached.

## Exit checklist

- [ ] The same live run streams simultaneously in TUI and VS Code with identical event sequences (exit criterion 1 — scripted test green).
- [ ] A context excerpt from an unsaved buffer is labeled `unsaved-ide-buffer` in the trace UI (exit criterion 2).
- [ ] Draft-PR creation under duplicate command delivery yields exactly one PR; all GitHub writes show an approval and a reconciled pending effect (exit criterion 3).
- [ ] Replayed webhook delivery (same GUID) produces no duplicate internal event; forged signature rejected (exit criterion 4).
- [ ] `/fix-ci` runs end-to-end against fixtures: failing check → investigated → patched in a worktree → tests → change set → approved push → PR updated with summary.
- [ ] Zed connects via ACP, can run a prompt, and approvals surface as permission requests.
- [ ] `fmt` / `clippy` / `test` green (plus `extensions/vscode` lint/test); commits made; tree clean.
