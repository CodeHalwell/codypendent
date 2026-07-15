# Contributing to Codypendent

## Principles

- Prefer a working vertical slice over speculative abstraction.
- Reuse `agent-framework-rs` where the framework already owns the concept.
- Do not add a crate merely to mirror an architecture diagram.
- Persist before publishing an externally visible state transition.
- Every privileged action needs a policy path.
- Every derived fact needs evidence.
- Every learned change needs evaluation and rollback.
- Mouse interactions need keyboard equivalents.

## Development workflow

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Recommended additional checks:

```bash
cargo nextest run --workspace
cargo deny check
cargo audit
```

## Design changes

Changes affecting any of the following require an ADR update:

- daemon/client authority;
- event ordering;
- persistent data;
- protocol compatibility;
- security boundary;
- plugin execution;
- framework ownership;
- model data policy.

## Pull requests

A pull request should explain:

- user-visible outcome;
- affected invariant;
- migration or compatibility impact;
- security impact;
- tests;
- recovery behavior;
- observability added;
- documentation updated.

## Commit scope

Keep commits intentional. Generated index data and local artifacts should not be committed unless they are fixtures.
