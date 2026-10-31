/**
 * Generated from the authoritative Rust protocol schema.
 * Do not edit by hand; run `npm run generate`.
 */

export type WorkflowCatalogSource =
  | {
      type: "BuiltIn";
    }
  | {
      type: "User";
    }
  | {
      type: "Repository";
    }
  | {
      type: "Unknown";
    };

/**
 * Bounded workflow discovery metadata. No manifest, prompt, or environment is exposed.
 */
export interface WorkflowCatalog {
  diagnostics: WorkflowCatalogDiagnostic[];
  items: WorkflowCatalogEntry[];
  /**
   * Canonical repository used for resolution; reuse it when starting the selected workflow.
   */
  repository?: string | null;
  /**
   * Some sources could not be inspected completely. No potentially shadowed entries are offered.
   */
  truncated: boolean;
}
/**
 * Static diagnostics intentionally exclude parser context and manifest scalar values.
 */
export interface WorkflowCatalogDiagnostic {
  code: string;
  message: string;
  workflow_id?: string | null;
}
export interface WorkflowCatalogEntry {
  /**
   * SHA-256 of the canonical parsed definition, including its inputs and budget.
   */
  definition_digest: string;
  description?: string | null;
  id: string;
  inputs: {
    [k: string]: WorkflowCatalogInput | undefined;
  };
  node_count: number;
  source: WorkflowCatalogSource;
  version: number;
}
/**
 * Input types are domain names, not JSON Schema or an invented control/default.
 */
export interface WorkflowCatalogInput {
  input_type: string;
  required: boolean;
}

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
