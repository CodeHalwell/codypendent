import { EventEmitter } from "node:events";
import { describe, expect, it } from "vitest";

import {
  computeBackoff,
  DaemonClient,
  DEFAULT_BACKOFF,
  type SocketLike,
} from "../src/client.js";
import { encodeEnvelope, FrameDecoder } from "../src/protocol/frame.js";
import {
  PROTOCOL_V1,
  type Command,
  type Envelope,
  type Payload,
  type ServerHello,
  type SessionEvent,
} from "../src/protocol/types.js";

/** A controllable in-memory socket that satisfies {@link SocketLike}. */
class FakeSocket extends EventEmitter implements SocketLike {
  readonly written: Buffer[] = [];
  destroyed = false;

  write(data: Uint8Array): boolean {
    this.written.push(Buffer.from(data));
    return true;
  }

  destroy(): void {
    this.destroyed = true;
    this.emit("close", false);
  }

  /** Decode every envelope this side has written so far. */
  sent(): Envelope[] {
    const decoder = new FrameDecoder();
    const out: Envelope[] = [];
    for (const chunk of this.written) {
      out.push(...decoder.push(chunk));
    }
    return out;
  }

  /** Simulate the daemon sending one envelope to the client. */
  deliver(payload: Payload): void {
    const envelope: Envelope = {
      protocol_version: PROTOCOL_V1,
      message_id: "00000000-0000-0000-0000-0000000000ff",
      client_id: "00000000-0000-0000-0000-0000000000aa",
      payload,
    };
    this.emit("data", encodeEnvelope(envelope));
  }
}

function serverHelloPayload(): Payload {
  const hello: ServerHello = {
    selected_protocol: PROTOCOL_V1,
    daemon_version: "0.1.0",
    daemon_instance: "33333333-3333-3333-3333-333333333333",
    heartbeat_interval_ms: 15000,
  };
  return { type: "ServerHello", ...hello };
}

function eventPayload(sequence: number): Payload {
  const event: SessionEvent = {
    sequence,
    occurred_at: "2026-07-17T00:00:00Z",
    actor: { type: "System" },
    body: { type: "SessionClosed" },
  };
  return { type: "Event", ...event };
}

const flush = (): Promise<void> => new Promise((resolve) => setImmediate(resolve));

function attachCommand(socket: FakeSocket): Extract<Command["body"], { type: "AttachSession" }> {
  const commands = socket
    .sent()
    .map((e) => e.payload)
    .filter((p): p is { type: "Command" } & Command => p.type === "Command");
  const attach = commands.find((c) => c.body.type === "AttachSession");
  if (!attach || attach.body.type !== "AttachSession") {
    throw new Error("no AttachSession command was sent");
  }
  return attach.body;
}

const SESSION_ID = "44444444-4444-4444-4444-444444444444";

describe("computeBackoff", () => {
  it("grows exponentially from the initial delay", () => {
    expect(computeBackoff(0, DEFAULT_BACKOFF)).toBe(500);
    expect(computeBackoff(1, DEFAULT_BACKOFF)).toBe(1000);
    expect(computeBackoff(2, DEFAULT_BACKOFF)).toBe(2000);
    expect(computeBackoff(3, DEFAULT_BACKOFF)).toBe(4000);
  });

  it("caps at maxMs", () => {
    const cfg = { initialMs: 500, maxMs: 15000, factor: 2 };
    expect(computeBackoff(10, cfg)).toBe(15000);
    expect(computeBackoff(100, cfg)).toBe(15000);
  });

  it("treats negative attempts as attempt 0", () => {
    expect(computeBackoff(-5, DEFAULT_BACKOFF)).toBe(500);
  });
});

describe("DaemonClient handshake + attach", () => {
  it("sends ClientHello on connect, then attaches as Approver with no resume cursor", async () => {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/does-not-matter.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
      wait: () => Promise.resolve(),
    });

    client.start();
    await flush();
    expect(sockets).toHaveLength(1);
    const socket = sockets[0];

    socket.emit("connect");
    // First frame out is the ClientHello.
    const first = socket.sent()[0].payload;
    expect(first.type).toBe("ClientHello");
    expect((first as { supported_protocols: unknown }).supported_protocols).toEqual([PROTOCOL_V1]);

    // Daemon answers with ServerHello -> client attaches.
    socket.deliver(serverHelloPayload());
    const attach = attachCommand(socket);
    expect(attach.session_id).toBe(SESSION_ID);
    expect(attach.requested_role).toEqual({ type: "Approver" });
    expect(attach.last_seen_sequence).toBeUndefined();
    expect(client.connectionStatus).toBe("attached");

    client.stop();
  });

  it("emits a typed serverHello and streams events, advancing the resume cursor", async () => {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/x.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
      wait: () => Promise.resolve(),
    });

    const hellos: ServerHello[] = [];
    const events: SessionEvent[] = [];
    client.on("serverHello", (h) => hellos.push(h));
    client.on("event", (e) => events.push(e));

    client.start();
    await flush();
    const socket = sockets[0];
    socket.emit("connect");
    socket.deliver(serverHelloPayload());
    socket.deliver(eventPayload(3));
    socket.deliver(eventPayload(5));

    expect(hellos).toHaveLength(1);
    expect(hellos[0].daemon_version).toBe("0.1.0");
    expect(events.map((e) => e.sequence)).toEqual([3, 5]);
    expect(client.sequenceCursor).toBe(5);

    client.stop();
  });
});

describe("DaemonClient reconnect with resume", () => {
  it("reconnects with backoff and re-attaches with last_seen_sequence", async () => {
    const sockets: FakeSocket[] = [];
    const waits: number[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/x.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
      wait: (ms) => {
        waits.push(ms);
        return Promise.resolve();
      },
    });

    client.start();
    await flush();

    // First connection: handshake, attach, observe up to sequence 9.
    const first = sockets[0];
    first.emit("connect");
    first.deliver(serverHelloPayload());
    expect(attachCommand(first).last_seen_sequence).toBeUndefined();
    first.deliver(eventPayload(9));
    expect(client.sequenceCursor).toBe(9);

    // The daemon (or a kill/reload) drops the connection.
    first.emit("close", false);
    await flush();

    // A second connection is opened after one backoff delay.
    expect(sockets).toHaveLength(2);
    expect(waits[0]).toBe(computeBackoff(0));
    const second = sockets[1];
    second.emit("connect");
    second.deliver(serverHelloPayload());

    // The re-attach resumes from the last-seen sequence.
    const resumed = attachCommand(second);
    expect(resumed.last_seen_sequence).toBe(9);
    expect(resumed.requested_role).toEqual({ type: "Approver" });

    client.stop();
  });

  it("resets backoff after a healthy attach so a later drop waits the initial delay again", async () => {
    const sockets: FakeSocket[] = [];
    const waits: number[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/x.sock",
      sessionId: SESSION_ID,
      backoff: { initialMs: 100, maxMs: 800, factor: 2 },
      createConnection: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
      wait: (ms) => {
        waits.push(ms);
        return Promise.resolve();
      },
    });

    client.start();
    await flush();

    // Fail before ever attaching, twice: backoff should escalate 100 -> 200.
    sockets[0].emit("close", false);
    await flush();
    sockets[1].emit("close", false);
    await flush();
    expect(waits).toEqual([100, 200]);

    // Now a real attach happens on the third socket.
    const third = sockets[2];
    third.emit("connect");
    third.deliver(serverHelloPayload());
    expect(third.sent().some((e) => e.payload.type === "Command")).toBe(true);

    // A subsequent drop should start backoff over from the initial delay.
    third.emit("close", false);
    await flush();
    expect(waits[2]).toBe(100);

    client.stop();
  });
});
