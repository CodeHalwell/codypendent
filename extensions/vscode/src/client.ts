/**
 * DaemonClient: a thin, reconnecting client for the Codypendent daemon over a
 * Unix domain socket.
 *
 * Lifecycle of one connection:
 *   connect -> send `ClientHello` -> receive `ServerHello`
 *           -> send `Command(AttachSession { requested_role: Approver })`
 *           -> receive `Catchup` and a live stream of `Event`s.
 *
 * This module imports its host-neutral protocol transport from
 * `@codypendent/protocol` and provides the default Node.js Unix socket connection
 * factory (`net.createConnection`).
 */
import {
  type DaemonClientOptions as BaseDaemonClientOptions,
} from "@codypendent/protocol";
import { NodeDaemonClient } from "@codypendent/protocol/node";

export {
  computeBackoff,
  DEFAULT_BACKOFF,
  MAX_QUEUED_COMMANDS,
  encodeEnvelope,
  FrameDecoder,
  FrameError,
  MAX_FRAME_BYTES,
  listInbox,
  listInboxCommand,
  mutateInbox,
  mutateInboxCommand,
  queryAnalytics,
  queryAnalyticsCommand,
  exportAnalytics,
  exportAnalyticsCommand,
  searchSessions,
  searchSessionsCommand,
  SessionSearchPager,
  type BackoffConfig,
  type ConnectionFactory,
  type ConnectionStatus,
  type DaemonClientEvents,
  type SocketLike,
} from "@codypendent/protocol";

export interface DaemonClientOptions extends BaseDaemonClientOptions {
  socketPath: string;
}

export class DaemonClient extends NodeDaemonClient {
  constructor(options: DaemonClientOptions) {
    super({
      clientName: "codypendent-vscode",
      ...options,
    });
  }
}
