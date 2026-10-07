# Codypendent Improvement Checklist

_Re-verified 2026-08-16: a second independent scan re-confirmed every spot-checked item below
against the unchanged baseline (`5c9dbbc` + working tree); no boxes were ticked because no fixes
have landed yet. Evidence: `findings-register.md` §"Re-verification"._

_Re-checked 2026-10-07 at `76448fc` (v0.14.0) during the whole-repository review in
`docs/reviews/2026-10-07-review.md`. Each item below was read against the current code; items
marked "verified 2026-10-07" were found fixed, and the rest were found still open. Fixes made by
that review are noted as such._

## Phase 1 — Correctness and data integrity

- [x] Fix `edit_match.rs` normalization span bug (`crates/runtime/src/tools/edit_match.rs:524-531`)
  - Verified 2026-09-23: stage 7 (`unicode_normalized`) now compares whole normalized lines/blocks and yields the raw text, so no normalized offset ever slices the raw buffer. Regression tests cover wide characters before the match, ASCII search over Unicode content, Unicode search over ASCII content, and the two-location ambiguity case.
  - Map normalized-space match offsets back to raw-byte offsets (char-boundary safe).
  - Add regression tests: NBSP/smart-quote/em-dash content + ASCII search, unicode search in ASCII content, multi-byte chars before match (panic case), no-match fallthrough.
  - Verify `find_unique_span` uniqueness search and final `replace_range` share one coordinate space.

- [x] Wire up or remove the `/undo` palette command (`crates/tui/src/reduce.rs:7175`, `palette.rs:155`)
  - Verified 2026-09-23: removed. No `undo` palette entry, action, or transcript note remains in `crates/tui/src`; the only mentions are copy stating that deletes have no undo.
  - Either dispatch `RestoreCheckpoint` (guard on `launch_checkpoint` like fork flows at `reduce.rs:3210/5448`) or remove the entry + misleading note.
  - Fix the `key: "u"` hint that conflicts with `input.rs:383` / has no binding.

- [x] Fix cancel/pause TOCTOU race (`crates/codypendentd/src/executor.rs:2515-2538`, `2674-2707`)
  - Verified 2026-09-23: both pending sets now live in one `RunControlRegistry` behind the single `run_control` leaf mutex, and `run_control_survives_concurrent_start_stop_traffic` exercises concurrent start/stop traffic.
  - Make check-and-insert atomic (consistent lock order or single lock).
  - Add concurrent cancel-vs-spawn stress test asserting an accepted cancel always takes effect.

## Phase 2 — Finish the working tree

- [x] Bring the four new SDK components to the sibling contract (`sdk/ui/src/first-party/*.tsx`, untracked)
  - Verified 2026-09-23: the first-party components are tracked, extend `SurfaceOptions`, spread `{...surface}`, derive child ids from `surface.id`, pass `Badge` a `message=`, and `diff-inspector.tsx` has no `onApplyHunk`. `test/first-party/catalogue.test.tsx` covers the catalogue.
  - Required `SurfaceOptions` + `{...surface}` spread (honor `state`/`density`/`width`/`id`).
  - Unique ids derived from caller `id` (no hardcoded `SurfaceFrame`/child ids).
  - Badge via `message=`, not children (`diff-inspector.tsx:70-71`).
  - Remove or implement dead `onApplyHunk` (`diff-inspector.tsx:32`); cap screenshot/patch payloads.
  - Add catalogue tests + snapshot coverage.

- [x] Fix SDK worker lifecycle bugs
  - Verified 2026-10-07: all three are fixed in `sdk/ui/src/worker`.
  - Handle rejection on the abort path (`bridge.ts:454`): the cancel promise now ends in `.catch(reject)`.
  - Make `#shutdown` idempotent and always close the transport (`runtime.ts`): it returns early once `disposing` or `disposed`, reports a rejecting `transport.close` through `onError`, and always reaches `disposed`.
  - Add timeout/error handling to the stdio drain wait (`stdio.ts`): the drain race has a 10 s timeout and `error`/`close` handlers, and tears all three down.

- [ ] Fix daemon PTY and audit-path issues
  - Missed wakeup in `collect_output_until_deadline` (`daemon/src/unified_exec/process.rs`). Still open at `76448fc`: the `Notify` future is created after the drain, so a notification in between is lost. It cannot hang, because the deadline sleep in the same `select!` bounds the wait, so the cost is added latency.
  - Real `verdict` in `DispatchAudit` instead of hardcoded `"deny"` (`daemon/src/hook_exec.rs`). Mostly fixed: the working-directory and exit-status failure paths now pick `deny` or `warn` from the hook's failure policy. One site still writes the literal, the sandbox-refused path (`Err(err)` near line 625), where a `warn` hook records `verdict = "deny"` beside `applied = "allowed"`.
  - Real artifact store on fork-stash failure. Verified 2026-10-07: fixed, `executor.rs` builds the store under the data directory and no longer uses the temp directory.

- [ ] Harden the LSP client
  - Cap `Content-Length` allocation. Verified 2026-10-07: fixed, `MAX_LSP_MESSAGE_BYTES` is 32 MiB (`knowledge/src/lsp/transport.rs`).
  - Sanitize diagnostics before embedding into model context. Verified 2026-10-07: fixed, `sanitize_diagnostic_message` runs on every message and related message (`knowledge/src/lsp/client.rs`).
  - Fix duplicate `.py`/`.pyi` ownership (PYRIGHT vs RUFF). Verified 2026-10-07: fixed, only pyright claims them in the production roster (`servers.rs`); the ruff spec that remains is test-only.
  - Worktree-wide fallback (`mod.rs`): not re-checked.

## Phase 3 — Hardening and low-severity sweep

- [ ] Fix low-severity findings (verify each against current code)
  - Instruction-file starvation at the byte cap (`runtime/src/instructions.rs`). Fixed 2026-10-07 by the review: the budget now keeps the most specific files and says what it dropped. The same change contains repository instruction files to the repository (a symlink to `~/.aws/credentials` was read into the prompt) and bounds the read.
  - Sink-side `workflow_id` path validation. Verified 2026-10-07: fixed, `persist_user_workflow` rejects `/`, `\`, `..` and empty ids (`codypendentd/src/workflows.rs`).
  - Dead network-allowlist branch in seatbelt profile (`sandbox/src/executor.rs`). Still open: validation refuses a non-empty allowlist, so the `(allow network-outbound (remote ip))` branch in the generator cannot be reached.
  - `agent.version` path validation. Verified 2026-10-07: fixed, `agent_dir` sanitises id and version through one function for install and lookup (`integrations/src/acp_registry.rs`).
  - Checksum `.trim()` asymmetry (`sandbox/src/verify.rs`). Not changed: the check trims, `signing_digest` serialises the manifest as written. Cosmetic, since the signature covers whatever padding is there.
  - Manifest id/version/publisher non-empty checks. Verified 2026-10-07: fixed, each has its own error (`sandbox/src/manifest.rs`).
  - Unicode Cf handling in `contains_unsafe_control` (`council/src/service.rs`). Partly fixed: bidirectional controls and the byte-order mark are refused. Zero-width characters (U+200B to U+200D, U+2060) are not.
  - Idempotency marker whole-body scan false positive. Verified 2026-10-07: fixed, every marker in the body is considered, with a test that an earlier quoted marker cannot hide the real one (`integrations/src/github/idempotency.rs`).
  - `UiWorker::selection()` pre-handshake panic. Verified 2026-10-07: fixed, it returns `Option` (`ui-host/src/runtime.rs`).
  - Migration numbering gap 0019 → 0022. Still open: there is no 0020 or 0021.

## Phase 4 — Roadmap reconciliation and release readiness

- [ ] Reconcile roadmap claims with the code
  - Phase 6: reconcile wasmi-vs-wasmtime wording; confirm hook engine, client capture, voice v1 status.
  - Confirm which gaps are genuinely absent: setup assistant, brokered secrets, CloudIam/OAuth signing, protocol `EndSession` (`council/src/service.rs:1099`), live LSP spawn, session forking, live measured routing/shadow-canary, OTLP export, protocol SDK generation, eval corpus scale-up, browser tool, GitHub App path, composer polish.

- [ ] Close CI and tooling gaps
  - Add a macOS CI job (macOS Seatbelt executor otherwise never exercised). Verified 2026-10-07: done, the `test-macos` job in `.github/workflows/ci.yml`. It has not been run by this review.
  - Add Dependabot/renovate. Done: `.github/dependabot.yml` now covers every lockfile (the 2026-10-07 review rewrote it; before, six npm packages and the desktop's own Cargo workspace were unwatched).
  - Resolve the 0003 migration-immutability violation. Not changed by the review: `check_migration_immutability.py` passes on the 53 recorded checksums, and the early-build database incompatibility that ROADMAP records is a shipped-database matter that this review did not touch.

## Definition of done

- [ ] Full workspace `cargo clippy --workspace --all-targets` green.
- [x] `sdk/ui` typecheck and test suite green. (Verified 2026-09-23: `npm run check` passes: typecheck, 14 files / 86 tests, build.)
- [ ] The edit tool never corrupts or panic-errors on unicode content.
- [ ] `/undo` restores a checkpoint or is removed; no misleading transcript notes.
- [ ] An accepted cancel/pause always takes effect.
- [ ] All new SDK components follow the sibling contract and have test coverage.
- [x] No unhandled rejections or hung writes in the SDK worker runtime. (Verified 2026-10-07, see Phase 2.)
- [ ] All untrusted wire sinks are capped; LSP diagnostics are sanitized.
- [ ] Roadmap and README claims match shipped behavior; absent features are tracked.
