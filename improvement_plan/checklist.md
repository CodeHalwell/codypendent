# Codypendent Improvement Checklist

_Re-verified 2026-08-16: a second independent scan re-confirmed every spot-checked item below
against the unchanged baseline (`5c9dbbc` + working tree); no boxes were ticked because no fixes
have landed yet. Evidence: `findings-register.md` §"Re-verification"._

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

- [ ] Fix SDK worker lifecycle bugs
  - Handle rejection on the abort path (`bridge.ts:454`).
  - Make `#shutdown` idempotent and always close the transport (`runtime.ts:402`).
  - Add timeout/error handling to the stdio drain wait (`stdio.ts:19`).

- [ ] Fix daemon PTY and audit-path issues
  - Missed wakeup in `collect_output_until_deadline` (`daemon/src/unified_exec/process.rs:257-292`).
  - Real `verdict` in `DispatchAudit` instead of hardcoded `"deny"` (`daemon/src/hook_exec.rs:241,354`).
  - Real artifact store on fork-stash failure (`codypendentd/src/executor.rs:3202-3209`).

- [ ] Harden the LSP client
  - Cap `Content-Length` allocation (`knowledge/src/lsp/transport.rs:116-129`).
  - Sanitize diagnostics before embedding into model context (`knowledge/src/lsp/client.rs`).
  - Fix duplicate `.py`/`.pyi` ownership (PYRIGHT vs RUFF, `servers.rs:24-28/48-52`) and worktree-wide fallback (`mod.rs:132`).

## Phase 3 — Hardening and low-severity sweep

- [ ] Fix low-severity findings (verify each against current code)
  - Instruction-file starvation at the byte cap (`runtime/src/instructions.rs:79-82`).
  - Sink-side `workflow_id` path validation (`codypendentd/src/workflows.rs:723`).
  - Dead network-allowlist branch in seatbelt profile (`sandbox/src/executor.rs`).
  - `agent.version` path validation (`integrations/src/acp_registry.rs:492-493`).
  - Checksum `.trim()` asymmetry (`sandbox/src/verify.rs`).
  - Manifest id/version/publisher non-empty checks (`sandbox/src/manifest.rs`).
  - Unicode Cf handling in `contains_unsafe_control` (`council/src/service.rs`).
  - Idempotency marker whole-body scan false positive (`integrations/src/github/idempotency.rs:42-48`).
  - `UiWorker::selection()` pre-handshake panic (`ui-host/src/runtime.rs:1930`).
  - Migration numbering gap 0019 → 0022.

## Phase 4 — Roadmap reconciliation and release readiness

- [ ] Reconcile roadmap claims with the code
  - Phase 6: reconcile wasmi-vs-wasmtime wording; confirm hook engine, client capture, voice v1 status.
  - Confirm which gaps are genuinely absent: setup assistant, brokered secrets, CloudIam/OAuth signing, protocol `EndSession` (`council/src/service.rs:1099`), live LSP spawn, session forking, live measured routing/shadow-canary, OTLP export, protocol SDK generation, eval corpus scale-up, browser tool, GitHub App path, composer polish.

- [ ] Close CI and tooling gaps
  - Add a macOS CI job (macOS Seatbelt executor otherwise never exercised).
  - Add Dependabot/renovate.
  - Resolve the 0003 migration-immutability violation.

## Definition of done

- [ ] Full workspace `cargo clippy --workspace --all-targets` green.
- [x] `sdk/ui` typecheck and test suite green. (Verified 2026-09-23: `npm run check` passes: typecheck, 14 files / 86 tests, build.)
- [ ] The edit tool never corrupts or panic-errors on unicode content.
- [ ] `/undo` restores a checkpoint or is removed; no misleading transcript notes.
- [ ] An accepted cancel/pause always takes effect.
- [ ] All new SDK components follow the sibling contract and have test coverage.
- [ ] No unhandled rejections or hung writes in the SDK worker runtime.
- [ ] All untrusted wire sinks are capped; LSP diagnostics are sanitized.
- [ ] Roadmap and README claims match shipped behavior; absent features are tracked.
