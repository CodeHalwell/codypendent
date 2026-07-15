# Glossary

**ACP** — Agent Client Protocol. An editor/agent interoperability protocol used as an adapter, not Codypendent's complete daemon protocol.

**A2A** — Agent2Agent protocol for agent interoperability.

**Artifact** — Immutable or versioned large content such as logs, images, patches, model outputs, or snapshots.

**Blackboard** — Structured shared artifact space used by multiple agents.

**Capability** — Narrow, explicit permission granted to an invocation.

**Client projection** — UI-oriented view derived from daemon state.

**Code graph** — Evidence-backed graph of symbols, files, dependencies, tests, endpoints, and runtime observations.

**Compaction** — Reducing active model context while preserving durable source evidence and resumability.

**Context provider** — Framework component that contributes messages, instructions, tools, or other context before a run.

**CRDT** — Conflict-free replicated data type used for collaborative editable content.

**Daemon** — `codypendentd`, the persistent process owning execution and durable runtime state.

**Episode** — Coherent phase of a run that can be summarized and rehydrated.

**Knowledge fabric** — Logical unified view across code, documents, memories, skills, tools, workflows, runs, and artifacts.

**MCP** — Model Context Protocol for exposing tools, resources, and prompts.

**Memory curator** — Service that validates, scopes, deduplicates, and stores memory candidates.

**Projection** — Materialized view derived from the event ledger for a client or subsystem.

**Registry** — Scoped catalogue of tools, skills, plugins, providers, workflows, and themes.

**Run** — One execution attempt toward an objective within a session.

**Scope** — System, organization, user, workspace, repository, branch, session, or task boundary.

**Skill** — Versioned procedural package containing instructions, references, dependencies, permissions, and evaluations.

**Tool** — Typed executable operation.

**Workflow** — Versioned graph of task nodes, dependencies, approvals, retries, and agents.

## Optional comedy theme

The technical APIs use normal terminology. The TUI may offer an optional theme:

| Technical | Comedy label |
|---|---|
| Memory | Baggage |
| Permissions | Boundaries |
| Plugins | Attachments |
| Multi-agent session | Group Therapy |
| Model router | Couples Counsellor |
| Compaction | Processing |
| Failed runs | Past Mistakes |
| Approval | Seeking Validation |
| Agent handoff | Seeing Other Models |

This vocabulary must remain cosmetic so logs, APIs, and documentation stay unambiguous.
