# Architecture Decisions

This file captures the initial accepted decisions. Each should become a separate ADR if the repository adopts a formal ADR workflow.

## ADR-001 — Persistent daemon and disposable clients

**Status:** Accepted

**Decision:** Agent execution and durable session state live in `codypendentd`. TUI, CLI, IDE, and headless clients attach through a versioned protocol.

**Reason:** Long-running work must survive editor closure and client switching.

**Consequence:** Client features require projection and synchronization design, but product state is consistent.

---

## ADR-002 — Custom internal protocol; ACP and MCP as adapters

**Status:** Accepted

**Decision:** Codypendent defines its own client protocol. ACP is used for compatible editor integration. MCP is used for tools/resources/prompts.

**Reason:** ACP and MCP do not define Codypendent's complete durable session, artifact, policy, and recovery model.

**Consequence:** The project owns protocol versioning and compatibility tests.

---

## ADR-003 — SQLite and content-addressed artifacts for local authority

**Status:** Accepted

**Decision:** SQLite in WAL mode is the first local metadata/event store. Large data uses a content-addressed filesystem store. PostgreSQL is a later deployment option.

**Reason:** A personal local-first daemon should not require a server database.

**Consequence:** Repository traits must avoid SQLite-specific semantics where a server implementation is expected.

---

## ADR-004 — CRDTs only for collaborative content

**Status:** Accepted

**Decision:** Use CRDTs for documents and shared editable content, not workflow execution or approvals.

**Reason:** Execution state requires authoritative ordering, transactions, and leases.

**Consequence:** Clients receive projection events for runtime state and CRDT updates for collaborative documents.

---

## ADR-005 — Adopt `agent-framework-rs`

**Status:** Accepted

**Decision:** Use `agent-framework-core` and selected provider/integration crates as the in-process agent framework.

**Reason:** The framework already supplies agents, clients, tools, sessions, middleware, skills, compaction, workflows, checkpointing, HITL, MCP, A2A, providers, and observability.

**Consequence:** Codypendent focuses on persistent product infrastructure and contributes reusable improvements upstream.

---

## ADR-006 — Git worktrees isolate writing agents

**Status:** Accepted

**Decision:** Writable agent tasks receive dedicated worktrees by default.

**Reason:** Parallel agents must not corrupt the developer's working directory or each other's intermediate state.

**Consequence:** The daemon owns worktree lifecycle and reconciliation.

---

## ADR-007 — Logical knowledge fabric, multiple physical stores

**Status:** Accepted

**Decision:** Present a unified entity graph while keeping transactional records, artifacts, CRDT state, Git, and derived indexes in appropriate stores.

**Reason:** One graph database is not the best authority for every data type.

**Consequence:** Graph and search indexes must be rebuildable and evidence-backed.

---

## ADR-008 — Single-agent default

**Status:** Accepted

**Decision:** The default is one agent executing a verified workflow. Multi-agent execution requires an explicit orchestration choice or router justification.

**Reason:** Swarms add cost, coordination overhead, and conflict risk.

**Consequence:** Every multi-agent workflow should have a single-agent baseline in evaluation.

---

## ADR-009 — Selected framework features rather than `full`

**Status:** Accepted

**Decision:** Codypendent enables only needed `agent-framework-rs` provider and ecosystem crates.

**Reason:** Reduce compile time, binary size, dependencies, and attack surface.

**Consequence:** Distribution feature matrices and runtime capability detection are required.

---

## ADR-010 — Agent-generated changes require promotion

**Status:** Accepted

**Decision:** Skills, prompts, policies, routing weights, and workflows created by agents remain drafts until evaluated and approved.

**Reason:** Self-learning must not become uncontrolled self-modification.

**Consequence:** Versioned evaluation and rollback are core platform features.

## ADR-011 — Specifications, plans, and change sets are first-class

**Status:** Accepted

Requirements, acceptance criteria, execution plans and proposed changes are versioned domain artifacts rather than transient chat prose.

---

## ADR-012 — Hooks are a separate extension primitive

**Status:** Accepted

Hooks are typed lifecycle reactions with their own permissions and failure policies.

---

## ADR-013 — Modes are policy presets

**Status:** Accepted

Ask, Explore, Spec, Plan, Build, Review, Verify, Autopilot and Fleet map to visible default tools and capability policies.

---

## ADR-014 — Worktrees and change sets solve different problems

**Status:** Accepted

Worktrees isolate executable filesystem state; change sets provide logical review, selective apply and publication.

---

## ADR-015 — One event model serves interactive and headless clients

**Status:** Accepted

JSONL, TUI, IDE and future remote clients consume the same domain event stream.

---

## ADR-016 — Loro is the collaborative-document CRDT

**Status:** Accepted

**Decision:** Back the Docs Studio's working documents (ADR-004) with **Loro**,
selected over Automerge and Yrs by the STEP 4.1 benchmark.

**Reason:** On Codypendent-shaped documents (paragraph-by-paragraph edit
histories from 1 KB to 1 MB) Loro wins snapshot **load** and **build** by two to
three orders of magnitude at the largest case, while all three converge on
concurrent edits. Loro's only non-first axis is encoded snapshot size (a few KiB,
~2.8× the most compact encoder but negligible in absolute terms, and far under
Yrs's ~1 MiB update-log snapshot). This is within the decision rule's guard (pick
Loro unless it loses by >2× on load or memory for the largest case): it does not
lose on load, and a few KiB is not a memory loss. Loro is also Rust-native with
first-class incremental updates, rich text, and history. Exact per-run numbers
(wall-clock varies by machine) are in the measured report, whose decision section
is generated from the same measurements as its table:
[`docs/docs/benchmarks/crdt-2026-07-18.md`](benchmarks/crdt-2026-07-18.md),
reproducible via `benches/crdt-bench`.

**Consequence:** `loro` is a product dependency of `codypendent-knowledge`; the
CRDT snapshot is authoritative for drafts (ADR-004) and Git stores reviewed
Markdown snapshots. The benchmark harness stays a standalone workspace so
Automerge/Yrs never enter the product's dependency graph.

---

## ADR-017 — Independent products own their runtime state

**Status:** Accepted

**Decision:** Codypendent and Son of Anton ship independently, each with a
pinned engine and SDK. An explicit product profile selects private data,
configuration, credentials, logs and socket paths. Codypendent remains the
default; embedding hosts can select `son-of-anton`. Explicit profile arguments
override inherited selectors. Only the selected product's path overrides are
read; the other product's environment variables do not redirect discovery.
Branded clients pin their product when launching daemon children.

Sharing requires explicit attachment through the authenticated, version-negotiated
protocol. The selected daemon retains authority over its sessions, approvals,
execution and durable store; attaching a client does not copy a live ledger or
restart work. Disconnecting a client does not cancel a run. Private SQLite files
are never the integration boundary.

**Reason:** Either application must install and operate without the other.
Sharing implementation must not silently share credentials, mutable stores or
execution authority. Product profiles provide that boundary without requiring an
upfront engine extraction or a separately installed runtime service.

**Consequence:** Each product carries and updates its own pinned engine, with
additional packaging size and compatibility verification. Independent daemons'
database-local leases do not coordinate same-checkout writes: use separate
writing worktrees and reviewed patch or commit exchange. Automatic cross-product
write coordination requires a future jointly enforced ownership contract.
Existing Codypendent defaults and repository-owned `.codypendent/` formats remain
compatible; this decision requires no database migration. See
[shared runtime integration](../SHARED_RUNTIME.md).

---

## ADR-018 — Workflow discovery binds explicit starts to reviewed definitions

**Status:** Accepted

**Decision:** Protocol 1.12 adds repository-scoped `ListWorkflows` and a bounded
catalogue of definition metadata, inputs, source, diagnostics and definition
digests. Discovery uses named execution's registry precedence and collision
rules, authorizes access to private daemon sources, and echoes the canonical
repository. Diagnostics expose static messages rather than raw YAML or parser
context. Listing or selecting a definition does not execute it or establish
model and tool readiness.

A guarded `StartWorkflow` carries the selected definition's SHA-256 digest of
the complete parsed definition, including inputs, budgets and steps. The daemon
checks the same resolved manifest it will execute before creating a new run.
Accepted-request recovery returns the original receipt; the store also compares
owner, repository, inputs and full definition identity atomically when duplicate
requests race. A conflicting request is rejected rather than creating another
run. Clients retain uncertain request identities and require explicit
reconciliation or retry; connection recovery never automatically replays starts.

**Reason:** A workflow may change between browsing and submission, and a lost
reply does not prove that a launch failed. Definition binding prevents silent
execution of a changed workflow; atomic identity checks prevent concurrent
retries from bypassing that guarantee. Parsed-definition digests allow formatting
changes while detecting semantic changes beyond graph shape.

**Consequence:** Clients must negotiate minor 12 before discovery or guarded
starts. The optional digest preserves legacy unguarded starts; clients connected
to older runtimes reject guarded requests before sending them. There is no
database migration. Launching a changed definition requires a fresh selection
and request identity; catalogue metadata is not an execution approval or a
readiness certificate. Desktop's retained form state is scoped to the owning
transport and app lifetime, not durable recovery of unsent inputs after exit.

Distribution acceptance remains separate: final self-contained installers must
include genuine pinned resources and pass packaging checks, followed by installed
workflow execution, approvals, cancellation and budget-limit checks. An explicitly
unbundled debug window test cannot satisfy those gates or weaken resource and
execution-policy checks. Native signing/notarization and Windows shared-engine
transport require platform evidence. See the
[readiness record](../reviews/2026-10-31-product-readiness.md) and
[native release smoke scope](../reviews/desktop-release-smoke.md).
