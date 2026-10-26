/** Node socket transport for host-neutral daemon sessions. */
import * as net from "node:net";
import { DaemonClient, type DaemonClientOptions, type SocketLike } from "./client.js";
import { resolveSocketPath, validateSocketPath } from "./discovery-node.js";

export class NodeDaemonClient extends DaemonClient {
  constructor(options: DaemonClientOptions = {}) {
    const socketPath = options.socketPath || resolveSocketPath();
    validateSocketPath(socketPath);
    super({
      ...options,
      socketPath,
      createConnection: options.createConnection ??
        ((target) => net.createConnection({ path: target }) as unknown as SocketLike),
    });
  }
}
