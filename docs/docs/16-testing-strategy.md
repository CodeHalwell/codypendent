# Testing and Acceptance Strategy

## Test pyramid

### Unit tests

- protocol serialization;
- event transitions;
- policy resolution;
- path canonicalization;
- registry scoring;
- compaction;
- graph deltas;
- model eligibility;
- document conversion.

### Property tests

- event replay produces the same projection;
- duplicate command delivery is idempotent;
- compaction preserves pinned evidence;
- scope merge cannot weaken higher policy;
- graph delta application is order-safe where declared;
- artifact hashes match contents.

### Integration tests

- daemon/client reconnect;
- SQLite transaction and outbox;
- framework agent event translation;
- provider tool calling;
- worktree lifecycle;
- GitHub webhook verification;
- IDE context synchronization;
- plugin sandbox.

### End-to-end tests

Run real repository workflows in fixtures.

## Recovery tests

Inject failures at:

- after command persistence;
- before external effect;
- after external effect but before outcome persistence;
- during model stream;
- during shell execution;
- during worktree creation;
- during artifact write;
- during checkpoint;
- during client catch-up.

For each injection point specify the expected restart state.

## Protocol compatibility

Maintain fixture corpora for previous protocol versions.

Tests verify:

- old client handshake;
- unknown fields;
- unknown enum variants;
- event replay;
- snapshot migration;
- rejected incompatible versions.

## Worktree tests

- nested path rejection;
- stale record reconciliation;
- unmerged commit protection;
- dirty file preservation;
- owned process cleanup;
- simultaneous creation;
- symlink boundary;
- branch collision;
- optional policy checks.

## Security tests

- path traversal;
- symlink escape;
- command injection;
- environment leakage;
- unauthorized network;
- malicious MCP output;
- skill prompt injection;
- plugin permission escalation;
- forged GitHub webhook;
- replayed approval;
- cross-repository memory leakage.

## Evaluation harness

Every agent task records:

```rust
pub struct EvalCase {
    pub repository_revision: GitRevision,
    pub prompt: String,
    pub policy: ModelPolicy,
    pub expected: Vec<Assertion>,
    pub maximum_cost: Option<Money>,
    pub maximum_duration: Option<Duration>,
}
```

Assertions may include:

- tests pass;
- file changed or unchanged;
- symbol exists;
- command was not executed;
- citation points to correct source;
- no forbidden network use;
- user approval requested;
- patch scope limit.

## Routing evaluation

Compare:

- static strongest model;
- static cheap model;
- router;
- router with escalation;
- local-first router.

Measure:

- task success;
- cost;
- latency;
- escalation rate;
- tool-call errors;
- unsafe proposals.

## Retrieval evaluation

For tools, skills, memories, and code:

- recall@k;
- precision@k;
- mean reciprocal rank;
- selection success;
- downstream task success;
- prompt token reduction;
- unsafe-item exclusion.

## TUI tests

Separate:

- reducer/state tests;
- widget rendering snapshots;
- keyboard and mouse equivalence;
- terminal capability fallbacks;
- resize behavior;
- screen-reader-friendly text exports;
- no blocking operation on render thread.

## Release gates

A release candidate must pass:

- unit/integration suite;
- migration replay;
- protocol fixtures;
- recovery matrix;
- security regression suite;
- core repository eval set;
- clean install/uninstall on supported OSes;
- artifact and config backup/restore test.

## Interaction-model acceptance tests

Test that Explore cannot write, Plan emits a versioned plan, Build stays within its worktree, plan changes trigger reapproval, steering applies at a safe point, forks isolate mutable state, model switching preserves artifacts, selective apply is correct, JSONL and TUI observe equivalent events, chronicles resume work and remote revocation works.
