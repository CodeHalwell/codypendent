//! Golden vectors: the Rust <-> TypeScript wire-codec drift guard (T16).
//!
//! The VS Code extension hand-duplicates this crate's wire codec in
//! `extensions/vscode/src/protocol/` (there is no generated SDK — see
//! ROADMAP.md's cross-cutting "Generate the protocol SDK" item and the
//! 2026-07-21 project review §9). That duplication drifted once for real: the
//! S1 bug, where the extension's approval card omitted the `environment`/`cwd`
//! fields the Rust `ProposedAction::ExecuteCommand` type carries. Golden
//! vectors are the pragmatic guard the review names as the alternative to a
//! full generated SDK (which remains the future direction, out of scope here).
//!
//! ## What lives here
//!
//! This file serializes one deterministic instance of every wire type the
//! extension consumes or produces — fixed sentinel ids/timestamps, never
//! `Uuid::now_v7()`/`Utc::now()`, so the output is byte-for-byte stable — into
//! a committed directory: `<repo-root>/protocol-vectors/*.json`. One file per
//! source module (`command.rs` -> `command.json`, `envelope.rs` ->
//! `envelope.json`, ...), each a JSON object mapping a descriptive vector name
//! to the serialized value, alphabetically sorted (via `serde_json::Value`'s
//! default `BTreeMap`-backed `Map`) and pretty-printed.
//!
//! A TypeScript vitest in `extensions/vscode/test/protocol-vectors.test.ts`
//! reads the SAME files (a relative path, no copy — see
//! `protocol-vectors/README.md`) and asserts the extension's hand-written
//! `CommandBody`/`Payload`/`EventBody`/`ProposedAction`/... types in
//! `src/protocol/types.ts` can represent every field. A Rust field the TS type
//! lacks makes that vector fail to round-trip on the TypeScript side — that is
//! the drift catch.
//!
//! ## Regenerating
//!
//! ```text
//! cargo test -p codypendent-protocol --test golden_vectors regenerate_vectors -- --ignored
//! ```
//!
//! Run this whenever a wire type changes shape, review the diff under
//! `protocol-vectors/`, and commit it alongside the code change. CI never runs
//! the regenerator (it is `#[ignore]`d, and it WRITES files — never something
//! CI should do); CI instead runs the two checks below, which FAIL if the
//! committed vectors are stale or do not round-trip:
//!
//! * [`committed_vectors_match_current_protocol_types`] — a fresh regeneration
//!   must equal the committed bytes exactly. A Rust-side wire change that
//!   is not paired with a vector regeneration fails this test.
//! * [`committed_vectors_round_trip_through_their_rust_types`] — every
//!   committed entry, deserialized through its concrete Rust type and
//!   re-serialized, must reproduce itself exactly.
//!
//! ## Scope
//!
//! The Rust side enumerates comprehensively (every `CommandBody` and
//! `Payload` variant, the nested `PromotionAction` enum, and the newer
//! `blackboard`/`workflow`/`capabilities`/`input` modules) because that is
//! cheap and protects the Rust wire format on its own merits. The TypeScript
//! side only checks the subset the extension actually types (documented in
//! `protocol-vectors/README.md`) — the extension does not model every Rust
//! variant (e.g. it has no `Workflow`/`Blackboard`/`Document` subscriptions
//! yet), and that is an intentional, bounded gap, not drift.

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;

use codypendent_protocol::{Actor, ApprovalId, ArtifactId, ChangeSetId, ClientId, CommandId};
use codypendent_protocol::{
    AgentId, CorrelationId, DaemonInstanceId, DocumentId, ModelId, RunId, SessionId, UserId,
    WorkspaceId,
};
use codypendent_protocol::{
    AgentMode, ApprovalDecision, ApprovalScope, ArtifactRef, AudioArtifact, BlackboardItemView,
    BudgetDimension, Catchup, ClientCapabilities, ClientHello, ClientRole, CodypendentError,
    Command, CommandBody, DaemonStatus, DataClassification, Diagnostic, DiagnosticSeverity,
    DiffRequest, DirtyBufferDigest, DocumentEditLease, DocumentLeaseGrant, DocumentMutation,
    DocumentSync, EditorSelection, EventBody, GitHubRefKind, GitHubReference, IdeContextUpdate,
    IdeRequest, ImageArtifact, ImageRegion, InputBlock, InputEnvelope, InputSource, Location,
    ModelObservation, OffDevicePolicy, Payload, Position, PromotionAction, ProposedAction,
    ProtocolError, PublishTarget, Range, ResumeToken, Risk, RiskLevel, RunDisposition, RunState,
    ScopeLevel, ServerHello, SessionEvent, SessionProjection, SourceProvenance, Subscription,
    SuggestionInput, SymbolRef, TextEdit, ToolOutcome, Transcript, TranscriptionMode, UserAction,
    WorkflowEvent, WorkflowNodeState, WorkflowNodeView, WorkflowRunPhase, WorkflowRunSnapshot,
    WorkspaceEdit, PROTOCOL_V1,
};

// ---------------------------------------------------------------------------
// Sentinel builders: fixed, readable, non-random. Every "kind" of id gets a
// distinct hex prefix so a reader can tell at a glance which domain an id in a
// vector belongs to (e.g. every session id here reads `2000...1`).
// ---------------------------------------------------------------------------

fn workspace_id() -> WorkspaceId {
    "10000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn session_id() -> SessionId {
    "20000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn run_id() -> RunId {
    "30000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn command_id() -> CommandId {
    "40000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn approval_id() -> ApprovalId {
    "50000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn client_id() -> ClientId {
    "60000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn document_id() -> DocumentId {
    "70000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn artifact_id() -> ArtifactId {
    "80000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn changeset_id() -> ChangeSetId {
    "90000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn daemon_instance_id() -> DaemonInstanceId {
    "a0000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn correlation_id() -> CorrelationId {
    "b0000000-0000-0000-0000-000000000001".parse().unwrap()
}
fn agent_id() -> AgentId {
    "c0000000-0000-0000-0000-000000000001".parse().unwrap()
}

/// A fixed instant — never `Utc::now()` — so every timestamp in the vector set
/// is byte-for-byte stable across regenerations.
fn sentinel_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn artifact_ref() -> ArtifactRef {
    ArtifactRef {
        id: artifact_id(),
        media_type: "text/x-diff".to_string(),
        byte_length: 128,
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85".to_string(),
        sensitivity: DataClassification::Internal,
    }
}

// ---------------------------------------------------------------------------
// Vector / manifest plumbing
// ---------------------------------------------------------------------------

/// One named wire-type instance: its serialized JSON, plus a way to prove that
/// JSON round-trips through its own concrete Rust type (deserialize ->
/// re-serialize -> identical). `round_trip` is a plain function pointer, not a
/// closure with captures — a generic fn's body referencing only its type
/// parameter compiles to a non-capturing closure, which coerces to `fn`.
struct Vector {
    name: &'static str,
    value: Value,
    round_trip: fn(&Value) -> Value,
}

fn vec_of<T>(name: &'static str, instance: T) -> Vector
where
    T: Serialize + DeserializeOwned,
{
    let value = serde_json::to_value(&instance)
        .unwrap_or_else(|e| panic!("{name}: failed to serialize: {e}"));
    Vector {
        name,
        value,
        round_trip: |v: &Value| {
            let parsed: T = serde_json::from_value(v.clone())
                .unwrap_or_else(|e| panic!("failed to deserialize: {e}"));
            serde_json::to_value(&parsed).expect("re-serialize")
        },
    }
}

/// Build the sorted JSON object for one manifest file. Panics on a duplicate
/// vector name within the file — a silent `Map` overwrite would otherwise drop
/// a vector without any signal.
fn manifest_value(vectors: &[Vector]) -> Value {
    let mut map = serde_json::Map::new();
    for v in vectors {
        let previous = map.insert(v.name.to_string(), v.value.clone());
        assert!(
            previous.is_none(),
            "duplicate vector name {:?} — every vector name must be unique within its file",
            v.name
        );
    }
    Value::Object(map)
}

/// Pretty-print with a trailing newline (a normal committed text file).
fn render(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("pretty-print vectors");
    text.push('\n');
    text
}

/// `<repo-root>/protocol-vectors` — resolved from `CARGO_MANIFEST_DIR`
/// (`crates/protocol`) so it works regardless of the caller's working
/// directory.
fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("protocol-vectors")
}

// ---------------------------------------------------------------------------
// command.rs: CommandBody, PromotionAction
// ---------------------------------------------------------------------------

fn command_vectors() -> Vec<Vector> {
    vec![
        vec_of(
            "CommandBody_CreateSession",
            CommandBody::CreateSession {
                workspace: workspace_id(),
                title: "fix the failing test".to_string(),
            },
        ),
        vec_of(
            "CommandBody_AttachSession",
            CommandBody::AttachSession {
                session_id: session_id(),
                last_seen_sequence: Some(42),
                subscriptions: vec![Subscription::SessionSummary, Subscription::AgentActivity],
                requested_role: ClientRole::Approver,
            },
        ),
        vec_of(
            "CommandBody_SubmitUserInput",
            CommandBody::SubmitUserInput {
                session_id: session_id(),
                text: "try again".to_string(),
                mode: AgentMode::Build,
            },
        ),
        vec_of(
            "CommandBody_StartRun",
            CommandBody::StartRun {
                session_id: session_id(),
                objective: "diagnose the failing test".to_string(),
                mode: AgentMode::Build,
                repository: Some("/home/user/project".to_string()),
            },
        ),
        vec_of(
            "CommandBody_ResolveApproval",
            CommandBody::ResolveApproval {
                approval_id: approval_id(),
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Once,
            },
        ),
        vec_of(
            "CommandBody_CancelRun",
            CommandBody::CancelRun { run_id: run_id() },
        ),
        vec_of(
            "CommandBody_PauseRun",
            CommandBody::PauseRun { run_id: run_id() },
        ),
        vec_of(
            "CommandBody_ResumeRun",
            CommandBody::ResumeRun { run_id: run_id() },
        ),
        vec_of(
            "CommandBody_QueueSteering",
            CommandBody::QueueSteering {
                run_id: run_id(),
                text: "focus on the parser".to_string(),
            },
        ),
        vec_of(
            "CommandBody_UpdateIdeContext",
            CommandBody::UpdateIdeContext {
                session_id: session_id(),
                update: IdeContextUpdate {
                    active_file: Some("src/lib.rs".to_string()),
                    selection: Some(EditorSelection {
                        path: "src/lib.rs".to_string(),
                        range: Range {
                            start: Position {
                                line: 1,
                                character: 0,
                            },
                            end: Position {
                                line: 2,
                                character: 4,
                            },
                        },
                    }),
                    open_files: vec!["src/lib.rs".to_string(), "Cargo.toml".to_string()],
                    dirty_buffers: vec![DirtyBufferDigest {
                        path: "src/lib.rs".to_string(),
                        sha256: "deadbeef".to_string(),
                        byte_length: 12,
                    }],
                    diagnostics_revision: 7,
                },
            },
        ),
        vec_of(
            "CommandBody_MutateDocument",
            CommandBody::MutateDocument {
                document_id: document_id(),
                mutation: DocumentMutation::EditText {
                    block_id: "b1".to_string(),
                    position: 0,
                    delete_len: 0,
                    insert: "hello".to_string(),
                },
            },
        ),
        vec_of(
            "CommandBody_AcquireDocumentLease",
            CommandBody::AcquireDocumentLease {
                lease: DocumentEditLease {
                    document_id: document_id(),
                    block_id: Some("b1".to_string()),
                },
                ttl_seconds: Some(300),
            },
        ),
        vec_of(
            "CommandBody_ReleaseDocumentLease",
            CommandBody::ReleaseDocumentLease {
                lease_id: "lease-1".to_string(),
            },
        ),
        vec_of(
            "CommandBody_PublishDocument",
            CommandBody::PublishDocument {
                document_id: document_id(),
                target: PublishTarget::RepositoryFile {
                    path: "docs/architecture.md".to_string(),
                },
            },
        ),
        vec_of(
            "CommandBody_StartWorkflow_inline_manifest",
            CommandBody::StartWorkflow {
                manifest: "schema_version: 1\nid: wf\nversion: 1\nsteps: []\n".to_string(),
                workflow_id: None,
                inputs: json!({ "pull_request": 42 }),
                repository: Some("/home/user/project".to_string()),
            },
        ),
        vec_of(
            "CommandBody_StartWorkflow_named_workflow",
            CommandBody::StartWorkflow {
                manifest: String::new(),
                workflow_id: Some("repair-github-check".to_string()),
                inputs: json!({ "pull_request": 42 }),
                repository: Some("/home/user/project".to_string()),
            },
        ),
        vec_of(
            "CommandBody_PauseWorkflow",
            CommandBody::PauseWorkflow {
                workflow_run_id: "wfrun-abc123".to_string(),
            },
        ),
        vec_of(
            "CommandBody_ResumeWorkflow",
            CommandBody::ResumeWorkflow {
                workflow_run_id: "wfrun-abc123".to_string(),
            },
        ),
        vec_of(
            "CommandBody_RetryWorkflowNode",
            CommandBody::RetryWorkflowNode {
                workflow_run_id: "wfrun-abc123".to_string(),
                node_id: "verify".to_string(),
            },
        ),
        vec_of(
            "CommandBody_CancelWorkflow",
            CommandBody::CancelWorkflow {
                workflow_run_id: "wfrun-abc123".to_string(),
            },
        ),
        vec_of(
            "CommandBody_ReadWorkflowRun",
            CommandBody::ReadWorkflowRun {
                workflow_run_id: "wfrun-abc123".to_string(),
            },
        ),
        vec_of(
            "CommandBody_ProposePromotion",
            CommandBody::ProposePromotion {
                kind: "router".to_string(),
                name: "tool-selection".to_string(),
                version: 12,
                requires_permission_review: false,
            },
        ),
        vec_of(
            "CommandBody_AdvancePromotion",
            CommandBody::AdvancePromotion {
                candidate_id: "cand-abc123".to_string(),
                action: PromotionAction::RunRegression { regressed: false },
            },
        ),
        vec_of(
            "CommandBody_ApprovePromotion",
            CommandBody::ApprovePromotion {
                candidate_id: "cand-abc123".to_string(),
            },
        ),
        vec_of(
            "CommandBody_RollbackPromotion",
            CommandBody::RollbackPromotion {
                candidate_id: "cand-abc123".to_string(),
            },
        ),
        vec_of(
            "CommandBody_ReadBlackboard",
            CommandBody::ReadBlackboard {
                workflow_run_id: "wfrun-abc123".to_string(),
                kind: Some("finding".to_string()),
                include_superseded: true,
            },
        ),
        vec_of(
            "PromotionAction_RunRegression",
            PromotionAction::RunRegression { regressed: false },
        ),
        vec_of("PromotionAction_StartShadow", PromotionAction::StartShadow),
        vec_of("PromotionAction_StartCanary", PromotionAction::StartCanary),
        vec_of(
            "PromotionAction_ObserveCanary",
            PromotionAction::ObserveCanary { regressed: true },
        ),
        vec_of(
            "PromotionAction_FinishCanary",
            PromotionAction::FinishCanary,
        ),
    ]
}

// ---------------------------------------------------------------------------
// envelope.rs: Payload, DaemonStatus, ProtocolError
// ---------------------------------------------------------------------------

fn envelope_vectors() -> Vec<Vector> {
    vec![
        vec_of("Payload_Ping", Payload::Ping),
        vec_of("Payload_Pong", Payload::Pong),
        vec_of("Payload_DaemonStatusRequest", Payload::DaemonStatusRequest),
        vec_of("Payload_Shutdown", Payload::Shutdown),
        vec_of("Payload_ShutdownAck", Payload::ShutdownAck),
        vec_of(
            "Payload_DaemonStatusResponse",
            Payload::DaemonStatusResponse(DaemonStatus {
                daemon_version: "0.1.0".to_string(),
                protocol_version: PROTOCOL_V1,
                instance_id: daemon_instance_id(),
                pid: 4242,
                started_at: sentinel_time(),
                uptime_seconds: 3600,
                boot_count: 1,
                database_path: "/home/user/.local/share/codypendent/codypendent.db".to_string(),
                socket_path: "/home/user/.local/share/codypendent/run/daemon.sock".to_string(),
                session_count: 2,
            }),
        ),
        vec_of(
            "Payload_Error",
            Payload::Error(ProtocolError {
                code: "protocol.unsupported-payload".to_string(),
                message: "unknown payload tag".to_string(),
                retryable: false,
            }),
        ),
        vec_of(
            "Payload_ClientHello",
            Payload::ClientHello(ClientHello {
                client_name: "codypendent-vscode".to_string(),
                client_version: "0.1.0".to_string(),
                supported_protocols: vec![PROTOCOL_V1],
                capabilities: ClientCapabilities {
                    rich_text: true,
                    image_display: false,
                    audio_capture: false,
                    editor_mutations: true,
                    diff_view: true,
                    mouse: true,
                    unicode: true,
                    true_color: true,
                },
                resume_token: Some(ResumeToken("resume-abc".to_string())),
            }),
        ),
        vec_of(
            "Payload_ServerHello",
            Payload::ServerHello(ServerHello {
                selected_protocol: PROTOCOL_V1,
                daemon_version: "0.1.0".to_string(),
                daemon_instance: daemon_instance_id(),
                heartbeat_interval_ms: 15_000,
                resume_token: Some(ResumeToken("opaque".to_string())),
            }),
        ),
        vec_of(
            "Payload_Command",
            Payload::Command(Command {
                command_id: command_id(),
                idempotency_key: "idem-1".to_string(),
                expected_revision: Some(7),
                body: CommandBody::StartRun {
                    session_id: session_id(),
                    objective: "diagnose the failing test".to_string(),
                    mode: AgentMode::Build,
                    repository: Some("/home/user/project".to_string()),
                },
            }),
        ),
        vec_of(
            "Payload_CommandAccepted",
            Payload::CommandAccepted {
                command_id: command_id(),
                sequence: Some(7),
                created_run: Some(run_id()),
            },
        ),
        vec_of(
            "Payload_CommandRejected",
            Payload::CommandRejected(CodypendentError {
                code: "policy.write-denied".to_string(),
                message: "writes are denied in Explore mode".to_string(),
                retryable: false,
                user_action: Some(UserAction::AdjustPolicy),
                details: json!({ "path": "/etc/passwd" }),
                correlation_id: correlation_id(),
            }),
        ),
        vec_of(
            "Payload_DocumentLeaseGranted",
            Payload::DocumentLeaseGranted {
                command_id: command_id(),
                grant: DocumentLeaseGrant {
                    lease_id: "lease-9".to_string(),
                    document_id: document_id(),
                    block_id: Some("b3".to_string()),
                    expires_at: sentinel_time(),
                },
            },
        ),
        vec_of(
            "Payload_WorkflowRunStarted",
            Payload::WorkflowRunStarted {
                command_id: command_id(),
                workflow_run_id: "wfrun-abc123".to_string(),
            },
        ),
        vec_of(
            "Payload_DocumentPublishRequested",
            Payload::DocumentPublishRequested {
                command_id: command_id(),
                approval_id: approval_id(),
                target: "repository file docs/architecture.md".to_string(),
                changed_files: vec!["docs/architecture.md".to_string()],
                git_action:
                    "write docs/architecture.md in the working tree (approval-gated change set)"
                        .to_string(),
            },
        ),
        vec_of(
            "Payload_PromotionProposed",
            Payload::PromotionProposed {
                command_id: command_id(),
                candidate_id: "cand-abc123".to_string(),
            },
        ),
        vec_of(
            "Payload_BlackboardItems",
            Payload::BlackboardItems {
                command_id: command_id(),
                items: vec![blackboard_item()],
            },
        ),
        vec_of(
            "Payload_BlackboardPosted",
            Payload::BlackboardPosted(blackboard_item()),
        ),
        vec_of(
            "Payload_WorkflowRunSnapshot",
            Payload::WorkflowRunSnapshot {
                command_id: command_id(),
                snapshot: WorkflowRunSnapshot {
                    workflow_run_id: "wfrun-abc123".to_string(),
                    phase: WorkflowRunPhase::Running,
                    nodes: vec![workflow_node_view()],
                },
            },
        ),
        vec_of(
            "Payload_WorkflowEvent",
            Payload::WorkflowEvent {
                event: WorkflowEvent::NodeTransitioned(workflow_node_view()),
            },
        ),
        vec_of(
            // The S1 case at the envelope level: an ApprovalRequested event
            // carrying ExecuteCommand with its environment + cwd populated —
            // exactly the shape the extension's approval card must render.
            "Payload_Event_ApprovalRequestedExecuteCommand",
            Payload::Event(execute_command_approval_event()),
        ),
        vec_of(
            "Payload_DocumentSync",
            Payload::DocumentSync(DocumentSync {
                document_id: document_id(),
                revision: 5,
                update: vec![1, 2, 3, 255],
            }),
        ),
        vec_of(
            "Payload_Catchup",
            Payload::Catchup {
                catchup: Catchup::Events {
                    from: 1,
                    through: 3,
                    events: vec![],
                },
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// events.rs: Actor, EventBody
// ---------------------------------------------------------------------------

/// The S1-closing vector: `ApprovalRequested` carrying
/// `ProposedAction::ExecuteCommand` with a populated `environment` and `cwd` —
/// the exact fields the extension's approval card used to omit.
fn execute_command_approval_event() -> SessionEvent {
    SessionEvent {
        sequence: 9,
        occurred_at: sentinel_time(),
        causation_id: Some(command_id()),
        correlation_id: Some(correlation_id()),
        actor: Actor::Agent {
            agent_id: agent_id(),
            run_id: run_id(),
            model: ModelId("claude-sonnet-5".to_string()),
        },
        body: EventBody::ApprovalRequested {
            approval_id: approval_id(),
            action: ProposedAction::ExecuteCommand {
                program: "cargo".to_string(),
                args: vec!["test".to_string(), "--all-features".to_string()],
                environment: vec![
                    ("RUST_BACKTRACE".to_string(), "1".to_string()),
                    ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ],
                cwd: Some("/home/user/project".to_string()),
            },
            risk: Risk {
                level: RiskLevel::High,
                reasons: vec![
                    "writes outside the worktree".to_string(),
                    "executes an external process".to_string(),
                ],
            },
        },
    }
}

fn event_with(body: EventBody) -> SessionEvent {
    SessionEvent {
        sequence: 9,
        occurred_at: sentinel_time(),
        causation_id: Some(command_id()),
        correlation_id: Some(correlation_id()),
        actor: Actor::Agent {
            agent_id: agent_id(),
            run_id: run_id(),
            model: ModelId("claude-sonnet-5".to_string()),
        },
        body,
    }
}

fn events_vectors() -> Vec<Vector> {
    vec![
        vec_of(
            "Actor_Human",
            Actor::Human {
                user_id: UserId("dana".to_string()),
            },
        ),
        vec_of(
            "Actor_Agent",
            Actor::Agent {
                agent_id: agent_id(),
                run_id: run_id(),
                model: ModelId("claude-sonnet-5".to_string()),
            },
        ),
        vec_of(
            "Actor_Client",
            Actor::Client {
                client_id: client_id(),
            },
        ),
        vec_of(
            "Actor_Integration",
            Actor::Integration {
                integration_id: "github-app".to_string(),
            },
        ),
        vec_of("Actor_System", Actor::System),
        vec_of(
            "EventBody_SessionCreated",
            event_with(EventBody::SessionCreated {
                title: "fixture session".to_string(),
            }),
        ),
        vec_of(
            "EventBody_NoteAppended",
            event_with(EventBody::NoteAppended {
                text: "first note".to_string(),
                run_id: Some(run_id()),
            }),
        ),
        vec_of(
            "EventBody_SessionClosed",
            event_with(EventBody::SessionClosed),
        ),
        vec_of(
            "EventBody_RunStarted",
            event_with(EventBody::RunStarted {
                run_id: run_id(),
                objective: "diagnose".to_string(),
                mode: AgentMode::Build,
            }),
        ),
        vec_of(
            "EventBody_RunStateChanged",
            event_with(EventBody::RunStateChanged {
                run_id: run_id(),
                state: RunState::WaitingForApproval,
            }),
        ),
        vec_of(
            "EventBody_ModelStreamDelta",
            event_with(EventBody::ModelStreamDelta {
                run_id: run_id(),
                text: "thinking...".to_string(),
            }),
        ),
        vec_of(
            "EventBody_ToolProposed",
            event_with(EventBody::ToolProposed {
                run_id: run_id(),
                approval_id: approval_id(),
                action: ProposedAction::ReadFiles {
                    paths: vec!["src/lib.rs".to_string(), "Cargo.toml".to_string()],
                },
            }),
        ),
        vec_of(
            "EventBody_ToolStarted",
            event_with(EventBody::ToolStarted {
                run_id: run_id(),
                tool: "shell.run".to_string(),
                args_digest: "abc123".to_string(),
            }),
        ),
        vec_of(
            "EventBody_ToolCompleted",
            event_with(EventBody::ToolCompleted {
                run_id: run_id(),
                tool: "shell.run".to_string(),
                outcome: ToolOutcome::Succeeded,
                artifact: Some(artifact_ref()),
            }),
        ),
        vec_of(
            "EventBody_PatchProposed",
            event_with(EventBody::PatchProposed {
                run_id: run_id(),
                changeset_id: changeset_id(),
                artifact: artifact_ref(),
            }),
        ),
        vec_of(
            // THE S1 VECTOR. See `execute_command_approval_event` docs.
            "EventBody_ApprovalRequested_ExecuteCommand",
            execute_command_approval_event(),
        ),
        vec_of(
            "EventBody_ApprovalResolved",
            event_with(EventBody::ApprovalResolved {
                approval_id: approval_id(),
                decision: ApprovalDecision::Approve,
            }),
        ),
        vec_of(
            "EventBody_SteeringQueued",
            event_with(EventBody::SteeringQueued { run_id: run_id() }),
        ),
        vec_of(
            "EventBody_SteeringApplied",
            event_with(EventBody::SteeringApplied { run_id: run_id() }),
        ),
        vec_of(
            "EventBody_BudgetWarning",
            event_with(EventBody::BudgetWarning {
                run_id: run_id(),
                dimension: BudgetDimension::Tokens,
                used: 90_000,
                limit: 100_000,
            }),
        ),
        vec_of(
            "EventBody_RunCompleted",
            event_with(EventBody::RunCompleted {
                run_id: run_id(),
                disposition: RunDisposition::Completed {
                    summary: Some("fixed".to_string()),
                },
                chronicle: artifact_ref(),
            }),
        ),
        vec_of(
            "EventBody_ClientPresenceChanged",
            event_with(EventBody::ClientPresenceChanged {
                client_id: client_id(),
                role: ClientRole::Contributor,
                present: true,
            }),
        ),
    ]
}

// ---------------------------------------------------------------------------
// run.rs: AgentMode, RunState, RunDisposition, ProposedAction, Risk,
// RiskLevel, ApprovalDecision, ApprovalScope, BudgetDimension, ToolOutcome
// ---------------------------------------------------------------------------

fn run_vectors() -> Vec<Vector> {
    vec![
        vec_of("AgentMode_Ask", AgentMode::Ask),
        vec_of("AgentMode_Explore", AgentMode::Explore),
        vec_of("AgentMode_Plan", AgentMode::Plan),
        vec_of("AgentMode_Build", AgentMode::Build),
        vec_of("AgentMode_Review", AgentMode::Review),
        vec_of("RunState_Queued", RunState::Queued),
        vec_of("RunState_Preparing", RunState::Preparing),
        vec_of("RunState_Running", RunState::Running),
        vec_of("RunState_WaitingForApproval", RunState::WaitingForApproval),
        vec_of(
            "RunState_WaitingForUserInput",
            RunState::WaitingForUserInput,
        ),
        vec_of("RunState_Paused", RunState::Paused),
        vec_of("RunState_Recovering", RunState::Recovering),
        vec_of("RunState_Completed", RunState::Completed),
        vec_of("RunState_Failed", RunState::Failed),
        vec_of("RunState_Cancelled", RunState::Cancelled),
        vec_of(
            "RunDisposition_Completed",
            RunDisposition::Completed {
                summary: Some("fixed the parser".to_string()),
            },
        ),
        vec_of(
            "RunDisposition_Failed",
            RunDisposition::Failed {
                reason: "daemon restart".to_string(),
            },
        ),
        vec_of(
            "RunDisposition_Cancelled",
            RunDisposition::Cancelled {
                reason: Some("superseded by a newer run".to_string()),
            },
        ),
        vec_of(
            "ProposedAction_ReadFiles",
            ProposedAction::ReadFiles {
                paths: vec!["src/lib.rs".to_string(), "Cargo.toml".to_string()],
            },
        ),
        vec_of(
            "ProposedAction_WritePatch",
            ProposedAction::WritePatch {
                patch: artifact_id(),
            },
        ),
        vec_of(
            // The S1 case, standalone: ExecuteCommand alone (see also the
            // envelope- and event-level vectors above, which carry the exact
            // same shape inside a realistic frame).
            "ProposedAction_ExecuteCommand",
            ProposedAction::ExecuteCommand {
                program: "cargo".to_string(),
                args: vec!["test".to_string(), "--all-features".to_string()],
                environment: vec![
                    ("RUST_BACKTRACE".to_string(), "1".to_string()),
                    ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ],
                cwd: Some("/home/user/project".to_string()),
            },
        ),
        vec_of(
            "ProposedAction_NetworkRequest",
            ProposedAction::NetworkRequest {
                destination: "https://api.github.com".to_string(),
            },
        ),
        vec_of(
            "ProposedAction_GitCommit",
            ProposedAction::GitCommit {
                repository: "/home/user/project".to_string(),
            },
        ),
        vec_of(
            "ProposedAction_GitPush",
            ProposedAction::GitPush {
                remote: "origin".to_string(),
                branch: "feature/fix-parser".to_string(),
            },
        ),
        vec_of(
            "ProposedAction_GitHubMutation",
            ProposedAction::GitHubMutation {
                repository: "octocat/hello-world".to_string(),
                summary: "create draft PR on octocat/hello-world".to_string(),
            },
        ),
        vec_of(
            "ProposedAction_PublishDocument",
            ProposedAction::PublishDocument {
                document_id: document_id(),
                target: "repository file docs/architecture.md".to_string(),
                changed_files: vec!["docs/architecture.md".to_string()],
                git_action:
                    "write docs/architecture.md in the working tree (approval-gated change set)"
                        .to_string(),
            },
        ),
        vec_of(
            "ProposedAction_BlackboardPost",
            ProposedAction::BlackboardPost {
                workflow_run_id: "wfrun-abc123".to_string(),
                kind: "finding".to_string(),
            },
        ),
        vec_of(
            "ProposedAction_BlackboardQuery",
            ProposedAction::BlackboardQuery {
                workflow_run_id: "wfrun-abc123".to_string(),
            },
        ),
        vec_of(
            "Risk",
            Risk {
                level: RiskLevel::Medium,
                reasons: vec!["writes outside the worktree".to_string()],
            },
        ),
        vec_of("RiskLevel_Low", RiskLevel::Low),
        vec_of("RiskLevel_Medium", RiskLevel::Medium),
        vec_of("RiskLevel_High", RiskLevel::High),
        vec_of("RiskLevel_Critical", RiskLevel::Critical),
        vec_of("ApprovalDecision_Approve", ApprovalDecision::Approve),
        vec_of("ApprovalDecision_Reject", ApprovalDecision::Reject),
        vec_of("ApprovalScope_Once", ApprovalScope::Once),
        vec_of("ApprovalScope_Run", ApprovalScope::Run),
        vec_of("ApprovalScope_Pattern", ApprovalScope::Pattern),
        vec_of("ApprovalScope_Repository", ApprovalScope::Repository),
        vec_of("BudgetDimension_Tokens", BudgetDimension::Tokens),
        vec_of("BudgetDimension_Cost", BudgetDimension::Cost),
        vec_of("BudgetDimension_WallClock", BudgetDimension::WallClock),
        vec_of("BudgetDimension_ToolCalls", BudgetDimension::ToolCalls),
        vec_of("ToolOutcome_Succeeded", ToolOutcome::Succeeded),
        vec_of(
            "ToolOutcome_Failed",
            ToolOutcome::Failed {
                message: "exit 1".to_string(),
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// handshake.rs: ClientHello, ServerHello, ClientRole, Subscription
// ---------------------------------------------------------------------------

fn handshake_vectors() -> Vec<Vector> {
    vec![
        vec_of(
            "ClientHello",
            ClientHello {
                client_name: "codypendent-tui".to_string(),
                client_version: "0.1.0".to_string(),
                supported_protocols: vec![PROTOCOL_V1],
                capabilities: ClientCapabilities {
                    rich_text: true,
                    image_display: false,
                    audio_capture: false,
                    editor_mutations: false,
                    diff_view: true,
                    mouse: true,
                    unicode: true,
                    true_color: true,
                },
                resume_token: Some(ResumeToken("opaque-token".to_string())),
            },
        ),
        vec_of(
            "ServerHello",
            ServerHello {
                selected_protocol: PROTOCOL_V1,
                daemon_version: "0.1.0".to_string(),
                daemon_instance: daemon_instance_id(),
                heartbeat_interval_ms: 15_000,
                resume_token: Some(ResumeToken("opaque".to_string())),
            },
        ),
        vec_of("ClientRole_Observer", ClientRole::Observer),
        vec_of("ClientRole_Contributor", ClientRole::Contributor),
        vec_of("ClientRole_Controller", ClientRole::Controller),
        vec_of("ClientRole_Approver", ClientRole::Approver),
        vec_of("Subscription_SessionSummary", Subscription::SessionSummary),
        vec_of(
            "Subscription_RunTrace",
            Subscription::RunTrace { run_id: run_id() },
        ),
        vec_of("Subscription_AgentActivity", Subscription::AgentActivity),
        vec_of(
            "Subscription_RepositoryStatus",
            Subscription::RepositoryStatus,
        ),
        vec_of("Subscription_BudgetState", Subscription::BudgetState),
        vec_of(
            "Subscription_Document",
            Subscription::Document {
                document_id: document_id(),
            },
        ),
        vec_of(
            "Subscription_Blackboard",
            Subscription::Blackboard {
                workflow_run_id: "wfrun-abc123".to_string(),
            },
        ),
        vec_of(
            "Subscription_Workflow",
            Subscription::Workflow {
                workflow_run_id: "wfrun-abc123".to_string(),
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// catchup.rs: Catchup, SessionProjection
// ---------------------------------------------------------------------------

fn catchup_vectors() -> Vec<Vector> {
    vec![
        vec_of(
            "Catchup_Events",
            Catchup::Events {
                from: 1,
                through: 1,
                events: vec![event_with(EventBody::SessionClosed)],
            },
        ),
        vec_of(
            "Catchup_Snapshot",
            Catchup::Snapshot {
                through: 512,
                projection: SessionProjection {
                    session_id: session_id(),
                    title: "long session".to_string(),
                    last_sequence: 512,
                    active_runs: vec![run_id()],
                    closed: false,
                },
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// artifact.rs: ArtifactRef, DataClassification
// ---------------------------------------------------------------------------

fn artifact_vectors() -> Vec<Vector> {
    vec![
        vec_of("ArtifactRef", artifact_ref()),
        vec_of("DataClassification_Public", DataClassification::Public),
        vec_of("DataClassification_Internal", DataClassification::Internal),
        vec_of(
            "DataClassification_Confidential",
            DataClassification::Confidential,
        ),
        vec_of("DataClassification_Secret", DataClassification::Secret),
    ]
}

// ---------------------------------------------------------------------------
// error.rs: CodypendentError, ProtocolError, UserAction
// ---------------------------------------------------------------------------

fn error_vectors() -> Vec<Vector> {
    vec![
        vec_of(
            "CodypendentError",
            CodypendentError {
                code: "policy.write-denied".to_string(),
                message: "writes are denied in Explore mode".to_string(),
                retryable: false,
                user_action: Some(UserAction::AdjustPolicy),
                details: json!({ "path": "/etc/passwd" }),
                correlation_id: correlation_id(),
            },
        ),
        vec_of(
            "ProtocolError",
            ProtocolError {
                code: "protocol.unsupported-payload".to_string(),
                message: "unknown payload tag".to_string(),
                retryable: false,
            },
        ),
        vec_of("UserAction_Retry", UserAction::Retry),
        vec_of("UserAction_Reauthenticate", UserAction::Reauthenticate),
        vec_of("UserAction_GrantApproval", UserAction::GrantApproval),
        vec_of("UserAction_AdjustPolicy", UserAction::AdjustPolicy),
        vec_of("UserAction_ReconfigureModel", UserAction::ReconfigureModel),
        vec_of("UserAction_ContactSupport", UserAction::ContactSupport),
    ]
}

// ---------------------------------------------------------------------------
// ide.rs
// ---------------------------------------------------------------------------

fn ide_vectors() -> Vec<Vector> {
    let range = Range {
        start: Position {
            line: 1,
            character: 0,
        },
        end: Position {
            line: 2,
            character: 4,
        },
    };
    vec![
        vec_of(
            "Position",
            Position {
                line: 1,
                character: 0,
            },
        ),
        vec_of("Range", range),
        vec_of(
            "EditorSelection",
            EditorSelection {
                path: "src/lib.rs".to_string(),
                range,
            },
        ),
        vec_of(
            "DirtyBufferDigest",
            DirtyBufferDigest {
                path: "src/lib.rs".to_string(),
                sha256: "abc123".to_string(),
                byte_length: 42,
            },
        ),
        vec_of(
            "IdeContextUpdate",
            IdeContextUpdate {
                active_file: Some("src/lib.rs".to_string()),
                selection: Some(EditorSelection {
                    path: "src/lib.rs".to_string(),
                    range,
                }),
                open_files: vec!["src/lib.rs".to_string(), "Cargo.toml".to_string()],
                dirty_buffers: vec![DirtyBufferDigest {
                    path: "src/lib.rs".to_string(),
                    sha256: "abc123".to_string(),
                    byte_length: 42,
                }],
                diagnostics_revision: 7,
            },
        ),
        vec_of(
            "Location",
            Location {
                path: "src/lib.rs".to_string(),
                range: None,
            },
        ),
        vec_of(
            "TextEdit",
            TextEdit {
                path: "src/lib.rs".to_string(),
                range,
                new_text: "fn fixed() {}".to_string(),
            },
        ),
        vec_of(
            "WorkspaceEdit",
            WorkspaceEdit {
                edits: vec![TextEdit {
                    path: "src/lib.rs".to_string(),
                    range,
                    new_text: "fn fixed() {}".to_string(),
                }],
            },
        ),
        vec_of(
            "DiffRequest",
            DiffRequest {
                title: "Codypendent change set c1".to_string(),
                left_label: "HEAD".to_string(),
                right_label: "proposed".to_string(),
                left: "fn broken() {}".to_string(),
                right: "fn fixed() {}".to_string(),
            },
        ),
        vec_of(
            "IdeRequest_ApplyEdit",
            IdeRequest::ApplyEdit {
                edit: WorkspaceEdit {
                    edits: vec![TextEdit {
                        path: "src/lib.rs".to_string(),
                        range,
                        new_text: "fn fixed() {}".to_string(),
                    }],
                },
            },
        ),
        vec_of(
            "IdeRequest_RevealLocation",
            IdeRequest::RevealLocation {
                location: Location {
                    path: "src/lib.rs".to_string(),
                    range: None,
                },
            },
        ),
        vec_of(
            "IdeRequest_ShowDiff",
            IdeRequest::ShowDiff {
                request: DiffRequest {
                    title: "Codypendent change set c1".to_string(),
                    left_label: "HEAD".to_string(),
                    right_label: "proposed".to_string(),
                    left: "fn broken() {}".to_string(),
                    right: "fn fixed() {}".to_string(),
                },
            },
        ),
        vec_of("DiagnosticSeverity_Error", DiagnosticSeverity::Error),
        vec_of("DiagnosticSeverity_Warning", DiagnosticSeverity::Warning),
        vec_of(
            "DiagnosticSeverity_Information",
            DiagnosticSeverity::Information,
        ),
        vec_of("DiagnosticSeverity_Hint", DiagnosticSeverity::Hint),
        vec_of(
            "Diagnostic",
            Diagnostic {
                path: "src/lib.rs".to_string(),
                range,
                severity: DiagnosticSeverity::Error,
                message: "mismatched types".to_string(),
                source: Some("rustc".to_string()),
            },
        ),
        vec_of(
            "SourceProvenance_CommittedAt",
            SourceProvenance::CommittedAt {
                revision: "a1b2c3d".to_string(),
            },
        ),
        vec_of("SourceProvenance_Filesystem", SourceProvenance::Filesystem),
        vec_of(
            "SourceProvenance_UnsavedIdeBuffer",
            SourceProvenance::UnsavedIdeBuffer,
        ),
        vec_of(
            "SourceProvenance_GeneratedPatch",
            SourceProvenance::GeneratedPatch,
        ),
        vec_of(
            "SourceProvenance_AgentWorktree",
            SourceProvenance::AgentWorktree,
        ),
    ]
}

// ---------------------------------------------------------------------------
// document.rs (Rust-only: not modeled in the extension yet)
// ---------------------------------------------------------------------------

fn document_vectors() -> Vec<Vector> {
    vec![
        vec_of(
            "DocumentMutation_Insert",
            DocumentMutation::Insert {
                index: 0,
                block_id: "b1".to_string(),
                content: json!({ "type": "paragraph", "text": "hi" }),
            },
        ),
        vec_of(
            "DocumentMutation_Delete",
            DocumentMutation::Delete {
                block_id: "b1".to_string(),
            },
        ),
        vec_of(
            "DocumentMutation_EditText",
            DocumentMutation::EditText {
                block_id: "b1".to_string(),
                position: 2,
                delete_len: 1,
                insert: "x".to_string(),
            },
        ),
        vec_of(
            "DocumentMutation_Annotate",
            DocumentMutation::Annotate {
                suggestion: SuggestionInput {
                    block_id: "b1".to_string(),
                    range_start: 0,
                    range_end: 3,
                    replacement: "new".to_string(),
                    rationale: Some("clearer".to_string()),
                },
            },
        ),
        vec_of(
            "DocumentMutation_AcceptSuggestion",
            DocumentMutation::AcceptSuggestion {
                suggestion_id: "s1".to_string(),
            },
        ),
        vec_of(
            "DocumentMutation_RejectSuggestion",
            DocumentMutation::RejectSuggestion {
                suggestion_id: "s1".to_string(),
            },
        ),
        vec_of(
            "PublishTarget_RepositoryFile",
            PublishTarget::RepositoryFile {
                path: "docs/architecture.md".to_string(),
            },
        ),
        vec_of(
            "PublishTarget_DocsBranchCommit",
            PublishTarget::DocsBranchCommit {
                branch: "docs/publish".to_string(),
                path: "docs/architecture.md".to_string(),
            },
        ),
        vec_of(
            "PublishTarget_DocumentationPr",
            PublishTarget::DocumentationPr {
                branch: "docs/publish".to_string(),
                path: "docs/architecture.md".to_string(),
                title: "Publish: Architecture".to_string(),
            },
        ),
        vec_of(
            "DocumentSync",
            DocumentSync {
                document_id: document_id(),
                revision: 7,
                update: vec![1, 2, 3, 255],
            },
        ),
        vec_of(
            "DocumentEditLease",
            DocumentEditLease {
                document_id: document_id(),
                block_id: Some("b1".to_string()),
            },
        ),
        vec_of(
            "DocumentLeaseGrant",
            DocumentLeaseGrant {
                lease_id: "lease-1".to_string(),
                document_id: document_id(),
                block_id: Some("b1".to_string()),
                expires_at: sentinel_time(),
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// blackboard.rs (Rust-only: not modeled in the extension yet)
// ---------------------------------------------------------------------------

fn blackboard_item() -> BlackboardItemView {
    BlackboardItemView {
        id: "0192-item".to_string(),
        workflow_run_id: "wfrun-abc".to_string(),
        kind: "finding".to_string(),
        payload: json!({ "summary": "the parser drops trailing commas" }),
        author: json!({ "role": "investigator", "node_id": "diagnose" }),
        confidence: Some(0.8),
        evidence: vec![json!({ "path": "src/parse.rs", "line": 42 })],
        revision: 1,
        superseded_by: None,
    }
}

fn blackboard_vectors() -> Vec<Vector> {
    vec![vec_of("BlackboardItemView", blackboard_item())]
}

// ---------------------------------------------------------------------------
// workflow.rs (Rust-only: not modeled in the extension yet)
// ---------------------------------------------------------------------------

fn workflow_node_view() -> WorkflowNodeView {
    WorkflowNodeView {
        workflow_run_id: "wfrun-abc".to_string(),
        node_id: "inspect".to_string(),
        state: WorkflowNodeState::Completed,
        attempt: 1,
        cost: Some(json!({ "wall_time_secs": 12, "tool_calls": 3 })),
        error: None,
        warnings: vec!["tool_calls at 4/5 (80%)".to_string()],
    }
}

fn workflow_vectors() -> Vec<Vector> {
    vec![
        vec_of("WorkflowNodeState_Pending", WorkflowNodeState::Pending),
        vec_of("WorkflowNodeState_Running", WorkflowNodeState::Running),
        vec_of(
            "WorkflowNodeState_WaitingApproval",
            WorkflowNodeState::WaitingApproval,
        ),
        vec_of("WorkflowNodeState_Blocked", WorkflowNodeState::Blocked),
        vec_of("WorkflowNodeState_Completed", WorkflowNodeState::Completed),
        vec_of("WorkflowNodeState_Failed", WorkflowNodeState::Failed),
        vec_of("WorkflowNodeState_Skipped", WorkflowNodeState::Skipped),
        vec_of("WorkflowRunPhase_Pending", WorkflowRunPhase::Pending),
        vec_of("WorkflowRunPhase_Running", WorkflowRunPhase::Running),
        vec_of("WorkflowRunPhase_Paused", WorkflowRunPhase::Paused),
        vec_of("WorkflowRunPhase_Completed", WorkflowRunPhase::Completed),
        vec_of("WorkflowRunPhase_Failed", WorkflowRunPhase::Failed),
        vec_of("WorkflowRunPhase_Cancelled", WorkflowRunPhase::Cancelled),
        vec_of("WorkflowNodeView", workflow_node_view()),
        vec_of(
            "WorkflowRunSnapshot",
            WorkflowRunSnapshot {
                workflow_run_id: "wfrun-abc".to_string(),
                phase: WorkflowRunPhase::Running,
                nodes: vec![workflow_node_view()],
            },
        ),
        vec_of(
            "WorkflowEvent_NodeTransitioned",
            WorkflowEvent::NodeTransitioned(workflow_node_view()),
        ),
        vec_of(
            "WorkflowEvent_RunPhaseChanged",
            WorkflowEvent::RunPhaseChanged {
                workflow_run_id: "wfrun-abc".to_string(),
                phase: WorkflowRunPhase::Cancelled,
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// capabilities.rs
// ---------------------------------------------------------------------------

fn capabilities_vectors() -> Vec<Vector> {
    vec![vec_of(
        "ClientCapabilities",
        ClientCapabilities {
            rich_text: true,
            image_display: false,
            audio_capture: false,
            editor_mutations: true,
            diff_view: true,
            mouse: true,
            unicode: true,
            true_color: true,
        },
    )]
}

// ---------------------------------------------------------------------------
// input.rs (Rust-only: not modeled in the extension yet)
// ---------------------------------------------------------------------------

fn input_vectors() -> Vec<Vector> {
    let image = ImageArtifact {
        original: ArtifactRef {
            id: artifact_id(),
            media_type: "image/png".to_string(),
            byte_length: 20_480,
            sha256: "1".repeat(64),
            sensitivity: DataClassification::Confidential,
        },
        extracted_text: Some(ArtifactRef {
            id: artifact_id(),
            media_type: "text/plain".to_string(),
            byte_length: 256,
            sha256: "2".repeat(64),
            sensitivity: DataClassification::Confidential,
        }),
        observations: vec![ModelObservation {
            text: "A terminal showing a failing test.".to_string(),
            model: Some(ModelId("claude-sonnet-5".to_string())),
        }],
        regions: vec![ImageRegion {
            label: Some("error message".to_string()),
            x: 10,
            y: 20,
            width: 300,
            height: 40,
        }],
        width: Some(1280),
        height: Some(720),
    };
    let audio = AudioArtifact {
        original: ArtifactRef {
            id: artifact_id(),
            media_type: "audio/wav".to_string(),
            byte_length: 4096,
            sha256: "3".repeat(64),
            sensitivity: DataClassification::Confidential,
        },
        transcript: Some(Transcript {
            text: "approve the patch".to_string(),
            mode: TranscriptionMode::Local,
            model: None,
            reviewed: true,
            source_audio: artifact_id(),
        }),
        duration_ms: Some(1500),
        sample_rate_hz: Some(16_000),
    };
    vec![
        vec_of("InputSource_Tui", InputSource::Tui),
        vec_of("InputSource_Ide", InputSource::Ide),
        vec_of("InputSource_Cli", InputSource::Cli),
        vec_of("InputSource_Web", InputSource::Web),
        vec_of("InputSource_Voice", InputSource::Voice),
        vec_of("ScopeLevel_System", ScopeLevel::System),
        vec_of("ScopeLevel_Organization", ScopeLevel::Organization),
        vec_of("ScopeLevel_User", ScopeLevel::User),
        vec_of("ScopeLevel_Workspace", ScopeLevel::Workspace),
        vec_of("ScopeLevel_Repository", ScopeLevel::Repository),
        vec_of("ScopeLevel_Branch", ScopeLevel::Branch),
        vec_of("ScopeLevel_Session", ScopeLevel::Session),
        vec_of("ScopeLevel_Task", ScopeLevel::Task),
        vec_of("TranscriptionMode_Local", TranscriptionMode::Local),
        vec_of("TranscriptionMode_Remote", TranscriptionMode::Remote),
        vec_of("GitHubRefKind_PullRequest", GitHubRefKind::PullRequest),
        vec_of("GitHubRefKind_Issue", GitHubRefKind::Issue),
        vec_of("GitHubRefKind_Commit", GitHubRefKind::Commit),
        vec_of("GitHubRefKind_Comment", GitHubRefKind::Comment),
        vec_of(
            "InputBlock_Text",
            InputBlock::Text {
                text: "approve the patch".to_string(),
            },
        ),
        vec_of("InputBlock_Audio", InputBlock::Audio(audio.clone())),
        vec_of("InputBlock_Image", InputBlock::Image(image.clone())),
        vec_of("InputBlock_File", InputBlock::File(artifact_ref())),
        vec_of(
            "InputBlock_EditorSelection",
            InputBlock::EditorSelection(EditorSelection {
                path: "src/lib.rs".to_string(),
                range: Range {
                    start: Position {
                        line: 1,
                        character: 0,
                    },
                    end: Position {
                        line: 2,
                        character: 4,
                    },
                },
            }),
        ),
        vec_of(
            "InputBlock_CodeSymbol",
            InputBlock::CodeSymbol(SymbolRef {
                path: "crates/workflow/src/drive.rs".to_string(),
                symbol: "WorkflowDriver::advance".to_string(),
                kind: Some("function".to_string()),
                line: Some(42),
            }),
        ),
        vec_of(
            "InputBlock_GitHubReference",
            InputBlock::GitHubReference(GitHubReference {
                owner: "CodeHalwell".to_string(),
                repo: "codypendent".to_string(),
                kind: GitHubRefKind::PullRequest,
                number: Some(14),
                url: None,
            }),
        ),
        vec_of("AudioArtifact", audio),
        vec_of("ImageArtifact", image),
        vec_of(
            "ModelObservation",
            ModelObservation {
                text: "A terminal showing a failing test.".to_string(),
                model: Some(ModelId("claude-sonnet-5".to_string())),
            },
        ),
        vec_of(
            "ImageRegion",
            ImageRegion {
                label: Some("error message".to_string()),
                x: 10,
                y: 20,
                width: 300,
                height: 40,
            },
        ),
        vec_of(
            "SymbolRef",
            SymbolRef {
                path: "crates/workflow/src/drive.rs".to_string(),
                symbol: "WorkflowDriver::advance".to_string(),
                kind: Some("function".to_string()),
                line: Some(42),
            },
        ),
        vec_of(
            "GitHubReference",
            GitHubReference {
                owner: "CodeHalwell".to_string(),
                repo: "codypendent".to_string(),
                kind: GitHubRefKind::PullRequest,
                number: Some(14),
                url: None,
            },
        ),
        vec_of(
            "OffDevicePolicy",
            OffDevicePolicy {
                max_off_device: DataClassification::Internal,
            },
        ),
        vec_of(
            "InputEnvelope",
            InputEnvelope {
                source: InputSource::Ide,
                blocks: vec![InputBlock::Text {
                    text: "approve the patch".to_string(),
                }],
                scope: ScopeLevel::Session,
                attachments: vec![],
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// The single source of truth both the regenerator and the checks iterate.
// ---------------------------------------------------------------------------

fn all_files() -> Vec<(&'static str, Vec<Vector>)> {
    vec![
        ("command.json", command_vectors()),
        ("envelope.json", envelope_vectors()),
        ("events.json", events_vectors()),
        ("run.json", run_vectors()),
        ("handshake.json", handshake_vectors()),
        ("catchup.json", catchup_vectors()),
        ("artifact.json", artifact_vectors()),
        ("error.json", error_vectors()),
        ("ide.json", ide_vectors()),
        ("document.json", document_vectors()),
        ("blackboard.json", blackboard_vectors()),
        ("workflow.json", workflow_vectors()),
        ("capabilities.json", capabilities_vectors()),
        ("input.json", input_vectors()),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Regenerate every committed vector file from the CURRENT protocol types.
/// Never runs in CI (see the module doc); run it explicitly after a wire
/// change:
///
/// ```text
/// cargo test -p codypendent-protocol --test golden_vectors regenerate_vectors -- --ignored
/// ```
#[test]
#[ignore = "writes committed vector files; run explicitly to regenerate them"]
fn regenerate_vectors() {
    let dir = vectors_dir();
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    for (filename, vectors) in all_files() {
        let path = dir.join(filename);
        let text = render(&manifest_value(&vectors));
        std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
}

/// CI gate #1: every committed vector file equals a fresh regeneration
/// byte-for-byte. A Rust-side wire change (new field, new variant, a changed
/// sentinel) that is not paired with running the regenerator FAILS this test.
#[test]
fn committed_vectors_match_current_protocol_types() {
    let dir = vectors_dir();
    for (filename, vectors) in all_files() {
        let path = dir.join(filename);
        let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e}\n\nrun `cargo test -p codypendent-protocol --test golden_vectors regenerate_vectors -- --ignored`, \
                 review the diff under protocol-vectors/, and commit it (see protocol-vectors/README.md)",
                path.display()
            )
        });
        let fresh = render(&manifest_value(&vectors));
        assert_eq!(
            committed, fresh,
            "{} is stale relative to the current protocol types.\n\
             Run `cargo test -p codypendent-protocol --test golden_vectors regenerate_vectors -- --ignored`, \
             review the diff, and commit it.",
            path.display()
        );
    }
}

/// CI gate #2: every committed entry, deserialized through its own concrete
/// Rust type and re-serialized, reproduces itself exactly. Reads the vectors
/// straight off disk (not the in-memory values above) so a hand-edited or
/// stale file on disk is caught even if gate #1 were ever bypassed.
#[test]
fn committed_vectors_round_trip_through_their_rust_types() {
    let dir = vectors_dir();
    for (filename, vectors) in all_files() {
        let path = dir.join(filename);
        let committed_text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let committed: Value = serde_json::from_str(&committed_text)
            .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
        let committed_map = committed
            .as_object()
            .unwrap_or_else(|| panic!("{} is not a JSON object", path.display()));
        for vector in &vectors {
            let entry = committed_map.get(vector.name).unwrap_or_else(|| {
                panic!(
                    "{} has no entry named {:?} — run the regeneration command",
                    path.display(),
                    vector.name
                )
            });
            let reserialized = (vector.round_trip)(entry);
            assert_eq!(
                &reserialized, entry,
                "{}::{} does not round-trip through its Rust type unchanged — the wire shape \
                 changed; regenerate the vectors",
                filename, vector.name
            );
        }
    }
}
