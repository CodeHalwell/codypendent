# Codypendent — Full Project Review

**Date:** 2026-07-17 · **Reviewed at:** `main` @ `ac4032a` (Phases 0–3)
**Method:** Full workspace build/lint/test run, four independent deep code reviews
(daemon core, integrations, runtime/knowledge, protocol/clients/docs), plus
first-hand verification of every Critical finding against source.

> **Remediation status (2026-07-18):** every Critical (C1–C3), every Major, and
> the listed Minors were fixed on this branch in the commits following this
> review — see the punch-list annotations at the bottom and the individual
> commit messages for the fix details. Three items were consciously deferred
> as design decisions: client-scoped command idempotency (needs a
> `UNIQUE(client_id, idempotency_key)` table rebuild), the `AlwaysApproval`
> veto of run-scoped approval reuse (needs policy↔broker threading), and
> rejecting attach-to-missing-session (the TUI's remembered-session fallback
> intentionally relies on the empty-catchup contract). The two product
> decisions (wiring policy files + the retrieved-content trust boundary,
> items 13–14) remain open for an owner's call.

---

## Bottom line

Codypendent is a **well-engineered beta**, not yet production-quality. The core
algorithms are unusually strong and honestly tested — the event-sourced command
path is genuinely crash-consistent and idempotent, the protocol's framing and
forward-compat discipline are textbook, the policy path-canonicalization and the
retrieval security filter do exactly what they claim. Every automated gate is
green.

The gap between "beta" and "production" is consistent across all four reviews and
worth stating plainly: **the wiring and concurrency envelope around the good
algorithms has not received the same rigor as the algorithms themselves, and
several roadmap ✅ checkmarks describe an implemented algorithm rather than a
wired-up feature.** The three most serious issues are all in that envelope: an
approval-gate bypass via unshown shell environment, a startup-recovery path that
runs before single-instance exclusivity, and a client that ignores the daemon
heartbeat.

None of this is alarming for the project's actual stage (internal Phases 0–3).
But the items below should be closed before Codypendent is trusted with live
credentials, a network-reachable webhook, or a shared multi-checkout daemon.

---

## Verified health (measured, not claimed)

| Gate | Result |
|------|--------|
| `cargo fmt --all -- --check` | ✅ clean |
| `cargo clippy --workspace --all-targets --all-features -D warnings` | ✅ 0 warnings |
| `cargo test --workspace --all-features` | ✅ ≈300 tests, 0 failed, 1 ignored |
| VS Code: `tsc --noEmit` / `eslint` / `vitest` | ✅ typecheck + lint + 27 tests green |
| CI on `main` (incl. HEAD `ac4032a`) | ✅ green |
| Release pipeline | ✅ published `v0.1.0-build.2` prerelease |

The every-release hygiene the ROADMAP advertises is real. The one un-run gate is
`cargo deny` / `cargo audit` (ROADMAP line 179 already flags it as not done).

**Process note:** issue #6 tracks six deferred findings. All six are in fact
fixed on `main` (commits `9d9dcb8`, `87acdaf`, `53d30d1`, `560e872`), but the
issue is still **open** — it should be closed, or its remaining scope clarified.

---

## Critical (verified first-hand)

### C1 — Shell tool environment bypasses the approval gate → code execution under a benign-looking approval
`crates/runtime/src/tools/shell.rs:99-104` (proposed action) and `:150-152`
(child env); approval payload `crates/runtime/src/agent.rs:810-829`.

`shell.run` correctly `.env_clear()`s the child, then sets **only the
model-supplied environment bindings** — parsed from model args with no filtering.
But `ProposedAction::ExecuteCommand` carries only `program` + `args`; it does
**not** carry the environment or cwd. So the approval card and the audit ledger
show the operator "execute `cargo test`" while the model has attached
`RUSTC_WRAPPER=<path>`, `LD_PRELOAD=…`, `GIT_SSH_COMMAND=…`, or a hijacked `PATH`
that the operator never sees. Approving the routine-looking command runs
attacker-chosen native code. Because every consequential control in this system
funnels through "the human approves what the ProposedAction says," an unshown,
unconstrained, model-controlled environment defeats the central security control.
This is directly reachable from prompt-injected content the agent is reasoning
over (a malicious PR body, CI log, or retrieved memory — see M-run-2).

**Fix:** fold `environment` and `cwd` into `ProposedAction::ExecuteCommand` so
policy/approval/audit see them, and/or enforce a denylist of execution-affecting
variables (`LD_*`, `DYLD_*`, `*_WRAPPER`, `GIT_SSH_COMMAND`, `GIT_EXTERNAL_DIFF`,
`PATH`).

### C2 — Startup recovery runs before single-instance exclusivity → a second daemon corrupts a live one's runs
`crates/codypendentd/src/main.rs:49` (recovery), `:114` (relaunch queued),
`:79` (code-graph wipe) all run **before** the only exclusivity check, which is
the socket bind inside `server::run_with_executor` → `prepare_socket`
(`crates/daemon/src/server.rs:106,180`).

If daemon A is mid-run and a second `codypendentd` starts (systemd restart, a
user double-start, a supervisor race), daemon B's `recover_on_startup` fails
**all** of A's live runs (appends `Recovering` + `RunCompleted{Failed}` and flips
projections) while A keeps streaming and later writes `Completed` — the ledger
ends with contradictory terminal events. B also `relaunch_queued_runs` (double
execution of side-effectful tools) and wipes/rebuilds the shared code graph.
Only *then* does B hit the taken socket and exit. This violates the headline
"duplicate delivery ≠ duplicate effect" guarantee at the process level.

**Fix:** acquire an exclusive lock (pidfile/flock, or bind the socket) **before**
running recovery.

### C3 — VS Code extension never answers the daemon heartbeat → idle panel disconnects every 45s and pollutes the ledger each cycle
`extensions/vscode/src/client.ts:384-388`.

The payload switch's default branch drops `Ping` (the comment claims this mirrors
"the Rust `Unknown` fallback" — but both Rust clients explicitly reply `Pong`:
`crates/cli/src/connection.rs:119-124`, `crates/cli/src/tui.rs:244-249`). The
daemon stamps liveness only on frames read from the client and drops any client
silent for 3×15s (`crates/daemon/src/server.rs:43-46,313-341`). An idle panel
(no typing → no `UpdateIdeContext` frames) is therefore dropped every 45s,
reconnects with backoff, re-attaches, and repeats forever. Each cycle appends two
durable `ClientPresenceChanged` events to the **immutable** ledger
(≈3,800 junk events/day per idle panel), which then pushes sessions past the
500-event catchup cutover where the extension renders nothing (see M-cli-2). The
27 vitest tests never send a Ping, so it went unnoticed.

**Fix:** reply `Pong` to `Ping` in the client payload switch.

---

## Major

### GitHub / webhook (integrations)

- **G1 — GitHub client interpolates path params with no percent-encoding.**
  `crates/integrations/src/github/client.rs` (every endpoint, e.g. `:161-163`
  for `git_ref`). `owner`/`repo`/`git_ref` go straight into the URL path; reqwest
  normalizes `..` segments, so a model-controlled `ref` on the **un-approval-gated**
  `github.list_check_runs` read (`crates/runtime/src/tools/github.rs:81-90`) —
  e.g. `ref = "x/../../../../repos/OTHER/OTHER/issues/1/comments"` — redirects the
  request to a *different* `api.github.com` endpoint under the user's token with
  no approval. The host cannot be escaped (base URL fixes the authority), so
  "network-scoped to api.github.com:443" holds; the finer repo/ref scope the
  approval card implies does not. **Fix:** percent-encode each path segment, or
  reject params containing `/ .. ? #`.
- **G2 — Webhook server has no timeouts anywhere → slowloris.**
  `crates/integrations/src/webhook/server.rs:55-152`. No timeout on the header
  read, body read, or connection lifetime, and an unbounded task is spawned per
  connection. `MAX_HEADER_BYTES` bounds memory-per-connection but not time; a
  client dribbling bytes (or sending `Content-Length` then stalling) holds the
  connection open forever. Loopback-by-default limits *default* exposure, but
  receiving GitHub webhooks requires a non-loopback bind, and the config accepts
  `0.0.0.0` — so in the configuration this feature is actually used, it's remotely
  exploitable. **Fix:** wrap `handle_connection` in a timeout; bound concurrent
  connections with a semaphore.
- **G3 — Idempotency "list-before-create" scans only the first API page.**
  `crates/integrations/src/github/client.rs:198,223-232`. The list calls send no
  pagination and no `per_page`, so only the first 30 items (and only `state=open`
  PRs) are checked for the hidden marker. A PR with >30 comments → **duplicate
  comment** on retry; a repo with >30 open PRs (or a closed target) → **duplicate
  PR**. The headline idempotency guarantee silently fails precisely under load.
  **Fix:** paginate (follow `Link`), or search for the marker via the search API.

### Runtime / knowledge

- **R1 — No token/cost budget in the agent loop; cost accounting is a hardcoded `0`.**
  `crates/runtime/src/agent.rs:81,555-571,1510-1514`. The only guard is
  `MAX_STEPS = 256`; there is no wall-clock, token, or cost ceiling, and
  `prompt_tokens`/`completion_tokens`/`cost_micros` are hardcoded `0` in both the
  trace and the chronicle. `SkillLimits` (`maximum_iterations`, `maximum_cost_usd`)
  are parsed but explicitly unenforced. The loop can't run *forever* but can burn
  large, unbounded-in-practice spend with zero accounting. **Fix:** enforce a
  per-run budget; populate usage from real provider counts.
- **R2 — Retrieved memory/skill content is injected into model context with no trust boundary.**
  `crates/knowledge/src/context.rs:143-174`. Memory statements and skill/tool card
  summaries are concatenated verbatim with no "this is untrusted data, not
  instructions" demarcation and no trust label; the default retrieval `min_trust`
  is `Untrusted`, so a keyword-stuffed community skill description can flow into
  context. Combined with C1 this gives prompt-injection a ready execution path.
  `ToolCard::of` also doesn't truncate despite its doc claim, so card size is
  attacker-controlled. **Fix:** label retrieved content with provenance/trust in
  the rendered block; raise the default `min_trust`; truncate summaries.
- **R3 — Salient-view keeps *every* error-matching line (unbounded).**
  `crates/runtime/src/tools/salient.rs:116-152`. Head+tail (40 each) plus every
  line matching `error|warning|panic|failed|fatal`, with no cap on the match set.
  A failing `cargo build` (thousands of `warning:` lines) produces a
  tens-of-MB "compacted" observation that is re-sent every step — defeating
  compaction exactly when the agent inspects a broken build, and amplifying R1.
- **R4 — Skill-package hashing loads the whole package into memory, on the async runtime.**
  `crates/knowledge/src/manifest.rs:370-408`, called synchronously from async
  `register_package`. `collect_files` reads every file fully into a `Vec` before
  hashing (no size cap) and uses blocking `std::fs` on a Tokio worker. A community
  package with one large asset OOMs / stalls the daemon. **Fix:** stream into the
  hasher, cap size, `spawn_blocking`.

### Daemon core

- **D1 — Deferred SQLite transactions → `SQLITE_BUSY_SNAPSHOT` under streaming load; `fail_run` has no retry.**
  `crates/daemon/src/db.rs:21` (pool of 8), no `BEGIN IMMEDIATE` anywhere.
  Transactions that open with a `SELECT` (`commands.rs:819` command commit;
  `recovery.rs:191,218,314` fail paths; `approvals.rs:433` resolve) take a read
  snapshot, and a later write fails immediately with `BUSY_SNAPSHOT` if another
  connection committed in between — and the executor journals a ledger event per
  model delta, i.e. continuous commits during any run. Result: a user's
  `CancelRun` sent mid-stream can be rejected `internal.command-apply-failed`;
  worse, `fail_run` has **no retry** (`executor.rs:491-506`), so a run that fails
  to start while another streams can be left non-terminal and a headless
  `codypendent run --jsonl` hangs — the exact outcome `fail_run` exists to
  prevent. **Fix:** `BEGIN IMMEDIATE` for write transactions (sqlx `begin_with`),
  or retry on code 517.
- **D2 — Policy files are never loaded in production.**
  `PolicyEngine::load` has zero non-test callers; the executor always uses
  `with_defaults*()` (`crates/codypendentd/src/executor.rs:271-274`). A user's
  `~/.config/codypendent/policy.toml` or `<repo>/.codypendent/policy.toml` changes
  nothing. Roadmap 1.5 "Policy engine ✅" is true of the engine, not the wiring.
- **D3 — Even when wired, policy allow-lists can only *narrow*, never extend; the
  merge invariant is bypassable via `..`/symlink entries.**
  `crates/daemon/src/policy/config.rs:108,137-153` — `network_allow` and
  `shell.allowed_programs` intersect with an empty/base set, so a user can never
  authorize `npm`, `pytest`, or a model endpoint via config (fails closed, but
  silently drops the user's `allow`). Separately, `intersect_roots` checks
  containment on **raw unexpanded strings** (`config.rs:186-233`) while
  enforcement canonicalizes at eval time (`policy/mod.rs:488-515`), so a repo
  policy `read = ["$WORKTREE/../../.."]` or a committed symlink passes the
  "narrower" check then resolves outside the worktree — a malicious repo could
  widen the model's read scope. Latent until D2 is fixed, but must be fixed
  *with* it.
- **D4 — Approval wake-before-register race can strand a run in `WaitingForApproval` forever.**
  `crates/daemon/src/approvals.rs:282` (commit) then `:291` (register waiter). A
  client that learns the approval id from catch-up can resolve it in the window
  between commit and registration; `wake()` finds no waiter and drops the
  decision, then an empty waiter is registered and the runtime parks forever
  (`agent.rs:838`). `expire_due`/`reload_pending` won't recover it. **Fix:**
  register the waiter before commit, or have `await_decision` fall back to a DB
  read.
- **D5 — Recovered runs break `projection = fold(events)`.**
  `crates/daemon/src/recovery.rs:218-249`: `fail_live_run` appends `Recovering` +
  `RunCompleted{Failed}` but no terminal `RunStateChanged{Failed}`, while the DB
  projection is set to `Failed` directly. A client folding an `Events` catch-up
  after restart shows the run stuck in `Recovering` forever. One-line fix: append
  `RunStateChanged{Failed}` (as the live-failure path already does).
- **D6 — Crash-relaunched queued runs lose their repository binding.**
  `executor.rs:147` launches every recovered Queued run against
  `current_dir()`, ignoring the per-run `repository` (the issue-#6 multi-checkout
  fix). Recoverable from the persisted `commands.body` JSON.

### Protocol / clients / docs

- **P1 — Resume tokens are unmintable.** `mint_resume_token`
  (`crates/daemon/src/server.rs:896`) is called only from tests; `ServerHello` has
  no field to issue one, and every client hardcodes `resume_token: None`. The
  verify path is reachable, the issue path is not — "opaque daemon-signed resume
  tokens" is half a feature (reconnect actually works via `last_seen_sequence`).
- **P2 — ACP turn cancellation can silently fail.**
  `crates/cli/src/acp.rs:127-141`: a `select!` races cancellation against a
  **non-cancellation-safe** `read_envelope`; on cancel mid-frame the connection is
  desynced, the follow-up `CancelRun` is sent on it and its error swallowed, so
  Zed shows "cancelled" while the run keeps executing.
- **P3 — TUI doesn't render presence, and rejected commands are invisible.**
  No `ClientPresenceChanged` arm anywhere in `crates/tui` — the flagship Phase-3
  handoff demo shows "? unsupported event" (`reduce.rs:316-322`,
  `render.rs:212-214`). And `crates/cli/src/tui.rs:259-263` drops
  `CommandAccepted`/`CommandRejected`, so a policy/`session-not-found` rejection
  shows the user nothing (ROADMAP line 90 honestly tracks the latter).
- **P4 — VS Code webview loses all state on hide/show, and ignores `Catchup::Snapshot`.**
  `extension.ts:87-111` re-renders fresh HTML with no `retainContextWhenHidden`
  and no state replay — switching the activity bar away and back yields a blank
  transcript and **loses pending approval cards**. `extension.ts:154-160` handles
  only `Catchup::Events`, so a session past the 500-event cutover (guaranteed by
  C3) reloads empty.
- **P5 — No LICENSE file, but the extension claims MIT.**
  No `LICENSE`/`COPYING` anywhere, no `license` field in any `Cargo.toml`, yet
  `extensions/vscode/package.json:7` declares `"license": "MIT"` — asserting a
  grant the repository does not make. Add a top-level LICENSE (and matching
  `license` fields) or correct the manifest.
- **P6 — ROADMAP contradicts itself on the headline status.** Line 23 ("At a
  glance": Phase 3 ✅) and line 29 ("Phases 0–3 are complete") vs line 114
  ("## Phase 3 — GitHub & IDE awareness 🟡").

---

## Minor (high-value, low-cost)

- **Worktree force-release can silently destroy work** (latent — `allocate`/
  `release` have no production callers yet). `worktrees.rs:485-491`: `export_patch`
  swallows `git diff` failures with `.unwrap_or_default()`, storing an **empty**
  patch before `remove --force`; and no `--binary`, so binary files are lost.
  Fix before wiring Phase-5 parallel worktrees.
- **Approval-expiry machinery is dead** — nothing calls `expire_due`; the executor
  requests approvals with `expires_at: None`.
- **Boot-time code-graph cap** — `scan.rs` clears then re-folds at most
  `SCAN_FILE_CAP = 60` `.rs` files in nondeterministic order; any repo (including
  this one) with >60 Rust files has a permanently capped, boot-varying
  `code_nodes` table.
- **`SubscriptionHub` channels and approval waiters leak** for the daemon's
  lifetime (`subscriptions.rs:31-49`; cancelled parked runs drop the waiter
  without removing the map entry).
- **`docs/MANIFEST.json` omits 11 files that exist** (`PROJECT_SCAFFOLD.md`,
  `TIMELINE.md`, `architecture/*`, `product/*`, `workflows/*`).
- **Conflicting phase taxonomies** — `docs/TIMELINE.md` and
  `docs/PROJECT_SCAFFOLD.md` describe a week-based plan and never-built crates
  (`codypendent-skills`, `codypendent-fabric`) / a CLI surface
  (`codypendent agent run fix-ci`, `codypendent skills edit`) that don't match the
  shipped product, yet both are linked from the README as current guidance.
- **`docs/SECURITY.md` has a process but no destination** — "the security contact
  configured by the project" is configured nowhere.
- **Unverifiable ROADMAP claim** — line 79 "verified via PTY smoke test": no PTY
  harness or dependency exists in-tree.
- **Stale ROADMAP follow-up** — line 88-89 lists "Catch-up Snapshot rendering in
  the TUI" as not done, but it is implemented and tested (`tui.rs:357-377`).
- **Documented role ≠ implemented role** — the extension attaches as `Approver`
  (a deliberate, commented choice) while ROADMAP 3.7, `extension.ts:9`, the
  extension README, and the build guide all say `Contributor`.
- **Missing index on `approvals.run_id`** — `run_scoped_match` full-scans an
  ever-growing table on every approval request.

_(The four review transcripts carry ~40 further minor/nit items — additional
resource-growth spots, unbounded ACP reads, non-constant-time token signature
check, `Math.random()` nonce in the webview, etc. — available on request.)_

---

## Genuine strengths (these are real and worth protecting)

1. **The command write path is the real thing.** One transaction covering the
   received row, the `expected_revision` guard + bump, in-tx sequence allocation,
   projections, and the applied flip, with typed unique-violation detection
   routing duplicates to verbatim outcome replay. The idempotency exit criterion
   demonstrably holds, and it's tested with kill-9 against the real binary.
2. **Protocol framing and forward-compat are textbook.** BE u32 length prefix,
   16 MiB cap enforced before allocation on both read and write, clean-EOF vs
   mid-frame-truncation distinguished; every wire enum is internally tagged,
   `#[non_exhaustive]`, with `#[serde(other)] Unknown` and an unknown-tag test.
   Phase-0 ledger bytes are pinned as fixtures. A v1.2 daemon genuinely won't
   break a v1.1 client at the parse layer. The attach race is solved with
   subscribe-before-read + a watermark giving exactly-once delivery.
3. **The security *primitives* are correctly built.** Path canonicalization
   before scope check with deny-wins and exact-string (never basename) command
   matching; retrieval security is a hard pre-ranking filter (no leak via
   ranking); memory cross-repo isolation is real parameterized SQL; the GitHub
   token broker has no `Display`/`Serialize`, a redacting `Debug`, and a single
   `expose()` site (the auth header); webhook HMAC uses constant-time
   `verify_slice` **before** JSON parse; replay dedup is an atomic
   `INSERT OR IGNORE` on a PK. No non-test `unwrap`/`panic` in these crates; no
   SQL injection anywhere (all bound parameters).
4. **Determinism and testability are designed in.** The `ModelDriver` seam makes
   the whole agent loop deterministically testable with no live model; the TUI is
   a pure reducer + I/O-free renderer with RAII terminal restore and
   mouse-parity enforced by a test; the CAS artifact store is stream-hash → fsync
   → atomic rename → dedup.
5. **The tests are honest.** ~300 Rust + 27 TS tests, including kill-9 crash
   injection, symlink-escape rejection, the merge-invariant property test, and
   wiremock idempotency tests that assert `.expect(1)` on the create POST. Coverage
   is substantive, not stubbed.

---

## Recommended punch list (in order)

**Before any live-credential / networked use:**
1. ✅ C1 — shell `environment`+`cwd` on `ProposedAction` (rendered on the TUI
   approval card) + an exec-hijacking env denylist enforced pre-spawn.
2. ✅ C2 — socket exclusivity is claimed before startup recovery.
3. ✅ C3 — the VS Code client answers `Ping` with `Pong` (with a test).
4. ✅ G1 — `owner`/`repo`/`ref` validated before URL interpolation
   (`InvalidParameter`; refusal proven pre-request by test).
5. ✅ D1 — `BEGIN IMMEDIATE` on all daemon write transactions; `fail_run`
   retried at the executor.

**Before trusting idempotency / the webhook listener at scale:**
6. ✅ G2 — 30s whole-connection timeout + 64-connection cap on the listener;
   malformed Content-Length is a 400.
7. ✅ G3 — marker scans paginate (`per_page=100`, `Link` next, `state=all`);
   POSTs are no longer auto-retried.
8. ✅ D4 — `wake` pre-creates a missing waiter with its decision;
   `register_waiter` never clobbers.

**Correctness / honesty cleanups (cheap, high signal):**
9. ✅ D5 — `fail_live_run` appends the terminal `RunStateChanged{Failed}`.
10. ✅ R1/R3 — salient error lines capped (200); 30-min per-run wall-clock
    budget with a `BudgetWarning` at 80%; token/cost fields documented as
    unpopulated-not-free (real usage counts still need driver plumbing).
11. ✅ P5 — MIT `LICENSE` + `license` fields across the workspace.
12. ✅ P6 + doc set — ROADMAP reconciled (Phase 3 ✅, stale/unverifiable claims
    corrected, Approver role), MANIFEST completed, TIMELINE/SCAFFOLD marked
    historical, SECURITY.md given a real destination, issue #6 closed.

Also fixed beyond the list: D2-adjacent `$HOME`-drop warning, D6 (queued-run
repository recovery), P1 (resume tokens minted + stored + presented), P2 (ACP
cancel over a fresh connection), P3 (presence + rejection notices in the TUI),
M7/M8-vscode (webview state retention + Snapshot rendering), R4 (streamed,
capped, spawn_blocking package hashing), codegraph depth guard, worktree
safety-patch hardening (`--binary`, propagate-not-swallow, refuse-on-empty),
orphaned-approval expiry at recovery + a live expiry driver, forwarder dedup,
Observer gate on `UpdateIdeContext`, subscription-hub pruning, TUI transcript
caps + gap re-attach, scan determinism (cap 2000, sorted), the approvals
run-scope index (migration 0007), and the webhook verify-before-parse
ordering test.

**Product decisions (not bugs — need an owner's call):**
13. ⏳ D2/D3 — decide whether policy files are wired, and design a baseline
    layer users may *extend* (not only narrow) before advertising config as a
    feature. (The silent-`$HOME`-drop weakening now warns loudly; the
    `..`/symlink intersection bypass remains latent until files are wired and
    must be fixed together with that wiring.)
14. ⏳ R2 — trust-boundary/labeling for retrieved content injected into
    prompts (summaries are now truncated; the labeling design is open).

**Deferred with rationale (design/coordination needed):**
15. ⏳ Client-scoped command idempotency — needs a
    `UNIQUE(client_id, idempotency_key)` table rebuild migration; exposure is
    theoretical today (clients key by UUID).
16. ⏳ `AlwaysApproval` veto of run-scoped approval reuse — needs the policy
    verdict threaded into the broker's auto-approval check.
17. ⏳ Structured rejection for attach-to-missing-session — the TUI's
    remembered-session fallback deliberately consumes the empty-catchup
    contract; changing the server contract requires coordinated client work.

---

*Generated by an automated multi-agent review. Every Critical finding was
verified against source; Major/Minor findings are from the four review passes and
carry file:line anchors for independent confirmation.*
