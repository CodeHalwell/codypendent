# Codypendent Improvement Plan

Last scan: 2026-08-15 · Baseline `5c9dbbc` + uncommitted working tree
Re-verified: 2026-08-16 by a second independent scan — every P0/P1 item spot-checked against the
current tree still stands; `cargo check` and `cargo clippy --workspace --all-targets` remain clean.
See "Re-verification" in `findings-register.md`.
Re-checked: 2026-10-07 at `76448fc` (v0.14.0) during the whole-repository review in
[`docs/reviews/2026-10-07-review.md`](../docs/reviews/2026-10-07-review.md). Most of the items
below have since been fixed; `checklist.md` and the "Re-check" section of the register record
which, and what is still open. The sections below are the original August scan, kept as written.
Detailed evidence: [`findings-register.md`](findings-register.md)

## Document map

| File | Role |
|---|---|
| `README.md` (this file) | Prioritised plan — what to do, in what order, and why. |
| [`findings-register.md`](findings-register.md) | Evidence for every finding: file, line, quoted code, reproduction, severity — plus a "verified sound" list so future scans don't re-litigate settled ground. |
| [`checklist.md`](checklist.md) | Task checklist. Independently corroborates the three headline items below and adds areas the register does not cover — see "Coverage boundary". |
| [`ratatui-styling-review.md`](ratatui-styling-review.md) | Architectural review & gap analysis against Ratatui production styling standards (Ratatui 0.30.2 design systems). |

### Coverage boundary

`checklist.md` raises items **not** verified in the register. Treat these as leads until checked:

- the four untracked SDK components' conformance to the sibling `SurfaceOptions` contract;
- SDK worker lifecycle — `bridge.ts:454` abort-path rejection, `runtime.ts:402` `#shutdown`
  idempotency, `stdio.ts:19` drain wait;
- a missed wakeup in `unified_exec/process.rs:257-292` (the register read
  `collect_output_until_deadline` for output loss, not wakeup correctness);
- a hardcoded `"deny"` verdict in `hook_exec.rs:241/354` (the register did not read `hook_exec.rs`);
- an uncapped `Content-Length` allocation in `knowledge/src/lsp/transport.rs:116-129`, and
  sanitising diagnostics before they enter model context;
- the Phase 3 low-severity sweep, and the CI gaps — no macOS job, so the Seatbelt executor is
  never exercised, and no Dependabot/renovate.

Where the documents overlap, the register's line numbers are the verified ones: the phantom
`/undo` key conflicts with `input.rs:488`, not `:383`.

## Purpose

Codypendent's committed baseline is healthy. The work that matters now is (a) not shipping the
defects sitting in the uncommitted working tree, (b) closing a small set of real correctness and
security holes the test suite does not cover, and (c) correcting documentation that is wrong in
**both** directions — overclaiming on Phase 7's measurement paths and understating five features
that are actually built.

## Baseline health

| Check | Result |
|---|---|
| `cargo clippy --workspace --all-targets` | clean, zero warnings |
| `cargo test --workspace` | **3165 passed, 0 failed, 2 ignored** |
| `cargo test -p codypendent-codypendentd --lib publish::` | 21/21 pass |

## What changed since the previous plan

Three items from the earlier revision of this document are **stale — do not work them**:

1. ~~"3 failing tests in the documentation publishing path"~~ — all 21 `publish::` tests pass.
   The `safe.bareRepository` failure is gone.
2. ~~"`models add` silently deletes `[voice]`/`[transcription]`/`[speech]`/`[embedding]`/`[retrieval]`"~~
   — **fixed**. All mutation is centralized in `crates/cli/src/models_file.rs`, which round-trips
   a parsed `toml::Value` root under a `flock`, with regression tests pinning table survival and
   8-way concurrent writes. Two narrower residues remain (P1-4, P2-3).
3. ~~"Phase 6/7 incomplete"~~ — only half right. See P3-1: the roadmap *understates* five
   shipped features.

The scan also found that the earlier plan's framing was too optimistic in one place: the
uncommitted working tree contains a **file-corrupting** defect and a **user-misleading** one.

---

## Priority 0 — Do not commit the working tree as-is

### P0-1 · `edit_file` panics and silently corrupts source files

`crates/runtime/src/tools/edit_match.rs:524-530` — the new `unicode_normalized` stage computes a
byte index on the **normalized** line and slices the **original** line with it. Normalization
shrinks byte length (3-byte `“ ” — –` and 2-byte NBSP → 1 ASCII byte), so the offsets are
invalid. `end <= line.len()` is a range check, not a char-boundary check.

Both failure modes were reproduced against the verbatim logic:

- **Panic** — content `x = “a”; let msg = "hi";`, search `let msg = "hi";` →
  `byte index 9 is not a char boundary; it is inside '”'`
- **Silent corruption** — content `“abcd let msg = "hi";`, search `let msg = "hi";` → returns
  the span `"d let msg = \"hi"`, shifted 2 bytes left. Same length as the search, so
  `is_disproportionate` does not fire, and `edit_file.rs:146` writes it.

The replacer runs over the **whole file**, so one stray smart-quoted comment breaks every
non-exact edit in that file. Contained today only because `spawn_blocking` catches the unwind;
**if any profile ever sets `panic = "abort"` this becomes a remote daemon kill from model output.**
The new test covers only the safe direction (content ASCII, search smart) — the bug needs the
*content* to carry the wide characters, which is the common real-world direction.

Actions:
- Restrict the stage to whole-line/whole-block candidates via `line_spans`, as all nine other
  stages do — or carry a normalized→original offset map. A boundary guard alone downgrades the
  panic but leaves the corruption.
- Drop the backtick → `'` mapping (`:494`): it makes `'a'` match `` `a` `` in JS/Rust/shell.
- Add tests for the content-carries-wide-characters direction, including the two inputs above.

### P0-2 · `/undo` is cosmetic and fabricates a completion record

`crates/tui/src/palette.rs` advertises "restore the worktree to the previous checkpoint
snapshot"; `crates/tui/src/reduce.rs:7175-7196` pushes a transcript note reading
"restoring latest checkpoint" and does nothing else — verified: no `state.outbox.push` in the arm.

The note is present-progressive and lands in durable scrollback indistinguishable from a real
system note, so a user who reads it and then commits is building on changes they believe were
reverted.

**The backend is fully built** — `CommandBody::RestoreCheckpoint`
(`protocol/src/command.rs:767`), daemon handler (`daemon/src/server.rs:3024`), approval-gated
via `ProposedAction::RestoreCheckpoint` and correctly non-reusable. The only missing piece is a
TUI `Intent`; no client sends the command.

Actions:
- Add the intent and route it through a `ConfirmCancel`-style confirmation (worktree rollback is
  destructive). Until then, **remove the palette entry** rather than ship a false affirmation.
- Fix the phantom `key: "u"` — `input.rs:488` binds only `Ctrl-U`.
- Strengthen `palette.rs`'s guard test to cross-check every `key` against `input.rs`; it
  currently asserts only `!key.is_empty()`, which is why the phantom hint passes CI.

### P0-3 · Every Python file now spawns two language servers

`crates/knowledge/src/lsp/servers.rs` gives `RUFF` the `.py`/`.pyi` extensions that `PYRIGHT`
already claims, and the `for spec in ROSTER` loop (`lsp/mod.rs:124-147`) has no `break` — so a
`.py` file spawns, touches and waits on both, doubling diagnostic latency and duplicating
diagnostics.

Actions:
- Decide whether Python should use one server or intentionally merge two; if merged, dedupe
  `all_diags` and run the waits concurrently rather than serially.
- Confirm `ruff server` satisfies the `initialize`/`touch`/diagnostics handshake `LspClient`
  expects — `ruff` is a linter, and it is currently handed `pyright_root`.
- Re-confirm the contract change: `_ => Some(worktree_canon)` plus `find_root_by_markers`'
  unconditional fallback mean a server now spawns even with no project marker, where `None`
  previously meant "don't". `tsserver` on a marker-less tree is expensive.
- Run the SDK typecheck: `sdk/ui/src/first-party/index.ts` re-exports four modules whose `.tsx`
  sources are still untracked.

---

## Priority 1 — Correctness and security holes not covered by tests

### P1-1 · Lost `CancelRun` / `PauseRun`

`crates/codypendentd/src/executor.rs:2515-2538` vs `:2674-2705`. `pending_cancellations` and
`cancellations` are independent mutexes with the guard released between check and act on both
sides, so a cancel arriving during `spawn_run` can be recorded as "pending" *after* `spawn_run`
has already checked, and then be silently overwritten by an un-cancelled handle. Nothing
re-reads the pending set for a launched run. The client is told the cancel was accepted and the
run completes anyway.

Fix: one mutex for both maps, or re-check the pending sets after installing the handle and fire
the handle if an entry appeared.

### P1-2 · Scope escape via symlink after a `..` pop

`crates/daemon/src/policy/scope.rs:213-236`. `canonicalize_lenient` canonicalizes only the
deepest existing ancestor then appends the remainder lexically, so a `..` that pops back into
existing territory leaves subsequent **real** components — including symlinks — unresolved.
`<worktree>/nope/../link/etc/passwd` with a committed `link -> /` classifies as `Allowed`.

File tools are saved by `secure_fs.rs`'s `openat(NOFOLLOW)` re-walk. The `cwd` path is **not**:
`shell.rs:135-141` classifies `request.cwd` then `:161` spawns with the **raw** path (same at
`git.rs:62`, `unified_exec.rs:33`), so an approved allow-listed command can run outside every
granted root. `search.rs:143` is also unmitigated.

Fix: resolve symlinks for every component of the remainder, and switch the three `cwd` sites
from `classify` to `resolve` and spawn with the **resolved** path (the module documents `resolve`
as the no-TOCTOU seam).

### P1-3 · Unauthenticated peer can force an arbitrary directory walk

`crates/daemon/src/server.rs:5782` calls `maybe_scan_repository` with a client-supplied path
**before** the ownership gate at `:5786-5803`. Any peer completing the handshake — including one
whose attach is about to be refused — makes the daemon recursively walk and index an arbitrary
directory as the daemon's uid. Also reached from `CreateSession` (`:3648`).

Fix: gate first, warm after; constrain the path as `principal_owns_repository` does.

### P1-4 · A fifth `models.toml` writer bypasses the lock

`crates/cli/src/tui.rs:4962-5052` (`write_remove_model`) does its own read-modify-write and
`rename` with **no `flock`** (verified), breaking `models_file.rs`'s stated invariant that
"independent CLI/TUI/ACP updates cannot erase each other". Its temp file is **pid-only**
(`.models-remove-<pid>.tmp`) — the exact flaw `models_file.rs:106-113` documents as fixed on its
side — so two removals in one process can clobber each other.

Fix: route it through `models_file::update_model_entries`. Consider making `models_file` the only
module permitted to open that path.

### P1-5 · `models add` on an existing id wipes `api_key_env` and `context_tokens`

`crates/cli/src/commands.rs:3944-3963` drops the old entry and pushes a fresh one with
`api_key_env = ""`, then prints `updated model <id>`. Same "success message over silent
destruction" shape as the original bug, one level down. Intermittently invisible because the
model still resolves if a key sits in `auth.json` or the catalog env var happens to be set.

Fix: merge with the existing entry — preserve `api_key_env` and `context_tokens` unless
explicitly overridden. Regression test the re-add path.

### P1-6 · `auth.json` has no lock and a fixed temp path

`crates/runtime/src/auth.rs:97-135`. Permissions are handled meticulously, but every caller does
`load` → `set` → `save` unsynchronized, so concurrent key saves silently discard a credential;
and `data_dir.join("auth.json.tmp")` (`:100`) is shared by all writers in all processes, so two
saves can interleave and rename a mixed document into place.

Fix: mirror `models_file.rs` — advisory lock plus a unique temp name.

### P1-7 · Panicking workflow drive poisons the run permanently

`crates/codypendentd/src/workflows.rs:294-360`. `cancellations.finish` and `prune_run_lock` are
inline after the await, not `Drop`-based. A panic leaks the drive lock **and** leaves
`drive_active`/`cancelled` set forever — so a later legitimate resume or restart `recover()`
calls `register`, sees `cancelled == true`, and is **born cancelled**: a run that silently
refuses to progress.

Fix: RAII guard for both cleanups. Same pattern needed in `remote_ui_workers.rs` (P2-1).

---

## Priority 2 — Hardening

| ID | Item | Location |
|---|---|---|
| P2-1 | Remote UI worker quota leak on panic (cleanup inline, not `Drop`); `stop_plugin`/`shutdown` `clear()` **every** session's epoch set, permanently defeating the guard for unrelated live sessions; first worker to exit removes a multi-launch epoch. Six `.expect()` on `Option` payload fields are a concrete panic source. | `daemon/src/remote_ui_workers.rs:194-341`, `:564-591` |
| P2-2 | Learned approval patterns leave the argument **tail** unconstrained, so a learned `rg *` auto-approves `rg --pre=/tmp/evil` (arbitrary exec) and `cargo build *` auto-approves `--config build.rustc=…`. The `git -c` guard is prefix-only. | `daemon/src/policy/arity.rs:214-256` |
| P2-3 | Unknown keys inside `[[model]]` entries dropped on every write (no `serde(flatten)` capture); clamped `context_tokens` is **persisted**, permanently editing the user's file. | `cli/src/models_file.rs:101-104` |
| P2-4 | Hook engine silently drops an approved hook whose `spec_json` fails to parse — fail-open on a `failure = "block"` `tool.pre` validator. | `daemon/src/hook_engine.rs:71-77` |
| P2-5 | SQLite DB (full transcripts + tool output) created at `0644`, bypassing the `owner_uid`/`SO_PEERCRED` isolation added in migration 0031. Every other sensitive file is `0600`. | `daemon/src/db.rs:12-28` |
| P2-6 | Unbounded `read_file` range → ~67M `String` allocations (>1.5 GB RSS) from one tool call. Only `start != 0` and `end >= start` are validated. | `runtime/src/agent.rs:7149-7157`, `runtime/src/tools/read_file.rs:105-155` |
| P2-7 | Paused agent runs are **failed** by restart recovery, the opposite of the workflow layer, which preserves them for explicit resume. Lost user work; pick one semantic. | `daemon/src/recovery.rs:78-89` vs `workflows.rs:249-262` |
| P2-8 | DEFERRED transactions on read-then-write with no `SQLITE_BUSY` retry, where the rest of the codebase uses `BEGIN IMMEDIATE`. Touches invariant 4 (checkpoints before destructive modification). Not reproduced. | `daemon/src/checkpoints.rs:60/187/216`, `model_profiles.rs:258` |
| P2-9 | `WorktreeReleaseGuard`: the normal path terminates processes under the whole repository root for read-only runs; the unwind path terminates **nothing** for them, so children outlive the run. | `codypendentd/src/executor.rs:3389-3437` |
| P2-10 | No retention or pruning anywhere on `events` / `learning_records` / `model_task_outcomes` / `memory_forget_audits`; `load_events` is a full unpaginated scan per session. No missing index found. | `migrations/0001_init.sql:25-35`, `daemon/src/ledger.rs:64-72` |
| P2-11 | `is_denied_env` omits `RIPGREP_CONFIG_PATH`, `CARGO_BUILD_RUSTC`, `CARGO_TARGET_*_RUNNER`, `CARGO_ALIAS_*`, `GIT_PAGER`/`PAGER`/`GIT_EDITOR`/`GIT_SEQUENCE_EDITOR` — all execution hijacks for default-allow-listed programs. Needs a fresh human approval to exploit. | `runtime/src/tools/shell.rs:251-289` |
| P2-12 | `ensure_scanned` stamps the pre-lock revision while the scan stamps its own, so the map can assert a revision the graph does not hold, and a later run skips a needed scan. | `codypendentd/src/executor.rs:559-581` |

Remaining lower-severity items (D1–D14) are in [`findings-register.md`](findings-register.md):
`bounded_head_tail` slice panic for `max < 5`, UTF-8-splitting head/tail truncation,
non-atomic `routing.toml`/`trust_store`/`councils.toml` writes, reserved-process-id leak,
cosmetic `Blackboard` palette command, silent no-op `Steer`/`PauseResume`, O(n) transcript
eviction, `QuestionBroker` waiter growth, `admitting_network` re-widening after the untrusted
narrow, unguarded per-repository code-graph access, `auth.json`-outranks-env surprise, and a
stale `TODO(ownership)` that reads as an open hole but is not.

---

## Priority 3 — Documentation truth and feature gaps

### P3-1 · Correct ROADMAP.md in both directions

**Overclaims to soften** (evidence in the register, §E1):
- The router is real but **default-OFF**, and `load` also silently defaults on a *malformed*
  `routing.toml`. No user gets routing without hand-writing config. README:271 does not say so.
- The "classification hard filter" never receives a real per-run classification — both
  production call sites pass `None`, per an in-code admission at `executor.rs:827-835`. It fails
  closed, so the *safety* claim holds; the *capability* claim does not.
- 7.3's "cascading escalation re-executes a failed node" is marked `[x]` but `escalate` has
  **only test callers**; the code itself says it is "not yet driven by the single-agent live loop".
- 7.5's shadow/canary are DB state flips fed **caller-supplied** `CanaryMetrics`. The regression
  gate, by contrast, genuinely requires stored evidence.
- Voice: "*Nothing here has been run against a real speech provider*" (`transcription.rs:19-23`),
  and with no `routing.toml` the off-device ceiling defaults to `Confidential`, so
  default-classified media may be sent to a remote transcriber.

**Understatements to correct** — five items ROADMAP lists as unbuilt are built and wired:
session forking (STEP 5.6), the WASM host (`wasmi`, not `wasmtime`), the hook engine, client
voice capture, and live language-server spawning. Details and file references in §E3.

Recommended action: make each `[x]` mean "wired into a shipped default path" and add a distinct
marker for "engine built, not yet driven". That single change removes most of the
overclaim/understatement whiplash.

### P3-2 · Genuinely missing work, in rough value order

1. **Composer and terminal-native polish** — multiline, input history / reverse-search, `@`
   mentions, paste placeholders, queue-while-working; resize reflow, paste-burst, IME,
   hyperlinks. Highest user-visible value per unit of effort.
2. **A real measured routing run** — derive a per-run `DataClassification` so the hard filter
   has an input, then drive `escalate` from the agent loop. This converts three Phase 7
   `[x]`-with-caveats into honest completions.
3. **Real shadow execution** — mirror traffic and *measure* `CanaryMetrics` instead of accepting
   them from the request.
4. **Eval corpus** — 13 → 50–100 pinned fixtures, and note in the docs that the current gate
   cannot detect prompt/skill regressions because the model is a deterministic stub.
5. **Agentic `setup` assistant** — referenced in four docs, no crate code.
6. **WASM SDK** — the host is done; the SDK is what makes it usable.
7. **Generated protocol SDK** — the VS Code extension hand-duplicates the Rust wire codec; this
   is live drift risk, not just duplication.
8. **Migration compatibility** — ROADMAP:668-686 records that DBs created by
   `v0.1.0-build.43/.44/.45` cannot be opened by any later release (changed checksum on
   `0003_phase2.sql`). Either ship a repair path or document the break in release notes.

---

## Recommended execution order

**Immediately, before any commit of the current tree**
P0-1, P0-2, P0-3 — plus tests for the content-carries-wide-characters edit direction and a
palette-key cross-check test.

**Next**
P1-1 through P1-7. These are the defects a user would actually hit: a cancel that does nothing, a
sandbox escape through `cwd`, two config writers that lose data, and a resume that is born
cancelled.

**Then**
P2-1 through P2-12, starting with the RAII cleanup pattern (P2-1 shares its root cause with
P1-7) and the `0600` DB fix (P2-5, a one-liner).

**In parallel, cheap and high-trust**
P3-1. Correcting the roadmap costs no engineering time and fixes the largest gap between what
the project claims and what it is.

## Success metrics

- Working tree commits with no panic-reachable or file-corrupting path in `edit_file`.
- No palette command that reports an action it did not perform.
- `cargo test --workspace` stays at 0 failures with new regression tests for every P0/P1 item.
- One writer per config file, each holding a lock and using a unique temp name.
- A cancel accepted by the daemon always stops the run.
- Every ROADMAP `[x]` corresponds to a path the shipped default binary executes.

## Release philosophy

Unchanged and reaffirmed by this scan: ship fewer, fully reliable features. The two P0 items are
both cases of a feature reaching the user interface ahead of its correctness — a match stage that
corrupts files, and a command that claims a rollback it never performs. Both would erode trust
faster than the features they add would build it.
