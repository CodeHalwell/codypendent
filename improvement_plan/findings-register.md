# Codypendent — Findings Register

Scan date: 2026-08-15 · Baseline: `5c9dbbc` + uncommitted working tree

## Verification status of the baseline

| Check | Command | Result |
|---|---|---|
| Lint | `cargo clippy --workspace --all-targets` | clean, zero warnings |
| Tests | `cargo test --workspace` | **3165 passed, 0 failed, 2 ignored** |
| Publish path | `cargo test -p codypendent-codypendentd --lib publish::` | 21/21 pass |

Committed `HEAD` is healthy. Every finding below is either in **uncommitted working-tree
code** or a latent defect that the current test suite does not cover.

### Corrections to the previous plan (2026-08-15 README)

Three items in the earlier plan are **stale and should not be worked**:

1. *"3 failing tests in the documentation publishing path"* — all 21 `publish::` tests pass.
   The `safe.bareRepository` issue is gone.
2. *"`models add` can silently delete `[voice]`/`[transcription]`/… tables"* — **fixed**.
   All mutation is centralized in `crates/cli/src/models_file.rs`, which round-trips a
   parsed `toml::Value` root, takes a `flock`, and is pinned by regression tests
   (`models_file.rs:159-215`, `:240-269`). A narrower residue remains — see C3, C4.
3. *"Phase 6/7 incomplete"* is only half right. ROADMAP.md is **stale-pessimistic** on five
   items that are in fact built and wired — see section E.

---

## Re-verification — second independent scan (2026-08-16)

A second scan ran against the identical baseline (`5c9dbbc` + the same uncommitted
working tree: 7 modified files, 4 untracked SDK components). Its purpose was to
independently confirm the register before acting on it.

| Check | Result |
|---|---|
| `cargo check --workspace --all-targets` | clean |
| `cargo clippy --workspace --all-targets` | clean, zero warnings |
| `cargo test --workspace` | exit 0 — all suites pass, 0 failed (count not re-aggregated; baseline table above has the per-suite detail) |

Notably, this scan derived **A1** (the `unicode_normalized` coordinate-space bug) and
**A2** (the `/undo` no-op stub) directly from the raw `git diff` *before* consulting
this register — the two most important findings reproduce independently.

Findings re-verified line-by-line against the current tree (all still present):

| Finding | Evidence re-checked |
|---|---|
| A1 `edit_match` span bug | `edit_match.rs:524-531` — normalized-space `idx`/`end` slice the raw line; `end <= line.len()` is a range check, not a boundary check |
| A2 `/undo` stub | `reduce.rs:7175-7196` pushes only a transcript `Note`; no `Action::Undo` variant in `action.rs`; `RestoreCheckpoint` backend confirmed wired (`protocol/command.rs:767`, `daemon/server.rs:3024`, `daemon/checkpoints.rs:135`) and never called by any client |
| A3 duplicate `.py`/`.pyi` ownership | `servers.rs:24-28` (PYRIGHT) and `:48-52` (RUFF) both claim `[".py", ".pyi"]`; the `mod.rs:124-147` roster loop has no `break` |
| B1 `canonicalize_lenient` escape | `policy/scope.rs:213-236` — remainder after a `..` pop is appended lexically; symlink components in it are never resolved |
| B6 SQLite default permissions | `db.rs:12-28` — `create_if_missing(true)`, no `chmod`, contrasted with `0600` on `auth.json`/`models.toml` |
| C1 cancel/pause TOCTOU | `executor.rs:2519-2538` vs `:2674-2707` — three independent mutexes, guard released between check and insert on both sides |
| C3 unlocked `models.toml` writer | `tui.rs:4962+` (`write_remove_model`) — direct `read_to_string`/`DocumentMut`/`rename`, no flock, pid-only temp name |
| C10 unbounded `read_file` range | `agent.rs:7149-7157` — arbitrary `u64` bounds cast to `usize` with no cap |
| SDK #4 contract misuse | siblings pass `<Badge title= message= />` and spread `{...surface}`; the four new components pass Badge children/title-only, hardcode `SurfaceFrame id`, and do not spread `surface` |

No register entry checked in this pass was found stale, and no new findings beyond the
register were identified. Process caveat: this pass was verification-focused (spot
checks of the headline items plus a fresh TODO/`unimplemented!` sweep — only the two
known intentional TODOs at `council/service.rs:1099` and `daemon/server.rs:2842`
exist); the "Not covered" list below remains accurate for both scans.

---

## Re-check — whole-repository review (2026-10-07)

Run at `76448fc` (v0.14.0) as part of `docs/reviews/2026-10-07-review.md`. Each entry named
below was read against the current code. Entries not named were not re-checked. New findings
from that review are in its section 4 and are not duplicated here.

| Entry | State on 2026-10-07 | Evidence |
|---|---|---|
| B2 directory walk before the ownership gate | Fixed for attach | `server.rs`: the attach path warms the graph only after `principal_may_use_session`; both paths refuse an implausible root through `plausible_repository_root`. A same-uid client can still name any plausible checkout at create time (see D11). |
| B3 learned-pattern argument tail | **Open** | `policy/arity.rs` unchanged in substance. Needs a product decision; see review section 4, item 1. |
| B4 hook engine fails open on an unparseable spec | Partly fixed | An approved hook whose stored spec will not parse is now quarantined with an operator-facing notice (`QuarantinedHook`) instead of vanishing silently. It still does not block when its policy said `failure = "block"`. |
| B6 database permissions | Fixed | `db.rs` sets 0600 on the database, `-wal` and `-shm`. |
| C1 lost cancel/pause | Fixed | Verified 2026-09-23 (`RunControlRegistry`). |
| C2 panicking workflow drive | **Open** | `cancellations.finish` and `prune_run_lock` still run inline after the drive returns (`workflows.rs`), so a panic skips both. See review section 4, item 7. |
| C5 `auth.json` lock and temp path | Fixed | `auth.rs` takes a process mutex plus an advisory `flock` on `.auth.lock`, and the temp name carries the pid. |
| C8 remote UI worker quota and epochs | **Open, narrowed** | `stop_session` and `stop_session_target` now remove only their own epochs, and an old task no longer removes a replacement generation's entry. `stop_plugin` and `shutdown` still clear the whole epoch set, and the task's cleanup still runs inline after the await, so a panic in `run_worker` leaks a quota slot. See review section 4, item 7. |
| C9 `ensure_scanned` pre-lock revision | **Fixed by the review** | The revision is re-sampled after the repository lock is taken. |
| C10 unbounded `read_file` range | **Fixed by the review** | 2,000 lines, 2,000 characters a line and 128 KiB a call; four new tests. |
| C11 `WorktreeReleaseGuard` paths disagree | **Fixed by the review** | Only a run's own leased worktree is swept, on the normal and the unwind path alike. Two new tests. The old behaviour was worse than the entry says: releasing a read-only run killed every other run's interactive processes in the same checkout. |
| C12 deferred read-then-write transactions | **Fixed** (`checkpoints.rs` earlier, the rest by the review) | `checkpoints.rs` already used `BEGIN IMMEDIATE` at `76448fc`. The review added it to `learning.rs` (three sites) and `model_profiles.rs`, which had the same defect. Reproduced first: three of three concurrent-capture runs lost records on the old code. |
| C13 no event retention | **Open** | Nothing deletes from `events`; see review section 4, item 4. |

Not in this register, found and fixed by the review: provider-unsafe tool names on the wire,
the Anthropic 4,096-token output cap with truncation treated as success, ambiguous fuzzy
edit matches resolved by file order, a whitespace-only edit search, repository-controlled
instruction-file symlinks, a repository `policy.toml` symlink widening the file scope, mode
0600 on agent-created files, and a transport-error classification gap on the native
Anthropic and Gemini paths. See the review, section 3.

---

## A. Blockers in uncommitted code — do not commit as-is

### A1 · `edit_file` panics and can silently corrupt source files — CRITICAL

`crates/runtime/src/tools/edit_match.rs:524-530` (new `unicode_normalized` stage, wired in at `:56`)

```rust
} else if let Some(idx) = norm_line.find(&norm_search) {   // idx is in NORMALIZED byte space
    let end = idx + norm_search.len();                      // so is end
    if end <= line.len() {                                  // range check, NOT a boundary check
        let slice = &line[idx..end];                        // slices the ORIGINAL line
```

`normalize_unicode_punctuation` maps 3-byte characters (`“ ” — –`) and 2-byte ones (NBSP) to
1 ASCII byte, so normalization **shrinks** the string. Offsets computed on `norm_line` are
therefore invalid for `line`. `end <= line.len()` does not protect a char boundary.

Both failure modes were reproduced against the verbatim source logic:

**Panic.** Content line `x = “a”; let msg = "hi";`, search `let msg = "hi";`:
```
idx=9 end=24, guard passed (24 <= 32)
byte index 9 is not a char boundary; it is inside '”' (bytes 8..11)
```
**Silent wrong span.** Content line `“abcd let msg = "hi";`, search `let msg = "hi";`:
```
returned span: "d let msg = \"hi"   <-- handed to replace_range
```
The span is the right *length* (15 bytes) but shifted 2 bytes left, so `is_disproportionate`
does not fire. `edit_file.rs:146` executes `buffer.replace_range(...)` on it and writes the
result. The file is corrupted in a way the model never saw.

Aggravating factors:
- The replacer is evaluated for the **whole file** (`edit_file.rs` iterates the full `Vec`), so
  the offending line need not be the line being edited. One stray smart-quoted comment breaks
  every non-exact edit in that file.
- Contained today only by `tokio::task::spawn_blocking` catching the unwind into an opaque
  `ToolError("edit worker failed")`. **If any profile ever sets `panic = "abort"`, this becomes
  a remote daemon kill from model output.**
- The accompanying new test (`:875-883`) exercises only the safe whole-line branch (content
  ASCII, search smart). The buggy `find`/slice branch is untested. The bug needs the *content*
  to carry the wide characters — the common real-world direction.

**Fix:** do not mix coordinate spaces. Restrict the stage to whole-line/whole-block candidates
via `line_spans` (as all nine other stages do), or carry a normalized→original offset map.
A `line.is_char_boundary(idx) && line.is_char_boundary(end)` guard downgrades the panic to a
miss but leaves the wrong-span corruption.

Also in this stage: `'\u{0060}'` (backtick) → `'` at `:494` makes `'a'` match `` `a` `` — a
semantic change in JS/Rust/shell. Recommend dropping the backtick mapping.

### A2 · `/undo` is cosmetic and fabricates a completion record — CRITICAL

`crates/tui/src/palette.rs` advertises:
```rust
title: "/undo  Roll back checkpoint",
description: "restore the worktree to the previous checkpoint snapshot",
key: "u",
```
`crates/tui/src/reduce.rs:7175-7196` delivers a transcript `Note` reading
`"Undo requested for run {}: restoring latest checkpoint"` and **nothing else** — verified no
`state.outbox.push` anywhere in the arm. Per the TUI's own invariant, an arm with no outbox
push does zero work outside local `AppState`. The worktree is never touched.

Worse than a dead menu item: the note is present-progressive, asserting the rollback *is
happening*, and `push_entry` puts it in durable scrollback indistinguishable from a real
system note. A user who reads it and then commits is building on changes they believe were
reverted.

**The backend is fully built.** `CommandBody::RestoreCheckpoint` exists
(`crates/protocol/src/command.rs:767`), the daemon handles it (`crates/daemon/src/server.rs:3024`),
it is approval-gated through `ProposedAction::RestoreCheckpoint`
(`crates/daemon/src/checkpoints.rs:135`, `policy/mod.rs:410`) and is correctly non-reusable.
The **only** missing piece is a TUI `Intent` — `crates/tui/src/action.rs` has no
checkpoint-restore variant, and no client anywhere sends the command.

Secondary: the advertised `key: "u"` does not exist. `input.rs:488` binds only `Ctrl-U` to
`DeleteToLineStart`. `palette.rs`'s guard test asserts only `!entry.key.is_empty()`, never
cross-checking `input.rs`, which is why the phantom hint passes CI.

**Fix:** add the intent and route it through `Overlay::ConfirmCancel`-style confirmation (a
worktree rollback is destructive). Until then, delete the palette entry. Strengthen the
palette test to cross-check every `key` against `input.rs`.

### A3 · Every Python file now spawns two language servers — MEDIUM

`crates/knowledge/src/lsp/servers.rs` adds `RUFF { extensions: &[".py", ".pyi"] }` while
`PYRIGHT` already claims `.py`. The `for spec in servers::ROSTER` loop in
`crates/knowledge/src/lsp/mod.rs:124-147` has no `break`, so a `.py` file matches both specs
and the code spawns, `touch`es and `wait_for_diagnostics` on **both** — doubling latency on
every Python diagnostic and duplicating overlapping diagnostics into `all_diags`.

Related in the same diff: `_ => Some(worktree_canon.clone())` and `find_root_by_markers`'
unconditional `Some(worktree_canon)` fallback change the contract — previously `None` meant
"don't spawn". Now a server is spawned for any matching extension even with no project marker
(no `package.json`, no `go.mod`). Confirm that is intended; `tsserver` on a marker-less tree
is expensive.

Also: `ruff` is a linter, not a full language server, and is given `pyright_root` and
`&["server"]`. Verify `ruff server` actually satisfies the `initialize`/`touch`/diagnostics
handshake `LspClient` expects.

### A4 · New SDK exports have no corresponding build check — LOW

`sdk/ui/src/first-party/index.ts` now re-exports `./agent-dashboard.js`, `./browser-view.js`,
`./checkpoint-timeline.js`, `./diff-inspector.js`. The four `.tsx` sources are untracked
(`git status` shows `??`). Run the SDK typecheck/build before committing, and confirm the
`.tsx` → `.js` extension mapping matches the module resolution used by the rest of the SDK.

---

## B. Security findings

### B1 · Scope escape: `canonicalize_lenient` misses symlinks after a `..` pop — HIGH — **FIXED (2026-08-25, released in v0.14.0)**

**Status:** both halves closed. (1) The `policy/scope.rs` copy was already
rewritten in v0.9.0 (#68) to re-canonicalize per component, with the regression
`policy::scope::tests::lenient_resolves_symlink_after_parent_dir_pop`. (2) The
**duplicate** `worktrees.rs` copy still carried the old walk-up-and-append-the-remainder-lexically
implementation and was deleted; `worktrees.rs` now uses the shared
`crate::policy::canonicalize_lenient` (re-exported `pub(crate)` from
`policy/mod.rs`), with regression
`worktrees::tests::ensure_outside_repository_sees_through_a_symlink_after_a_pop`.
(3) The second defect (check/act gap) is closed: `runtime/src/tools/shell.rs`,
`unified_exec.rs`, and `git.rs::guard` now call `PathScope::resolve` and act on
the returned resolved cwd (`current_dir`/`git -C`/`OpenProcessSpec.cwd`),
with regression `runtime/tests/tools_it.rs::shell_run_refuses_cwd_that_escapes_via_symlink_after_pop`.
`search.rs` needs no change: its defensive filter goes through
`scope.classify`, which canonicalizes internally.

Original finding — `crates/daemon/src/policy/scope.rs:213-236`. The function canonicalizes only the deepest
*existing* ancestor, then appends the remainder **lexically**. If the remainder contains a `..`
that pops back into existing territory, every component after it is a real filesystem
component that is never resolved — including a symlink.

With `fs_read/fs_write = $WORKTREE` and a symlink `<worktree>/link -> /` (which can be
committed in the repo, so no write capability is needed):

- input `<worktree>/nope/../link/etc/passwd`
- full `canonicalize` fails (`nope` missing) → loop walks up to `<worktree>`, which resolves
- remainder walks `Normal(nope)` push, `ParentDir` pop, then `link`, `etc`, `passwd` verbatim
- result `<worktree>/link/etc/passwd` → `classify_canonical` returns **`Allowed`**

The module doc (`scope.rs:5-10`) promises "a traversal or symlinked parent cannot escape
containment". That is false for this shape. Existing tests only cover a symlink in the
*prefix* (`lenient_resolves_symlink_prefix`).

Blast radius:
- **Mitigated** for `read_file`/`write_file`/`edit_file`: `crates/runtime/src/tools/secure_fs.rs:39-140`
  re-walks with `openat(..., NOFOLLOW)` anchored at the granted root, so a symlink component
  fails `ELOOP`. This is the only reason it is not arbitrary file write.
- **Not mitigated** for `cwd`: `crates/runtime/src/tools/shell.rs:135-141` classifies
  `request.cwd` then `:161` spawns with `.current_dir(&request.cwd)` — the *raw* path. Same at
  `git.rs:62`, `unified_exec.rs:33`. An approved allow-listed command runs with a working
  directory outside every granted root.
- **Not mitigated** for `crates/runtime/src/tools/search.rs:143`.

Second defect at those three sites: they use `classify` where the module explicitly documents
`resolve` as the no-TOCTOU seam. `classify(cwd)` + `current_dir(cwd)` is a check/act gap even
once B1 is fixed.

### B2 · Unauthenticated peer can force an arbitrary directory walk — MEDIUM — **FIXED for attach (verified 2026-10-07)**

`crates/daemon/src/server.rs:5782` calls `maybe_scan_repository(state, repository)` **before**
the ownership gate at `:5786-5803`, deliberately ("so a probing re-attach with a remembered id
still warms the graph"). The path comes verbatim from the request with no scope and no relation
to anything the principal owns. Any peer that completes the handshake — including a different
uid whose attach is about to be refused as `session-not-found` — makes the daemon recursively
walk and parse an arbitrary directory as the daemon's uid and fold it into the daemon-wide code
graph. Unauthorized read side effect plus trivial CPU/IO amplification (`/`, another user's
`$HOME`). Reached from `CreateSession` too (`:3648`). **Fix:** gate first, warm after, and
constrain the path the way `principal_owns_repository` does for `SearchWorkspaceFiles`.

### B3 · Learned approval patterns leave the argument tail unconstrained — MEDIUM-HIGH — **OPEN (re-checked 2026-10-07)**

`crates/daemon/src/policy/arity.rs:214-256`. `command_pattern` refuses to learn when a flag
appears in the learned **prefix**, but `pattern_matches` accepts **any tail**. For allow-listed
programs whose code-execution switch is a trailing flag, one "always allow" is a blank cheque:

- `rg` is absent from the `ARITY` table → pattern is `rg *`. `rg --pre=/tmp/evil <pat>` makes
  ripgrep **execute** that program per file. Auto-approved by `approvals.rs:920-941`.
- `cargo build --config build.rustc="/tmp/evil"` matches a learned `cargo build *` and runs an
  arbitrary binary as the compiler.

The `git -c` case already guarded at `:229-238` is one instance of a general problem. Not
reachable via env (`command_pattern` refuses non-empty `environment`) — purely via arg flags.

### B4 · Hook engine fails open on an unparseable spec — MEDIUM — **PARTLY FIXED (verified 2026-10-07)**

`crates/daemon/src/hook_engine.rs:71-77`: `if let Ok(spec) = serde_json::from_str::<HookSpec>(...)`
drops an approved hook whose stored `spec_json` no longer parses (schema drift, downgraded
daemon, DB tampering) with no log, no audit row, no error. For a `tool.pre` hook with
`[policy] failure = "block"` the entire point is that failure blocks; here it silently never
runs and `dispatch_tool_pre` returns `Proceed`. Every other failure path in the file at least
warns. Should be a hard error or a synthesized block verdict.

### B5 · `is_denied_env` gaps for default-allow-listed programs — LOW-MEDIUM

`crates/runtime/src/tools/shell.rs:251-289` denies `LD_*`, `DYLD_*`, `GIT_CONFIG*`,
`GIT_SSH_COMMAND`, `*_WRAPPER`, `PYTHON*`, `RUSTC` — but not `RIPGREP_CONFIG_PATH` (can carry
`--pre=`), `CARGO_BUILD_RUSTC` / `CARGO_TARGET_*_RUNNER` / `CARGO_ALIAS_*`, or
`GIT_PAGER`/`PAGER`/`GIT_EDITOR`/`EDITOR`/`GIT_SEQUENCE_EDITOR` (each executes a command on an
ordinary `git log`/`commit`/`rebase`). Held below HIGH because env-bearing invocations are never
learnable and the env is in the action digest shown on the approval card, so exploitation needs
a fresh human approval.

### B6 · SQLite database created at default permissions — MEDIUM — **FIXED (verified 2026-10-07)**

`crates/daemon/src/db.rs:12-28` — `create_if_missing(true)` with no subsequent `chmod`, so the
DB plus `-wal`/`-shm` land at `0666 & ~umask`, typically `0644`. Contrast the deliberate `0600`
on `models.toml` (`models_file.rs:130-137`), `auth.json` (`auth.rs:104-134`) and even the lock
file. The DB holds full session transcripts and tool output — strictly more sensitive than the
endpoint list that *is* hardened. Migration 0031 added `owner_uid` + `SO_PEERCRED` specifically
so one local user cannot read another's sessions; that control is bypassed by reading the file.

---

## C. Correctness and data-integrity findings

### C1 · Lost `CancelRun` / `PauseRun` — check-then-act across two mutexes — HIGH — **FIXED (verified 2026-09-23)**

`crates/codypendentd/src/executor.rs:2515-2538` (`spawn_run`) vs `:2674-2705`
(`cancel_run`/`pause_run`). `pending_cancellations` and `cancellations` are independent
`std::sync::Mutex`es and the guard is released between check and act on both sides. No `.await`
is needed — the lock release is the yield point:

1. `spawn_run` removes from `pending_cancellations` → `false`, releases.
2. `spawn_run` removes from `pending_pauses` → `false`, releases.
3. `cancel_run` locks `cancellations`, `get` → `None` (handle not installed yet), releases;
   inserts into `pending_cancellations`.
4. `spawn_run` inserts an **un-cancelled** handle.

Nothing re-reads `pending_cancellations` for a launched run — the only other reader (`:2637-2646`)
just deletes the entry. The run drives to completion after the client was told the cancel was
accepted. **Fix:** one mutex for both maps, or re-check the pending sets *after* installing the
handle and fire the handle if an entry appeared.

### C2 · Panicking workflow drive poisons the run permanently — MEDIUM-HIGH — **OPEN (re-checked 2026-10-07)**

`crates/codypendentd/src/workflows.rs:294-360`. `spawn_drive` puts
`host.cancellations.finish(&run_id)` (`:356`) and `host.prune_run_lock(...)` (`:358`) inline
after the await, not in a `Drop` guard. A panic in `conductor.drive`, a `NodeExecutor` or an
observer unwinds past both:

- the `DriveLockRegistry` entry is never pruned → unbounded growth (exactly what
  `prune_run_lock`'s doc comment says it exists to prevent);
- `drive_active` stays `true` forever. `deregister` (`workflow_exec.rs:620-636`) refuses to
  prune while it is set and `cancel` (`:648-657`) keeps the entry, so only `finish` can clear
  it. The sticky `cancelled` flag survives the dead drive, and a **later legitimate drive** of
  the same run id — a resume, or a restart `recover()` — calls `register` (`:604-616`), sees
  `cancelled == true`, and is **born cancelled**: a resumed run that silently refuses to
  progress.

**Fix:** RAII guard for both cleanups.

### C3 · A fifth `models.toml` writer bypasses the advisory lock — HIGH

`crates/cli/src/tui.rs:4962-5052` (`write_remove_model`) correctly uses `toml_edit::DocumentMut`
so foreign tables and comments survive, but does its own read-modify-write and its own `rename`
and **never takes `.models.toml.lock`** (verified: no `lock`/`flock`/`models_file::` reference in
the function). `models_file.rs:51-54` promises "independent CLI/TUI/ACP updates cannot erase each
other" — that invariant does not hold. A TUI removal concurrent with `models add`/`models pull`/
an ACP connect is a lost update: the removed model reappears, or the added model vanishes. The
window is a full file read plus user-visible work.

Same function: the temp file is `.models-remove-<pid>.tmp` — **pid-only**, the exact flaw
`models_file.rs:106-113` documents as fixed on its side. Two removals in one TUI process share
the path, so one can `rename` the other's half-written render into place.

### C4 · `models add` on an existing id wipes `api_key_env` and `context_tokens` — MEDIUM-HIGH

`crates/cli/src/commands.rs:3944-3963`: `api_key_env: key_env.unwrap_or_default().to_string()`
then `configs.retain(|c| c.id.0 != display_id); configs.push(config);`. Re-running
`codypendent models add <provider> <model>` without `--key-env` deletes the existing entry and
pushes a fresh one with `api_key_env = ""`, then prints `updated model <id>` — the same
"success message over silent destruction" shape as the original bug, one level down.
`context_tokens` is likewise reset to the catalog value. Intermittently invisible: the model
still resolves *if* a key sits in `auth.json` or the catalog's documented env var happens to be
set; otherwise requests start 401ing.

### C5 · `auth.json` has no lock and a fixed temp path — MEDIUM — **FIXED (verified 2026-10-07)**

`crates/runtime/src/auth.rs:97-135`. Permissions are handled meticulously (temp at `0600`,
re-`chmod` before secret bytes, `sync_all`, `rename`, post-rename `chmod`, all pinned by tests).
Two gaps: (1) **no lock** — every caller does `load` → `set`/`remove` → `save`
(`tui.rs:5032`, `:5180-5185`, `:4730`), an unsynchronized read-modify-write over the whole map,
so concurrent key saves silently discard one credential; (2) `let tmp = data_dir.join("auth.json.tmp")`
(`:100`) is a **fixed path shared by all writers in all processes**, so two saves can interleave
and rename a mixed document over `auth.json`. `models_file.rs` solved both; `auth.rs` did not.

### C6 · Unknown `[[model]]` keys dropped, and clamped values persisted — MEDIUM

`crates/cli/src/models_file.rs:101-104` re-serializes the `model` array from
`Vec<ModelConfig>`, which has no `#[serde(flatten)]` capture. The doc's claim that "an unknown
table is carried through untouched" is true at *table* level and false at *per-entry key* level:
any key a user or future version writes inside a `[[model]]` block is deleted by the next
add/remove/pull. Related: `load_models` clamps `context_tokens` to
`MAX_PLAUSIBLE_CONTEXT_TOKENS` (`models.rs:166-180`), so an over-large value is not merely
rejected at read time — the next write **persists** the clamp, permanently editing the file.

### C7 · Paused agent runs destroyed by restart recovery — MEDIUM — **FIXED (2026-08-25, released in v0.14.0)**

**Status:** the agent layer now matches the workflow layer — paused runs are
preserved. (1) `recover_live_runs` skips `RunState::Paused` (reported in the
new `RecoveryReport.preserved_paused` field, logged at boot); their pending
approvals survive because `expire_orphaned` only touches terminal runs. (2)
`RuntimeExecutor::resume_run` now re-drives a run with **no live handle**:
when `ResumeRun`'s projection flipped a parked run `Paused` -> `Running` but
the in-process loop died with the previous process, the executor re-spawns it
from the durable row — `spawn_run` reconstructs the seed transcript from the
session ledger (`build_run_seed`/`reconstruct_prior`), and the repository and
pinned model are recovered from the originating command exactly as
`relaunch_queued_runs` does. The re-drive spawns only for a row still
`Running` (a raced `PauseRun`/`CancelRun` is a clean no-op), and a
pause-arrives-before-spawn race is consumed by `RunControlRegistry::register`
via `pending_pauses`. Re-driving is safe because the agent loop parks only at
step boundaries (the two `wait_until_running` sites in
`runtime/src/agent.rs`): the worst case re-derives one unrecorded model
response and never re-executes a tool call. Regressions:
`daemon/tests/recovery_it.rs::paused_run_at_boot_is_preserved_awaiting_resume`,
`codypendentd executor::tests::resume_without_a_live_handle_redrives_the_run`,
`executor::tests::resume_does_not_redrive_a_run_that_was_paused_again`.

Original finding — `crates/daemon/src/recovery.rs:78-89`. includes `RunState::Paused` in `is_live`, and step 4 of
`recover_on_startup` fails every live run with `"daemon restart"`. A run the user deliberately
paused is failed on the next boot. The workflow layer does the **opposite** —
`WorkflowConductorHost::recover` (`workflows.rs:249-262`) explicitly `continue`s on
`WorkflowRunState::Paused` to await an explicit resume. Not a double-execution bug (recovery is
idempotent), but lost user work and an inconsistency between the two orchestration layers.
Decide which is correct and align them.

### C8 · Remote UI worker: quota leak and broken epoch bookkeeping — MEDIUM-HIGH — **OPEN, narrowed (re-checked 2026-10-07)**

`crates/daemon/src/remote_ui_workers.rs:194-230`. Three defects:

1. **Quota leak.** Cleanup of the `active` map is inline after the await, not `Drop`-based, so a
   panic in `run_worker` leaves the `ActiveWorker { cancellation, memory_mb }` entry forever.
   That entry is what `worker_quota_denial` counts against `MAX_ACTIVE_WORKERS` and the memory
   budget, so each panic permanently burns a slot until every `ensure_session_*` reports
   "aggregate admission quota is full". A concrete panic source is on that path:
   `forward_to_worker` (`:564-591`) uses six `.expect("broker …")` calls on `Option` payload
   fields whose `kind` string is matched separately — any kind/payload mismatch panics.
2. **`stop_plugin` / `shutdown` clear the epoch set globally** (`:294-300`, `:334-341`). They
   remove `active` entries for one plugin but `self.ensured.lock().clear()` wipes **every**
   session's epoch marker. The next `ensure_session_filtered` for an unrelated session
   re-inserts the epoch, skips all launches via `active.contains_key`, ends with `started == 0`
   and therefore removes the epoch again (`:232-238`) — the guard is permanently defeated and
   every later call repeats the walk reporting `0`.
3. **First worker to exit removes a multi-launch epoch.** The epoch key is
   `(session_id, target)` but one epoch can start many launches (one per plugin); each task
   removes `ensured[epoch]` when *it* exits, wedging siblings into the state in (2).

### C9 · `ensure_scanned` records a pre-lock revision — LOW-MEDIUM — **FIXED (2026-10-07, branch `claude/review-2026-10-07`)**

`crates/codypendentd/src/executor.rs:559-581` samples `scan::head_revision(root)` **before**
`scan::lock_repository(repository).await`, but `scan_repository` stamps the graph with
`working_tree_revision` sampled *inside* the scan (`scan.rs:110`). If the tree changes while
this caller blocks on the lock (branch switch, `git pull`, the agent's own `edit_file` folded by
the watcher), the in-process map asserts "revision R is folded" while the graph holds R'. A
later run at R skips the scan and opens with a repository map that does not match the tree. The
double-check-under-lock correctly fixes the concurrent-rebuild race; this stale stamp is a
separate remaining hole.

### C10 · Unbounded `read_file` range → daemon memory DoS — MEDIUM — **FIXED (2026-10-07, branch `claude/review-2026-10-07`)**

`crates/runtime/src/agent.rs:7149-7157` converts arbitrary `u64` range bounds to `usize` with
no upper bound; `crates/runtime/src/tools/read_file.rs:105-155` validates only `start != 0` and
`end >= start`. `{"path": "big.txt", "range": [1, 18446744073709551615]}` retains one heap
`String` per line for the whole window. The only ceiling is `MAX_READ_BYTES = 64 MiB` on bytes
*read*, so a 64 MiB file of short lines (~67M newlines) allocates ~67M `String`s → well over
1.5 GB RSS. **Fix:** clamp `want_end` to `want_start + HARD_CAP` and/or cap retained bytes.
The no-`range` default path is safely capped at 200 lines.

### C11 · `WorktreeReleaseGuard`: normal and unwind paths disagree — LOW-MEDIUM — **FIXED (2026-10-07, branch `claude/review-2026-10-07`)**

`crates/codypendentd/src/executor.rs:3389-3437`. `release()` always calls
`terminate_under(&binding.worktree)`; `Drop` calls it **only** `if binding.lease.is_some()`. For
a read-only run `lease` is `None` and `worktree` is the repository root, so: on the normal path
a read-only run terminates processes under the *whole repository root* (wider blast radius than
it owns), and on the unwind path its `shell.run` children are **not** terminated at all and
outlive the run. Note the cancel path is fine for leasing runs — the token unwinds
`runtime.execute_run` and `release()` is the next statement (`:1167`).

### C12 · DEFERRED transactions on read-then-write, no busy retry — SUSPICION, MEDIUM — **FIXED, and reproduced (2026-10-07, branch `claude/review-2026-10-07`)**

`crates/daemon/src/checkpoints.rs:60` (also `:187`, `:216`) and
`crates/daemon/src/model_profiles.rs:258` use `pool.begin()` then `SELECT` then `INSERT` then
commit. The codebase knows the right pattern and uses it elsewhere —
`ledger.rs:260` and `questions.rs:116/308/393` all use `begin_with("BEGIN IMMEDIATE")`. In WAL a
DEFERRED transaction takes its read snapshot on the first `SELECT`; if another connection
commits before the later `INSERT`, SQLite returns `SQLITE_BUSY` on a snapshot conflict and
`busy_timeout` **cannot** resolve it — the transaction must be retried, and no retry logic
exists (`grep SQLITE_BUSY` in `db.rs`/`ledger.rs`/`approvals.rs` returns nothing) while
`max_connections(8)` makes concurrency real. If it fires, checkpoint recording fails
intermittently — which touches runtime invariant 4. Not reproduced.

### C13 · No retention or pruning on the event store — MEDIUM — **OPEN (re-checked 2026-10-07)**

`migrations/0001_init.sql:25-35` defines `events` as an append-only ledger whose `body` holds
every prompt, model message and tool observation. No `DELETE FROM events`, no `VACUUM`, no TTL
anywhere in `crates/daemon/src` — the five `prune_*` functions all prune in-memory broadcast
channels and processes, never rows. `learning_records` (0024), `model_task_outcomes` (0025) and
`memory_forget_audits` (0029) are likewise declared append-only with no compaction path. The
local DB grows without bound with conversation volume, and `load_events`
(`ledger.rs:64-72`) does a full unpaginated `SELECT ... ORDER BY sequence ASC` per session.
No *missing index* was found — every hot path is covered by the `(session_id, sequence)` PK or
an explicit index.

---

## D. Lower-severity and hygiene

| ID | Finding | Location |
|---|---|---|
| D1 | `bounded_head_tail` hardcodes head=3/tail=2 against a caller-supplied `max`; with `max < 5`, `lines[..3]` panics and `lines.len() - tail_count` underflows. Safe only because both callers pass 5. | `crates/tui/src/render.rs:2895-2912` |
| D2 | Head/tail truncation splits UTF-8 mid-character at both seams; consumers use `from_utf8_lossy` so no panic, just U+FFFD garbling. The runtime is otherwise scrupulous here (`artifact.rs:66-84`, `git.rs:180-189`, `salient.rs:148-157` all walk to a boundary) — this is the outlier. | `crates/daemon/src/unified_exec/head_tail_buffer.rs:62-137` |
| D3 | `routing.toml` written with plain `std::fs::write` — no temp+rename, no fsync, no lock. A crash mid-write truncates the operator's `DataClassification` declarations. Same shape in `sandbox/src/trust_store.rs:149` (trusted-publisher keys) and `council/src/service.rs:1488`. | `crates/cli/src/commands.rs:4440-4446` |
| D4 | Reserved process-id leak: cancellation between `allocate_process_id` and the store insert leaves the id in `reserved_ids` with no TTL. The process itself is not leaked (`Drop for UnifiedExecProcess` kills the child). Worst case the unbounded `loop` retry spins while holding the store mutex — needs ~99k leaks. | `crates/daemon/src/unified_exec/manager.rs:61-83`, `:141-250` |
| D5 | `exec` computes its read deadline from a `start` taken *before* the 150 ms early-exit grace wait, silently shortening the collection window by up to 150 ms (effective floor 100 ms against `MIN_YIELD_TIME_MS = 250`). | `crates/daemon/src/unified_exec/manager.rs:148-164` |
| D6 | `PaletteCommand::Blackboard` sets the overlay then calls `watch_focused_blackboard_run`, which pushes an intent only if a focused item/node already resolves. On a cold client both are empty, so the overlay opens blank and nothing is requested. Every comparable command refreshes unconditionally. | `crates/tui/src/reduce.rs:7217-7220` |
| D7 | `Steer` and `PauseResume` return silently with no `notice` when there is no selected run / the run is terminal — palette closes, nothing visibly happens. | `crates/tui/src/reduce.rs:3885-3889`, `:3476-3491` |
| D8 | Transcript is correctly capped at `MAX_TRANSCRIPT_ENTRIES = 2000` (so **no** memory leak), but eviction uses `Vec::remove(0)` — O(n) shifting of up to 2000 elements twice per streamed entry once at the cap. Only `transcript_selected`/`scroll` are fixed up; any other retained index is silently invalidated. | `crates/tui/src/state.rs:3338-3347` |
| D9 | `QuestionBroker` waiter entries survive a dropped awaiting future (run cancelled mid-question) until something else resolves or expires the question. Bounded per daemon lifetime by boot-time expiry. | `crates/daemon/src/questions.rs:169-199` |
| D10 | `admitting_network` extends `network_allow` on an already-merged engine, i.e. **after** `apply_untrusted_overlay` intersected it, so a repo-local `network.allow = []` can be undone. The overlay function itself only narrows; this is the composition around it. Unverified impact — GitHub writes stay approval-gated. | `crates/daemon/src/policy/mod.rs:289-292` |
| D11 | `BuildCodeGraph`/`ReadCodeGraph` name only `DaemonStore::CodeGraph`, which reduces to `principal.owns(daemon_uid)`. The comment claims a per-repository gate "lives inside the seam", but `codegraph.rs` only resolves repository *identity* from the path — it never compares against what the principal owns. Same-uid clients can build/read a graph for any checkout. | `crates/protocol/src/command.rs:1015-1021` |
| D12 | `auth.json` **outranks the environment** in `api_key_for`. Documented as intentional (`tui/src/state.rs:1887`), but a user who rotates `OPENAI_API_KEY` after ever saving a key in the TUI keeps authenticating with the stale stored key, with no diagnostic pointing at `auth.json` outside `codypendent doctor`. | `crates/runtime/src/models.rs:850-895` |
| D13 | Stale `TODO(ownership)` — `SearchWorkspaceFiles` **is** guarded by `principal_owns_repository` (`server.rs:4766-4791`); the TODO only asks for the check to move into the central gate. Reword so it does not read as an open hole. | `crates/daemon/src/server.rs:2842` |
| D14 | `report_rewrite` updates every `hook_dispatches` row matching `(run_id, subject_digest, verdict='rewrite')` with no bound — audit-fidelity smell. | `crates/daemon/src/hook_engine.rs:270-282` |

---

## E. Feature gaps — and ROADMAP.md corrections

### E1 · Genuine overclaims (docs promise more than the code delivers)

| Claim | Reality |
|---|---|
| Phase 7 router is wired (ROADMAP:99-108) | Real seam — `executor.rs:814-864` calls `routing.select(...)` on the production path — but `RoutingConfig::default { enabled: false }` (`routing.rs:114-127`) and `select` early-returns `None` (`:374`). `load` also returns the default when `routing.toml` is absent **or malformed**. No user gets routing without hand-writing a config. README:271 does not disclose this. |
| "classified data can never be routed to a hosted provider" (ROADMAP:100, 519-522) | Both production call sites pass `None` for classification, with an in-code admission at `executor.rs:827-835`: *"`RunLaunch` carries no per-run classification… Deriving a real per-run classification is a documented follow-up."* Falls back to `DataClassification::Unknown`, so the **safety** claim survives (fails closed) but the **capability** claim does not: nothing classifies the data. |
| 7.3 `[x]` "cascading escalation re-executes a failed node on the next chain tier" (ROADMAP:522-526) | `escalate`/`record_escalation` (`routing.rs:471-545`) have **only test callers**. The code says so at `:239-241`: "not yet driven by the single-agent live loop." Nothing in the agent loop detects node failure and re-drives. |
| 7.5 `[x]` shadow/canary state machine (ROADMAP:534-541) | `StartShadow`/`StartCanary` (`promotion.rs:254-263`) only flip DB state — no candidate executes, no traffic mirrors. `ObserveCanary { metrics }` takes `CanaryMetrics` **from the request payload**, so the auto-rollback is real but its trigger is whatever an operator types. By contrast the *regression* gate genuinely requires a stored `SuiteReport` (`:200-247`). |
| "multimodal input (text/voice/image)" (README:9) | Implementation is real (OpenAI-compatible `/audio/transcriptions`) but `transcription.rs:19-23` states outright: *"This machine has no audio hardware and no provider credentials… Nothing here has been run against a real speech provider."* `from_paths` returns `None` with no `[transcription]` table, so the daemon rejects audio with `voice.transport-unavailable`. **Privacy note:** voice reuses `routing.toml`'s `max_off_device`, which with no `routing.toml` is `Confidential` — default-classified media may be sent to a remote transcriber (`:73-84`). |
| Eval corpus | 13 cases against a stated target of 50–100 (ROADMAP:512-517, self-disclosed). The `eval-regression` baseline is 13/13, and ROADMAP:499-505 warns the gate cannot detect prompt/skill regressions because the model is a deterministic stub. |
| Migration stability | ROADMAP:668-686 records, with hashes, that a DB created by `v0.1.0-build.43/.44/.45` **cannot be opened** by any later release: `sqlx::migrate` rejects the changed checksum of `migrations/0003_phase2.sql`. A genuine shipped-compatibility break, honestly logged. |

### E2 · Genuinely missing (no implementation found)

- **Agentic `setup` assistant** — `SetupAssistant`/"setup assistant" appears only in docs
  (`docs/docs/15-roadmap.md`, `build/16-phase-6-*`, `99-master-acceptance-checklist.md`).
  No crate code.
- **WASM SDK** — the *host* is built (see E3) but `wasm.rs:47-49` says "There is no WASM SDK
  yet, which is why the table is normative rather than illustrative."
- **Composer polish** — multiline, input history / reverse-search, `@` mentions, paste
  placeholders, queue-while-working (ROADMAP:620-628, `[ ]`).
- **Terminal-native polish** — resize reflow, paste-burst, IME, hyperlinks (same section).
- **Generated protocol SDK** — the VS Code extension hand-duplicates the Rust wire codec, a
  live drift risk (ROADMAP:653-656).
- Not verified either way: 7.4's OTLP exporter for graders/clustering (ROADMAP:527-533) and
  7.5's eval-export privacy scrubbing (`:540-541`).

### E3 · ROADMAP.md is stale-pessimistic — five items listed as unbuilt are built and wired

Correct these so the roadmap stops understating the product:

1. **Session forking (STEP 5.6)** — ROADMAP:45, :86, :427-429 call it "the one Fleet-adjacent
   overlay not built". It is built end to end: `CommandBody::ForkSession`
   (`protocol/src/command.rs:772-790`, round-trip test `:1770`), daemon handler
   (`daemon/src/server.rs:3190`), full post-validation flow in **`daemon/src/forks.rs`**,
   idempotent reservation/finalize (`daemon/src/commands.rs:380-470`), TUI `Intent::ForkSession`
   (`tui/src/reduce.rs:3215`, `:5453`), CLI driver `fork_session_live`
   (`cli/src/tui.rs:6136-6167`). Only the TUI "side conversations & forks" UX bullet
   (ROADMAP:624) is plausibly open.
2. **WASM runtime** — ROADMAP:441-443, :487-497 list "the `wasmtime` component runtime" as
   unbuilt. **`crates/sandbox/src/wasm.rs`** is a complete guest host on **`wasmi`** (0.51,
   `default-features = false`, with a written rationale for rejecting wasmtime): enforced
   fuel/instruction budget, linear-memory cap, wall-clock deadline that terminates the guest,
   output and host-I/O byte budgets, no WASI linked, `start`-section modules refused, a
   three-import ABI through `CapabilityBroker`. Wired into production skill execution at
   `knowledge/src/skill_exec.rs:583`, with adversary tests in
   `sandbox/tests/wasm_adversary_it.rs`.
3. **Hook engine** — ROADMAP:441, :487-491 list it as remaining. `daemon/src/hook_engine.rs`,
   `hook_exec.rs` and `hooks.rs` consume the sandbox hook types, `policy_gate.rs:35` uses
   `PolicyReentry`, `runtime/src/agent.rs:75` imports it, and `hook_exec` pins
   `HookNetwork::Deny`.
4. **Client voice capture** — ROADMAP:433, :443, :503 list it as remaining.
   **`crates/cli/src/voice.rs`** probes and spawns real recorders in order `rec` (sox) →
   `arecord` → `ffmpeg` (`:177-200`, `:441`), stops with SIGINT so the WAV finalizes
   (`:16-21`, `:572`), emits a legible no-recorder message (`:269`), and has a playback/TTS side
   (`:403-530`) driven by a palette toggle. TUI shows a hot-mic indicator
   (`render.rs:3241`, `reduce.rs:328`). Caveat: `voice.rs:66` — "*Treat first-run capture on a
   real machine as unverified.*"
5. **Live language servers** — ROADMAP:40-44, :57-63 say spawning rust-analyzer/pyright remains
   and "edges are proven with synthesized data today". `LspClient::spawn`
   (`knowledge/src/lsp/client.rs:77-98`) really does `tokio::process::Command` with piped
   stdio, sends `initialize` with `processId`, and pumps notifications on a background task
   (`:226`). Wired into the daemon (`executor.rs:259`/`:307` construct
   `Some(Arc::new(LspManager::new()))`; `workflow_exec.rs:496/728/746` thread it) and consumed
   via `AgentBuilder::with_lsp` (`runtime/src/agent.rs:1761`, `:1817`), with integration tests
   in `knowledge/tests/lsp_it.rs`. **Narrower caveat that may still hold:** the seam answers
   only `file_diagnostics(file, worktree)` — diagnostics, not graph *edges* — so
   "code-graph edges are synthesized" was not disproven; `adapter.rs:434/471`
   (`with_live_lsp`) is where edge-level use would land and was not traced. Two honest limits
   remain in code: diagnostics are best-effort (the method never errors, returning empty on
   missing binary / spawn failure / timeout), and a `broken` server is never retried for the
   manager's lifetime.

No trait was found whose only implementation is a mock. The closest things to "gated off in the
shipped binary" are `RoutingConfig::enabled = false` and the config-absent `None` from
`HostedTranscriber::from_paths`.

---

## F. Verified sound — do not spend effort here

Recorded so future scans don't re-litigate these:

- **Secret redaction** (invariant 6). `AuthStore` deliberately has no derived `Debug`;
  `AuthEntry` has none at all; the hand-written `impl Debug` emits `<redacted>` while keeping
  model ids visible, with a test (`auth.rs:194-208`). `ResolvedCredential` mirrors it
  (`providers/src/credential.rs:26-40`). A workspace-wide grep for `tracing` macros
  interpolating `api_key|secret|token|password|Authorization` returned exactly one hit, and it
  logs an *absence* (`codypendentd/src/lib.rs:209`). No key is ever a CLI argument —
  `models add` takes `--key-env`, a variable **name**.
- **Credential precedence** single-sourced in `ModelRegistry::api_key_for`, deliberately shared
  with `credentials_resolvable` and `check_model` so discovery and the live request cannot
  disagree. Empty/whitespace keys filtered at each step. (Caveat D12.)
- **Migrations**: all 38 files additive — no `DROP`, no table rebuild, no data-rewriting
  `UPDATE`/`DELETE`. New columns nullable or defaulted.
- **`CommandScope::allows_program`** is exact-string, never basename (`scope.rs:167-186`), so
  `./cargo` cannot impersonate `cargo`.
- **`git_subcommand`** (`policy/mod.rs:1112-1187`) parses git's global-option prefix and **fails
  closed** on any unknown option, so `-c`/`-C`/`--git-dir` values cannot be mistaken for the
  subcommand.
- **`apply_untrusted_overlay`** is narrow-only on every axis (`intersect_roots`, `union` for
  deny, `intersect_exact`, `min` timeout, `more_restrictive`, `Deny`-only default).
  `fs_write` stays on `intersect_roots` even on the **trusted** path.
- **Sandbox is fail-closed**: `enforcing_executor` returns `UnsupportedPlatform` off
  macOS/Linux, `RefusingSandbox` refuses everything, and `validate_enforceable_profile` rejects
  any non-empty `network_allowlist` on every platform — which makes the macOS
  "coarsen to allow-all-outbound" branch unreachable, so there is no macOS/Linux network
  asymmetry in practice. SBPL strings escaped; non-absolute/root grants rejected.
- **Approval reuse** honors non-reusable dispositions: `approvals.rs:302-308` skips both digest
  and pattern reuse when `allow_run_reuse` is false, so `require_once` results (external path
  scan hit, force-push, checkpoint restore, MCP always-approval) cannot be auto-approved.
- **No `std::sync::MutexGuard` held across an `.await`** anywhere in non-test daemon/orchestration
  code (mechanically scanned every `.lock()` within 8 lines of an `.await`). No inconsistent
  two-lock ordering that closes a cycle.
- **`WorkflowRunCancellations`** `begin`/`finish` bracketing correctly closes the lost-wakeup for
  a tool node parked at `WaitingApproval`. Its only weakness is C2's unwind gap.
- **`prune_run_lock`'s `Arc::strong_count(&lock) <= 2`** heuristic is correct: a waiter takes its
  third reference under the same registry mutex before suspending.
- **`spawn_run`'s outer/inner task split** converts an agent-loop panic into a terminal `Failed`
  via the `JoinError` arm, with a retry loop against `SQLITE_BUSY` — a panic cannot wedge a run
  non-terminal.
- **`shell.rs` timeout path**: process-group kill on expiry, unconditional group kill after,
  bounded drain joins, and `drain` caps at `MAX_CAPTURE_BYTES` while sinking the remainder so
  the child never blocks on a full pipe.
- **`read_file`** refuses non-regular files (FIFO/device) before reading and byte-bounds the
  reader. **`edit_file`** is atomic (all edits must match before any write), rejects non-UTF-8
  and empty `search`.
- **`edit_match`'s other nine stages** derive candidates from `line_spans` or `char_indices`, so
  they are all boundary-safe. The new stage 7 is the sole exception.
- **TUI byte-offset slicing** in `render.rs` (`:2210`, `:10678`, `:13336`, `:17278`, `:17303`,
  `:17351`) all derive offsets from `str::find`, which returns a char boundary — none can panic
  on multibyte text.
- **TUI event routing**: `apply_event` and the `RunStarted`/`RunStateChanged` handlers route by
  explicit `run_id` with idempotency guards. No handler falls back to `selected_run()`.
- **Ownership gate** is compile-enforced: `named_resources()` is an exhaustive, wildcard-free
  match over a `#[non_exhaustive]` enum. `ListSessions` is scoped with
  `COALESCE(owner_uid, daemon_uid) = principal_uid`; `AttachSession` re-authorizes per
  subscription; `WorkflowOwner` handles the unbound-run case.
- **No missing index** on any hot query path traced (`events` via the `(session_id, sequence)`
  PK; `runs`, `approvals`, `questions`, `pending_prompts`, `run_checkpoints` all indexed).

---

## Coverage and limits of this scan

Covered: policy/scope/sandbox/hooks, ownership gating, runtime tools + `unified_exec`, the TUI
palette surface exhaustively, config/persistence/migrations/secrets, concurrency across
`executor.rs`/`workflow_exec.rs`/`workflows.rs`/`remote_ui_workers.rs`/recovery, and
doc-vs-code feature verification.

Not covered:
- Exhaustive small-terminal (width/height 0–1) arithmetic across the 17,946-line `render.rs`,
  and `markdown.rs` streaming finalization.
- Whether a malformed `UiWireMessage` is client-reachable (C8's `.expect` trigger) — the leak is
  unconditional on any panic regardless.
- `UnifiedExecManager::terminate_under`'s own implementation, so "cancel kills child processes"
  is confirmed only as far as the call site.
- Code-graph **edge** construction (`knowledge/src/adapter.rs::with_live_lsp`) — only
  diagnostics consumption was traced.
- `publish.rs::recover_pending` idempotency, `prompt_queue.rs`, `hook_exec.rs`'s
  `SandboxProfile` construction, `sandbox/src/trust_store.rs` and `council/src/service.rs`
  beyond the non-atomic-write spot check.
- No exhaustive enumeration of `let _ =` / `.ok()` swallowing across 523k LOC.
