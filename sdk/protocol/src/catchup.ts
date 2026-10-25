/** Mirrors `crates/protocol/src/catchup.rs`. */

import type { SessionEvent } from "./events.js";
import type { ApprovalId, RunId, SessionId } from "./ids.js";
import type { PendingPromptView, ProposedAction, Risk } from "./run.js";

export interface PendingApprovalProjection {
  approval_id: ApprovalId;
  run_id: RunId;
  action: ProposedAction;
  risk: Risk;
  pattern?: string;
}

export interface SessionProjection {
  session_id: SessionId;
  title: string;
  last_sequence: number;
  /** `skip_serializing_if = "Vec::is_empty"`. */
  active_runs?: RunId[];
  /** `skip_serializing_if = "Vec::is_empty"`. */
  pending_approvals?: PendingApprovalProjection[];
  /** `skip_serializing_if = "Vec::is_empty"`. */
  pending_prompts?: PendingPromptView[];
  closed: boolean;
}

/** `#[serde(tag = "type")]` + `#[serde(other)] Unknown`. */
export type Catchup =
  | { type: "Events"; from: number; through: number; events: SessionEvent[] }
  | { type: "Snapshot"; through: number; projection: SessionProjection }
  | { type: "Unknown" };
