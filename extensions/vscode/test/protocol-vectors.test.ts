/**
 * Golden-vector drift guard (T16): reads the SAME committed vectors the Rust
 * side emits (`crates/protocol/tests/golden_vectors.rs`, committed under
 * `<repo-root>/protocol-vectors/`, no copy — see `protocol-vectors/README.md`)
 * and proves the extension's hand-written `src/protocol/types.ts` can
 * represent every field of the wire types it actually sends/consumes.
 *
 * The mechanism: each committed vector is `unknown` JSON at runtime (there is
 * no schema-validation library in this codebase, and TypeScript's types are
 * erased at runtime). To meaningfully exercise `types.ts` — not just parse
 * JSON, which proves nothing about the TS *type* — every vector is run
 * through a `reconstructX` function that copies named fields, one by one, from
 * the raw parsed object into an object literal ANNOTATED with the exact
 * imported TS type (mirroring the field-plucking idiom `src/client.ts` already
 * uses for its real decode path, e.g. `handlePayload`'s `ServerHello`/`Event`
 * cases). Two independent things can now go wrong, and each is a real drift
 * signal:
 *
 * 1. `npm run typecheck` fails to compile — a field in the literal doesn't
 *    exist on the TS type (an *excess* property the Rust vector carries that
 *    the TS type does not declare). This is exactly the S1 shape: if
 *    `ProposedAction`'s `ExecuteCommand` variant did not declare
 *    `environment`/`cwd`, writing `environment: ...` in its reconstruction
 *    below would not compile.
 * 2. The reconstructed value, JSON-round-tripped, does not deep-equal the
 *    original vector — a field present in the vector was never copied over
 *    (whether because the reconstruction forgot it, or because the TS type
 *    genuinely has nowhere to put it). This is the runtime half of the same
 *    guard.
 *
 * Vectors for wire types the extension does not yet model at all (workflow
 * runs, the blackboard, collaborative documents, multimodal input — see
 * `protocol-vectors/README.md` for the full list) are intentionally NOT
 * exercised here; a `Partition` completeness check below still asserts every
 * committed vector name is accounted for as either covered or explicitly
 * excluded, so a future Rust vector nobody accounted for fails loudly instead
 * of silently slipping through a gap.
 */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import type {
  Actor,
  AgentMode,
  ApprovalDecision,
  ApprovalScope,
  ArtifactRef,
  BudgetDimension,
  Catchup,
  ClientCapabilities,
  ClientHello,
  ClientRole,
  Command,
  CommandBody,
  CodypendentError,
  DataClassification,
  DirtyBufferDigest,
  EditorSelection,
  EventBody,
  IdeContextUpdate,
  Payload,
  Position,
  ProposedAction,
  ProtocolError,
  ProtocolVersion,
  Range,
  Risk,
  RiskLevel,
  RunDisposition,
  RunState,
  ServerHello,
  SessionEvent,
  SessionProjection,
  Subscription,
  ToolOutcome,
} from "../src/protocol/types.js";

// ---------------------------------------------------------------------------
// Vector loading: the SAME committed files the Rust emitter writes. A
// relative path, no copy — one source of truth for both languages.
// ---------------------------------------------------------------------------

const VECTORS_DIR = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "protocol-vectors");

function loadVectors(filename: string): Record<string, unknown> {
  const path = join(VECTORS_DIR, filename);
  const text = readFileSync(path, "utf8");
  return JSON.parse(text) as Record<string, unknown>;
}

/** Every key in `manifest` whose name starts with `prefix + "_"`. */
function keysWithPrefix(manifest: Record<string, unknown>, prefix: string): string[] {
  return Object.keys(manifest).filter((k) => k.startsWith(`${prefix}_`));
}

/**
 * Assert every key in `allKeys` is accounted for by exactly one of `covered`
 * or `excluded` — the drift guard's "guard the guard": a future vector this
 * suite neither tests nor explicitly excludes fails loudly instead of falling
 * through a silent gap.
 */
function assertPartitionIsComplete(label: string, allKeys: string[], covered: string[], excluded: string[]): void {
  const accounted = new Set([...covered, ...excluded]);
  const unaccounted = allKeys.filter((k) => !accounted.has(k));
  expect(unaccounted, `${label}: vector(s) neither covered nor explicitly excluded`).toEqual([]);
  const coveredSet = new Set(covered);
  const excludedSet = new Set(excluded);
  const overlap = covered.filter((k) => excludedSet.has(k));
  expect(overlap, `${label}: vector(s) listed as both covered and excluded`).toEqual([]);
  // Every entry claimed as "covered" or "excluded" must actually exist —
  // otherwise a typo'd or renamed vector name would silently stop being
  // checked at all.
  const phantom = [...coveredSet, ...excludedSet].filter((k) => !allKeys.includes(k));
  expect(phantom, `${label}: covered/excluded name(s) that do not exist in the vector file`).toEqual([]);
}

/**
 * The core round-trip assertion: `reconstructed`, JSON-normalized (so
 * `undefined`-valued keys drop out exactly as Rust's `skip_serializing_if`
 * does), must deep-equal the original vector.
 */
function expectReconstructionMatches(vectorName: string, original: unknown, reconstructed: unknown): void {
  const originalNormalized: unknown = JSON.parse(JSON.stringify(original));
  const reconstructedNormalized: unknown = JSON.parse(JSON.stringify(reconstructed));
  expect(reconstructedNormalized, vectorName).toEqual(originalNormalized);
}

// ---------------------------------------------------------------------------
// Small, safe readers over `unknown` JSON (no `any` anywhere — required by
// this project's `@typescript-eslint/no-explicit-any: error`).
// ---------------------------------------------------------------------------

function asRecord(value: unknown, context: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${context}: expected an object, got ${JSON.stringify(value)}`);
  }
  return value as Record<string, unknown>;
}

function rec(r: Record<string, unknown>, key: string): Record<string, unknown> {
  return asRecord(r[key], key);
}

function optRec(r: Record<string, unknown>, key: string): Record<string, unknown> | undefined {
  return r[key] === undefined ? undefined : asRecord(r[key], key);
}

function str(r: Record<string, unknown>, key: string): string {
  const v = r[key];
  if (typeof v !== "string") throw new Error(`expected string field '${key}', got ${JSON.stringify(v)}`);
  return v;
}

function optStr(r: Record<string, unknown>, key: string): string | undefined {
  const v = r[key];
  if (v === undefined) return undefined;
  if (typeof v !== "string") throw new Error(`expected optional string field '${key}', got ${JSON.stringify(v)}`);
  return v;
}

function num(r: Record<string, unknown>, key: string): number {
  const v = r[key];
  if (typeof v !== "number") throw new Error(`expected number field '${key}', got ${JSON.stringify(v)}`);
  return v;
}

function optNum(r: Record<string, unknown>, key: string): number | undefined {
  const v = r[key];
  if (v === undefined) return undefined;
  if (typeof v !== "number") throw new Error(`expected optional number field '${key}', got ${JSON.stringify(v)}`);
  return v;
}

function bool(r: Record<string, unknown>, key: string): boolean {
  const v = r[key];
  if (typeof v !== "boolean") throw new Error(`expected boolean field '${key}', got ${JSON.stringify(v)}`);
  return v;
}

function arr(r: Record<string, unknown>, key: string): unknown[] {
  const v = r[key];
  if (!Array.isArray(v)) throw new Error(`expected array field '${key}', got ${JSON.stringify(v)}`);
  return v;
}

function optArr(r: Record<string, unknown>, key: string): unknown[] | undefined {
  if (r[key] === undefined) return undefined;
  return arr(r, key);
}

function strArr(r: Record<string, unknown>, key: string): string[] {
  return arr(r, key).map((v, i) => {
    if (typeof v !== "string") throw new Error(`expected string at '${key}[${i}]', got ${JSON.stringify(v)}`);
    return v;
  });
}

function optStrArr(r: Record<string, unknown>, key: string): string[] | undefined {
  if (r[key] === undefined) return undefined;
  return strArr(r, key);
}

// ---------------------------------------------------------------------------
// Reconstructors: unknown JSON -> the exact imported TS type. Each is a
// literal object annotated with the TS type, so a Rust field the type lacks
// fails `npm run typecheck`, and a field this function forgets to copy fails
// the runtime deep-equal in `expectReconstructionMatches`.
// ---------------------------------------------------------------------------

function reconstructAgentMode(r: Record<string, unknown>): AgentMode {
  switch (str(r, "type")) {
    case "Ask":
      return { type: "Ask" };
    case "Explore":
      return { type: "Explore" };
    case "Plan":
      return { type: "Plan" };
    case "Build":
      return { type: "Build" };
    case "Review":
      return { type: "Review" };
    default:
      throw new Error(`unknown AgentMode tag: ${str(r, "type")}`);
  }
}

function reconstructRunState(r: Record<string, unknown>): RunState {
  switch (str(r, "type")) {
    case "Queued":
      return { type: "Queued" };
    case "Preparing":
      return { type: "Preparing" };
    case "Running":
      return { type: "Running" };
    case "WaitingForApproval":
      return { type: "WaitingForApproval" };
    case "WaitingForUserInput":
      return { type: "WaitingForUserInput" };
    case "Paused":
      return { type: "Paused" };
    case "Recovering":
      return { type: "Recovering" };
    case "Completed":
      return { type: "Completed" };
    case "Failed":
      return { type: "Failed" };
    case "Cancelled":
      return { type: "Cancelled" };
    default:
      throw new Error(`unknown RunState tag: ${str(r, "type")}`);
  }
}

function reconstructRiskLevel(r: Record<string, unknown>): RiskLevel {
  switch (str(r, "type")) {
    case "Low":
      return { type: "Low" };
    case "Medium":
      return { type: "Medium" };
    case "High":
      return { type: "High" };
    case "Critical":
      return { type: "Critical" };
    default:
      throw new Error(`unknown RiskLevel tag: ${str(r, "type")}`);
  }
}

function reconstructRisk(r: Record<string, unknown>): Risk {
  return {
    level: reconstructRiskLevel(rec(r, "level")),
    reasons: optStrArr(r, "reasons"),
  };
}

function reconstructApprovalDecision(r: Record<string, unknown>): ApprovalDecision {
  switch (str(r, "type")) {
    case "Approve":
      return { type: "Approve" };
    case "Reject":
      return { type: "Reject" };
    default:
      throw new Error(`unknown ApprovalDecision tag: ${str(r, "type")}`);
  }
}

function reconstructApprovalScope(r: Record<string, unknown>): ApprovalScope {
  switch (str(r, "type")) {
    case "Once":
      return { type: "Once" };
    case "Run":
      return { type: "Run" };
    case "Pattern":
      return { type: "Pattern" };
    case "Repository":
      return { type: "Repository" };
    default:
      throw new Error(`unknown ApprovalScope tag: ${str(r, "type")}`);
  }
}

function reconstructBudgetDimension(r: Record<string, unknown>): BudgetDimension {
  switch (str(r, "type")) {
    case "Tokens":
      return { type: "Tokens" };
    case "Cost":
      return { type: "Cost" };
    case "WallClock":
      return { type: "WallClock" };
    case "ToolCalls":
      return { type: "ToolCalls" };
    default:
      throw new Error(`unknown BudgetDimension tag: ${str(r, "type")}`);
  }
}

function reconstructToolOutcome(r: Record<string, unknown>): ToolOutcome {
  switch (str(r, "type")) {
    case "Succeeded":
      return { type: "Succeeded" };
    case "Failed":
      return { type: "Failed", message: str(r, "message") };
    default:
      throw new Error(`unknown ToolOutcome tag: ${str(r, "type")}`);
  }
}

function reconstructRunDisposition(r: Record<string, unknown>): RunDisposition {
  switch (str(r, "type")) {
    case "Completed":
      return { type: "Completed", summary: optStr(r, "summary") };
    case "Failed":
      return { type: "Failed", reason: str(r, "reason") };
    case "Cancelled":
      return { type: "Cancelled", reason: optStr(r, "reason") };
    default:
      throw new Error(`unknown RunDisposition tag: ${str(r, "type")}`);
  }
}

function reconstructDataClassification(r: Record<string, unknown>): DataClassification {
  switch (str(r, "type")) {
    case "Public":
      return { type: "Public" };
    case "Internal":
      return { type: "Internal" };
    case "Confidential":
      return { type: "Confidential" };
    case "Secret":
      return { type: "Secret" };
    default:
      throw new Error(`unknown DataClassification tag: ${str(r, "type")}`);
  }
}

function reconstructArtifactRef(r: Record<string, unknown>): ArtifactRef {
  return {
    id: str(r, "id"),
    media_type: str(r, "media_type"),
    byte_length: num(r, "byte_length"),
    sha256: str(r, "sha256"),
    sensitivity: reconstructDataClassification(rec(r, "sensitivity")),
  };
}

function reconstructClientRole(r: Record<string, unknown>): ClientRole {
  switch (str(r, "type")) {
    case "Observer":
      return { type: "Observer" };
    case "Contributor":
      return { type: "Contributor" };
    case "Controller":
      return { type: "Controller" };
    case "Approver":
      return { type: "Approver" };
    default:
      throw new Error(`unknown ClientRole tag: ${str(r, "type")}`);
  }
}

/** Only the 5 variants the extension actually types (SessionSummary, RunTrace,
 * AgentActivity, RepositoryStatus, BudgetState) — Document/Blackboard/Workflow
 * are intentionally excluded (see the file-level doc + protocol-vectors/README.md). */
function reconstructSubscription(r: Record<string, unknown>): Subscription {
  switch (str(r, "type")) {
    case "SessionSummary":
      return { type: "SessionSummary" };
    case "RunTrace":
      return { type: "RunTrace", run_id: str(r, "run_id") };
    case "AgentActivity":
      return { type: "AgentActivity" };
    case "RepositoryStatus":
      return { type: "RepositoryStatus" };
    case "BudgetState":
      return { type: "BudgetState" };
    default:
      throw new Error(`unmodeled or unknown Subscription tag: ${str(r, "type")}`);
  }
}

/** Only the 7 variants the extension actually types — PublishDocument,
 * BlackboardPost, BlackboardQuery are workflow-scoped and intentionally
 * excluded (see the file-level doc + protocol-vectors/README.md). */
function reconstructProposedAction(r: Record<string, unknown>): ProposedAction {
  switch (str(r, "type")) {
    case "ReadFiles":
      return { type: "ReadFiles", paths: strArr(r, "paths") };
    case "WritePatch":
      return { type: "WritePatch", patch: str(r, "patch") };
    case "ExecuteCommand": {
      // THE S1 CASE: environment + cwd must be present on the reconstructed
      // literal, or this line fails `npm run typecheck` outright.
      const envRaw = optArr(r, "environment");
      return {
        type: "ExecuteCommand",
        program: str(r, "program"),
        args: strArr(r, "args"),
        environment: envRaw?.map((pair, i) => {
          const p = pair;
          if (!Array.isArray(p) || p.length !== 2 || typeof p[0] !== "string" || typeof p[1] !== "string") {
            throw new Error(`expected a [string, string] pair at environment[${i}], got ${JSON.stringify(p)}`);
          }
          return [p[0], p[1]] as [string, string];
        }),
        cwd: r.cwd === null ? null : optStr(r, "cwd"),
      };
    }
    case "NetworkRequest":
      return { type: "NetworkRequest", destination: str(r, "destination") };
    case "GitCommit":
      return { type: "GitCommit", repository: str(r, "repository") };
    case "GitPush":
      return { type: "GitPush", remote: str(r, "remote"), branch: str(r, "branch") };
    case "GitHubMutation":
      return { type: "GitHubMutation", repository: str(r, "repository"), summary: str(r, "summary") };
    default:
      throw new Error(`unmodeled or unknown ProposedAction tag: ${str(r, "type")}`);
  }
}

function reconstructActor(r: Record<string, unknown>): Actor {
  switch (str(r, "type")) {
    case "Human":
      return { type: "Human", user_id: str(r, "user_id") };
    case "Agent":
      return { type: "Agent", agent_id: str(r, "agent_id"), run_id: str(r, "run_id"), model: str(r, "model") };
    case "Client":
      return { type: "Client", client_id: str(r, "client_id") };
    case "Integration":
      return { type: "Integration", integration_id: str(r, "integration_id") };
    case "System":
      return { type: "System" };
    default:
      throw new Error(`unknown Actor tag: ${str(r, "type")}`);
  }
}

/** All 17 named variants the extension types — full coverage, no exclusions. */
function reconstructEventBody(r: Record<string, unknown>): EventBody {
  switch (str(r, "type")) {
    case "SessionCreated":
      return { type: "SessionCreated", title: str(r, "title") };
    case "NoteAppended":
      return { type: "NoteAppended", text: str(r, "text"), run_id: optStr(r, "run_id") };
    case "SessionClosed":
      return { type: "SessionClosed" };
    case "RunStarted":
      return {
        type: "RunStarted",
        run_id: str(r, "run_id"),
        objective: str(r, "objective"),
        mode: reconstructAgentMode(rec(r, "mode")),
      };
    case "RunStateChanged":
      return { type: "RunStateChanged", run_id: str(r, "run_id"), state: reconstructRunState(rec(r, "state")) };
    case "ModelStreamDelta":
      return { type: "ModelStreamDelta", run_id: str(r, "run_id"), text: str(r, "text") };
    case "ToolProposed":
      return {
        type: "ToolProposed",
        run_id: str(r, "run_id"),
        approval_id: str(r, "approval_id"),
        action: reconstructProposedAction(rec(r, "action")),
      };
    case "ToolStarted":
      return {
        type: "ToolStarted",
        run_id: str(r, "run_id"),
        tool: str(r, "tool"),
        args_digest: str(r, "args_digest"),
      };
    case "ToolCompleted": {
      const artifactRaw = optRec(r, "artifact");
      return {
        type: "ToolCompleted",
        run_id: str(r, "run_id"),
        tool: str(r, "tool"),
        outcome: reconstructToolOutcome(rec(r, "outcome")),
        artifact: artifactRaw ? reconstructArtifactRef(artifactRaw) : undefined,
      };
    }
    case "PatchProposed":
      return {
        type: "PatchProposed",
        run_id: str(r, "run_id"),
        changeset_id: str(r, "changeset_id"),
        artifact: reconstructArtifactRef(rec(r, "artifact")),
      };
    case "ApprovalRequested":
      return {
        type: "ApprovalRequested",
        approval_id: str(r, "approval_id"),
        action: reconstructProposedAction(rec(r, "action")),
        risk: reconstructRisk(rec(r, "risk")),
      };
    case "ApprovalResolved":
      return {
        type: "ApprovalResolved",
        approval_id: str(r, "approval_id"),
        decision: reconstructApprovalDecision(rec(r, "decision")),
      };
    case "SteeringQueued":
      return { type: "SteeringQueued", run_id: str(r, "run_id") };
    case "SteeringApplied":
      return { type: "SteeringApplied", run_id: str(r, "run_id") };
    case "BudgetWarning":
      return {
        type: "BudgetWarning",
        run_id: str(r, "run_id"),
        dimension: reconstructBudgetDimension(rec(r, "dimension")),
        used: num(r, "used"),
        limit: num(r, "limit"),
      };
    case "RunCompleted":
      return {
        type: "RunCompleted",
        run_id: str(r, "run_id"),
        disposition: reconstructRunDisposition(rec(r, "disposition")),
        chronicle: reconstructArtifactRef(rec(r, "chronicle")),
      };
    case "ClientPresenceChanged":
      return {
        type: "ClientPresenceChanged",
        client_id: str(r, "client_id"),
        role: reconstructClientRole(rec(r, "role")),
        present: bool(r, "present"),
      };
    default:
      throw new Error(`unmodeled or unknown EventBody tag: ${str(r, "type")}`);
  }
}

function reconstructSessionEvent(r: Record<string, unknown>): SessionEvent {
  return {
    sequence: num(r, "sequence"),
    occurred_at: str(r, "occurred_at"),
    causation_id: optStr(r, "causation_id"),
    correlation_id: optStr(r, "correlation_id"),
    actor: reconstructActor(rec(r, "actor")),
    body: reconstructEventBody(rec(r, "body")),
  };
}

function reconstructProtocolVersion(r: Record<string, unknown>): ProtocolVersion {
  return { major: num(r, "major"), minor: num(r, "minor") };
}

function reconstructClientCapabilities(r: Record<string, unknown>): ClientCapabilities {
  return {
    rich_text: bool(r, "rich_text"),
    image_display: bool(r, "image_display"),
    audio_capture: bool(r, "audio_capture"),
    editor_mutations: bool(r, "editor_mutations"),
    diff_view: bool(r, "diff_view"),
    mouse: bool(r, "mouse"),
    unicode: bool(r, "unicode"),
    true_color: bool(r, "true_color"),
  };
}

function reconstructClientHello(r: Record<string, unknown>): ClientHello {
  return {
    client_name: str(r, "client_name"),
    client_version: str(r, "client_version"),
    supported_protocols: arr(r, "supported_protocols").map((p) =>
      reconstructProtocolVersion(asRecord(p, "supported_protocols[]")),
    ),
    capabilities: reconstructClientCapabilities(rec(r, "capabilities")),
    resume_token: optStr(r, "resume_token"),
  };
}

function reconstructServerHello(r: Record<string, unknown>): ServerHello {
  return {
    selected_protocol: reconstructProtocolVersion(rec(r, "selected_protocol")),
    daemon_version: str(r, "daemon_version"),
    daemon_instance: str(r, "daemon_instance"),
    heartbeat_interval_ms: num(r, "heartbeat_interval_ms"),
    resume_token: optStr(r, "resume_token"),
  };
}

function reconstructCommandBody(r: Record<string, unknown>): CommandBody {
  switch (str(r, "type")) {
    case "AttachSession":
      return {
        type: "AttachSession",
        session_id: str(r, "session_id"),
        last_seen_sequence: optNum(r, "last_seen_sequence"),
        subscriptions: arr(r, "subscriptions").map((s) => reconstructSubscription(asRecord(s, "subscriptions[]"))),
        requested_role: reconstructClientRole(rec(r, "requested_role")),
      };
    case "SubmitUserInput":
      return {
        type: "SubmitUserInput",
        session_id: str(r, "session_id"),
        text: str(r, "text"),
        mode: reconstructAgentMode(rec(r, "mode")),
      };
    case "StartRun":
      return {
        type: "StartRun",
        session_id: str(r, "session_id"),
        objective: str(r, "objective"),
        mode: reconstructAgentMode(rec(r, "mode")),
        repository: optStr(r, "repository"),
      };
    case "ResolveApproval":
      return {
        type: "ResolveApproval",
        approval_id: str(r, "approval_id"),
        decision: reconstructApprovalDecision(rec(r, "decision")),
        scope: reconstructApprovalScope(rec(r, "scope")),
      };
    case "CancelRun":
      return { type: "CancelRun", run_id: str(r, "run_id") };
    case "PauseRun":
      return { type: "PauseRun", run_id: str(r, "run_id") };
    case "ResumeRun":
      return { type: "ResumeRun", run_id: str(r, "run_id") };
    case "QueueSteering":
      return { type: "QueueSteering", run_id: str(r, "run_id"), text: str(r, "text") };
    case "UpdateIdeContext":
      return {
        type: "UpdateIdeContext",
        session_id: str(r, "session_id"),
        update: reconstructIdeContextUpdate(rec(r, "update")),
      };
    default:
      throw new Error(`unmodeled or unknown CommandBody tag: ${str(r, "type")}`);
  }
}

function reconstructRange(r: Record<string, unknown>): Range {
  const start = rec(r, "start");
  const end = rec(r, "end");
  return {
    start: { line: num(start, "line"), character: num(start, "character") },
    end: { line: num(end, "line"), character: num(end, "character") },
  };
}

function reconstructEditorSelection(r: Record<string, unknown>): EditorSelection {
  return { path: str(r, "path"), range: reconstructRange(rec(r, "range")) };
}

function reconstructDirtyBufferDigest(r: Record<string, unknown>): DirtyBufferDigest {
  return { path: str(r, "path"), sha256: str(r, "sha256"), byte_length: num(r, "byte_length") };
}

function reconstructIdeContextUpdate(r: Record<string, unknown>): IdeContextUpdate {
  const selectionRaw = optRec(r, "selection");
  const dirtyBuffersRaw = optArr(r, "dirty_buffers");
  return {
    active_file: optStr(r, "active_file"),
    selection: selectionRaw ? reconstructEditorSelection(selectionRaw) : undefined,
    open_files: optStrArr(r, "open_files"),
    dirty_buffers: dirtyBuffersRaw?.map((d) => reconstructDirtyBufferDigest(asRecord(d, "dirty_buffers[]"))),
    diagnostics_revision: num(r, "diagnostics_revision"),
  };
}

function reconstructCommand(r: Record<string, unknown>): Command {
  return {
    command_id: str(r, "command_id"),
    idempotency_key: str(r, "idempotency_key"),
    expected_revision: optNum(r, "expected_revision"),
    body: reconstructCommandBody(rec(r, "body")),
  };
}

function reconstructSessionProjection(r: Record<string, unknown>): SessionProjection {
  return {
    session_id: str(r, "session_id"),
    title: str(r, "title"),
    last_sequence: num(r, "last_sequence"),
    active_runs: optStrArr(r, "active_runs"),
    closed: bool(r, "closed"),
  };
}

function reconstructCatchup(r: Record<string, unknown>): Catchup {
  switch (str(r, "type")) {
    case "Events":
      return {
        type: "Events",
        from: num(r, "from"),
        through: num(r, "through"),
        events: arr(r, "events").map((e) => reconstructSessionEvent(asRecord(e, "events[]"))),
      };
    case "Snapshot":
      return {
        type: "Snapshot",
        through: num(r, "through"),
        projection: reconstructSessionProjection(rec(r, "projection")),
      };
    default:
      throw new Error(`unknown Catchup tag: ${str(r, "type")}`);
  }
}

function reconstructCodypendentError(r: Record<string, unknown>): CodypendentError {
  const userActionRaw = optRec(r, "user_action");
  return {
    code: str(r, "code"),
    message: str(r, "message"),
    retryable: bool(r, "retryable"),
    user_action: userActionRaw ? { type: str(userActionRaw, "type") } : undefined,
    details: r.details,
    correlation_id: str(r, "correlation_id"),
  };
}

function reconstructProtocolError(r: Record<string, unknown>): ProtocolError {
  return { code: str(r, "code"), message: str(r, "message"), retryable: bool(r, "retryable") };
}

/** Only the 12 variants the extension's `Payload` union names explicitly. */
function reconstructModeledPayload(r: Record<string, unknown>): Payload {
  switch (str(r, "type")) {
    case "ClientHello":
      return { type: "ClientHello", ...reconstructClientHello(r) };
    case "ServerHello":
      return { type: "ServerHello", ...reconstructServerHello(r) };
    case "Command":
      return { type: "Command", ...reconstructCommand(r) };
    case "CommandAccepted":
      return {
        type: "CommandAccepted",
        command_id: str(r, "command_id"),
        sequence: optNum(r, "sequence"),
        created_run: optStr(r, "created_run"),
      };
    case "CommandRejected":
      return { type: "CommandRejected", ...reconstructCodypendentError(r) };
    case "Event":
      return { type: "Event", ...reconstructSessionEvent(r) };
    case "Catchup":
      return { type: "Catchup", catchup: reconstructCatchup(rec(r, "catchup")) };
    case "Error":
      return { type: "Error", ...reconstructProtocolError(r) };
    case "Ping":
      return { type: "Ping" };
    case "Pong":
      return { type: "Pong" };
    case "Shutdown":
      return { type: "Shutdown" };
    case "ShutdownAck":
      return { type: "ShutdownAck" };
    default:
      throw new Error(`not a modeled Payload variant: ${str(r, "type")}`);
  }
}

// ---------------------------------------------------------------------------
// command.json: CommandBody (9 of 26 modeled — the extension sends only
// these), PromotionAction (not modeled — nested inside a command the
// extension never sends).
// ---------------------------------------------------------------------------

describe("command.json against CommandBody (src/protocol/types.ts)", () => {
  const vectors = loadVectors("command.json");
  const commandBodyKeys = keysWithPrefix(vectors, "CommandBody");

  const modeled = [
    "CommandBody_AttachSession",
    "CommandBody_CancelRun",
    "CommandBody_PauseRun",
    "CommandBody_QueueSteering",
    "CommandBody_ResolveApproval",
    "CommandBody_ResumeRun",
    "CommandBody_StartRun",
    "CommandBody_SubmitUserInput",
    "CommandBody_UpdateIdeContext",
  ];
  // Not modeled: workflow lifecycle, promotion, document, and blackboard
  // commands the extension never issues (see protocol-vectors/README.md).
  const notModeled = [
    "CommandBody_AcquireDocumentLease",
    "CommandBody_AdvancePromotion",
    "CommandBody_ApprovePromotion",
    "CommandBody_CancelWorkflow",
    "CommandBody_CreateSession",
    "CommandBody_MutateDocument",
    "CommandBody_PauseWorkflow",
    "CommandBody_ProposePromotion",
    "CommandBody_PublishDocument",
    "CommandBody_ReadBlackboard",
    "CommandBody_ReadWorkflowRun",
    "CommandBody_ReleaseDocumentLease",
    "CommandBody_ResumeWorkflow",
    "CommandBody_RetryWorkflowNode",
    "CommandBody_RollbackPromotion",
    "CommandBody_StartWorkflow_inline_manifest",
    "CommandBody_StartWorkflow_named_workflow",
  ];

  it("accounts for every CommandBody vector as modeled or explicitly excluded", () => {
    assertPartitionIsComplete("CommandBody", commandBodyKeys, modeled, notModeled);
  });

  for (const name of modeled) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      const reconstructed = reconstructCommandBody(asRecord(original, name));
      expectReconstructionMatches(name, original, reconstructed);
    });
  }
});

// ---------------------------------------------------------------------------
// envelope.json: Payload (12 of 23 modeled explicitly; the rest fall through
// the union's permissive `{ type: string; ... }` catch-all, matching the
// extension's actual forward-compatible handling).
// ---------------------------------------------------------------------------

describe("envelope.json against Payload (src/protocol/types.ts)", () => {
  const vectors = loadVectors("envelope.json");
  const payloadKeys = keysWithPrefix(vectors, "Payload");

  const modeled = [
    "Payload_Catchup",
    "Payload_ClientHello",
    "Payload_Command",
    "Payload_CommandAccepted",
    "Payload_CommandRejected",
    "Payload_Error",
    "Payload_Event_ApprovalRequestedExecuteCommand",
    "Payload_Ping",
    "Payload_Pong",
    "Payload_ServerHello",
    "Payload_Shutdown",
    "Payload_ShutdownAck",
  ];
  // Not modeled by name: these payload tags all fall through the union's
  // permissive `{ type: string; [key: string]: unknown }` member. Verified
  // below (not merely assumed) by the "still parses structurally" check.
  const passthrough = [
    "Payload_BlackboardItems",
    "Payload_BlackboardPosted",
    "Payload_DaemonStatusRequest",
    "Payload_DaemonStatusResponse",
    "Payload_DocumentLeaseGranted",
    "Payload_DocumentPublishRequested",
    "Payload_DocumentSync",
    "Payload_PromotionProposed",
    "Payload_WorkflowEvent",
    "Payload_WorkflowRunSnapshot",
    "Payload_WorkflowRunStarted",
  ];

  it("accounts for every Payload vector as modeled or explicit passthrough", () => {
    assertPartitionIsComplete("Payload", payloadKeys, modeled, passthrough);
  });

  for (const name of modeled) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      const reconstructed = reconstructModeledPayload(asRecord(original, name));
      expectReconstructionMatches(name, original, reconstructed);
    });
  }

  for (const name of passthrough) {
    it(`${name} still parses structurally (forward-compatible catch-all)`, () => {
      const original = asRecord(vectors[name], name);
      const asPayload: Payload = original as Payload;
      expect(typeof asPayload.type, name).toBe("string");
    });
  }
});

// ---------------------------------------------------------------------------
// events.json: Actor (5/5, full coverage) and EventBody (17/17, full
// coverage — this is where ProposedAction::ExecuteCommand's S1 fields are
// exercised inside a realistic ApprovalRequested event).
// ---------------------------------------------------------------------------

describe("events.json against Actor + EventBody (src/protocol/types.ts)", () => {
  const vectors = loadVectors("events.json");

  for (const name of keysWithPrefix(vectors, "Actor")) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      const reconstructed = reconstructActor(asRecord(original, name));
      expectReconstructionMatches(name, original, reconstructed);
    });
  }

  for (const name of keysWithPrefix(vectors, "EventBody")) {
    it(`decodes and re-encodes ${name} identically`, () => {
      // Each vector is a full SessionEvent (sequence, occurred_at, actor,
      // body: EventBody), not a bare EventBody — reconstruct the whole event
      // so `reconstructEventBody`'s field-by-field extraction runs on its
      // actual `body` sub-object, not the enclosing wrapper.
      const original = vectors[name];
      const reconstructed = reconstructSessionEvent(asRecord(original, name));
      expectReconstructionMatches(name, original, reconstructed);
    });
  }

  it("closes the S1 drift: ExecuteCommand's environment + cwd survive the TS type", () => {
    const original = asRecord(vectors.EventBody_ApprovalRequested_ExecuteCommand, "EventBody_ApprovalRequested_ExecuteCommand");
    const body = rec(original, "body");
    expect(body.type).toBe("ApprovalRequested");
    const action = reconstructProposedAction(rec(body, "action"));
    if (action.type !== "ExecuteCommand") {
      throw new Error(`expected ExecuteCommand, got ${action.type}`);
    }
    expect(action.environment).toEqual([
      ["RUST_BACKTRACE", "1"],
      ["PATH", "/usr/bin:/bin"],
    ]);
    expect(action.cwd).toBe("/home/user/project");
  });
});

// ---------------------------------------------------------------------------
// run.json: the small run-domain enums (fully modeled) plus ProposedAction
// (7 of 10 modeled — the extension does not model the 3 workflow-scoped
// variants). ProposedAction_ExecuteCommand here is the S1 vector standalone.
// ---------------------------------------------------------------------------

describe("run.json against run-domain types (src/protocol/types.ts)", () => {
  const vectors = loadVectors("run.json");

  const enumReconstructors: Array<{
    prefix: string;
    reconstruct: (r: Record<string, unknown>) => unknown;
  }> = [
    { prefix: "AgentMode", reconstruct: reconstructAgentMode },
    { prefix: "RunState", reconstruct: reconstructRunState },
    { prefix: "RiskLevel", reconstruct: reconstructRiskLevel },
    { prefix: "ApprovalDecision", reconstruct: reconstructApprovalDecision },
    { prefix: "ApprovalScope", reconstruct: reconstructApprovalScope },
    { prefix: "BudgetDimension", reconstruct: reconstructBudgetDimension },
    { prefix: "ToolOutcome", reconstruct: reconstructToolOutcome },
    { prefix: "RunDisposition", reconstruct: reconstructRunDisposition },
  ];

  for (const { prefix, reconstruct } of enumReconstructors) {
    for (const name of keysWithPrefix(vectors, prefix)) {
      it(`decodes and re-encodes ${name} identically`, () => {
        const original = vectors[name];
        expectReconstructionMatches(name, original, reconstruct(asRecord(original, name)));
      });
    }
  }

  it("decodes and re-encodes Risk identically", () => {
    expectReconstructionMatches("Risk", vectors.Risk, reconstructRisk(asRecord(vectors.Risk, "Risk")));
  });

  const proposedActionKeys = keysWithPrefix(vectors, "ProposedAction");
  const modeledActions = [
    "ProposedAction_ExecuteCommand",
    "ProposedAction_GitCommit",
    "ProposedAction_GitHubMutation",
    "ProposedAction_GitPush",
    "ProposedAction_NetworkRequest",
    "ProposedAction_ReadFiles",
    "ProposedAction_WritePatch",
  ];
  // Not modeled: workflow-scoped actions the extension never receives (it
  // does not subscribe to any workflow run's tool activity).
  const notModeledActions = ["ProposedAction_BlackboardPost", "ProposedAction_BlackboardQuery", "ProposedAction_PublishDocument"];

  it("accounts for every ProposedAction vector as modeled or explicitly excluded", () => {
    assertPartitionIsComplete("ProposedAction", proposedActionKeys, modeledActions, notModeledActions);
  });

  for (const name of modeledActions) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      const reconstructed = reconstructProposedAction(asRecord(original, name));
      expectReconstructionMatches(name, original, reconstructed);
    });
  }

  it("closes the S1 drift: the standalone ExecuteCommand vector keeps environment + cwd", () => {
    const original = asRecord(vectors.ProposedAction_ExecuteCommand, "ProposedAction_ExecuteCommand");
    const action = reconstructProposedAction(original);
    if (action.type !== "ExecuteCommand") {
      throw new Error(`expected ExecuteCommand, got ${action.type}`);
    }
    expect(action.environment, "environment must survive the TS ProposedAction type").toEqual([
      ["RUST_BACKTRACE", "1"],
      ["PATH", "/usr/bin:/bin"],
    ]);
    expect(action.cwd, "cwd must survive the TS ProposedAction type").toBe("/home/user/project");
  });
});

// ---------------------------------------------------------------------------
// handshake.json: ClientHello, ServerHello, ClientRole (full coverage);
// Subscription (5 of 8 modeled — Document/Blackboard/Workflow excluded).
// ---------------------------------------------------------------------------

describe("handshake.json against handshake types (src/protocol/types.ts)", () => {
  const vectors = loadVectors("handshake.json");

  it("decodes and re-encodes ClientHello identically", () => {
    expectReconstructionMatches("ClientHello", vectors.ClientHello, reconstructClientHello(asRecord(vectors.ClientHello, "ClientHello")));
  });

  it("decodes and re-encodes ServerHello identically", () => {
    expectReconstructionMatches("ServerHello", vectors.ServerHello, reconstructServerHello(asRecord(vectors.ServerHello, "ServerHello")));
  });

  for (const name of keysWithPrefix(vectors, "ClientRole")) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      expectReconstructionMatches(name, original, reconstructClientRole(asRecord(original, name)));
    });
  }

  const subscriptionKeys = keysWithPrefix(vectors, "Subscription");
  const modeled = [
    "Subscription_AgentActivity",
    "Subscription_BudgetState",
    "Subscription_RepositoryStatus",
    "Subscription_RunTrace",
    "Subscription_SessionSummary",
  ];
  // Not modeled: the extension does not yet subscribe to a document, a
  // workflow run, or a blackboard.
  const notModeled = ["Subscription_Blackboard", "Subscription_Document", "Subscription_Workflow"];

  it("accounts for every Subscription vector as modeled or explicitly excluded", () => {
    assertPartitionIsComplete("Subscription", subscriptionKeys, modeled, notModeled);
  });

  for (const name of modeled) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      expectReconstructionMatches(name, original, reconstructSubscription(asRecord(original, name)));
    });
  }
});

// ---------------------------------------------------------------------------
// catchup.json: Catchup + SessionProjection (full coverage).
// ---------------------------------------------------------------------------

describe("catchup.json against Catchup (src/protocol/types.ts)", () => {
  const vectors = loadVectors("catchup.json");

  for (const name of keysWithPrefix(vectors, "Catchup")) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      expectReconstructionMatches(name, original, reconstructCatchup(asRecord(original, name)));
    });
  }
});

// ---------------------------------------------------------------------------
// artifact.json: ArtifactRef + DataClassification (full coverage).
// ---------------------------------------------------------------------------

describe("artifact.json against ArtifactRef (src/protocol/types.ts)", () => {
  const vectors = loadVectors("artifact.json");

  it("decodes and re-encodes ArtifactRef identically", () => {
    expectReconstructionMatches("ArtifactRef", vectors.ArtifactRef, reconstructArtifactRef(asRecord(vectors.ArtifactRef, "ArtifactRef")));
  });

  for (const name of keysWithPrefix(vectors, "DataClassification")) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      expectReconstructionMatches(name, original, reconstructDataClassification(asRecord(original, name)));
    });
  }
});

// ---------------------------------------------------------------------------
// error.json: CodypendentError + ProtocolError (full coverage). UserAction is
// only loosely typed in the extension (`{ type: string }` on
// `CodypendentError.user_action`, not a dedicated closed union) — each vector
// is checked to still carry a string `type`, matching that intentionally
// loose contract exactly (nothing more to drift on).
// ---------------------------------------------------------------------------

describe("error.json against CodypendentError / ProtocolError (src/protocol/types.ts)", () => {
  const vectors = loadVectors("error.json");

  it("decodes and re-encodes CodypendentError identically", () => {
    expectReconstructionMatches(
      "CodypendentError",
      vectors.CodypendentError,
      reconstructCodypendentError(asRecord(vectors.CodypendentError, "CodypendentError")),
    );
  });

  it("decodes and re-encodes ProtocolError identically", () => {
    expectReconstructionMatches(
      "ProtocolError",
      vectors.ProtocolError,
      reconstructProtocolError(asRecord(vectors.ProtocolError, "ProtocolError")),
    );
  });

  for (const name of keysWithPrefix(vectors, "UserAction")) {
    it(`${name} carries a string 'type' (the extension's user_action is loosely typed)`, () => {
      const original = asRecord(vectors[name], name);
      expect(typeof str(original, "type")).toBe("string");
    });
  }
});

// ---------------------------------------------------------------------------
// capabilities.json: ClientCapabilities (full coverage).
// ---------------------------------------------------------------------------

describe("capabilities.json against ClientCapabilities (src/protocol/types.ts)", () => {
  const vectors = loadVectors("capabilities.json");

  it("decodes and re-encodes ClientCapabilities identically", () => {
    expectReconstructionMatches(
      "ClientCapabilities",
      vectors.ClientCapabilities,
      reconstructClientCapabilities(asRecord(vectors.ClientCapabilities, "ClientCapabilities")),
    );
  });
});

// ---------------------------------------------------------------------------
// ide.json: Position, Range, EditorSelection, DirtyBufferDigest,
// IdeContextUpdate (5 of 21 modeled — IdeRequest/Diagnostic/SourceProvenance/
// Location/TextEdit/WorkspaceEdit/DiffRequest are daemon<->IDE types the
// extension does not model; only IdeContextUpdate travels client->daemon
// today via `UpdateIdeContext`).
// ---------------------------------------------------------------------------

describe("ide.json against IDE-context types (src/protocol/types.ts)", () => {
  const vectors = loadVectors("ide.json");
  const allKeys = Object.keys(vectors);

  const modeled = ["DirtyBufferDigest", "EditorSelection", "IdeContextUpdate", "Position", "Range"];
  const notModeled = [
    "Diagnostic",
    "DiagnosticSeverity_Error",
    "DiagnosticSeverity_Hint",
    "DiagnosticSeverity_Information",
    "DiagnosticSeverity_Warning",
    "DiffRequest",
    "IdeRequest_ApplyEdit",
    "IdeRequest_RevealLocation",
    "IdeRequest_ShowDiff",
    "Location",
    "SourceProvenance_AgentWorktree",
    "SourceProvenance_CommittedAt",
    "SourceProvenance_Filesystem",
    "SourceProvenance_GeneratedPatch",
    "SourceProvenance_UnsavedIdeBuffer",
    "TextEdit",
    "WorkspaceEdit",
  ];

  it("accounts for every ide.json vector as modeled or explicitly excluded", () => {
    assertPartitionIsComplete("ide", allKeys, modeled, notModeled);
  });

  it("decodes and re-encodes Position identically", () => {
    const original = vectors.Position;
    const r = asRecord(original, "Position");
    const reconstructed: Position = { line: num(r, "line"), character: num(r, "character") };
    expectReconstructionMatches("Position", original, reconstructed);
  });

  it("decodes and re-encodes Range identically", () => {
    expectReconstructionMatches("Range", vectors.Range, reconstructRange(asRecord(vectors.Range, "Range")));
  });

  it("decodes and re-encodes EditorSelection identically", () => {
    expectReconstructionMatches(
      "EditorSelection",
      vectors.EditorSelection,
      reconstructEditorSelection(asRecord(vectors.EditorSelection, "EditorSelection")),
    );
  });

  it("decodes and re-encodes DirtyBufferDigest identically", () => {
    expectReconstructionMatches(
      "DirtyBufferDigest",
      vectors.DirtyBufferDigest,
      reconstructDirtyBufferDigest(asRecord(vectors.DirtyBufferDigest, "DirtyBufferDigest")),
    );
  });

  it("decodes and re-encodes IdeContextUpdate identically", () => {
    expectReconstructionMatches(
      "IdeContextUpdate",
      vectors.IdeContextUpdate,
      reconstructIdeContextUpdate(asRecord(vectors.IdeContextUpdate, "IdeContextUpdate")),
    );
  });
});
