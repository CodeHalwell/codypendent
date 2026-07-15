# Code Intelligence

## Objective

Codypendent continuously maps a repository into a knowledge graph that supports navigation, retrieval, impact analysis, documentation maintenance, and agent planning.

No single parser can provide the complete graph.

## Three evidence layers

### Syntax layer

Tree-sitter or language-native parsers provide:

- declarations;
- lexical scopes;
- imports;
- calls as written;
- comments;
- annotations;
- incremental changed ranges.

### Semantic layer

Language servers, SCIP indexers, and compiler metadata provide:

- resolved definitions and references;
- type information;
- implementations;
- inheritance and traits;
- diagnostics;
- symbol identity across files.

### Runtime layer

Tests and instrumentation provide:

- observed call edges;
- coverage;
- stack traces;
- request routes;
- performance profiles;
- dynamic dispatch evidence.

Each edge records its evidence and confidence.

## Graph entities

```rust
pub enum CodeNodeKind {
    Repository,
    Package,
    Module,
    File,
    Namespace,
    Type,
    TraitOrInterface,
    Function,
    Method,
    Field,
    Global,
    Constant,
    Endpoint,
    DatabaseTable,
    Test,
    Configuration,
    ExternalDependency,
}
```

## Relationships

```rust
pub enum CodeRelation {
    Contains,
    Defines,
    Imports,
    References,
    Calls,
    Implements,
    Extends,
    Reads,
    Writes,
    Mutates,
    Returns,
    Accepts,
    Tests,
    Configures,
    Serializes,
    DependsOn,
    GeneratedFrom,
}
```

## Evidence-backed edge

```rust
pub struct CodeEdge {
    pub from: CodeNodeId,
    pub to: CodeNodeId,
    pub relation: CodeRelation,
    pub confidence: f32,
    pub evidence_kind: EvidenceKind,
    pub evidence: EvidenceRef,
    pub repository_revision: GitRevision,
}
```

Example confidence:

```text
syntax-inferred call             0.45
LSP-resolved reference           0.90
compiler/indexer resolution      0.98
observed runtime call            1.00 for that execution
```

## Local variables

Persisting every local variable and statement globally creates a noisy graph. The durable graph should prioritize:

- public symbols;
- functions and methods;
- types and fields;
- modules;
- important constants and globals;
- tests and endpoints;
- package and service dependencies.

Statement and local-variable graphs can be generated on demand for active files.

## Incremental pipeline

```text
filesystem change
    ↓
ignore and generated-file policy
    ↓
changed-range syntax parse
    ↓
symbol/reference delta
    ↓
semantic resolver request
    ↓
graph delta transaction
    ↓
summary invalidation
    ↓
lexical/vector update
    ↓
CodeGraphUpdated event
```

## Stable symbol identity

A symbol ID should survive line movement:

```rust
pub struct SymbolKey {
    pub repository: RepositoryId,
    pub language: LanguageId,
    pub package: Option<String>,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub signature_hash: Option<ContentHash>,
}
```

Renames may be linked through Git diff, semantic refactoring events, or similarity heuristics.

## Git revision awareness

Graph snapshots are revision-aware. Queries can ask:

- what changed between two commits;
- which callers changed;
- what documentation references removed symbols;
- which tests cover the modified path;
- what is the transitive blast radius;
- which workflow or skill depends on an API.

## Repository map

The context builder should expose a compact repository map:

```text
workspace
├── packages and services
├── important modules
├── public APIs
├── runtime entry points
├── tests
├── build and CI
└── current change surface
```

This is generated from graph and build metadata, not merely directory names.

## Language adapters

```rust
#[async_trait]
pub trait LanguageAdapter {
    async fn parse(&self, input: ParseInput) -> Result<ParseOutput>;
    async fn symbols(&self, workspace: &Workspace) -> Result<SymbolIndex>;
    async fn diagnostics(&self, workspace: &Workspace) -> Result<Vec<Diagnostic>>;
    async fn build_metadata(&self, workspace: &Workspace) -> Result<BuildMetadata>;
}
```

Initial priority:

1. Rust;
2. Python;
3. TypeScript/JavaScript.

For Rust, use Tree-sitter plus rust-analyzer, Cargo metadata, rustdoc JSON where stable enough, compiler diagnostics, and test output.

## Documentation link maintenance

Documents may embed a symbol reference:

```text
{{ symbol:payments::charge_customer }}
```

A signature or existence change emits a stale-reference signal. The docs maintenance workflow proposes an update but does not silently rewrite published documentation.

## Repository maps and large-repository context

Maintain workspace, repository, package/service, current-change and task-specific maps. Maps are revision-aware artifacts.

For very large repositories, use hierarchical summaries rather than one flat symbol list. The user can inspect which map nodes caused a file or symbol to enter context.
