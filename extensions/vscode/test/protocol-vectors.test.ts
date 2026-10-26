/**
 * Golden-vector drift guard (T16): reads the SAME committed vectors the Rust
 * side emits (`crates/protocol/tests/golden_vectors.rs`, committed under
 * `<repo-root>/protocol-vectors/`, no copy — see `protocol-vectors/README.md`)
 * and proves the `@codypendent/protocol` types the extension consumes can
 * represent every field of the wire types it actually sends/consumes.
 *
 * The extension used to carry its own hand-written `src/protocol/types.ts`
 * mirror of these same Rust types, and this suite guarded THAT. The mirror is
 * gone: TypeScript contracts are generated from the Rust schemas into
 * `@codypendent/protocol`, and nothing in this extension re-declares the wire.
 * The suite is retargeted onto the generated package rather than retired,
 * because it checks something the schema generator cannot — that the shapes
 * the extension actually reads and writes still line up, field for field, with
 * the bytes the Rust emitter produces.
 *
 * The mechanism: each committed vector is `unknown` JSON at runtime (there is
 * no schema-validation library in this codebase, and TypeScript's types are
 * erased at runtime). To meaningfully exercise the types — not just parse
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
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { isKnownPayloadTag } from "@codypendent/protocol";
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
  JsonValue,
  ModelProbe,
  ModelReadiness,
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
  UserAction,
} from "@codypendent/protocol";

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

/**
 * Every committed vector file under `protocol-vectors/`, walked RECURSIVELY and
 * returned as `/`-separated paths relative to that directory.
 *
 * This used to be a bare `readdirSync(VECTORS_DIR)`, which only ever saw the
 * top level: a vector added in a subdirectory (`protocol-vectors/<domain>/v1/`)
 * was invisible to the file-inventory partition below, so that guard passed
 * while checking nothing about it — the exact silent gap the partition exists
 * to close. Directory entries are joined with `/` regardless of platform so the
 * `COVERED_FILES`/`UNMODELED_FILES` lists read identically everywhere.
 */
function listVectorFiles(directory: string = VECTORS_DIR, prefix = ""): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const relative = prefix === "" ? entry.name : `${prefix}/${entry.name}`;
    if (entry.isDirectory()) {
      found.push(...listVectorFiles(join(directory, entry.name), relative));
    } else if (entry.name.endsWith(".json")) {
      found.push(relative);
    }
  }
  return found.sort();
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

function optBool(r: Record<string, unknown>, key: string): boolean | undefined {
  const v = r[key];
  if (v === undefined) return undefined;
  if (typeof v !== "boolean") throw new Error(`expected optional boolean '${key}', got ${JSON.stringify(v)}`);
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
    case "Failed": {
      const error = r.error === undefined ? undefined : reconstructCodypendentError(rec(r, "error"));
      return { type: "Failed", reason: str(r, "reason"), error };
    }
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

/** Only the 8 variants the extension actually types — PublishDocument,
 * BlackboardPost, BlackboardQuery are workflow-scoped and intentionally
 * excluded (see the file-level doc + protocol-vectors/README.md). */
function reconstructProposedAction(r: Record<string, unknown>): ProposedAction {
  switch (str(r, "type")) {
    case "ReadFiles":
      return { type: "ReadFiles", paths: strArr(r, "paths") };
    case "WritePatch":
      return { type: "WritePatch", patch: str(r, "patch") };
    case "ExecuteCommand": {
      // THE S1 CASE. `environment` and `cwd` are `#[serde(default)]` WITHOUT
      // `skip_serializing_if`, so both are always on the wire — and the
      // generated type makes both REQUIRED. Omitting either from this literal
      // fails `npm run typecheck` outright; a vector that stopped carrying
      // either fails the reader below. The approver's only view of a command's
      // environment cannot go missing quietly in either direction.
      if (!("cwd" in r)) {
        throw new Error("ExecuteCommand vector is missing the always-serialized 'cwd' field");
      }
      return {
        type: "ExecuteCommand",
        program: str(r, "program"),
        args: strArr(r, "args"),
        environment: arr(r, "environment").map((pair, i) => {
          const p = pair;
          if (!Array.isArray(p) || p.length !== 2 || typeof p[0] !== "string" || typeof p[1] !== "string") {
            throw new Error(`expected a [string, string] pair at environment[${i}], got ${JSON.stringify(p)}`);
          }
          return [p[0], p[1]] as [string, string];
        }),
        cwd: r.cwd === null ? null : str(r, "cwd"),
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
    case "McpToolCall":
      return { type: "McpToolCall", server: str(r, "server"), tool: str(r, "tool"), summary: str(r, "summary"), args: str(r, "args") };
    case "AcpToolCall":
      return { type: "AcpToolCall", agent: str(r, "agent"), title: str(r, "title"), details: str(r, "details") };
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

/** Every named variant the extension types — full coverage, no exclusions. */
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
      // `thought` marks reasoning rather than reply. Optional because the Rust
      // side skips it when false, so an ordinary reply carries no such key.
      return {
        type: "ModelStreamDelta",
        run_id: str(r, "run_id"),
        text: str(r, "text"),
        thought: optBool(r, "thought"),
      };
    case "ToolProposed":
      return {
        type: "ToolProposed",
        run_id: str(r, "run_id"),
        approval_id: str(r, "approval_id"),
        action: reconstructProposedAction(rec(r, "action")),
      };
    case "ToolDenied":
      return {
        type: "ToolDenied",
        run_id: str(r, "run_id"),
        action: reconstructProposedAction(rec(r, "action")),
        reasons: optStrArr(r, "reasons"),
      };
    case "ToolStarted":
      return {
        type: "ToolStarted",
        run_id: str(r, "run_id"),
        tool: str(r, "tool"),
        args_digest: str(r, "args_digest"),
        label: optStr(r, "label"),
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
    case "RunUsage":
      return {
        type: "RunUsage",
        run_id: str(r, "run_id"),
        prompt_tokens: optNum(r, "prompt_tokens"),
        completion_tokens: optNum(r, "completion_tokens"),
        cost_micros: optNum(r, "cost_micros"),
      };
    case "LearningsCaptured":
      return {
        type: "LearningsCaptured",
        run_id: str(r, "run_id"),
        proposed_count: num(r, "proposed_count"),
        proposed_ids: optStrArr(r, "proposed_ids"),
        activated_count: num(r, "activated_count"),
        activated_ids: optStrArr(r, "activated_ids"),
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
    ...(r.session_library === undefined ? {} : { session_library: bool(r, "session_library") }),
    ...(r.editor_actions === undefined ? {} : { editor_actions: bool(r, "editor_actions") }),
    ...(r.inbox === undefined ? {} : { inbox: bool(r, "inbox") }),
    ...(r.analytics === undefined ? {} : { analytics: bool(r, "analytics") }),
    ...(r.automation === undefined ? {} : { automation: bool(r, "automation") }),
    ...(r.bundles === undefined ? {} : { bundles: bool(r, "bundles") }),
    ...(r.marketplace === undefined ? {} : { marketplace: bool(r, "marketplace") }),
    ...(r.secrets === undefined ? {} : { secrets: bool(r, "secrets") }),
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
    build_id: str(r, "build_id"),
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
        repository: optStr(r, "repository"),
      };
    case "SubmitUserInput":
      return {
        type: "SubmitUserInput",
        session_id: str(r, "session_id"),
        text: str(r, "text"),
        mode: reconstructAgentMode(rec(r, "mode")),
        model: optStr(r, "model"),
      };
    case "StartRun":
      return {
        type: "StartRun",
        session_id: str(r, "session_id"),
        objective: str(r, "objective"),
        mode: reconstructAgentMode(rec(r, "mode")),
        repository: optStr(r, "repository"),
        model: optStr(r, "model"),
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

function reconstructUserAction(r: Record<string, unknown>): UserAction {
  switch (str(r, "type")) {
    case "Retry":
      return { type: "Retry" };
    case "Reauthenticate":
      return { type: "Reauthenticate" };
    case "GrantApproval":
      return { type: "GrantApproval" };
    case "AdjustPolicy":
      return { type: "AdjustPolicy" };
    case "ReconfigureModel":
      return { type: "ReconfigureModel" };
    case "ContactSupport":
      return { type: "ContactSupport" };
    default:
      throw new Error(`unknown UserAction tag: ${str(r, "type")}`);
  }
}

/**
 * Narrow arbitrary parsed JSON to the generated `JsonValue`. `CodypendentError.details`
 * is an opaque `serde_json::Value` on the wire; checking the shape here beats
 * asserting it, so a non-JSON value (which `JSON.parse` cannot produce, but a
 * future hand-built vector could) fails loudly.
 */
function jsonValue(value: unknown, context: string): JsonValue {
  if (value === null || typeof value === "boolean" || typeof value === "number" || typeof value === "string") {
    return value;
  }
  if (Array.isArray(value)) {
    return value.map((entry, index) => jsonValue(entry, `${context}[${index}]`));
  }
  if (typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>).map(([key, entry]) => [key, jsonValue(entry, `${context}.${key}`)]),
    );
  }
  throw new Error(`${context}: not a JSON value, got ${typeof value}`);
}

function reconstructCodypendentError(r: Record<string, unknown>): CodypendentError {
  const userActionRaw = optRec(r, "user_action");
  return {
    code: str(r, "code"),
    message: str(r, "message"),
    retryable: bool(r, "retryable"),
    user_action: userActionRaw ? reconstructUserAction(userActionRaw) : undefined,
    details: r.details === undefined ? undefined : jsonValue(r.details, "details"),
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
    case "ShutdownIfIdle":
      return { type: "ShutdownIfIdle" };
    case "ShutdownRefused":
      return { type: "ShutdownRefused", active_run_count: num(r, "active_run_count") };
    default:
      throw new Error(`not a modeled Payload variant: ${str(r, "type")}`);
  }
}

// ---------------------------------------------------------------------------
// command.json: CommandBody (9 of 26 modeled — the extension sends only
// these), PromotionAction (not modeled — nested inside a command the
// extension never sends).
// ---------------------------------------------------------------------------

describe("command.json against CommandBody (@codypendent/protocol)", () => {
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
    "CommandBody_CheckDocuments",
    "CommandBody_CloseSession",
    "CommandBody_CreateDocument",
    "CommandBody_CreateSession",
    "CommandBody_ListSessions",
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
    "CommandBody_SearchWorkspaceFiles",
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
// envelope.json: Payload. 14 tags are reconstructed field-by-field (the ones
// the extension sends or reads); the rest are payloads it never handles.
//
// The hand-written mirror gave those a permissive `{ type: string; … }`
// catch-all member, so "not modeled" there meant "typed as an open bag". The
// generated `Payload` is a CLOSED union with no catch-all, so the check below
// is now the stronger one: every not-reconstructed vector's tag must be a tag
// the generated union actually names (`isKnownPayloadTag`, which `tags.ts`
// pins to the union at compile time in both directions). A payload the Rust
// side emits that the bindings never generated now fails here.
// ---------------------------------------------------------------------------

describe("envelope.json against Payload (@codypendent/protocol)", () => {
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
    "Payload_ShutdownIfIdle",
    "Payload_ShutdownRefused",
  ];
  // Not reconstructed here: payloads the extension never sends or reads. The
  // generated union still names every one of them; the check below proves it
  // rather than assuming it.
  const passthrough = [
    "Payload_BlackboardItems",
    "Payload_BlackboardPosted",
    "Payload_DaemonStatusRequest",
    "Payload_DaemonStatusResponse",
    "Payload_DocsCheckCompleted",
    "Payload_DocumentCreated",
    "Payload_DocumentLeaseGranted",
    "Payload_DocumentPublishRequested",
    "Payload_DocumentSync",
    "Payload_FileSearchResults",
    "Payload_PromotionProposed",
    "Payload_SessionList",
    "Payload_WorkflowEvent",
    "Payload_WorkflowRunSnapshot",
    "Payload_WorkflowRunStarted",
  ];

  it("accounts for every Payload vector as reconstructed or explicitly named-only", () => {
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
    it(`${name} carries a tag the generated Payload union names`, () => {
      const original = asRecord(vectors[name], name);
      const tag = str(original, "type");
      expect(isKnownPayloadTag(tag), `${name}: tag '${tag}' is absent from the generated Payload union`).toBe(true);
      // `isKnownPayloadTag` is a type predicate over `PayloadTag`, so this
      // narrowing is the compiler agreeing the tag really is in the union.
      if (!isKnownPayloadTag(tag)) throw new Error(`unreachable: ${tag}`);
      const payload: Payload["type"] = tag;
      expect(payload, name).toBe(tag);
    });
  }
});

// ---------------------------------------------------------------------------
// events.json: Actor (5/5, full coverage) and EventBody (18/19 — this is where
// ProposedAction::ExecuteCommand's S1 fields are exercised inside a realistic
// ApprovalRequested event). The 19th, RunUsage, ships its own vector file and
// is covered by the usage.json block below, so this count is deliberately not
// "all of EventBody" — read the two blocks together.
// ---------------------------------------------------------------------------

describe("events.json against Actor + EventBody (@codypendent/protocol)", () => {
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
// usage.json: EventBody::RunUsage (outcome 20), the measured-usage event. It
// lives in its own vector file rather than in events.json, so this block is
// what keeps it inside the drift suite at all — without it a new wire event
// would be invisible here and the extension's silent `default:` arm would be
// the only thing "handling" it.
// ---------------------------------------------------------------------------

describe("usage.json against EventBody (@codypendent/protocol)", () => {
  const vectors = loadVectors("usage.json");

  for (const name of keysWithPrefix(vectors, "EventBody")) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      const reconstructed = reconstructSessionEvent(asRecord(original, name));
      expectReconstructionMatches(name, original, reconstructed);
    });
  }

  it("keeps an unmeasured field absent rather than defaulting it to zero", () => {
    const original = asRecord(vectors.EventBody_RunUsage, "EventBody_RunUsage");
    const body = reconstructEventBody(rec(original, "body"));
    if (body.type !== "RunUsage") {
      throw new Error(`expected RunUsage, got ${body.type}`);
    }
    expect(body.prompt_tokens).toBe(1002);
    expect(body.completion_tokens).toBe(60);
    // The daemon omits what the provider never measured; a 0 here would assert
    // the run was free, which is a different and false claim.
    expect(body.cost_micros).toBeUndefined();
    expect("cost_micros" in rec(original, "body")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// model.json: `ProbeModel` and its reply. The extension has the types (the SDK
// exports `ModelProbe`/`ModelReadiness`), so the file is modeled here and
// guarded against drift, whether or not a view consumes it yet.
// ---------------------------------------------------------------------------

describe("model.json against model-domain types (@codypendent/protocol)", () => {
  const vectors = loadVectors("model.json");

  function reconstructModelReadiness(r: Record<string, unknown>): ModelReadiness {
    switch (str(r, "state")) {
      case "ready":
        return { state: "ready", detail: str(r, "detail") };
      case "unverified":
        return { state: "unverified", detail: str(r, "detail") };
      case "unavailable": {
        const error =
          r.error === undefined ? undefined : reconstructCodypendentError(rec(r, "error"));
        return { state: "unavailable", detail: str(r, "detail"), error };
      }
      case "unknown":
        return { state: "unknown" };
      default:
        throw new Error(`unknown ModelReadiness state: ${str(r, "state")}`);
    }
  }

  function reconstructModelProbe(r: Record<string, unknown>): ModelProbe {
    return {
      id: str(r, "id"),
      readiness: reconstructModelReadiness(rec(r, "readiness")),
      probed: bool(r, "probed"),
    };
  }

  for (const name of keysWithPrefix(vectors, "ModelReadiness")) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      expectReconstructionMatches(
        name,
        original,
        reconstructModelReadiness(asRecord(original, name)),
      );
    });
  }

  it("decodes and re-encodes ModelProbe_row identically", () => {
    expectReconstructionMatches(
      "ModelProbe_row",
      vectors.ModelProbe_row,
      reconstructModelProbe(asRecord(vectors.ModelProbe_row, "ModelProbe_row")),
    );
  });

  // The command half: `network` is `#[serde(default)]`, so the "all models"
  // vector omits nothing else and the reconstruction must not invent it.
  for (const name of keysWithPrefix(vectors, "CommandBody_ProbeModel")) {
    it(`decodes and re-encodes ${name} identically`, () => {
      const r = asRecord(vectors[name], name);
      const rebuilt: Record<string, unknown> = { type: "ProbeModel", network: bool(r, "network") };
      if (r.model !== undefined) {
        rebuilt.model = str(r, "model");
      }
      expectReconstructionMatches(name, vectors[name], rebuilt);
    });
  }

  it("decodes and re-encodes Payload_ModelProbes identically", () => {
    const r = asRecord(vectors.Payload_ModelProbes, "Payload_ModelProbes");
    expectReconstructionMatches("Payload_ModelProbes", vectors.Payload_ModelProbes, {
      type: "ModelProbes",
      command_id: str(r, "command_id"),
      models: arr(r, "models").map((entry, index) =>
        reconstructModelProbe(asRecord(entry, `Payload_ModelProbes.models[${index}]`)),
      ),
    });
  });
});

// run.json: the small run-domain enums (fully modeled) plus ProposedAction
// (7 of 10 modeled — the extension does not model the 3 workflow-scoped
// variants). ProposedAction_ExecuteCommand here is the S1 vector standalone.
// ---------------------------------------------------------------------------

describe("run.json against run-domain types (@codypendent/protocol)", () => {
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
    "ProposedAction_AcpToolCall",
    "ProposedAction_ExecuteCommand",
    "ProposedAction_GitCommit",
    "ProposedAction_GitHubMutation",
    "ProposedAction_GitPush",
    "ProposedAction_McpToolCall",
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

describe("handshake.json against handshake types (@codypendent/protocol)", () => {
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

describe("catchup.json against Catchup (@codypendent/protocol)", () => {
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

describe("artifact.json against ArtifactRef (@codypendent/protocol)", () => {
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
// error.json: CodypendentError + ProtocolError + UserAction (full coverage).
// The retired mirror typed `CodypendentError.user_action` as a loose
// `{ type: string }`, so its vectors could only be checked for "carries a
// string tag". The generated type is the closed `UserAction` union, so each
// vector is now reconstructed variant-by-variant like every other enum here.
// ---------------------------------------------------------------------------

describe("error.json against CodypendentError / ProtocolError (@codypendent/protocol)", () => {
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
    it(`decodes and re-encodes ${name} identically`, () => {
      const original = vectors[name];
      expectReconstructionMatches(name, original, reconstructUserAction(asRecord(original, name)));
    });
  }
});

// ---------------------------------------------------------------------------
// capabilities.json: ClientCapabilities (full coverage).
// ---------------------------------------------------------------------------

describe("capabilities.json against ClientCapabilities (@codypendent/protocol)", () => {
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

describe("ide.json against IDE-context types (@codypendent/protocol)", () => {
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

// ---------------------------------------------------------------------------
// The file-level "guard the guard". The per-file partition checks above only
// cover vector FILES this suite loads, so a wholly new vector file was
// invisible here: outcome 20 shipped usage.json, memory.json and
// promotion_evidence.json at once, and every assertion in this suite still
// passed while a new wire event went unmodeled. This closes that: every
// committed vector file must be either exercised above or listed as a known
// gap with a reason.
// ---------------------------------------------------------------------------

describe("protocol-vectors/ file inventory", () => {
  /** Vector files some describe block above calls `loadVectors` on. */
  const COVERED_FILES = [
    "artifact_retrieval.json",
    "artifact.json",
    "capabilities.json",
    "catchup.json",
    "command.json",
    "envelope.json",
    "error.json",
    "events.json",
    "handshake.json",
    "ide.json",
    "model.json",
    "run.json",
    "usage.json",
  ];

  /**
   * Wire domains the extension has no TypeScript type for at all, so there is
   * nothing here to drift against. Adding a type for one of these means moving
   * its file up into `COVERED_FILES` with a describe block.
   */
  const UNMODELED_FILES = [
    // No Document/Blackboard/Workflow subscriptions and no InputEnvelope
    // capture path in the extension (see protocol-vectors/README.md).
    "blackboard.json",
    "board.json",
    "document.json",
    "input.json",
    "workflow.json",
    "workflow_graph.json",
    // Daemon-side or CLI-side domains the extension never issues or receives:
    // the code graph (`codypendent graph build/status/show` is a CLI surface;
    // the extension has no CodeGraph* type), session history compaction, the
    // curated-memory triad, promotion evidence, and voice capture.
    "codegraph.json",
    "analytics.json",
    "automation.json",
    "bundle.json",
    "history.json",
    "inbox.json",
    "memory.json",
    // This editor does not browse knowledge cards; the SDK conformance suite covers their full wire shapes.
    "knowledge.json",
    "promotion_evidence.json",
    "voice.json",
    "session.json",
    // The hosted control plane and the remote runner are separate wires the
    // editor client never speaks: it talks to a local daemon over a Unix
    // socket. `@codypendent/protocol` models the daemon wire only, so there is
    // no TypeScript type here for any of these to drift against.
    "control-plane/audit.json",
    "control-plane/auth.json",
    "control-plane/daemon.json",
    "control-plane/error.json",
    "control-plane/events.json",
    "control-plane/identity.json",
    "control-plane/ids.json",
    "control-plane/object_storage.json",
    "control-plane/organization.json",
    "control-plane/page.json",
    "control-plane/publication.json",
    "control-plane/rbac.json",
    "control-plane/repository.json",
    "control-plane/sync.json",
    "control-plane/user.json",
    "control-plane/version.json",
    "control-plane/workload.json",
    "control-plane/workspace.json",
    "runner/attestation.json",
    "runner/job.json",
    "runner/runner.json",
    // Cross-repository federation: campaigns, the federated code graph, blast
    // radius, migration plans, reviewer suggestions and publication policy.
    // The extension has no federation surface at all — it issues none of these
    // commands and renders none of these payloads (nothing under `src/` names
    // a campaign, a federated graph or a publication policy), so there is no
    // TypeScript shape here for the vectors to drift against.
    //
    // These two files carry `CommandBody_*` / `Payload_*` keys for the SAME
    // Rust enums the extension does model, but only the variants it does NOT
    // speak; the partitions in the `command.json` and `envelope.json` describe
    // blocks above still account for every variant that reaches this client.
    // Giving the extension a federation surface means moving these into
    // `COVERED_FILES` with describe blocks.
    "federation/command.json",
    "federation/envelope.json",
    "federation/graph.json",
  ];

  it("accounts for every committed vector file", () => {
    assertPartitionIsComplete("protocol-vectors", listVectorFiles(), COVERED_FILES, UNMODELED_FILES);
  });

  // The walk itself is load-bearing: if it silently stopped descending, the
  // assertion above would keep passing while nested vectors went unguarded.
  it("walks nested vector directories, not just the top level", () => {
    const walked = listVectorFiles();
    const topLevelOnly = readdirSync(VECTORS_DIR, { withFileTypes: true })
      .filter((entry) => entry.isFile() && entry.name.endsWith(".json"))
      .map((entry) => entry.name);
    const nestedDirectories = readdirSync(VECTORS_DIR, { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => entry.name);

    // Every top-level vector is still reported, and nothing outside the tree is.
    expect(walked).toEqual(expect.arrayContaining(topLevelOnly));
    expect(walked.every((name) => name.endsWith(".json"))).toBe(true);
    // Anything the walk found beyond the top level came from a subdirectory,
    // and every subdirectory that holds vectors is represented.
    for (const directory of nestedDirectories) {
      const nested = listVectorFiles(join(VECTORS_DIR, directory), directory);
      expect(walked).toEqual(expect.arrayContaining(nested));
    }
  });
});
