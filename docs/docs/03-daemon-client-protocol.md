# Daemon and Client Protocol

## Protocol position

Codypendent needs its own client/daemon protocol.

The Agent Client Protocol (ACP) is an interoperability adapter for editors and agents. It should not define Codypendent's complete internal state model. Model Context Protocol (MCP) is an integration protocol for tools, prompts, and resources. Neither replaces a durable local control protocol.

```text
TUI / CLI / native extensions
        │
        └── Codypendent Client Protocol ── codypendentd

ACP-compatible editor
        │
        └── ACP adapter ── codypendentd

MCP servers
        │
        └── MCP host ── agent runtime
```

## Transport

Initial local transports:

- Unix domain socket on Linux and macOS;
- named pipe on Windows.

Remote transports may later include authenticated WebSocket or Connect/gRPC.

The protocol schema is transport-independent.

## Framing

A simple length-prefixed frame is adequate:

```text
+----------------------+-------------------------+
| u32 payload length   | serialized envelope     |
+----------------------+-------------------------+
```

The first implementation should use JSON for inspection and debugging. MessagePack may be negotiated later. Large data should be exchanged through artifact references, not repeatedly embedded in envelopes.

## Envelope

```rust
#[derive(Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: ProtocolVersion,
    pub message_id: MessageId,
    pub correlation_id: Option<MessageId>,
    pub client_id: ClientId,
    pub workspace_id: Option<WorkspaceId>,
    pub session_id: Option<SessionId>,
    pub sequence: Option<u64>,
    pub payload: Payload,
}
```

## Handshake

```rust
pub struct ClientHello {
    pub client_name: String,
    pub client_version: String,
    pub supported_protocols: Vec<ProtocolVersion>,
    pub capabilities: ClientCapabilities,
    pub resume_token: Option<ResumeToken>,
}

pub struct ServerHello {
    pub selected_protocol: ProtocolVersion,
    pub daemon_version: String,
    pub daemon_instance: DaemonInstanceId,
    pub authentication: AuthenticationResult,
    pub heartbeat_interval_ms: u64,
}
```

Local authentication should combine:

- restrictive socket/pipe filesystem permissions;
- operating-system peer identity where available;
- a per-user daemon secret;
- short-lived resume tokens.

## Attach and resume

```rust
pub struct AttachSession {
    pub session_id: SessionId,
    pub last_seen_sequence: Option<u64>,
    pub subscriptions: Vec<Subscription>,
    pub requested_role: ClientRole,
}
```

The daemon responds with either missing events or a projection snapshot:

```rust
pub enum Catchup {
    Events {
        from: u64,
        through: u64,
        events: Vec<SessionEvent>,
    },
    Snapshot {
        through: u64,
        projection: SessionProjection,
    },
}
```

A client must be able to reconnect after hours and resume from a known sequence.

## Multi-client roles

Several clients may observe one session simultaneously.

```rust
pub enum ClientRole {
    Observer,
    Contributor,
    Controller,
    Approver,
}
```

Exclusivity is attached to specific resources, not to the whole session:

- document edit lease;
- debugger control;
- worktree write lease;
- deployment approval;
- interactive terminal control.

## Commands and events

Commands request state changes:

```rust
pub struct Command {
    pub command_id: CommandId,
    pub idempotency_key: String,
    pub expected_revision: Option<u64>,
    pub body: CommandBody,
}
```

Events record accepted state changes or observations:

```rust
pub struct SessionEvent {
    pub sequence: u64,
    pub occurred_at: DateTime<Utc>,
    pub causation_id: Option<CommandId>,
    pub correlation_id: Option<CorrelationId>,
    pub actor: Actor,
    pub body: EventBody,
}
```

Example command categories:

- create or attach session;
- submit user input;
- approve or reject action;
- cancel, pause, or resume run;
- apply patch;
- edit document;
- change active model policy;
- install or enable plugin;
- invoke UI command.

Example event categories:

- run started;
- model stream delta;
- tool proposed, approved, started, or completed;
- patch proposed;
- workflow node transitioned;
- approval requested;
- document changed;
- artifact created;
- budget warning;
- client presence changed.

## Projection subscriptions

Clients should subscribe to views rather than receive every internal event:

```rust
pub enum Subscription {
    SessionSummary,
    RunTrace { run_id: RunId },
    AgentActivity,
    WorkflowGraph,
    Document { document_id: DocumentId },
    RepositoryStatus,
    GitHubState,
    BudgetState,
}
```

This reduces load and allows UI-specific projection models.

## Semantic mutations, not raw keystrokes

Clients process local keyboard and mouse input. They send semantic changes:

```rust
pub struct DocumentInsert {
    pub document_id: DocumentId,
    pub anchor: TextAnchor,
    pub text: String,
}
```

IDE context should be debounced and semantic:

```rust
pub struct IdeContextUpdate {
    pub active_file: Option<Uri>,
    pub selection: Option<TextRange>,
    pub open_files: Vec<Uri>,
    pub dirty_buffers: Vec<DirtyBufferDigest>,
    pub diagnostics_revision: Option<u64>,
}
```

## Artifact references

```rust
pub struct ArtifactRef {
    pub id: ArtifactId,
    pub media_type: String,
    pub byte_length: u64,
    pub sha256: String,
    pub sensitivity: DataClassification,
}
```

An event may say that test output is available and link to the artifact. The client can stream or page the artifact separately.

## Crash consistency

Important state transitions follow:

1. validate the command;
2. persist the command and intended transition;
3. commit the transaction;
4. perform the external side effect;
5. persist the outcome;
6. publish the resulting event.

On restart the daemon reconciles incomplete operations.

## Versioning

- protocol versions are negotiated during handshake;
- fields are additive by default;
- unknown enum variants must be handled safely;
- breaking changes require a new major protocol version;
- persisted event payloads use explicit schema versions;
- migration tests must replay old fixtures into the new daemon.

## Steering, session branching, and headless streams

```rust
pub enum SessionControlCommand {
    QueueSteering { text: String, apply_at: SafePoint },
    InterruptModelCall { run_id: RunId },
    PauseBeforeNextTool { run_id: RunId },
    ForkSession { checkpoint: CheckpointId, name: String },
    SwitchModelPolicy { run_id: RunId, policy: ModelPolicyId },
    TightenBudget { run_id: RunId, budget: RunBudget },
}
```

A headless client consumes the same event stream as the TUI and serializes envelopes as JSONL. Future remote attachment uses the same roles, subscriptions, resume cursors and revocation model.
