# Codypendent Manual

Codypendent is a local-first agentic developer environment built around a persistent Rust daemon and disposable user-interface clients.

This manual is arranged in the order an implementer should understand the system.

Two consolidated companions to this manual:

- [The Codypendent Story](21-the-codypendent-story.md) — the entire design as one coherent narrative. Read it first if you are new.
- [End-to-End Build Guide](build/00-how-to-use-this-guide.md) — step-by-step implementation plans (Phase 0–7) written for an implementation agent with no prior context, with verified code for Phase 0 and explicit specifications, tests, and exit checklists for every later phase.

## Reading path

1. [Vision and architectural invariants](01-vision-and-invariants.md)
2. [System architecture](02-system-architecture.md)
3. [Daemon and client protocol](03-daemon-client-protocol.md)
4. [Agent runtime and workflows](04-agent-runtime-and-workflows.md)
5. [Skills, tools, and plugins](05-skills-tools-and-plugins.md)
6. [Memory and knowledge fabric](06-memory-and-knowledge-fabric.md)
7. [Code intelligence](07-code-intelligence.md)
8. [Collaborative Docs Studio](08-docs-studio.md)
9. [Models, routing, and compaction](09-model-routing-and-compaction.md)
10. [IDE, GitHub, and multimodal integration](10-ide-github-and-inputs.md)
11. [Security and governance](11-security-and-governance.md)
12. [`agent-framework-rs` integration](12-agent-framework-rs-integration.md)
13. [Observability, evaluation, and learning](13-observability-evaluation-learning.md)
14. [Core data contracts](14-core-data-contracts.md)
15. [Implementation roadmap](15-roadmap.md)
16. [Testing and acceptance strategy](16-testing-strategy.md)
17. [Architecture decisions](17-architecture-decisions.md)
18. [Glossary](18-glossary.md)
19. [Competitive design synthesis](19-competitive-design-synthesis.md)
20. [Interaction and autonomy model](20-interaction-and-autonomy-model.md)

## Normative language

The words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** indicate requirement strength.

- **MUST**: required for architectural correctness or security.
- **SHOULD**: strongly recommended; deviation requires a documented reason.
- **MAY**: optional or implementation-dependent.

## Design stance

This is an implementation-oriented specification, but it does not pretend that every technology choice has already been benchmarked. Candidate technologies such as Loro, Qdrant, and Wasmtime are described with explicit validation gates rather than asserted as universally superior.
