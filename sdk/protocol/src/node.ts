/** Node transport, discovery, and session views shared by editor hosts. */
export { NodeDaemonClient } from "./node-client.js";
export * from "./discovery-node.js";
export { SharedSession as NodeSession, type SharedSessionOptions as NodeSessionOptions,
  type SharedSessionState as NodeSessionState } from "./node-session.js";
export { projectSession as projectSessionView, type SessionProjection as SessionViewProjection,
  type ApprovalCard, type QuestionCard, type TranscriptRow } from "./session-view.js";
