import { EventEmitter } from "node:events";
import { afterEach, expect, it, vi } from "vitest";
import { NodeSession } from "../src/node.js";
import { encodeEnvelope, FrameDecoder } from "../src/framing.js";
import { PROTOCOL_V1 } from "../src/version.js";
import type { Envelope, Payload } from "../src/envelope.js";
import type { SessionEvent } from "../src/index.js";

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

it("recovers a completed Plan run's recorded mode after a snapshot-only attach", async () => {
  const sockets: SessionSocket[] = [];
  const errors: string[] = [];
  const session = new NodeSession({ socketPath: "/tmp/codypendent-test.sock", canWrite: () => true,
    onChange: () => {}, onError: error => errors.push(error),
    createConnection: () => { const socket = new SessionSocket(); sockets.push(socket); return socket; } });
  sessions.push(session);
  const recorded: SessionEvent[] = [
    { sequence: 1, occurred_at: "2026-10-31T00:00:00Z", actor: { type: "System" },
      body: { type: "RunStarted", run_id: "plan", objective: "Plan the change", mode: { type: "Plan" } } },
    { sequence: 2, occurred_at: "2026-10-31T00:00:01Z", actor: { type: "System" },
      body: { type: "RunCompleted", run_id: "plan", disposition: { type: "Completed", summary: "The plan" },
        chronicle: { id: "chronicle", media_type: "text/plain", byte_length: 0, sha256: "0".repeat(64), sensitivity: { type: "Internal" } } } },
  ];
  const connected = session.connect({ session_id: "session", title: "Restored", state: "active",
    created_at: "", updated_at: "", last_run_id: "plan" });
  const socket = sockets[0];
  socket.hello();
  socket.deliver({ type: "Catchup", catchup: { type: "Snapshot", through: 2, projection: {
    session_id: "session", title: "Restored", last_sequence: 2, closed: false,
  } } });
  expect(session.snapshot.status).toBe("restoring");
  const request = socket.written.at(-1)!;
  expect(request.payload.type === "Command" && request.payload.body.type).toBe("ReadSessionHistory");
  socket.deliver({ type: "SessionHistory", command_id: request.message_id, session_id: "session",
    page: { items: recorded, next_cursor: null } }, request.message_id);
  await connected;
  expect({ status: session.snapshot.status, runs: session.snapshot.projection.runs, errors }).toEqual({
    status: "attached", errors: [], runs: [{ id: "plan", state: "Completed", objective: "Plan the change",
      mode: "Plan", disposition: { type: "Completed", summary: "The plan" } }],
  });
});
