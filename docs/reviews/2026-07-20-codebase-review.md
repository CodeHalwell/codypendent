# Codebase Review — 2026-07-20

Reviewed at commit `dc02967` (merge of PR #12), branch `main`. All file:line
references are as of that commit.

> **Fix-pass addendum (same day, this branch).** A defect-fix pass landed after
> this review; see the commits following it on this branch. **Fixed:** S1–S6,
> C1–C10 and C12–C15 (C2 = wire-safe tool replay + unit pin, though a live
> provider test still does not exist; C3 = schema + `external_id` idempotency,
> while the personal-token 403 is documented, not worked around; C12 = absolute
> timeout ceiling, search/git bounds, bounded drain — the `kill`-binary
> dependency remains), plus the Phase-4 suggestion-lease scope, manifest
> `outputs` validation (and the canonical manifest's invalid kinds), the stale
> protocol doc comments, and the ROADMAP under-claims. **Still open:** C11
> (lexicographic revision comparison — needs a schema-level decision), S7 (OS
> sandbox enforcement + trust-tier rendering — tracked roadmap work), the §4
> performance cluster, the §5 dead-code inventory, cross-language golden
> vectors / the generated protocol SDK, and `cargo deny`/`audit` in CI.

**Method.** Every phase marked ✅/🟡 in [`ROADMAP.md`](../../ROADMAP.md) was
audited claim-by-claim against the code by six parallel reviewers (daemon +
protocol, knowledge, runtime + integrations, workflow, TUI + CLI, VS Code
extension), and the release-hygiene claims were re-run from scratch. This
document consolidates what was verified, where the claims and the code
disagree, the defects found, and what remains to build.

---

## 1. Verdict

The roadmap is **substantially honest**. Every phase claimed complete is
genuinely implemented, with tests that assert the claimed properties rather
than trivia — including a real kill-9 recovery test against the actual daemon
binary, both-direction CRDT merge convergence, policy deny-wins property
tests, and wiremock-enforced GitHub idempotency. Hygiene is real: `fmt` clean,
`clippy --workspace --all-targets --all-features -- -D warnings` clean,
**472 tests passing / 0 failing** (1 ignored doc-test), zero
`TODO`/`FIXME`/`unimplemented!` in non-test code, and non-test
`unwrap`/`expect` limited to a handful of documented invariant sites.

Two systemic patterns temper that verdict:

1. **Engine ahead of wiring.** Several subsystems are built and tested as
   libraries but have no production caller yet: the entire Phase 5 durable
   store + blackboard (admitted in the roadmap), the integrations crate's
   debounce + provenance engines (silently duplicated, divergently, elsewhere),
   the index outbox (written in every transaction, consumed by nothing), the
   retrieval "history" source, and `/update-docs` (a registry card with no
   executor).
2. **The least-tested path is the one real users hit first.** The live model
   driver (`FrameworkModelDriver`) has zero tests and its transcript replay is
   likely rejected by strict OpenAI-wire servers, and `/fix-ci`'s final step
   (check-run summary) sends a schema real GitHub ignores and needs an App
   token personal mode doesn't have. Everything proves out against fakes;
   the two real-world edges don't.

No critical/data-loss defects were found in the daemon core. The defect list
below is dominated by medium-severity issues clustered around the approval
surface, protocol drift in the hand-written TypeScript client, and a few
correctness traps that only bite once currently-unwired seams get wired.

---

## 2. Hygiene claims — re-verified

| Claim (ROADMAP "Every-release hygiene") | Result |
|---|---|
| `cargo fmt --all -- --check` clean | ✅ pass |
| `cargo clippy --workspace --all-targets` clean | ✅ pass (with `--all-features -D warnings`) |
| `cargo test --workspace` green | ✅ 472 passed, 0 failed, 1 ignored (doc-test), `--all-features` |
| `cargo deny check` / `cargo audit` | ❌ honestly unchecked — no `deny.toml`, no audit in CI |
| CI green, tree clean, migrations unchanged | ✅ CI well-structured (split lint/test, bounded run step); tree clean |

VS Code extension: **28/28 vitest tests pass**, `tsc --noEmit` and eslint
clean (run during this review; the roadmap's "27" is stale by one test in the
under-claiming direction).

Stale counts in ROADMAP.md (all under-claims): "64 TUI tests" → 68;
"27 vitest tests" → 28; follow-up "Surface `CommandRejected` in the TUI" is
listed unchecked but **is implemented** (`crates/cli/src/tui.rs:329-340` →
`crates/tui/src/reduce.rs:50` → status-line render).

---

## 3. Phase-by-phase assessment

### Phase 0 — Workspace bootstrap ✅ (verified, no discrepancies)

Instance identity/boot-count persistence, ledger with atomic in-transaction
sequence allocation, deterministic fixture replay (fold == DB projection,
duplicate sequence rejected), socket lifecycle incl. version-reject and
pidfile cleanup — all present and tested (`crates/daemon/src/instance.rs`,
`ledger.rs:128-157`, `tests/replay.rs`, `tests/socket.rs`).

### Phase 1 — Persistent coding-agent slice ✅ (verified, with caveats)

**Holds:** idempotency-first command handling with typed unique-violation
race recovery and verbatim result replay (`commands.rs:130-136,786-794`);
single `BEGIN IMMEDIATE` transaction carrying command row + event append +
projection + state flip (`commands.rs:807-955`); durable approval broker with
race-free waiters, restart re-registration, expiry-as-rejection driven by the
server tick; policy canonicalize-then-classify with deny-wins, allow-root
intersection property tests, exact-string shell allowlist; worktree
unmerged-work rescue that **provably captures untracked file content in the
exported patch and refuses deletion when export looks wrong**
(`worktrees.rs:351-364,498-512`); recovery ordering (tmp sweep → worktree
reconcile → effect reconcile → fail live runs → expire orphaned approvals)
with a true kill-9 integration test of the real binary; agent loop
persist-then-publish, safe-point steering, cancel-while-parked; JSONL/TUI
event parity with asserted exit codes.

**Caveats:**

- The "6-step write path" is partly scaffolding: **no production code writes
  `pending_effects`** — the only inserts are in test modules
  (`commands.rs:1587`). The reconciliation machinery is real but only ever
  consumes injected rows; real tool effects live in the runtime crate outside
  this ledger.
- **Run lifecycle transitions are unvalidated** — see defect C1.
- Worktree lease `expires_at` is stored, never enforced (no reaper);
  `LeaseMode::Read` is declared and unused.
- The Phase-1 follow-up stands: the executor still runs in the repo root; the
  per-run worktree module exists but is not bound to the loop (lands with
  Phase 5 parallel worktrees).
- `ApprovalScope::Pattern`/`Repository` are accepted and stored but **inert**
  — auto-approval matches only `scope='run'` (`approvals.rs:727-737`), so a
  broader grant silently behaves as `Once`.

### Phase 2 — Skills & knowledge ✅ (engine verified; three claims overstate)

**Holds:** strict-key `skill.toml` loading with content-hash change detection
and entrypoint-escape checks; the retrieval funnel (candidate union → hard
filters → rerank → dependency closure resolved *within survivors only* →
budgeted disclosure) with a dedicated test proving the **risk ceiling**, not
ranking luck, excludes destructive decoys; memory scope filtering as an
indexed SQL `WHERE` with a symmetric two-repo leak test; supersession as
insert-never-delete in one transaction; curator gate ordering
(secret-before-provenance) tested; tree-sitter Rust code graph with byte-span
evidence and file-scoped symbol keys; provenance cards.

**Overstatements:**

- **"Dense" retrieval is not semantic.** `HashingEmbedder` is a hashed
  character-trigram TF vector (`retrieval/embed.rs:37-38` admits it); no
  embedding model exists in the dependency tree; the vector index is a
  brute-force in-memory cosine scan. Dense + BM25 + exact are three lexical
  variants. The retrieval eval (recall@8 = 1.0) is a real harness with real
  decoy traps, but over a 28-item pool whose query vocabulary mirrors the
  authored intents — treat the number as a floor-gate (≥0.8 asserted), not a
  capability measurement.
- **`codypendent index rebuild` restoring from deletion is vacuous** — both
  indexes are built in RAM per process (`bm25.rs:69`); deleting
  `<data_dir>/index/` is a no-op the CLI itself admits
  (`cli/src/commands.rs:351-357`). Corollary: the index outbox is written in
  every transaction and consumed by nothing in production.
- The "history" retrieval source is plumbed but never fed
  (`context.rs:208-212` always passes an empty vec).

### Phase 3 — GitHub & IDE awareness ✅ (verified; two real-world breaks)

**Holds:** `GitHubToken` genuinely leak-proof (manual redacted `Debug`, no
`Serialize`, tests assert every wire model excludes it); HMAC verified over
raw bytes before parse with constant-time compare and an
ordering-pinning test; GUID replay dedup; hidden-marker list-before-create
idempotency for draft-PR and review-comment, wiremock-enforced
POST-exactly-once; `eval_github_mutation` always approval-gated; five
`github.*` tools with reads-vs-mutations split; end-to-end approve/reject/
deny tests through the agent loop; unsaved-buffer provenance labeling E2E;
ACP stdio adapter with round-trip + cancellation tests; VS Code extension
codec/discovery faithfully mirroring framing (16 MiB cap enforced pre-
allocation both sides) with CSP'd, injection-free webview.

**Breaks and drift:**

- **`/fix-ci`'s final step doesn't work against real GitHub** (defect C3):
  wrong check-run schema (`summary` top-level instead of `output:{title,
  summary}` — silently ignored) and check-run creation requires an App token,
  which personal mode (`gh auth token`/PAT) can't provide → 403. Invisible to
  the wiremock tests.
- `create_check_run_summary` is also the one create with **no idempotency
  marker** — a replay duplicates it.
- The TypeScript protocol duplication the roadmap worries about has already
  drifted: 3 missing fields + 1 missing event variant, one security-relevant
  (defect S1), plus `ServerHello.resume_token` untyped/unused, so a VS Code
  window reload falls back to full snapshot catch-up instead of identity
  resume.
- "Network-scoped to `api.github.com:443`" is decision-scoped, not
  enforcement-scoped: the policy checks a string constant; nothing binds the
  client's `base_url` to it.
- The integrations crate's deterministic `Debouncer` and exact-match
  provenance resolver are **shelf-ware** — no production caller; debouncing
  happens client-side in the extension and the runtime read path
  re-implements provenance with a weaker suffix match that is defect C4.
- `vscode.diff` opens a placeholder (artifact metadata), not the patch.
- `provider-anthropic` is a vestigial feature: gates a dependency, referenced
  by zero code.

### Phase 4 — Docs Studio & code intelligence 🟡 (engine verified; wiring honest)

**Holds:** Loro CRDT layer with block↔CRDT bijection over all 12 block kinds,
byte-identical export→import→export, both-direction concurrent-merge
convergence with no-loss assertions, per-mutation attribution in-transaction;
collaboration modes with **suggest-by-default for Organization scope** and
accept applying exactly the annotated range behind three drift guards (all
tested, including zero-length-insertion drift); deterministic Markdown render
(no maps/time/randomness — deterministic by construction); pure
`PublishPlan`; revision↔commit publication record; LSP-edge supersession
replacing (not duplicating) syntax edges; revision-aware graph queries
(`callers_of`/`blast_radius`/`tests_covering`/`changed_between`) tested
cross-file; staleness engine with file-scoped negative tests; the daemon
transport as claimed — `MutateDocument` intercepted at connection level, mode
derived from document scope, lease `require()` pre-apply, `DocumentSync`
fan-out via per-document hub, lease acquire/release with Observer role-denial
and `document.range-leased` conflicts.

**Discrepancies:**

- **Python/TypeScript "adapters" are toy line-scanners** — `scan_python`
  misses `async def` entirely; `scan_typescript` misses `async function` and
  arrow-function bindings (`adapter.rs:482-538`). The roadmap's parallel
  listing "Rust/Python/TypeScript adapters" implies parity that doesn't
  exist. No LSP is ever spawned (roadmap does disclose this).
- **Suggestion accept/reject takes a whole-document lease**, contradicting
  its own comment ("takes no lease here", `codypendentd/src/documents.rs:
  185-196` passing `None` → whole-doc semantics in `leases.rs:211-215`). Net:
  an approver cannot accept a suggestion while anyone holds any block lease.
- `/update-docs` is a **registry card with no execution wiring** — nothing in
  daemon/CLI/TUI/runtime dispatches it; the staleness engine is consumed only
  by its own tests.
- No end-to-end socket test covers the wired `MutateDocument → DocumentSync`
  fan-out (parts are tested separately; the composed path only has the
  unwired-rejection test).
- Stale protocol doc comments still deny the wired features
  (`command.rs:97-100`, `handshake.rs:96-101` say "not yet handled/wired").
- TUI docs "tree" is a flat list; the roadmap's "`D`/`G`" key claim is stale —
  in the conversation shell those keys are composer text; the palette is the
  real entry point.

### Phase 5 — Workflows & multi-agent 🟡 (libraries verified; zero production callers)

**Holds:** every claimed compiler validation exists and is tested (schema
version, unique/non-empty ids, one-action-per-step, skill⇒agent, resolvable +
acyclic deps via Kahn with deterministic tie-break, budget sanity, ADR-008
`orchestration_reason`); structural-before-registry validation proven by
test; the canonical manifest compiles as a regression test; the durable store
does what it says — signature-guarded `resume`/`retry_from_node` (correct
BFS downstream closure, full node reset in one tx), `list_incomplete_runs`,
`ready_nodes` frontier that correctly excludes dependents of `Failed` nodes;
the blackboard refuses claim-kinds without evidence on both `post` and
`supersede`, supersedes in a genuinely race-guarded single transaction
(`BEGIN IMMEDIATE` + conditional-update rowcount + rollback), and isolates
per run.

**Caveats:**

- **Only `compile_yaml` has a production caller** (the CLI). `WorkflowStore`,
  `BlackboardStore`, `compile_with_registry`, `parse_agent_profile`,
  `ready_nodes`, `list_incomplete_runs` are exercised solely by the crate's
  own tests. The roadmap says so; stating it starkly: Phase 5 is currently a
  well-tested library, not a feature.
- `resume` vs `ready_nodes` semantic gap — defect C5.
- The graph signature **excludes budget, inputs, `orchestration_reason`, and
  node `outputs`** — a resume accepts a run whose caps changed, despite the
  doc claiming "any change that alters what executes changes it". Also
  unlength-prefixed separator hashing admits contrived collisions (control
  characters in ids — no charset rule prevents them).
- The canonical manifest's `outputs` (`patch`, `test-result`, `review`)
  don't match the typed blackboard kinds (`proposed_patch`, `test_result`, no
  `review`) — `outputs` are documented as "blackboard artifact kinds" but are
  unvalidated free strings.
- `AgentProfile` has no `role` field (identity is `id`) though ROADMAP 5.1
  says "reads role/…"; manifest short roles ("implementer") match nothing yet
  — role→profile resolution is the admitted missing piece.

### TUI/CLI — Codex-informed backlog (claims verified; three real bugs)

All seven `[x]` backlog items verified in code with tests (conversation
shell, palette, F2 toggle, rich approval cards with full env + cwd rendered,
narrative transcript, priority-dropping contextual footer, auto-scroll
follow). Reducer is genuinely pure (no I/O, no clock — tick-count expiry; the
crate has no tokio/sqlx). Mouse-parity and semantic-theme-token claims from
the README are enforced by tests/grep. Defects C6–C8 below are the cost of
the shell rework; the async CLI harness core (event loop, gap repair,
reconnect) has **zero test coverage** and contains C6.

---

## 4. Defects

Severity reflects impact under the project's own local-first trust model
(user-private socket, self-asserted roles are documented Phase-1 scope).

### Security-flavored

- **S1 (medium) — VS Code approval card omits `environment` and `cwd`.**
  `ProposedAction::ExecuteCommand` in `extensions/vscode/src/protocol/types.ts:153`
  lacks both fields (Rust: `run.rs:94-109`); the card renders `program args`
  only (`extension.ts:430-431`). The Rust doc comment states these fields
  exist precisely so approvers can't be tricked by `LD_PRELOAD`-style
  smuggling — a VS Code approver approves blind to the exact attack the field
  prevents. (The TUI card renders both, with a comment explaining why.)
- **S2 (medium) — shell env deny-list gaps.** `shell.rs:237-252` blocks
  `PATH`/`LD_*`/`DYLD_*`/`*_WRAPPER`/`BASH_ENV`/`ENV`/`SHELLOPTS` but not
  `NODE_OPTIONS`, `PYTHONPATH`/`PYTHONSTARTUP`, `PERL5OPT`, `RUBYOPT`,
  `GIT_CONFIG_COUNT`/`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`, `CDPATH`.
  Exploitable once an interpreter is allow-listed and an approval is reused.
- **S3 (medium) — GitHub pagination origin check is prefix-based.**
  `client.rs:174` (`next_url.starts_with(base_url)`) admits
  `https://api.github.com.evil.com/…` from a hostile `Link` header and
  attaches the bearer token to it. Needs boundary-aware compare or URL parse.
- **S4 (medium) — policy deny entries silently dropped when `$HOME` is
  unset.** `policy/mod.rs:488-510` drops unexpandable entries — including
  `$HOME/.ssh` *deny* rules — with only a warning. Fail-open on deny; should
  be fail-closed (refuse to start, or keep the entry unexpanded-and-denying).
- **S5 (low) — webhook secret has derived `Debug`** (`config.rs:14-29`)
  despite the "MUST NEVER be logged" doc; `GitHubToken` got the manual
  redaction, this type didn't. Also: `Authorization` header never
  `set_sensitive` (`client.rs:79`); unbounded `response.text()` on non-2xx
  (`client.rs:137`) flowing verbatim into the model transcript.
- **S6 (low) — resume tokens** use `sha256(secret‖payload‖secret)` with
  non-constant-time compare (`server.rs:1254-1288`) instead of HMAC.
- **S7 (standing, roadmap-acknowledged)** — policy decides but does not
  enforce (no OS sandbox, canonicalize-then-use TOCTOU windows), and
  untrusted GitHub text (PR bodies, check names, error bodies) enters the
  model transcript unlabeled on the `/fix-ci` path. Both are tracked
  cross-cutting items; noted here because they are live today.

### Correctness

- **C1 (medium) — run-state transitions unvalidated.** `CancelRun`/`PauseRun`/
  `ResumeRun` check existence only (`commands.rs:364-375,547-580`);
  `ResumeRun` on a `Completed` run flips the projection back to `Running`
  with no executor attached — a zombie that pollutes `active_runs` until
  next-boot recovery force-fails it and appends contradictory terminal events.
- **C2 (medium) — live model driver untested and likely wire-broken.**
  `FrameworkModelDriver` replays tool results as bare `Role::tool` messages
  with no `tool_call_id` and never re-inserts the assistant tool-call turn
  (`agent.rs:1724-1742`); strict OpenAI-wire servers reject that (400). Zero
  tests (self-acknowledged). This is the only path a real model runs through.
- **C3 (medium) — check-run summary wrong schema + wrong token type** (see
  Phase 3 above): silently dropped by real GitHub, 403 under personal-mode
  tokens; also non-idempotent.
- **C4 (medium) — provenance suffix matching mislabels reads.**
  `agent.rs:1049-1053` uses bidirectional `ends_with` — a dirty buffer for
  `b.rs` matches a read of `lib.rs`, falsely labeling it
  `unsaved-ide-buffer`. The correct exact-match engine sits unused in
  `integrations/src/ide/provenance.rs:37-72`.
- **C5 (medium, latent until wired) — `resume` points at permanently stuck
  nodes.** After a dependency `Failed`/`Skipped`, `resume` still reports the
  downstream `Pending` node as next (`store.rs:493-497`) while `ready_nodes`
  never will; nothing distinguishes "stuck forever" from "resumable" — a
  recovery loop composing `list_incomplete_runs` + `resume` can livelock.
- **C6 (medium) — TUI gap-repair discards the events it re-fetches.** On a
  detected sequence gap the client re-attaches with `last_seen_sequence`, but
  advances `last_seen` to the gap-revealing event *first*
  (`cli/src/tui.rs:210-250`), so every replayed event fails the
  `sequence > last_seen` fold guard and is dropped. The daemon fan-out is
  genuinely lossy under lag (`server.rs:1084,1110` `Lagged => continue`), so
  a lagged TUI permanently loses state — including, worst case, an
  `ApprovalRequested`. The async harness containing this path has no tests.
- **C7 (medium) — approval hotkeys live under overlays.** With a browser/help
  overlay open (input mode `Normal`), `a`/`A`/`r` resolve a pending approval
  whose card is *not on screen* (`input.rs:172-174`, `reduce.rs:593-602`;
  modal only renders when `overlay == None`). Undermines "a pending approval
  owns the input".
- **C8 (medium, multi-client) — `RunStarted` steals selection mid-draft.**
  `ensure_run` re-selects on every `RunStarted` (`state.rs:632-646`); another
  client starting a run retargets the local user's drafted message, which
  submits against `selected_run` at Enter time.
- **C9 (medium) — VS Code drops approval decisions while disconnected.**
  `sendEnvelope` silently no-ops with no socket (`client.ts:476-479`) — no
  queue, no error; a decision clicked during a backoff window is lost while
  the card stays pending.
- **C10 (low) — memory context injection unbounded.** `assemble_context`
  injects *every* live in-scope memory with no top-k/budget
  (`context.rs:218`) — the exact failure mode the 2.3 funnel exists to
  prevent, on the run-context path.
- **C11 (low, latent) — lexicographic revision comparison.**
  `MemoryStore::query(at_revision)` does TEXT range comparison
  (`memory.rs:157`); meaningless for git-SHA revisions. The observer works
  around it with zero-padded sequence revisions; nothing prevents SHA input.
- **C12 (low) — cancel/wall-clock blind during tool execution**; model-
  supplied `timeout_secs` unclamped when config is 0; `workspace.search`,
  `git.diff`, `git.apply_patch` have no timeout at all; timeout kill shells
  out to a `kill` binary that may not exist.
- **C13 (low) — transcript entries bypass the cap** in four reducer arms
  (`reduce.rs:216-227,247-258,298-300,311-313` push directly, skipping
  `push_entry`'s trim); tool-completion correlation is heuristic (newest
  non-completed card) and can mislabel interleaved tools.
- **C14 (low) — worktree re-allocation for the same run fails**
  (`worktree_path` UNIQUE across all states + surviving branch), and a lost
  insert race leaks the just-created worktree until next-boot adoption.
- **C15 (low) — attach catch-up loads the full session event history** and
  filters in Rust (`server.rs:946-950`) — O(session) per reconnect; needs
  `WHERE sequence > ?`. Related: attaching to a nonexistent session silently
  succeeds with an empty catch-up.

### Performance (not urgent at current scale, flagged for trajectory)

Per-run full registry list + in-RAM tantivy/vector index rebuild
(`context.rs:206-207`); BFS graph queries issuing one SQL query per node per
layer; O(n²) TS frame decoder on fragmented input; TUI redraw per stream
delta with no batching; `DocumentSync` CRDT bytes as JSON number arrays (~4×
inflation against the 16 MiB frame cap).

---

## 5. Dead / unwired inventory

Worth either wiring or deleting — each is currently misleading weight:

- `pending_effects` effect-ledger (production writers: none)
- Index outbox consumers; `<data_dir>/index/` directory
- Retrieval "history" source (never fed)
- `integrations` `Debouncer` + provenance resolver (duplicated elsewhere)
- `/update-docs` execution; `detect_staleness`/`resolve_links` callers
- `provider-anthropic` feature (dep only, zero code)
- `ApprovalScope::Pattern`/`Repository` (stored, inert)
- `shell_interpreter_requires_approval` policy knob (parsed, never read)
- Worktree lease TTL + `LeaseMode::Read`; `Envelope.sequence`;
  resume-token `last_sequence` (always 0); `ClientCapabilities` flags
- `DocumentCrdt::set_block` / `MutationKind::SetBlock` (zero callers)
- `SetRegistry::add_*` helpers; `Subscription::RepositoryStatus`/`BudgetState`
- TUI: `Action::FocusPane` (never produced), pane focus/`CyclePane`/
  per-entry `Expand` unreachable in the shipped shell; unused `syntax.*`,
  `diff.*`, `agent.thinking` theme tokens; likely-unused `serde`/`serde_json`
  deps
- VS Code: webview `startRun` path (no UI posts it), `submitUserInput`,
  `payloadType`, `resolveSocketPath`

---

## 6. What remains to build

### Close Phase 4 (the roadmap's own "finish this vertical first" is right)

1. Client-side CRDT replica consuming the `DocumentSync` stream (TUI first).
2. `PublishPlan` execution through the approval-gated change-set / Phase 3
   GitHub write path.
3. Live language server spawn (rust-analyzer/pyright) feeding the
   already-proven supersession path.
4. An end-to-end socket test of mutate → sync fan-out (the flagship transport
   currently rests on composition of separately-tested parts).
5. Fix the suggestion-resolution lease semantics (block-scoped or
   suggestion-scoped, per its own comment).
6. Either real Py/TS adapters (tree-sitter grammars are already a workspace
   pattern) or demote the claim.

### Wire Phase 5 (library → feature)

1. Daemon commands + ledger events for workflow run/node lifecycle;
   startup recovery loop over `list_incomplete_runs` (the store half exists).
2. Role→profile resolution (define it; the manifest's short roles currently
   resolve to nothing), then lowering onto framework graphs, then replace the
   prompt-encoded `/fix-ci` with the declarative definition.
3. Blackboard daemon read/write commands + subscription delivery; TUI
   workflow-graph + blackboard views over the existing projections.
4. STEP 5.4 delegation (supervisor/specialists with per-agent worktrees —
   also closes the Phase-1 "bind a dedicated per-run worktree" follow-up;
   reviewer profile structurally excludes write tools).
5. STEP 5.5 hierarchical budget ledger — replaces the hard-coded
   `MAX_STEPS`/30-min constants and the all-zeros token/cost accounting.
6. STEP 5.6 session forking (`ForkSession{checkpoint}`).
7. Resolve C5 (resume-vs-ready semantics) and validate manifest `outputs`
   against blackboard kinds before the daemon starts trusting them.

### Phase 6 — plugins & multimodal (not started; correctly sequenced after sandbox)

The roadmap's cross-cutting note is the right call and worth restating as a
hard gate: **OS sandbox enforcement (bubblewrap/seccomp, Seatbelt,
AppContainer) lands before the plugin host**, treating the policy engine as a
compiler emitting sandbox profiles. Then: plugin manifests/lifecycle, MCP
host, WASM SDK, executable hooks/skill scripts, voice/image input, theme
packs, setup assistant.

### Phase 7 — routing & learning (not started)

Benchmark task set first (the yardstick), then model profiles, router,
cascading escalation, trace graders, promotion pipeline with
nothing-promotes-itself.

### Cross-cutting (all already identified in the roadmap; all confirmed real needs)

- **Generated protocol SDK** — the drift risk is no longer hypothetical
  (3 missing fields, 1 missing variant, one security-relevant). Until
  generation exists, add cross-language golden vectors: the Rust fixture
  corpus (`events.rs:317-337`) consumed by vitest.
- **Trust-boundary rendering** — retrieved memories, skill text, and
  GitHub/CI text as evidence, not instructions (live injection surface today).
- **Real embeddings** behind the existing vector trait (the "dense" leg is
  currently lexical).
- **Supply chain**: `cargo deny` + `cargo audit` in CI (the one unchecked
  hygiene box) — plus `npm audit` for the extension.

---

## 7. Suggested order of attack

1. **Bug-fix pass on the approval/safety surface** (S1–S4, C6, C7, C9; C1).
   Small diffs, high leverage — this is the product's core promise.
2. **Prove the live-model path** (C2, C3): one integration test against a
   real OpenAI-compatible endpoint (recorded or gated behind an env var);
   fix tool-call replay; make the check-run step degrade to a PR comment in
   personal mode.
3. **Finish the Phase 4 vertical** (§6.1) — one demo-able end-to-end slice:
   open → concurrent edit → suggestion → accept → publish through approval →
   reconnect.
4. **Wire Phase 5 into the daemon** (§6.2, items 1–3) — recovery and
   graph-view visibility make everything after cheaper to debug.
5. **Hygiene batch**: sync ROADMAP/doc-comment claims (both directions),
   prune or ticket the dead inventory (§5), add deny/audit + golden vectors.

A closing observation: for a six-day-old repository this is an unusually
disciplined codebase — the transaction and bounded-I/O discipline, the
honesty of inline doc comments, and the depth of the test suite are all well
above the norm. The risks worth managing are the two systemic ones named in
§1: engines racing ahead of wiring, and the untested real-world edges.
