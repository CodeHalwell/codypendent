# Project Review — 2026-07-21

Reviewed on branch `main`, which **moved twice during the review**: started at
`37196b7` (PR #15), advanced to `c859077` (PR #16) and then `fb55713` (PR #17).
Each section states the commit its refs are pinned to; hygiene and Phases 5–7
were re-verified at `fb55713`, the fix-pass section at `37196b7` (PR #17
shifted line numbers in `runtime/src/agent.rs` and `daemon/src/server.rs` but
did not alter those fixes).

**Method.** This review builds on
[`docs/reviews/2026-07-20-codebase-review.md`](2026-07-20-codebase-review.md)
(reviewed at `dc02967`) rather than repeating it. Since that review, five PRs
landed ~16,000 lines: the defect-fix pass (PR #13), Phase 5
driver/StartWorkflow/TUI views (PR #14), the Phase 6 & 7 engines (PR #15),
security hardening on them (PR #16), and the Phase 5 daemon wiring +
trust-boundary framing + cargo-deny gate (PR #17). Four parallel reviewers
audited (1) each claimed defect fix, (2) the Phase 5 code and its new daemon
wiring, (3) the Phase 6 sandbox/multimodal/theme code adversarially as a
security boundary, and (4) the Phase 7 routing/eval crates; hygiene was re-run
from scratch.

## 1. Verdict

**The project is in very good shape, and — more importantly — its
review-and-fix loop demonstrably works.** Every defect the 2026-07-20 review
reported as fixed was verified genuinely fixed (19/19, 15 test-pinned), and
two further hardening PRs (#16, #17) landed *during this review*, closing
several of its would-have-been findings before it could publish them
(whole-manifest plugin signing, sanitizer DoS bounds, promotion-candidate
opacity, the cargo-deny gate, trust-boundary evidence framing, and the entire
Phase 5 daemon wiring). Hygiene is fully green at fb55713: fmt, clippy
`--all-features -D warnings`, **706 tests / 0 failures**, CI green including
the first cargo-deny run, VS Code extension 30/30.

**Zero critical defects were found.** The new-defect list is 6 medium + ~15
low across ~16,000 reviewed lines, and every medium is in code that is either
freshly wired (Phase 5 conductor) or not yet wired at all (routing/eval,
sandbox enforcement) — the mature daemon core surfaced nothing new.

The two systemic patterns from the last review have both *narrowed but not
closed*:

1. **Engine ahead of wiring.** PR #17 wired the workflow driver, but the
   pattern persists one layer down: tool nodes cannot execute (the canonical
   `repair-github-check` workflow cannot complete in production), the
   blackboard write path has no caller (the TUI blackboard view reads a
   permanently empty surface), role→profile resolution is validation-only,
   and the entire sandbox/routing/eval layer remains a tested decision engine
   with no runtime consumer.
2. **The least-tested paths are the real-world edges.** The async CLI harness
   (reconnect/gap-repair — the code carrying C6/FP-2) still has zero tests;
   there is still no live-provider model test; OS sandbox *enforcement* still
   does not exist (decision layer only); and node cost accounting is still
   all-zeros while the roadmap checks "cost visible ✅".

## 2. Hygiene — re-verified

| Claim | Result |
|---|---|
| `cargo fmt --all -- --check` clean | ✅ pass |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean | ✅ pass |
| `cargo test --workspace --all-features` green | ✅ 706 passed / 0 failed (59 suites, at fb55713; was 472 at the 2026-07-20 review) |
| `cargo deny check` | ✅ closed by PR #17 — `deny.toml` (advisories/licenses/bans/sources) + `cargo-deny-action@v2` in CI, green on fb55713 |
| CI green on main | ✅ every commit through fb55713 (incl. the first cargo-deny run) |
| VS Code extension | ✅ 30/30 vitest, `tsc --noEmit` clean (still no CI job runs them) |

## 3. Fix-pass verification (PR #13 claims)

**Every one of the 19 claimed fixes verified as genuinely landed and correct**
(S1–S6, C1, C2, C4–C10, C12–C15, plus the suggestion-lease scope and manifest
`outputs` validation). 15 of 19 are test-pinned; not pinned: S1 (TS types
only), S4 (unresolvable-deny poisoning), C6 (the async CLI harness still has
**zero tests** — the fix is sound by inspection only), C10 (memory cap), C12
(new timeout bounds), C13 (cap-through-fallback-arms).

Notable fix quality: S4 now *poisons the scope* (empty roots ⇒ everything
denied) rather than dropping deny entries; C6 buffers mid-repair events and
re-repairs; C14 proves branch losslessness via `merge-base --is-ancestor`
before deletion; the C1 idempotency-replay-before-validation ordering is
correct.

**Five residues found in the fix diffs (all low/info):**

| # | Sev | Location | Finding |
|---|---|---|---|
| FP-1 | Low | `runtime/src/tools/shell.rs:253-277` | `RUBYLIB` missing from the env deny-list (`PERL5LIB`/`PYTHONPATH` were added; the Ruby analogue wasn't) |
| FP-2 | Low | `cli/src/tui.rs:228,~290` | Sequence-0 events arriving mid-repair are silently discarded; `gap_buffer` unbounded; `repairing` has no timeout |
| FP-3 | Low | `daemon/src/commands.rs:363-371` | Run-state transition guard is check-then-act (validated outside the write transaction); narrow race remains |
| FP-4 | Low | `extensions/vscode/src/client.ts` | Offline queue covers only socket-absent; a decision sent connected-but-pre-attach can be rejected rather than queued |
| FP-5 | Info | `extensions/vscode/src/client.ts` | Queue overflow (256) drops the oldest queued intent — possibly an approval decision — near-silently |

## 4. Phase 5 — workflows (PR #14 + PR #17, reviewed at fb55713)

**Verdict: PR #17 genuinely converts Phase 5 from "well-tested library" to a
wired feature** — create → drive → recover → pause/resume/retry through the
daemon, agent nodes executing on the real agent loop, with good test
discipline at every layer (73 workflow-crate tests green). No critical
defects.

**Claims verified** (each with a pinning test): completed nodes never re-run;
Running-node crash recovery re-drives exactly once at the persisted attempt;
failure blocks only dependents; diamond frontier; retry-to-success;
StartWorkflow idempotent for identical duplicates; CLI seams skip
non-compiling manifests; startup recovery over `list_incomplete_runs` is real
(`main.rs:140-142`). Prior-review items **fixed**: C5 resume-vs-ready gap
(`blocked_node_ids` + moot in production), manifest `outputs` validated
against blackboard kinds. **Still present**: graph signature excludes
outputs/budget/inputs/orchestration_reason with un-length-prefixed id hashing.

**Claims that overstate:** "observer sees every transition" is PARTIAL
(recovery resets and intermediate retry failures are unobserved; failure
*reasons* are never persisted anywhere — no error column, dropped at
`drive.rs:351-364`); "node-level cost visible ✅" — cost is never populated in
production (`workflow_exec.rs:213-216`); role→profile resolution exists but
execution ignores it (every agent node runs `Build` mode, hard-coded
`"hosted-default"` policy).

**New defects:**

| # | Sev | Location | Finding |
|---|---|---|---|
| P5-D1 | Medium | `codypendentd/src/workflow_exec.rs:197-199` | Agent nodes execute in the daemon's **cwd** as both repository and worktree — every node shares one writable tree ("never share writable worktrees" exit criterion unmet), and it's whatever directory the daemon started in |
| P5-D2 | Medium | `workflow/src/store.rs:278-313,708-714` | Idempotency key is the bare command key — same key + different manifest silently returns the *first* run's id as success; no signature comparison on the duplicate path |
| P5-D3 | Medium | `codypendentd/src/workflows.rs:277-289` | `RetryWorkflowNode`/`PauseWorkflow` mutate without the per-run drive lock — retrying a node on an actively driving run lets the in-flight executor overwrite the reset, silently skipping the retry |
| P5-D4 | Low | `workflow/src/drive.rs:188-202,342-364` | Observer gaps + failure reasons never durable (see above) |
| P5-D5 | Low | `workflow/src/drive.rs:204-206` | Library-level `drive` resurrects paused/terminal runs (`Running` set before the state check); only the daemon host guards it |
| P5-D6 | Low | various | `workflow.no-manifest` for nonexistent runs; stale `agent_run_id` via COALESCE; TUI never overlays live run state (non-pending `node_state_color` branches dead); `with_github` rebuilds drive locks (safe only pre-startup) |

**Still unwired after PR #17:** tool-node execution (every tool node fails
with a canned "not executable yet" — the canonical `repair-github-check`
manifest **cannot complete in production**); the blackboard write path
(`post`/`supersede` have no caller — the TUI blackboard view reads a
permanently empty surface); checkpoints; `WaitingApproval`/`Blocked`/`Skipped`
node states and `Cancelled` run state (no producers, no CancelWorkflow
command); workspace_mode/approval/budget/model_policy compiled and displayed
but ignored by execution.

## 5. Phase 6 — sandbox/multimodal/themes (PR #15, re-verified at c859077)

**Verdict: every ROADMAP claim holds with a pinning test (49 sandbox tests
green; roadmap's "42" is stale in the under-claiming direction).** The
adversarial pass found no exploitable hole in the verification, diff, profile,
lifecycle, multimodal-gate, or theme-pack layers.

**Crypto (PR #16 whole-manifest signing): sound.** Signature covers
`SHA256(domain-tag ‖ len_be64(canonical) ‖ canonical)` where canonical is the
full manifest minus only the signature field — injective (all named fields,
ordered Vecs, JSON escaping, length prefix; delimiter-injection pinned by
test), version-tagged, checksum still bound and checked first. Missing
`[security]` section fails closed (`MalformedChecksum` → and unsigned →
default-deny). `verify_strict` rejects malleable keys. No intra-crate TOCTOU;
the real verify→exec window lives in the unbuilt OS enforcer (STEP 6.2).

**Key security properties verified:** permission diff can never *under*-report
an expansion (exact-string, fails toward re-approval); new capability classes
cannot bypass it (`deny_unknown_fields` fails at parse); `SandboxProfile` is
deny-by-default everywhere (empty set ⇒ deny, fixed closed env allowlist);
`DataClassification::Unknown` ranks above Secret so forged/newer
classifications fail closed off-device; theme packs are double-guarded
(deny_unknown_fields + explicit permission rejection). PR #16 also fixed:
sanitizer CPU-DoS (input budget + 256-char escape cap, both pinned), and the
over-broad-grant hole at install (`GrantExceedsManifest`).

**New findings at c859077:**

| # | Sev | Location | Finding |
|---|---|---|---|
| P6-A | Medium | `sandbox/src/permission.rs:152`, `lifecycle.rs:204-206` | Resource caps (memory/cpu/wall/output) sit outside `CapabilitySet`/`PermissionDiff` — an update raising them is "identical" and auto-applies with no re-approval surface (they are signature-bound, but the human diff never shows them) |
| P6-B | Low | `sandbox/src/sanitize.rs:96` | Unicode bidi-override + zero-width chars pass through unstripped, undercutting the module's own spoof-UI goal (mitigated by the evidence-block label) |
| P6-C | Low | `sandbox/src/lifecycle.rs:204,248` | Install-time "grant ⊆ manifest" invariant not re-asserted on update paths; currently safe via `diff_update`, fragile under refactor |
| P6-E | Low | `sandbox/src/permission.rs:22-34` | Exact-string capability matching (no case/host/path normalization) — safe for the gate, a trap for the future enforcer |

**Unwired:** essentially the whole layer is a tested decision engine with no
production caller beyond `plugin inspect`/`diff` (which bypass verification by
design): `SandboxProfile`, the lifecycle state machine, `verify_artifact` (no
trusted-publisher key store exists), `sanitize_untrusted` (not invoked on any
real MCP/plugin output), `InputEnvelope`/`transcription_allowed` (no capture
path), and the theme-pack loader + `ColorDepth::detect` (the live TUI
hardcodes `Theme::dark()`, `render.rs:2117`).

## 6. Phase 7 — routing/eval (PR #15, re-verified at c859077)

**Verdict: claims substantially accurate; engines are real and well-tested
(84 tests green at HEAD: routing 41, eval 43).** Every ROADMAP 7.1–7.5 claim
verified with a pinning test (table in agent report). Test-count claims were
accurate at PR #15 ("37" exact for routing; "12" for 7.5 was 11 + 1 IT step).

**Security hard-filter: no leak path found.** All five routing entry points
funnel through `is_eligible` (classification checked first,
`router.rs:265-284`); escalation re-applies the filter per tier
(`router.rs:227-231`, pinned by `secret_data_stays_local_and_never_escalates_off_device`);
empty candidates → `NoEligibleModel`, never fallback. `DataClassification::rank`
treats forward-compat `Unknown` as most restrictive. Caveat (design): default
policy ships `max_off_device = Confidential` (`policy.rs:92`) — permissive
out of the box.

**Promotion pipeline: the PR #15 headline hole is fixed by #16.** `Candidate`/
`PromotionRecord` fields were `pub` (any code could set `stage = Promoted`);
now private, `PromotionRecord` is Serialize-only, and `activate()` requires a
`Promoted` receipt. `approve()` verified as the only path to `Promoted` across
every method. Trust boundary (authenticating `Actor::Human` is the daemon's
job, ADR-010) now explicitly documented.

**New defects at c859077:**

| # | Sev | Location | Finding |
|---|---|---|---|
| P7-1 | Medium | `routing/src/router.rs:221-231` | Escalation can cycle forever on a chain with duplicate model ids (`position()` returns first match); chain uniqueness never validated |
| P7-2 | Medium | `eval/src/promote.rs:322-326` | `finish_canary` legal with zero observations — canary can "pass" unobserved |
| P7-3 | Low | `eval/src/cluster.rs:29-37` | Cluster-key separator collisions (`Some("-")` vs `None`, `\|` injection) can silently merge distinct failure clusters |
| P7-4 | Low | `routing/src/router.rs:257` | `artifacts_preserved: true` hardcoded — asserted, not implemented |
| P7-5 | Low | `eval/src/promote.rs:353` | Every rollback attributed to `"system"` — actor identity lost from audit record |
| P7-6 | Low | `routing/src/policy.rs:47-66` | No validation of deserialized policy/profile numbers (NaN/negative λ accepted, silently tie-broken) |

Also fixed by #16: `[11,12,12]` rollback-to-self corruption; token-size bypass
of the context-fit filter; `is_negative` silent-default; `./`-prefix false
negatives in file assertions. Integration tests are honest module compositions;
the five-arm release-gate half runs on fabricated numbers, and routing⇄eval are
never composed (that seam is unbuilt — matches roadmap). Error-fingerprint
stability is wholly delegated to the unwritten daemon-side producer.

## 7. The engine/wiring boundary (verified first-hand; moved during review)

Main advanced twice while this review ran (PR #16 at c859077, PR #17 at
fb55713), and PR #17 moved the boundary substantially:

- `StartWorkflow` is wired through daemon + assembly, and — new in PR #17 —
  the **driver is now wired too**: `WorkflowDriver`/`NodeExecutor` are consumed
  by `crates/codypendentd/src/workflow_exec.rs` (new, agent nodes execute
  through the agent loop), a `conductor` (`crates/workflow/src/conductor.rs`)
  drives runs in the daemon, with recovery + pause/resume/retry commands
  (protocol `command.rs`), migration `0011`, and CLI + integration tests.
  (Correctness review in §4.)
- PR #17 also landed two cross-cutting items from the 2026-07-20 review:
  **trust-boundary framing** (context manifest opens with an
  "EVIDENCE, NOT INSTRUCTIONS" preamble; tool/skill cards carry trust tiers;
  GitHub read observations wrapped in untrusted-evidence labels — with tests)
  and the **cargo-deny supply-chain gate** (`deny.toml` +
  `EmbarkStudios/cargo-deny-action@v2` in CI).
- `codypendent-routing` and `codypendent-eval`: **still zero consumers**
  outside their own crates at fb55713.
- `codypendent-sandbox`: still consumed only by the CLI
  (`plugin inspect`/`diff`).

## 8. Standing items from the 2026-07-20 review — status

- **C11 (lexicographic revision comparison): still open.**
  `crates/knowledge/src/memory.rs:157` still compares revision strings with
  TEXT range operators.
- **Dead/unwired inventory: largely unchanged.** Still present with no
  production caller: `pending_effects` writers, `shell_interpreter_requires_approval`
  (parsed, never read), integrations `Debouncer` + provenance resolver,
  `provider-anthropic` feature, `DocumentCrdt::set_block`, `/update-docs`
  execution, `ApprovalScope::Pattern`/`Repository` (still stored-but-inert).
  Newly *retired* from the inventory by PR #17's refactors:
  `WorkflowStore::resume`/`ResumePlan` and `ready_nodes` are now test-only
  (the driver uses `ready_node_ids`), joining the list rather than leaving it.
- **S7 (OS sandbox enforcement): unchanged as enforcement**, but the decision
  layer (`SandboxProfile`) now exists in `codypendent-sandbox`.
- **§4 performance cluster, cross-language golden vectors / generated protocol
  SDK: unchanged** (no golden vectors landed; the VS Code codec is still
  hand-written).
- **Trust-boundary rendering: closed by PR #17** (evidence preamble, trust
  tiers, GitHub-read labels — with tests).
- **`cargo deny`: closed by PR #17.** `cargo audit`/`npm audit` still absent,
  largely subsumed by deny's advisories check.

## 9. What remains to build

**Make Phase 5 demoable (closest to done):** tool-node execution through the
existing tool layer; blackboard `post`/`supersede` wiring from agent outputs
(+ daemon read/write commands + subscription); per-node worktree binding
(P5-D1 — also closes the Phase-1 follow-up); role→profile/model-policy/budget
enforcement in execution; node cost accounting; live run-state overlay in the
TUI views; `WorkflowNodeTransitioned` ledger events; CancelWorkflow.

**Close Phase 4 (unchanged from last review):** client-side CRDT replica
consuming the `DocumentSync` stream; `PublishPlan` execution through the
approval-gated GitHub write path; live language-server spawn (or demote the
Py/TS adapter claim); an end-to-end socket test of mutate→sync fan-out.

**Phase 6 enforcement (the hard gate, correctly sequenced in the roadmap):**
OS sandbox (bubblewrap/seccomp, Seatbelt, AppContainer) + WASM runtime
consuming the existing `SandboxProfile`; executable hooks/skill scripts
through it; a trusted-publisher key store so verification has real keys;
client capture paths feeding `InputEnvelope`; wiring `sanitize_untrusted`
onto real MCP/plugin output; theme-pack loading + `ColorDepth::detect` in the
live TUI (currently hardcodes `Theme::dark()`).

**Phase 7 wiring:** daemon model-execution seam for the router; `model_profiles`
migration + bench harness; `codypendent eval run` CLI + the 50–100 pinned
fixture cases; OTLP export; promotion persistence + real shadow/canary
execution; error-fingerprint production (the determinism contract is currently
delegated to code that doesn't exist).

**Cross-cutting:** generated protocol SDK or golden vectors (drift already
bit once); real embeddings behind the vector trait ("dense" retrieval is
still lexical); a CI job for the VS Code extension; `/update-docs` execution
or removal; the §5 dead-code inventory (largely still standing).

## 10. Suggested order of attack

1. **Small-diff medium-defect batch** (one PR): P5-D1 (bind the executor's
   known repo root / per-run worktree, not cwd), P5-D2 (compare stored
   signature on idempotent-duplicate start), P5-D3 (take the run lock in
   retry/pause), P6-A (fold resource caps into the permission diff), P7-1
   (validate escalation-chain uniqueness), P7-2 (require ≥1 canary
   observation), FP-1 (`RUBYLIB`). All are cheap; all close real holes.
2. **Tool-node execution + blackboard writes** — the two seams that make the
   canonical workflow complete end-to-end and light up the already-built TUI
   views. This is the shortest path to the product's flagship demo.
3. **Finish the Phase 4 vertical** (replica + publish), per the roadmap's own
   "finish this before deepening Phase 5" note — now arguably *after* step 2,
   since Phase 5 got so close.
4. **Test the untested edges:** async CLI harness tests (C6/FP-2 code), a
   gated live-provider test, extension CI job, pin the six unpinned fixes.
5. **OS sandbox enforcement** before any plugin executes anything.
6. Then Phase 7 daemon wiring, routing arms over a real suite, and the
   learning loop against real traces.
