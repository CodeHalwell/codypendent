import { EventEmitter } from "node:events";
import { afterEach, expect, it, vi } from "vitest";
import { NodeSession } from "../src/node.js";
import { encodeEnvelope, FrameDecoder } from "../src/framing.js";
import { PROTOCOL_V1 } from "../src/version.js";
import type { Envelope, Payload } from "../src/envelope.js";

class SessionSocket extends EventEmitter {
  readonly written: Envelope[] = [];
  readonly decoder = new FrameDecoder();
  write(data: Uint8Array): boolean { this.written.push(...this.decoder.push(data)); return true; }
  destroy(): void { this.emit("close"); }
  deliver(payload: Payload, correlationId?: string): void {
    this.emit("data", encodeEnvelope({ protocol_version: PROTOCOL_V1, message_id: "reply", client_id: "client", payload,
      ...(correlationId ? { correlation_id: correlationId } : {}) }));
  }
  hello(): void {
    this.emit("connect");
    this.deliver({ type: "ServerHello", selected_protocol: PROTOCOL_V1, daemon_version: "fixture",
      daemon_instance: "daemon", heartbeat_interval_ms: 15_000, build_id: "fixture" });
  }
}

const sessions: NodeSession[] = [];
afterEach(() => { for (const session of sessions.splice(0)) session.disconnect(); vi.useRealTimers(); });

it("restores unattached session readiness and session discovery after reconnecting", async () => {
  vi.useFakeTimers();
  const sockets: SessionSocket[] = [];
  const session = new NodeSession({ socketPath: "/tmp/codypendent-test.sock", canWrite: () => true,
    onChange: () => {}, onError: () => {},
    createConnection: () => { const socket = new SessionSocket(); sockets.push(socket); return socket; } });
  sessions.push(session);
  const connected = session.connect();
  sockets[0].hello();
  await connected;
  expect(session.snapshot.status).toBe("ready");
  sockets[0].destroy();
  await vi.advanceTimersByTimeAsync(600);
  expect(sockets).toHaveLength(2);
  sockets[1].hello();
  expect(session.snapshot.status).toBe("ready");
  const listing = session.listSessions();
  const request = sockets[1].written.at(-1)!;
  sockets[1].deliver({ type: "SessionSearchResults", command_id: request.message_id,
    page: { items: [], next_cursor: null } }, request.message_id);
  await expect(listing).resolves.toEqual([]);
});
