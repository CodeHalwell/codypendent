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
import {
  encodeEnvelope,
  FrameDecoder,
  PROTOCOL_V1,
  type CommandBody,
  type Envelope,
  type Payload,
  type ServerHello,
  type SessionEvent,
} from "@codypendent/protocol";

/**
 * Every wire type here is the schema-generated one from
 * `@codypendent/protocol` — the same view `DaemonClient` itself decodes into.
 * The extension keeps no second copy of the wire, so nothing on this path is
 * re-viewed or converted.
 */

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

  deliverReply(correlationId: string, payload: Payload): void {
    const envelope: Envelope = {
      protocol_version: PROTOCOL_V1,
      message_id: randomUUID(),
      correlation_id: correlationId,
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
    build_id: "0.1.0+a1b2c3d4e5f6",
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
  return { type: "Catchup", catchup: { type: "Events", from: through + 1, through, events: [] } };
}

const flush = (): Promise<void> => new Promise((resolve) => setImmediate(resolve));

type CommandPayload = Extract<Payload, { type: "Command" }>;

function attachCommand(socket: FakeSocket): Extract<CommandBody, { type: "AttachSession" }> {
  const commands = socket
    .sent()
    .map((e) => e.payload)
    .filter((p): p is CommandPayload => p.type === "Command");
  const attach = commands.find((c) => c.body.type === "AttachSession");
  if (!attach || attach.body.type !== "AttachSession") {
    throw new Error("no AttachSession command was sent");
  }
  return attach.body as Extract<CommandBody, { type: "AttachSession" }>;
}

/** Every command a socket has been sent, in order. */
function sentCommands(socket: FakeSocket): CommandPayload[] {
  return socket
    .sent()
    .map((e) => e.payload)
    .filter((p): p is CommandPayload => p.type === "Command");
}

/** The `approval_id` of every `ResolveApproval` command in `commands`, in order. */
function approvalIds(commands: CommandPayload[]): string[] {
  return commands
    .filter(
      (
        c,
      ): c is CommandPayload & {
        body: Extract<CommandBody, { type: "ResolveApproval" }>;
      } => c.body.type === "ResolveApproval",
    )
    .map((c) => c.body.approval_id);
}

/** Drive a fresh socket through connect -> ServerHello -> Catchup so any
 * queued commands flush, then return every command it received, in order. */
async function connectAttachAndCollect(
  client: DaemonClient,
  sockets: FakeSocket[],
): Promise<CommandPayload[]> {
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

describe("Remote UI lifecycle commands", () => {
  it("queues a confirmed plugin revoke with a stable command envelope", async () => {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/x.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
      wait: () => Promise.resolve(),
    });
    client.revokeUiPlugin("acme.failed-surface");
    const commands = await connectAttachAndCollect(client, sockets);
    expect(commands.find((command) => command.body.type === "RevokeUiPlugin")?.body).toEqual({
      type: "RevokeUiPlugin",
      plugin_id: "acme.failed-surface",
    });
    client.stop();
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
    socket.deliver(catchupPayload());
    socket.deliver(eventPayload(1));
    socket.deliver(eventPayload(2));

    expect(hellos).toHaveLength(1);
    expect(hellos[0].daemon_version).toBe("0.1.0");
    expect(events.map((e) => e.sequence)).toEqual([1, 2]);
    expect(client.sequenceCursor).toBe(2);

    client.stop();
  });

  it("attach_drains_all_history_pages_before_live and deduplicates catch-up overlap", async () => {
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

    const receivedOrder: Array<{ kind: "catchup" | "event"; sequence: number }> = [];
    client.on("catchup", (c) => {
      if (c.type === "Events") {
        for (const ev of c.events) {
          receivedOrder.push({ kind: "catchup", sequence: ev.sequence });
        }
      }
    });
    client.on("event", (e) => {
      receivedOrder.push({ kind: "event", sequence: e.sequence });
    });

    client.start();
    await flush();
    const socket = sockets[0];
    socket.emit("connect");
    socket.deliver(serverHelloPayload());

    // Deliver catchup containing history events 1, 2, 3 through sequence 3
    const historyEvents: SessionEvent[] = [
      { sequence: 1, occurred_at: "2026-07-17T00:00:01Z", actor: { type: "System" }, body: { type: "SessionCreated", title: "Test" } },
      { sequence: 2, occurred_at: "2026-07-17T00:00:02Z", actor: { type: "System" }, body: { type: "NoteAppended", text: "Note" } },
      { sequence: 3, occurred_at: "2026-07-17T00:00:03Z", actor: { type: "System" }, body: { type: "RunStarted", run_id: randomUUID(), objective: "Obj", mode: { type: "Build" } } },
    ];
    socket.deliver({
      type: "Catchup",
      catchup: {
        type: "Events",
        from: 1,
        through: 3,
        events: historyEvents,
      },
    });

    // Deliver overlapping live event 3 (already in history) -> must be deduplicated
    socket.deliver(eventPayload(3));

    // Deliver subsequent live event 4 -> must be received as live event
    socket.deliver(eventPayload(4));

    expect(receivedOrder).toEqual([
      { kind: "catchup", sequence: 1 },
      { kind: "catchup", sequence: 2 },
      { kind: "catchup", sequence: 3 },
      { kind: "event", sequence: 4 },
    ]);
    expect(client.sequenceCursor).toBe(4);

    client.stop();
  });

  it("ignores every payload from an obsolete socket after reconnect", async () => {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/x.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
      wait: () => Promise.resolve(),
    });
    const events: SessionEvent[] = [];
    const hellos: ServerHello[] = [];
    client.on("event", (event) => events.push(event));
    client.on("serverHello", (hello) => hellos.push(hello));

    client.start();
    await flush();
    const stale = sockets[0]!;
    stale.emit("connect");
    stale.destroy();
    await flush();
    const replacement = sockets[1]!;
    replacement.emit("connect");
    const replacementWrites = replacement.written.length;

    stale.deliver(serverHelloPayload());
    stale.deliver(catchupPayload(50));
    stale.deliver(eventPayload(51));
    stale.deliver({ type: "Ping" });
    expect(hellos).toEqual([]);
    expect(events).toEqual([]);
    expect(client.sequenceCursor).toBeUndefined();
    expect(replacement.written).toHaveLength(replacementWrites);

    replacement.deliver(serverHelloPayload());
    replacement.deliver(catchupPayload());
    replacement.deliver(eventPayload(1));
    expect(hellos).toHaveLength(1);
    expect(events.map((event) => event.sequence)).toEqual([1]);
    expect(client.sequenceCursor).toBe(1);
    client.stop();
  });
});

describe("DaemonClient Remote UI transport", () => {
  it("forwards dedicated RemoteUi envelopes only after attachment", async () => {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/ui.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
      wait: () => Promise.resolve(),
    });
    const message = { type: "resync", messageId: "ui-message-1", resync: { documentId: "doc" } };
    expect(client.sendRemoteUi(message)).toBe(false);
    client.start();
    await flush();
    const socket = sockets[0];
    socket.emit("connect");
    socket.deliver(serverHelloPayload());
    socket.deliver(catchupPayload());
    expect(client.sendRemoteUi(message)).toBe(true);
    expect(socket.sent().at(-1)?.payload).toEqual({ type: "RemoteUi", message });
    client.stop();
  });

  it("emits RemoteUi envelopes received from the daemon", async () => {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/ui.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
      wait: () => Promise.resolve(),
    });
    const received: unknown[] = [];
    client.on("remoteUi", (message) => received.push(message));
    client.start();
    await flush();
    const socket = sockets[0];
    socket.emit("connect");
    socket.deliver(serverHelloPayload());
    socket.deliver(catchupPayload());
    const message = { type: "error", messageId: "ui-message-2", error: { code: "ui.test", message: "test error" } };
    socket.deliver({ type: "RemoteUi", message });
    expect(received).toEqual([message]);
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
    first.deliver({ type: "Catchup", catchup: { type: "Snapshot", through: 9, projection: { session_id: SESSION_ID, title: "fixture", last_sequence: 9, closed: false } } });
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

describe("DaemonClient artifact request correlation", () => {
  const artifact = {
    id: "77777777-7777-7777-7777-777777777777",
    media_type: "text/plain",
    byte_length: 3,
    sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    sensitivity: { type: "Internal" as const },
  };

  async function attachedClient(wait: () => Promise<void> = () => Promise.resolve()) {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/artifact.sock", sessionId: SESSION_ID, wait,
      createConnection: () => { const socket = new FakeSocket(); sockets.push(socket); return socket; },
    });
    client.start();
    await flush();
    sockets[0]!.emit("connect");
    sockets[0]!.deliver(serverHelloPayload());
    sockets[0]!.deliver(catchupPayload());
    await flush();
    return { client, sockets };
  }

  it("correlates replies while leaving unmatched replies on the normal event path", async () => {
    const { client, sockets } = await attachedClient();
    const socket = sockets[0]!;
    const events: number[] = [];
    client.on("event", (event) => events.push(event.sequence));
    const read = client.readArtifact(artifact);
    const request = socket.sent().at(-1)!;
    socket.deliverReply(randomUUID(), eventPayload(1));
    socket.deliverReply(request.message_id, {
      type: "ArtifactChunk", artifact_id: artifact.id, offset: 0,
      bytes_base64: "YWJj", eof: true, sha256: artifact.sha256,
    });
    await expect(read).resolves.toEqual(Buffer.from("abc"));
    expect(events).toEqual([1]);
    client.stop();
  });

  it("rejects pending reads on disconnect without letting a stale close reject the replacement", async () => {
    const { client, sockets } = await attachedClient();
    const first = sockets[0]!;
    const firstRead = client.readArtifact(artifact);
    first.emit("close", false);
    await expect(firstRead).rejects.toThrow(/connection closed/i);
    await flush();
    const second = sockets[1]!;
    second.emit("connect"); second.deliver(serverHelloPayload()); second.deliver(catchupPayload());
    const secondRead = client.readArtifact(artifact);
    const request = second.sent().at(-1)!;
    first.emit("close", false);
    second.deliverReply(request.message_id, {
      type: "ArtifactChunk", artifact_id: artifact.id, offset: 0,
      bytes_base64: "YWJj", eof: true, sha256: artifact.sha256,
    });
    await expect(secondRead).resolves.toEqual(Buffer.from("abc"));
    client.stop();
  });

  it("rejects malformed base64 and zero-progress chunks", async () => {
    for (const bytesBase64 of ["%%%", ""]) {
      const { client, sockets } = await attachedClient();
      const read = client.readArtifact(artifact);
      const request = sockets[0]!.sent().at(-1)!;
      sockets[0]!.deliverReply(request.message_id, {
        type: "ArtifactChunk", artifact_id: artifact.id, offset: 0,
        bytes_base64: bytesBase64, eof: false, sha256: artifact.sha256,
      });
      await expect(read).rejects.toThrow(bytesBase64 === "" ? /progress/i : /base64/i);
      client.stop();
    }
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
      .filter((p): p is CommandPayload => p.type === "Command");
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
