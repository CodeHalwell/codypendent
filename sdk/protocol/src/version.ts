/** Mirrors `crates/protocol/src/version.rs`. */

export interface ProtocolVersion {
  major: number;
  minor: number;
}

/**
 * `PROTOCOL_V1` in `version.rs`. Additive changes bump `minor`; breaking
 * changes bump `major` and require negotiation.
 */
export const PROTOCOL_V1: ProtocolVersion = { major: 1, minor: 12 };
export const WORKFLOW_CATALOG_MIN_MINOR = 12;
