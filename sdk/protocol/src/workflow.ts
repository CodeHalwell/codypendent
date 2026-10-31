/** Mirrors `crates/protocol/src/workflow.rs`. */

import type { JsonValue } from "./ids.js";
export type { WorkflowCatalog, WorkflowCatalogEntry, WorkflowCatalogInput, WorkflowCatalogSource,
  WorkflowCatalogDiagnostic } from "./generated/workflow-catalog.js";

/** `#[serde(tag = "type")]` + `#[serde(other)] Unknown`. */
export type WorkflowNodeState =
  | { type: "Pending" }
  | { type: "Running" }
  | { type: "WaitingApproval" }
  | { type: "Blocked" }
  | { type: "Completed" }
  | { type: "Failed" }
  | { type: "Skipped" }
  | { type: "Unknown" };

/** `#[serde(tag = "type")]` + `#[serde(other)] Unknown`. */
export type WorkflowRunPhase =
  | { type: "Pending" }
  | { type: "Running" }
  | { type: "Paused" }
  | { type: "Completed" }
  | { type: "Failed" }
  | { type: "Cancelled" }
  | { type: "Unknown" };

export interface WorkflowNodeView {
  workflow_run_id: string;
  node_id: string;
  state: WorkflowNodeState;
  attempt: number;
  cost?: JsonValue;
  error?: string | null;
  /** `skip_serializing_if = "Vec::is_empty"`. */
  warnings?: string[];
  /** `skip_serializing_if = "Vec::is_empty"`. */
  depends_on?: string[];
}

export interface WorkflowRunSnapshot {
  workflow_run_id: string;
  phase: WorkflowRunPhase;
  nodes: WorkflowNodeView[];
}

/**
 * `#[serde(tag = "type")]` + `#[serde(other)] Unknown`. `NodeTransitioned` is a
 * newtype variant over {@link WorkflowNodeView}, so its fields are flattened
 * alongside the tag.
 */
export type WorkflowEvent =
  | ({ type: "NodeTransitioned" } & WorkflowNodeView)
  | { type: "RunPhaseChanged"; workflow_run_id: string; phase: WorkflowRunPhase }
  | { type: "Unknown" };
