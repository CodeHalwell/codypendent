# Core Data Contracts

The following types define the stable domain language. Exact serialization may evolve, but semantic identity should remain consistent.

## IDs

Use opaque UUIDv7 or equivalent sortable identifiers.

```rust
pub struct SessionId(Uuid);
pub struct RunId(Uuid);
pub struct TaskId(Uuid);
pub struct AgentId(Uuid);
pub struct ArtifactId(Uuid);
pub struct WorkflowId(Uuid);
pub struct ToolId(Uuid);
pub struct SkillId(Uuid);
pub struct PluginId(Uuid);
pub struct DocumentId(Uuid);
pub struct ModelId(String);
```

## Actor

```rust
pub enum Actor {
    Human(UserId),
    Agent {
        agent_id: AgentId,
        run_id: RunId,
        model: ModelId,
    },
    Client(ClientId),
    Integration(IntegrationId),
    System,
}
```

## Session

```rust
pub struct Session {
    pub id: SessionId,
    pub workspace: WorkspaceId,
    pub title: String,
    pub state: SessionState,
    pub active_runs: Vec<RunId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub revision: u64,
}
```

## Run

```rust
pub struct Run {
    pub id: RunId,
    pub session_id: SessionId,
    pub objective: String,
    pub state: RunState,
    pub workflow: Option<WorkflowId>,
    pub workspace_lease: Option<WorkspaceLeaseId>,
    pub budget: RunBudget,
    pub model_policy: ModelPolicyId,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}
```

## Artifact

```rust
pub struct Artifact {
    pub id: ArtifactId,
    pub hash: ContentHash,
    pub media_type: String,
    pub byte_length: u64,
    pub classification: DataClassification,
    pub provenance: Vec<EvidenceRef>,
    pub storage: ArtifactLocation,
}
```

## Proposed action

```rust
pub enum ProposedAction {
    ReadFiles { paths: Vec<PathBuf> },
    WritePatch { patch: ArtifactId },
    ExecuteCommand { request: CommandRequest },
    NetworkRequest { destination: NetworkDestination },
    GitCommit { repository: RepositoryId },
    GitPush { remote: String, branch: String },
    GitHubMutation { operation: GitHubOperation },
    InstallPlugin { plugin: PluginId },
}
```

## Policy decision

```rust
pub struct PolicyDecision {
    pub decision: Decision,
    pub reasons: Vec<PolicyReason>,
    pub required_approval: Option<ApprovalPolicy>,
    pub capability_grant: Option<CapabilityGrant>,
    pub policy_version: PolicyVersion,
}
```

## Evidence

```rust
pub struct EvidenceRef {
    pub artifact: ArtifactId,
    pub range: Option<ArtifactRange>,
    pub source_revision: Option<String>,
    pub observed_at: DateTime<Utc>,
}
```

## Finding

```rust
pub struct Finding {
    pub id: FindingId,
    pub statement: String,
    pub confidence: f32,
    pub evidence: Vec<EvidenceRef>,
    pub status: FindingStatus,
}
```

## Model policy

```rust
pub struct ModelPolicy {
    pub allowed_providers: Vec<ProviderId>,
    pub allowed_classifications: Vec<DataClassification>,
    pub maximum_cost: Option<Money>,
    pub required_capabilities: ModelCapabilities,
    pub routing_strategy: RoutingStrategy,
}
```

## Workspace lease

```rust
pub struct WorkspaceLease {
    pub id: WorkspaceLeaseId,
    pub repository: RepositoryId,
    pub worktree: PathBuf,
    pub branch: String,
    pub owner: RunId,
    pub mode: LeaseMode,
    pub expires_at: DateTime<Utc>,
}
```

## Registry descriptor

```rust
pub struct RegistryDescriptor {
    pub id: RegistryItemId,
    pub kind: RegistryItemKind,
    pub name: String,
    pub version: Version,
    pub scope: Scope,
    pub trust: TrustTier,
    pub capabilities: Vec<CapabilityRequest>,
    pub content_hash: ContentHash,
}
```

## Error model

Errors should be structured:

```rust
pub struct CodypendentError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub user_action: Option<UserAction>,
    pub details: serde_json::Value,
    pub correlation_id: CorrelationId,
}
```

Never require clients to parse human text to decide whether a retry or approval is possible.

## Specification, change set, hook, and chronicle

```rust
pub struct TaskSpec {
    pub id: TaskSpecId,
    pub objective: String,
    pub requirements: Vec<Requirement>,
    pub constraints: Vec<Constraint>,
    pub acceptance: Vec<AcceptanceCriterion>,
    pub budget: TaskBudget,
}

pub struct ChangeSet {
    pub id: ChangeSetId,
    pub base_revision: GitRevision,
    pub worktree: WorkspaceLeaseId,
    pub patches: Vec<PatchRef>,
    pub verification: Vec<VerificationResult>,
    pub status: ChangeSetStatus,
}

pub struct HookDefinition {
    pub id: HookId,
    pub event: HookEvent,
    pub kind: HookKind,
    pub runtime: HookRuntime,
    pub failure_policy: HookFailurePolicy,
}
```
