# Skills, Tools, and Plugins

## Distinctions

### Tool

A typed executable operation:

```text
read_file(path, range) → file excerpt
```

### Skill

A versioned procedural package describing how to perform a class of task. It may depend on tools, references, scripts, and evaluations.

### Plugin

A distributable extension that may provide tools, resources, prompts, skills, indexers, providers, UI components, or workflows.

These concepts overlap but should not be conflated.

## Existing framework foundation

`agent-framework-rs` already provides:

- `Tool`, `ToolDefinition`, `FunctionTool`, and tool source/kind metadata;
- function-invocation middleware;
- approval modes;
- a `Skill` type;
- `SkillsProvider` with progressive disclosure through generated `load_skill` and `read_skill_resource` tools.

Codypendent extends this from an in-memory list into a governed registry.

## Registry item

```rust
pub struct RegistryItem {
    pub id: RegistryItemId,
    pub kind: RegistryItemKind,
    pub name: String,
    pub version: Version,
    pub scope: Scope,
    pub description: String,
    pub intents: Vec<String>,
    pub keywords: Vec<String>,
    pub examples: Vec<UsageExample>,
    pub input_schema: Option<JsonSchema>,
    pub output_schema: Option<JsonSchema>,
    pub dependencies: Vec<RegistryDependency>,
    pub permissions: Vec<CapabilityRequest>,
    pub risk: RiskClass,
    pub provenance: Provenance,
    pub trust: TrustMetadata,
}
```

## Skill package

```text
skills/fix-rust-ci/
├── SKILL.md
├── skill.toml
├── tools.toml
├── tests/
├── references/
├── scripts/
└── assets/
```

A skill is probabilistic when an LLM interprets it. Deterministic scripts may be included, but the package itself must not be advertised as deterministic.

## Semantic retrieval

The registry should retrieve a broad candidate set and then rerank.

```text
task
├── dense retrieval
├── BM25
├── exact identifiers and tags
├── grep over local definitions
├── dependency graph
└── historical task-conditioned success
        ↓
candidate union
        ↓
scope, trust, policy, and capability filtering
        ↓
reranking
        ↓
dependency closure
        ↓
context-budget selection
```

Suggested initial candidate sizes:

- dense top 100;
- BM25 top 100;
- exact/tag top 50;
- graph/history top 50;
- rerank 30–50;
- disclose 6–12 tools and 1–3 skills.

The final count is model-profile-dependent.

## Scoring

```text
score =
    dense relevance
  + lexical relevance
  + exact identifier match
  + graph dependency relevance
  + task-conditioned success
  + model compatibility
  - permission risk
  - expected latency
  - expected cost
```

Security is a hard filter, not merely a negative ranking weight.

## Progressive disclosure

Pass compact cards first:

```rust
pub struct ToolCard {
    pub id: ToolId,
    pub name: String,
    pub summary: String,
    pub risk: RiskClass,
}
```

Load full schemas only after selection.

This complements the framework's existing progressive skill disclosure. Codypendent's semantic selector chooses the candidate skills; the framework provider exposes their detailed instructions and resources during the run.

## Skill Studio

The TUI should allow users to:

- browse by scope, trust, language, and status;
- create or clone a skill;
- edit instructions and metadata;
- inspect dependencies and permissions;
- run evaluation cases;
- compare versions;
- inspect past traces;
- publish to Git;
- promote, deprecate, or roll back;
- ask an agent to propose improvements.

AI edits appear as diffs and remain attributed.

## Trust

Every item records:

- publisher identity;
- source repository and commit;
- content hash;
- signature;
- requested capabilities;
- static scan;
- sandbox test;
- trust tier;
- installed scopes;
- revocation status.

Descriptions and instructions are untrusted content. Semantic relevance never implies trust.

## Plugin classes

```text
MCP servers
A2A agents
WASM component plugins
native process plugins
provider adapters
workflow packs
skill packs
indexers
themes
IDE bridges
input/output plugins
```

## Runtime choices

### WASM component

Preferred for new Codypendent-native plugins:

- explicit imports/capabilities;
- portable;
- resource-metered;
- no ambient filesystem or network access.

### Native process

Required for compatibility with existing Node, Python, Go, Java, .NET, and Rust MCP servers.

Native plugins run under OS-level restrictions and receive:

- a minimal environment;
- pre-opened paths only;
- network allowlist;
- CPU/memory/time limits;
- no inherited secrets;
- sanitized outputs.

## Plugin lifecycle

```text
discover
→ inspect manifest
→ verify signature/checksum
→ evaluate permissions
→ install disabled
→ sandbox smoke test
→ user enables at selected scope
→ monitor
→ update with permission diff
→ revoke or remove
```

An update that broadens permissions requires a new approval.

## Example specifications

See:

- [`../specs/skill.toml`](../specs/skill.toml)
- [`../specs/plugin.toml`](../specs/plugin.toml)

## Hooks, commands, compatibility, and hot reload

Hooks are first-class registry items, not hidden shell snippets inside skills. Types include observe, transform, validate, authorize, notify and agent-evaluate.

Custom commands are named entry points into prompts, workflows or deterministic actions. Packages may bundle agents, skills, commands, hooks, workflows, themes and MCP configuration.

Development-mode packages may hot reload non-privileged metadata, prompts and themes. Executable code, signatures or permission changes require controlled restart and renewed review.

Compatibility importers normalize `AGENTS.md`, `CLAUDE.md`, selected editor rules and Agent Skills while preserving source path, scope and precedence.
