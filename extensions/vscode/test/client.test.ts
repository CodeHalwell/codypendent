import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";
import { describe, expect, it } from "vitest";

import {
  computeBackoff,
  DaemonClient,
  DEFAULT_BACKOFF,
  MAX_QUEUED_COMMANDS,
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

/** The daemon's acknowledgement of a successful attach. */
function catchupPayload(through = 0): Payload {
  return { type: "Catchup", catchup: { type: "Events", from: 0, through, events: [] } };
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

/** Every command a socket has been sent, in order. */
function sentCommands(socket: FakeSocket): ({ type: "Command" } & Command)[] {
  return socket
    .sent()
    .map((e) => e.payload)
    .filter((p): p is { type: "Command" } & Command => p.type === "Command");
}

/** The `approval_id` of every `ResolveApproval` command in `commands`, in order. */
function approvalIds(commands: ({ type: "Command" } & Command)[]): string[] {
  return commands
    .filter(
      (
        c,
      ): c is { type: "Command" } & Command & {
        body: Extract<Command["body"], { type: "ResolveApproval" }>;
      } => c.body.type === "ResolveApproval",
    )
    .map((c) => c.body.approval_id);
}

/** Drive a fresh socket through connect -> ServerHello -> Catchup so any
 * queued commands flush, then return every command it received, in order. */
async function connectAttachAndCollect(
  client: DaemonClient,
  sockets: FakeSocket[],
): Promise<({ type: "Command" } & Command)[]> {
  client.start();
  await flush();
  const socket = sockets[0];
  socket.emit("connect");
  socket.deliver(serverHelloPayload());
  await flush();
  socket.deliver(catchupPayload());
  await flush();
  return sentCommands(socket);
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
    // Attach sent, but not yet acknowledged: the daemon proves a successful
    // attach with a Catchup, and only then is the client "attached".
    expect(client.connectionStatus).toBe("attaching");
    socket.deliver(catchupPayload());
    expect(client.connectionStatus).toBe("attached");

    client.stop();
  });

  it("answers a heartbeat Ping with a Pong so an idle client is not dropped", async () => {
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

    client.start();
    await flush();
    const socket = sockets[0];
    socket.emit("connect");
    socket.deliver(serverHelloPayload());

    const before = socket.sent().length;
    socket.deliver({ type: "Ping" });
    const sent = socket.sent();
    expect(sent.length).toBe(before + 1);
    expect(sent[sent.length - 1].payload.type).toBe("Pong");

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

    // Now a real attach happens on the third socket. Backoff resets only once
    // the daemon acknowledges the attach with a Catchup, not on attach-sent.
    const third = sockets[2];
    third.emit("connect");
    third.deliver(serverHelloPayload());
    expect(third.sent().some((e) => e.payload.type === "Command")).toBe(true);
    third.deliver(catchupPayload());

    // A subsequent drop should start backoff over from the initial delay.
    third.emit("close", false);
    await flush();
    expect(waits[2]).toBe(100);

    client.stop();
  });
});

describe("DaemonClient offline queue + resume token", () => {
  it("queues an approval decision while disconnected and flushes it after the next attach", async () => {
    const sockets: FakeSocket[] = [];
    let releaseWait: (() => void) | undefined;
    const client = new DaemonClient({
      socketPath: "/tmp/does-not-matter.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
      // Park the reconnect loop until the test releases it, so there is a
      // genuine no-socket window to click Approve in.
      wait: () =>
        new Promise<void>((resolve) => {
          releaseWait = resolve;
        }),
    });
    client.start();
    await flush();
    const first = sockets[0];
    first.emit("connect");
    first.deliver(serverHelloPayload());
    await flush();
    first.deliver(catchupPayload(0));
    await flush();
    expect(client.connectionStatus).toBe("attached");

    // The daemon drops; the user clicks Approve during the backoff window.
    first.destroy();
    await flush();
    expect(sockets.length).toBe(1);
    client.resolveApproval("55555555-5555-5555-5555-555555555555", "Approve");

    // Reconnect and attach; the queued decision must flush after the Catchup.
    releaseWait?.();
    await flush();
    const second = sockets[1];
    second.emit("connect");
    second.deliver(serverHelloPayload());
    await flush();
    second.deliver(catchupPayload(0));
    await flush();

    const commands = second
      .sent()
      .map((e) => e.payload)
      .filter((p): p is { type: "Command" } & Command => p.type === "Command");
    const resolve = commands.find((c) => c.body.type === "ResolveApproval");
    expect(resolve, "the queued ResolveApproval must be delivered").toBeDefined();
    if (resolve && resolve.body.type === "ResolveApproval") {
      expect(resolve.body.approval_id).toBe("55555555-5555-5555-5555-555555555555");
      expect(resolve.body.decision).toEqual({ type: "Approve" });
    }
    // Ordering: the attach was sent before the flushed decision.
    const kinds = commands.map((c) => c.body.type);
    expect(kinds.indexOf("AttachSession")).toBeLessThan(kinds.indexOf("ResolveApproval"));
    client.stop();
  });

  it("presents the daemon-minted resume token on the next ClientHello", async () => {
    const sockets: FakeSocket[] = [];
    let releaseWait: (() => void) | undefined;
    const client = new DaemonClient({
      socketPath: "/tmp/does-not-matter.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
      wait: () =>
        new Promise<void>((resolve) => {
          releaseWait = resolve;
        }),
    });
    client.start();
    await flush();
    const first = sockets[0];
    first.emit("connect");
    await flush();
    // The first ClientHello carries no token.
    const firstHello = first.sent().find((e) => e.payload.type === "ClientHello");
    expect(firstHello).toBeDefined();
    expect((firstHello?.payload as { resume_token?: string }).resume_token).toBeUndefined();

    first.deliver({ ...serverHelloPayload(), resume_token: "tok-1" } as Payload);
    await flush();
    first.destroy();
    await flush();

    releaseWait?.();
    await flush();
    const second = sockets[1];
    second.emit("connect");
    await flush();
    const secondHello = second.sent().find((e) => e.payload.type === "ClientHello");
    expect(secondHello).toBeDefined();
    expect((secondHello?.payload as { resume_token?: string }).resume_token).toBe("tok-1");
    client.stop();
  });
});

// FP-4: the offline queue previously gated only on "no socket exists", so a
// decision sent after `connect` but before the daemon acknowledged attach
// (with a Catchup) went straight over the wire to a session that was not
// attached yet, instead of being queued.
describe("DaemonClient queues a decision sent connected-but-pre-attach (FP-4)", () => {
  it("queues a decision sent between connect and Catchup, then flushes it after attach", async () => {
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
    const socket = sockets[0];
    socket.emit("connect");
    await flush();
    // A socket exists and the ClientHello is already out, but the daemon has
    // not even seen ServerHello yet — the connection is not attached.
    expect(client.connectionStatus).toBe("handshaking");

    const approvalId = randomUUID();
    client.resolveApproval(approvalId, "Approve");
    expect(
      sentCommands(socket).some((c) => c.body.type === "ResolveApproval"),
      "the decision must not reach the wire before attach completes",
    ).toBe(false);

    // AttachSession is sent on ServerHello, but the connection is still only
    // "attaching" until the daemon replies with a Catchup — the decision must
    // stay queued through this window too.
    socket.deliver(serverHelloPayload());
    await flush();
    expect(client.connectionStatus).toBe("attaching");
    expect(sentCommands(socket).some((c) => c.body.type === "ResolveApproval")).toBe(false);

    socket.deliver(catchupPayload());
    await flush();
    expect(client.connectionStatus).toBe("attached");

    const commands = sentCommands(socket);
    const resolve = commands.find((c) => c.body.type === "ResolveApproval");
    expect(resolve, "the queued decision must flush once attach completes").toBeDefined();
    if (resolve && resolve.body.type === "ResolveApproval") {
      expect(resolve.body.approval_id).toBe(approvalId);
    }
    // Ordering: the attach command was sent before the flushed decision.
    const kinds = commands.map((c) => c.body.type);
    expect(kinds.indexOf("AttachSession")).toBeLessThan(kinds.indexOf("ResolveApproval"));

    client.stop();
  });

  it("re-queues a decision across a second connection that drops before attaching", async () => {
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
    const first = sockets[0];
    first.emit("connect");
    await flush();

    const approvalId = randomUUID();
    client.resolveApproval(approvalId, "Approve");

    // The connection drops before ever attaching. The decision must survive
    // in the offline queue rather than being lost with the socket.
    first.emit("close", false);
    await flush();
    expect(sockets.length).toBe(2);

    const second = sockets[1];
    second.emit("connect");
    second.deliver(serverHelloPayload());
    await flush();
    second.deliver(catchupPayload());
    await flush();

    const resolve = sentCommands(second).find((c) => c.body.type === "ResolveApproval");
    expect(resolve, "the decision must flush on the next successful attach").toBeDefined();
    if (resolve && resolve.body.type === "ResolveApproval") {
      expect(resolve.body.approval_id).toBe(approvalId);
    }
    client.stop();
  });
});

// FP-5: queue overflow (256) previously dropped the OLDEST queued intent
// unconditionally — possibly an approval decision — near-silently. The fix
// never drops an approval decision: a non-approval is evicted first, and if
// the queue is saturated with nothing but approvals, an incoming approval is
// refused with a dedicated, visible signal instead of evicting one.
describe("DaemonClient offline queue overflow policy (FP-5)", () => {
  it("evicts the oldest non-approval intent to make room, never an approval", async () => {
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
    const errors: string[] = [];
    const approvalsDropped: string[] = [];
    client.on("error", (e) => errors.push(e.message));
    client.on("approvalDropped", ({ approvalId }) => approvalsDropped.push(approvalId));

    const firstApprovalId = randomUUID();
    const secondApprovalId = randomUUID();

    // Fill the queue to exactly its bound: one approval up front, then
    // non-approval StartRun intents for the rest.
    client.resolveApproval(firstApprovalId, "Approve");
    for (let i = 1; i < MAX_QUEUED_COMMANDS; i += 1) {
      client.startRun(`objective ${i}`);
    }

    // One more approval overflows the queue by one command. Fix: evict the
    // oldest NON-approval (a StartRun), never an approval.
    client.resolveApproval(secondApprovalId, "Approve");

    expect(approvalsDropped, "no approval was refused here").toHaveLength(0);
    expect(errors, "exactly one eviction must be reported").toHaveLength(1);
    expect(errors[0]).toContain("StartRun");
    expect(errors[0]).not.toContain("ResolveApproval");

    const commands = await connectAttachAndCollect(client, sockets);
    const approvals = approvalIds(commands);
    // Both approvals survived, in their original relative order.
    expect(approvals).toEqual([firstApprovalId, secondApprovalId]);
    // The queue never grew past its bound (one extra command is the
    // AttachSession the client itself sends to complete the handshake).
    const nonAttach = commands.filter((c) => c.body.type !== "AttachSession");
    expect(nonAttach).toHaveLength(MAX_QUEUED_COMMANDS);

    client.stop();
  });

  it("refuses a new approval, visibly, rather than evict an existing one once the queue is all approvals", async () => {
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
    const errors: string[] = [];
    const approvalsDropped: string[] = [];
    client.on("error", (e) => errors.push(e.message));
    client.on("approvalDropped", ({ approvalId }) => approvalsDropped.push(approvalId));

    const queuedIds = Array.from({ length: MAX_QUEUED_COMMANDS }, () => randomUUID());
    for (const id of queuedIds) {
      client.resolveApproval(id, "Approve");
    }

    // The queue is now saturated with nothing but approvals. One more cannot
    // be safely queued: refuse it, loudly, rather than silently evict an
    // existing approval or grow past the bound.
    const refusedId = randomUUID();
    client.resolveApproval(refusedId, "Approve");

    expect(approvalsDropped, "the refused approval must fire a visible signal").toEqual([
      refusedId,
    ]);
    expect(errors.some((m) => m.includes(refusedId))).toBe(true);

    const commands = await connectAttachAndCollect(client, sockets);
    const approvals = approvalIds(commands);
    // Every originally queued approval survived; the refused one never did,
    // and the queue never grew past its bound.
    expect(approvals).toEqual(queuedIds);
    expect(approvals).not.toContain(refusedId);
    expect(approvals).toHaveLength(MAX_QUEUED_COMMANDS);

    client.stop();
  });

  it("drops an incoming non-approval intent, not an approval, when the queue is all approvals", async () => {
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
    const errors: string[] = [];
    const approvalsDropped: string[] = [];
    client.on("error", (e) => errors.push(e.message));
    client.on("approvalDropped", (info) => approvalsDropped.push(info.approvalId));

    const queuedIds = Array.from({ length: MAX_QUEUED_COMMANDS }, () => randomUUID());
    for (const id of queuedIds) {
      client.resolveApproval(id, "Approve");
    }
    client.startRun("one objective too many");

    expect(approvalsDropped, "a non-approval overflow must never raise approvalDropped").toHaveLength(
      0,
    );
    expect(errors).toHaveLength(1);
    expect(errors[0]).toContain("StartRun");

    const commands = await connectAttachAndCollect(client, sockets);
    expect(commands.some((c) => c.body.type === "StartRun")).toBe(false);
    expect(approvalIds(commands)).toEqual(queuedIds);

    client.stop();
  });
});
