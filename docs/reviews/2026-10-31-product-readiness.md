# Independent products: production-readiness iteration

Evidence recorded on 2026-10-31 for `feat/independent-agent-products`.
These are local validation results; no release, signature, or remote CI result is
implied. Beadsmith remains a retired reference and its checkout is unchanged.

## Technical competitive baseline

This is a workflow comparison against current official documentation, accessed
2026-10-31. It identifies useful acceptance scenarios, rather than establishing
performance superiority. Claude's agent teams are experimental; Cursor's cloud
coordinator and its editor/worktree surfaces have different availability.

| Acceptance scenario | Official reference | Current implementation and remaining evidence |
| --- | --- | --- |
| Install, select a repository, configure a model, complete a small change | [Cursor quickstart](https://cursor.com/docs/get-started/quickstart) | Both products own their configuration and runtime. Anton's complete extension dependencies and pinned Node setup now compile. Fresh signed installers and real-provider onboarding still need platform evidence. |
| Prepare a plan, review it, then explicitly begin implementation | [Cursor Plan Mode](https://cursor.com/docs/agent/plan-mode) | Anton's Plan → Act and Desktop's Plan → Build prepare editable drafts, preserving feedback. Recorded provenance and terminal outcomes gate eligibility; preparing never submits work. |
| Run parallel work without mixing checkouts | [Codex worktrees](https://learn.chatgpt.com/docs/environments/git-worktrees), [Claude agent teams](https://code.claude.com/docs/en/agent-teams) | Existing workflow execution owns concurrency, budgets and worktrees. The new catalogue makes its definitions accessible in both UIs. Separate engines require separate writing worktrees; automatic cross-product write coordination is not implemented. |
| Discover reusable workflows, provide inputs, observe progress and intervene | [Cursor project coordination](https://cursor.com/docs/agent/projects) | Both UIs now read the owning daemon's effective repository catalogue and launch explicit, definition-bound requests. Input types are domain names, not JSON Schemas or a promise of model/tool readiness. |
| Review the actual patch and validation evidence | [Codex IDE workflows](https://learn.chatgpt.com/docs/codex/ide), [Cursor Agent Review](https://cursor.com/docs/agent/agent-review) | Existing proposal/patch review remains available. A completed workflow node or moved board card does not establish test success. Comparative patch acceptance and regression rates have not been measured. |
| Resume work, reconcile uncertain submissions, and retain approval authority | [Claude common workflows](https://code.claude.com/docs/en/common-workflows), [Claude permissions](https://code.claude.com/docs/en/permissions) | Durable sessions and owner-scoped controls remain authoritative. Catalogue starts carry stable request identities and definition fingerprints. Native close/reopen evidence is assessed separately below. |
| Maintain useful code context without overstating the graph | [Codex IDE context](https://learn.chatgpt.com/docs/codex/ide), [Claude context window](https://code.claude.com/docs/en/context-window) | Both products expose bounded structural graph evidence. Semantic retrieval, unsaved-buffer parity, automatic specialist selection and comparative context-efficiency measurements remain convergence work. |

## Delivered source boundary

- Protocol 1.12 adds `ListWorkflows` with bounded, private-source-authorized
  metadata. Discovery uses the same registry precedence and collision rules as
  named execution. It returns static diagnostics instead of YAML or parser
  context, and echoes the canonical repository.
- A selected definition carries a SHA-256 fingerprint of the complete parsed
  definition, including inputs and budgets. The runtime checks that fingerprint
  against the same manifest it executes. Legacy unguarded starts remain supported;
  a guarded start is rejected before sending to an older runtime.
- Accepted-request recovery binds owner, repository, inputs, named definition and
  full definition identity. The duplicate insertion check enforces the same
  identity atomically, including simultaneous conflicting starts.
- Anton stores uncertain launch payloads in SecretStorage and receipt metadata
  in workspace state. Recovery requires an explicit reconciliation action.
  Unsubmitted values remain in the live panel, not plaintext webview state.
- Desktop retains drafts, launch identities and owned receipts in App memory
  across view navigation. A transport-owner change clears them. Late background
  receipts cannot silently replace the request identity shown by another view.
  This in-memory state does not promise recovery of unsent inputs after app exit.
- Linux release staging now includes the matching CLI, confined worker, Node,
  Monty and embedding resources. It verifies platform/version and complete
  staged bytes before an AppImage can be uploaded. A damaged package cannot
  silently adopt a different CLI from PATH.

## Observed checks for this iteration

| Boundary | Result |
| --- | --- |
| Anton Code OSS dependency setup | Missing locked dependencies restored; Node 22.23.4 checksum verified |
| Anton built-in extensions and core | Full extension compilation and core TypeScript check passed |
| Anton private engine source | Vendored inputs include cross-root Rust contracts; integrity and build-input verification are required after each runtime refresh |
| Anton primary extension | Build and 716 unit tests passed |
| Anton workflow UI | 29 workflow tests passed; Chromium checked 280, 420 and 1000px layouts |
| Anton daemon extension | 106 passed; one opt-in real-daemon integration skipped in this package run |
| SDK | Build/typecheck passed; 542 tests passed, one opt-in integration skipped |
| Real dual-product isolation | Opt-in SDK integration passed against the fresh CLI: private models, user workflow catalogues and sessions; stopping Anton left Codypendent running; zero provider requests |
| Desktop frontend | Production build/typecheck passed; 692 tests passed across 83 files |
| Desktop workflow UI | Chromium checked selection without execution, retained inputs, explicit guarded start and unclipped layouts at 280, 390 and 1360px |
| Workflow library | 120 unit tests passed; 7 atomic identity integration cases and 18 existing store cases passed |
| Workflow assembly | Four catalogue, definition and accepted-request recovery cases passed |
| CLI integration and native build | Two workflow integration cases passed; matching CLI and worker built; strict CLI Clippy, schema drift and formatting checks passed |
| Desktop native bridge and launcher | Two bridge tests and 16 launcher tests passed, including missing whole-resource-tree refusal; native library check passed |
| Native protocol | 186 tests passed; regeneration helpers are separately ignored |
| Linux staging and Apple compatibility helpers | Six Linux and seven Apple fixture checks passed |
| Native smoke harness isolation | Two tests passed, including awaited owned-process termination |
| Fresh native Desktop window | Unbundled GTK/Tauri debug build and real window smoke passed on Debian 13 x86-64 with WebKitGTK 2.54; all 19 frontend files match the checked build |

These counts describe their named suites. The source test-attribute count in
ROADMAP is not a full Rust workspace test result. Earlier native installer
results on other commits do not validate these source changes.

The native window check exercised SDK-issued work, closing the app while the
daemon finished, replay after reopening, authenticated curated memory, and the
repository workflow catalogue with a required input preview. Two loopback model
requests were expected and observed: one Ask and one automatic memory extraction.
Catalogue browsing added zero requests; the fixture workflow was not executed.
Owned processes exited and private state was removed before the success report
was written. The task's Docker container was then stopped and removed.

This fixture enabled the WebKit renderer sandbox override for container
automation. Daemon and worker execution policy remained unchanged; unavailable
worker confinement failed closed. Distribution resources and semantic embedding
retrieval were not validated by this explicitly unbundled debug build.

## Remaining release gates

1. Build and verify real self-contained installers on the final reviewed source.
   This environment's enforced network policy excludes the pinned Hugging Face
   embedding asset/model cards and the Apache license URL. No substituted assets
   or empty resource files are acceptable. See `embedding-runtime.json` and the
   [native smoke scope](desktop-release-smoke.md).
2. Exercise complete workflow execution, approvals, cancellation and budget-limit
   scenarios in installed builds. The native-window catalogue preview does not
   execute its fixture workflow or establish model/tool readiness.
3. Obtain native macOS signing/notarization and update/reinstall evidence.
   Windows shared-engine transport remains a separate implementation gate.
4. Run a reproducible comparative task corpus before making market-leadership
   claims: same repositories, objectives, budgets and acceptance checks; retain
   patch quality, failures, recovery outcomes, measured latency and cost coverage.
   No paid-provider or competitor-agent evaluation ran in this iteration.

The runtime build/archive, new protocol path and existing UI integrations require
normal CI and review before merge. This work does not publish or sign software.
