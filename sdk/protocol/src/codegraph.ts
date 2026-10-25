/** Mirrors `crates/protocol/src/codegraph.rs`. */

import type { RunId, SessionId } from "./ids.js";

export interface CodeGraphLanguageCount {
  language: string;
  files: number;
  nodes: number;
  edges: number;
}

export interface CodeGraphTally {
  label: string;
  count: number;
}

export interface CodeGraphSkippedExtension {
  extension: string;
  files: number;
}

export interface CodeGraphGrammar {
  language: string;
  extensions: string[];
}

export interface CodeGraphScanReport {
  repository_root: string;
  revision: string;
  files_walked: number;
  files_supported: number;
  files_folded: number;
  files_unsupported: number;
  files_ignored: number;
  nodes: number;
  edges: number;
  by_language: CodeGraphLanguageCount[];
  /** `skip_serializing_if = "Vec::is_empty"`. */
  not_folded?: CodeGraphSkippedExtension[];
  /** `skip_serializing_if = "Vec::is_empty"`. */
  grammars?: CodeGraphGrammar[];
  file_cap: number;
  cap_hit: boolean;
  elapsed_ms: number;
}

export interface CodeGraphStatusView {
  repository_root: string;
  nodes: number;
  edges: number;
  files: number;
  by_language: CodeGraphLanguageCount[];
  by_kind: CodeGraphTally[];
  revisions: CodeGraphTally[];
  head_revision: string;
  working_tree_dirty: boolean;
  stale: boolean;
  stale_reason?: string;
}

/** One-based lines/Unicode-character columns; optional end is exclusive. */
export interface CodeGraphSourceLocation {
  source_path: string;
  line: number;
  column: number;
  end_line?: number;
  end_column?: number;
}

export interface CodeGraphNodeView {
  id: string;
  language: string;
  package?: string;
  source_path?: string;
  qualified_name: string;
  kind: string;
  revision: string;
  location?: CodeGraphSourceLocation;
}

export interface CodeGraphEdgeAssertion {
  session_id: SessionId;
  run_id: RunId;
  rationale: string;
}

export interface CodeGraphEdgeView {
  from_id: string;
  from_name: string;
  to_id: string;
  to_name: string;
  relation: string;
  confidence: number;
  evidence_kind: string;
  revision: string;
  call_site?: CodeGraphSourceLocation;
  asserted_by?: CodeGraphEdgeAssertion;
}

/** Every field is skipped when empty/false/zero, so `{}` is a valid query. */
export interface CodeGraphQuery {
  path?: string;
  language?: string;
  kind?: string;
  name?: string;
  node_id?: string;
  connected_view?: boolean;
  include_neighbors?: boolean;
  include_edges?: boolean;
  include_nodes?: boolean;
  limit?: number;
}

export interface CodeGraphPage {
  nodes: CodeGraphNodeView[];
  edges: CodeGraphEdgeView[];
  related_nodes?: CodeGraphNodeView[];
  total_nodes: number;
  total_edges: number;
  limit: number;
}
