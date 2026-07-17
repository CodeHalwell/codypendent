/**
 * TypeScript wire types for the Codypendent protocol.
 *
 * These reproduce the serde serialization contract from
 * `crates/protocol/src/*.rs` EXACTLY. Notes that drove the shapes below:
 *
 * - Every frame is one serialized `Envelope` (see `envelope.rs`).
 * - Enums are internally tagged with a `"type"` field and PascalCase variant
 *   names (`#[serde(tag = "type")]`). Receivers must tolerate an unknown `type`
 *   (the Rust side maps it to an `Unknown` variant — we treat it as ignorable).
 * - serde's internally-tagged NEWTYPE variants FLATTEN the inner struct's fields
 *   next to the tag. So `Payload::ClientHello(ClientHello { .. })` is on the wire
 *   `{ "type": "ClientHello", "client_name": .., .. }` — the ClientHello fields
 *   sit at the same level as `type`, not nested. The same holds for
 *   `Payload::Command`, `Payload::ServerHello`, `Payload::Event`,
 *   `Payload::CommandRejected`, and `Payload::Error`.
 * - `Option::None` and empty `Vec`s marked `skip_serializing_if` are omitted from
 *   the wire; on read, missing == default. We omit `undefined` fields when
 *   sending and default missing fields when reading.
 */

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/** UUIDv7 strings on the wire (serde `transparent` newtypes over `Uuid`). */
export type Uuid = string;

/** `version.rs` — `{ major, minor }`. */
export interface ProtocolVersion {
  major: number;
  minor: number;
}

/** `version.rs::PROTOCOL_V1`. Additive Phase 1 revision. */
export const PROTOCOL_V1: ProtocolVersion = { major: 1, minor: 1 };

// ---------------------------------------------------------------------------
// capabilities.rs
// ---------------------------------------------------------------------------

/** `ClientCapabilities` — all eight flags always serialize (no skip). */
export interface ClientCapabilities {
  rich_text: boolean;
  image_display: boolean;
  audio_capture: boolean;
  editor_mutations: boolean;
  diff_view: boolean;
  mouse: boolean;
  unicode: boolean;
  true_color: boolean;
}

/** Capabilities an editor-aware client advertises. */
export const IDE_CAPABILITIES: ClientCapabilities = {
  rich_text: true,
  image_display: false,
  audio_capture: false,
  editor_mutations: true,
  diff_view: true,
  mouse: true,
  unicode: true,
  true_color: true,
};

// ---------------------------------------------------------------------------
// handshake.rs
// ---------------------------------------------------------------------------

export interface ClientHello {
  client_name: string;
  client_version: string;
  supported_protocols: ProtocolVersion[];
  capabilities: ClientCapabilities;
  /** Opaque resume token, omitted when absent. */
  resume_token?: string;
}

export interface ServerHello {
  selected_protocol: ProtocolVersion;
  daemon_version: string;
  daemon_instance: Uuid;
  heartbeat_interval_ms: number;
}

/** `ClientRole` — internally tagged, `{ "type": "Contributor" }` etc. */
export type ClientRole =
  | { type: "Observer" }
  | { type: "Contributor" }
  | { type: "Controller" }
  | { type: "Approver" }
  | { type: "Unknown" };

/** `Subscription` — internally tagged; only the variants used here are typed. */
export type Subscription =
  | { type: "SessionSummary" }
  | { type: "RunTrace"; run_id: Uuid }
  | { type: "AgentActivity" }
  | { type: "RepositoryStatus" }
  | { type: "BudgetState" }
  | { type: "Unknown" };

// ---------------------------------------------------------------------------
// run.rs
// ---------------------------------------------------------------------------

export type AgentMode =
  | { type: "Ask" }
  | { type: "Explore" }
  | { type: "Plan" }
  | { type: "Build" }
  | { type: "Review" }
  | { type: "Unknown" };

export type RunState =
  | { type: "Queued" }
  | { type: "Preparing" }
  | { type: "Running" }
  | { type: "WaitingForApproval" }
  | { type: "WaitingForUserInput" }
  | { type: "Paused" }
  | { type: "Recovering" }
  | { type: "Completed" }
  | { type: "Failed" }
  | { type: "Cancelled" }
  | { type: "Unknown" };

export type RiskLevel =
  | { type: "Low" }
  | { type: "Medium" }
  | { type: "High" }
  | { type: "Critical" }
  | { type: "Unknown" };

export interface Risk {
  level: RiskLevel;
  reasons?: string[];
}

export type ApprovalDecision = { type: "Approve" } | { type: "Reject" } | { type: "Unknown" };

export type ApprovalScope =
  | { type: "Once" }
  | { type: "Run" }
  | { type: "Pattern" }
  | { type: "Repository" }
  | { type: "Unknown" };

/** `ProposedAction` — internally tagged; carried on approval events. */
export type ProposedAction =
  | { type: "ReadFiles"; paths: string[] }
  | { type: "WritePatch"; patch: Uuid }
  | { type: "ExecuteCommand"; program: string; args: string[] }
  | { type: "NetworkRequest"; destination: string }
  | { type: "GitCommit"; repository: string }
  | { type: "GitPush"; remote: string; branch: string }
  | { type: "GitHubMutation"; repository: string; summary: string }
  | { type: string; [key: string]: unknown };

export type ToolOutcome =
  | { type: "Succeeded" }
  | { type: "Failed"; message: string }
  | { type: string; [key: string]: unknown };

export type RunDisposition =
  | { type: "Completed"; summary?: string }
  | { type: "Failed"; reason: string }
  | { type: "Cancelled"; reason?: string }
  | { type: string; [key: string]: unknown };

export type BudgetDimension =
  | { type: "Tokens" }
  | { type: "Cost" }
  | { type: "WallClock" }
  | { type: "ToolCalls" }
  | { type: "Unknown" };

// ---------------------------------------------------------------------------
// artifact.rs
// ---------------------------------------------------------------------------

export type DataClassification =
  | { type: "Public" }
  | { type: "Internal" }
  | { type: "Confidential" }
  | { type: "Secret" }
  | { type: "Unknown" };

export interface ArtifactRef {
  id: Uuid;
  media_type: string;
  byte_length: number;
  sha256: string;
  sensitivity: DataClassification;
}

// ---------------------------------------------------------------------------
// error.rs
// ---------------------------------------------------------------------------

export interface CodypendentError {
  code: string;
  message: string;
  retryable: boolean;
  user_action?: { type: string };
  details?: unknown;
  correlation_id: Uuid;
}

/** `envelope.rs::ProtocolError` — the transport-level error shape. */
export interface ProtocolError {
  code: string;
  message: string;
  retryable: boolean;
}

// ---------------------------------------------------------------------------
// events.rs
// ---------------------------------------------------------------------------

export type Actor =
  | { type: "Human"; user_id: string }
  | { type: "Agent"; agent_id: Uuid; run_id: Uuid; model: string }
  | { type: "Client"; client_id: Uuid }
  | { type: "Integration"; integration_id: string }
  | { type: "System" };

/**
 * `EventBody` — internally tagged, `#[non_exhaustive]` with an `Unknown`
 * fallback. The named variants are the ones the extension renders / acts on;
 * the trailing open member keeps forward-compat variants parseable.
 */
export type EventBody =
  | { type: "SessionCreated"; title: string }
  | { type: "NoteAppended"; text: string; run_id?: Uuid }
  | { type: "SessionClosed" }
  | { type: "RunStarted"; run_id: Uuid; objective: string; mode: AgentMode }
  | { type: "RunStateChanged"; run_id: Uuid; state: RunState }
  | { type: "ModelStreamDelta"; run_id: Uuid; text: string }
  | { type: "ToolProposed"; run_id: Uuid; approval_id: Uuid; action: ProposedAction }
  | { type: "ToolStarted"; run_id: Uuid; tool: string; args_digest: string }
  | {
      type: "ToolCompleted";
      run_id: Uuid;
      tool: string;
      outcome: ToolOutcome;
      artifact?: ArtifactRef;
    }
  | { type: "PatchProposed"; run_id: Uuid; changeset_id: Uuid; artifact: ArtifactRef }
  | { type: "ApprovalRequested"; approval_id: Uuid; action: ProposedAction; risk: Risk }
  | { type: "ApprovalResolved"; approval_id: Uuid; decision: ApprovalDecision }
  | { type: "SteeringQueued"; run_id: Uuid }
  | { type: "SteeringApplied"; run_id: Uuid }
  | { type: "BudgetWarning"; run_id: Uuid; dimension: BudgetDimension; used: number; limit: number }
  | { type: "RunCompleted"; run_id: Uuid; disposition: RunDisposition; chronicle: ArtifactRef }
  | { type: string; [key: string]: unknown };

export interface SessionEvent {
  sequence: number;
  occurred_at: string;
  causation_id?: Uuid;
  correlation_id?: Uuid;
  actor: Actor;
  body: EventBody;
}

// ---------------------------------------------------------------------------
// catchup.rs
// ---------------------------------------------------------------------------

export interface SessionProjection {
  session_id: Uuid;
  title: string;
  last_sequence: number;
  active_runs?: Uuid[];
  closed: boolean;
}

export type Catchup =
  | { type: "Events"; from: number; through: number; events: SessionEvent[] }
  | { type: "Snapshot"; through: number; projection: SessionProjection }
  | { type: string; [key: string]: unknown };

// ---------------------------------------------------------------------------
// ide.rs
// ---------------------------------------------------------------------------

export interface Position {
  line: number;
  character: number;
}

export interface Range {
  start: Position;
  end: Position;
}

export interface EditorSelection {
  path: string;
  range: Range;
}

export interface DirtyBufferDigest {
  path: string;
  /** Lowercase hex SHA-256 of the buffer bytes. */
  sha256: string;
  byte_length: number;
}

/**
 * `IdeContextUpdate` — pushed client -> daemon, debounced >= 300 ms.
 * Optional / empty-collection fields are `skip_serializing_if` in Rust; we omit
 * them when empty. `diagnostics_revision` always serializes (default 0).
 */
export interface IdeContextUpdate {
  active_file?: string;
  selection?: EditorSelection;
  open_files?: string[];
  dirty_buffers?: DirtyBufferDigest[];
  diagnostics_revision: number;
}

// ---------------------------------------------------------------------------
// command.rs
// ---------------------------------------------------------------------------

/**
 * `CommandBody` — internally tagged. `UpdateIdeContext` is the STEP 3.4/3.5
 * variant being added to the Rust protocol concurrently (client side is
 * implemented here against the ide.rs `IdeContextUpdate` shape). Only the
 * variants the extension issues are typed.
 */
export type CommandBody =
  | {
      type: "AttachSession";
      session_id: Uuid;
      last_seen_sequence?: number;
      subscriptions: Subscription[];
      requested_role: ClientRole;
    }
  | { type: "SubmitUserInput"; session_id: Uuid; text: string; mode: AgentMode }
  | { type: "StartRun"; session_id: Uuid; objective: string; mode: AgentMode; repository?: string }
  | {
      type: "ResolveApproval";
      approval_id: Uuid;
      decision: ApprovalDecision;
      scope: ApprovalScope;
    }
  | { type: "CancelRun"; run_id: Uuid }
  | { type: "PauseRun"; run_id: Uuid }
  | { type: "ResumeRun"; run_id: Uuid }
  | { type: "QueueSteering"; run_id: Uuid; text: string }
  | { type: "UpdateIdeContext"; session_id: Uuid; update: IdeContextUpdate };

export interface Command {
  command_id: Uuid;
  idempotency_key: string;
  expected_revision?: number;
  body: CommandBody;
}

// ---------------------------------------------------------------------------
// envelope.rs
// ---------------------------------------------------------------------------

/**
 * `Payload` — internally tagged. Newtype variants flatten their inner struct's
 * fields next to `type` (see the module doc comment). This union covers the
 * payloads the extension sends and receives; the trailing open member keeps
 * unknown / future payload tags parseable so a single frame never fails.
 */
export type Payload =
  | ({ type: "ClientHello" } & ClientHello)
  | ({ type: "ServerHello" } & ServerHello)
  | ({ type: "Command" } & Command)
  | { type: "CommandAccepted"; command_id: Uuid; sequence?: number }
  | ({ type: "CommandRejected" } & CodypendentError)
  | ({ type: "Event" } & SessionEvent)
  | { type: "Catchup"; catchup: Catchup }
  | ({ type: "Error" } & ProtocolError)
  | { type: "Ping" }
  | { type: "Pong" }
  | { type: "Shutdown" }
  | { type: "ShutdownAck" }
  | { type: string; [key: string]: unknown };

/**
 * `Envelope` — one per frame. `Envelope::request` sets a fresh `message_id`,
 * `PROTOCOL_V1`, and leaves the optionals absent. Absent optionals are omitted
 * on the wire (`skip_serializing_if`).
 */
export interface Envelope {
  protocol_version: ProtocolVersion;
  message_id: Uuid;
  correlation_id?: Uuid;
  client_id: Uuid;
  workspace_id?: Uuid;
  session_id?: Uuid;
  sequence?: number;
  payload: Payload;
}

// ---------------------------------------------------------------------------
// Narrowing helpers (payload `type` is a plain string on the wire)
// ---------------------------------------------------------------------------

export function payloadType(payload: Payload): string {
  return payload.type;
}
