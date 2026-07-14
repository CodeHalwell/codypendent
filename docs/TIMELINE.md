# Codypendent Timeline

This timeline is scoped as a practical path from naming decision to public launch readiness.

## Phase 0: Identity Lock (Week 0-1)

Goal: lock naming and ownership prerequisites.

- Confirm `Codypendent` as product name.
- Run trademark screening (UK + US).
- Check and reserve key domains and GitHub org.
- Run package name checks (`crates.io`, npm).
- Publish a short naming/style guide in `docs/product/`.

Exit criteria:

- Name decision ratified.
- Known legal/search conflicts documented with mitigation.

## Phase 1: Repository Scaffold (Week 1-2)

Goal: establish technical and product documentation structure.

- Finalize repository scaffold from `docs/PROJECT_SCAFFOLD.md`.
- Add architecture notes for daemon, protocol, policy hierarchy.
- Define CLI command surface and naming guardrails.
- Add launch checklist and terminology docs.

Exit criteria:

- New contributors can locate product, architecture, and workflow docs in <5 minutes.

## Phase 2: Core Runtime Slice (Week 2-5)

Goal: implement the thinnest useful local-first runtime.

- Create workspace crates (`cli`, `daemon`, `protocol`).
- Implement daemon start/stop/status.
- Implement session bootstrap and task dispatch stub.
- Add structured event log with basic compaction hooks.

Exit criteria:

- `codypendent daemon status` works locally.
- `codypendent agent run <task>` executes a stub flow end-to-end.

## Phase 3: Skills + Docs Loop (Week 5-7)

Goal: make the system editable and useful beyond demos.

- Add skill package loading and metadata validation.
- Add one maintained skill example (`fix-ci`).
- Define collaborative docs model and maintenance workflow.
- Add PR workflow docs and CI observability notes.

Exit criteria:

- Skill can be edited and re-run locally.
- Docs maintenance workflow is documented and reproducible.

## Phase 4: Integrations (Week 7-10)

Goal: connect runtime to real-world developer surfaces.

- Add initial IDE bridge contract and one client integration.
- Add GitHub automation baseline (PR read/comment/status checks).
- Add plugin permission prompts and policy enforcement.
- Add local-model routing defaults and failure fallback behavior.

Exit criteria:

- One IDE integration is usable in daily development.
- GitHub PR lifecycle flow works for at least one repo.

## Phase 5: Hardening + Public Readiness (Week 10-12)

Goal: prepare for public repository exposure.

- Run security pass on sandboxing, permissions, and secret handling.
- Add onboarding docs and setup assistant behavior constraints.
- Freeze terminology and external messaging.
- Publish roadmap and known limitations.

Exit criteria:

- Security and policy checks pass.
- Public docs support first-time setup without private guidance.

## Risk Track (Runs Across All Phases)

- Naming overlap with Sourcegraph Cody: enforce `codypendent` binary naming in docs and code.
- Scope creep: keep each phase exit criteria strict and measurable.
- Integration drag: prioritize one IDE and one GitHub flow before expanding.
- Policy regressions: test permission-deny precedence at every release gate.
