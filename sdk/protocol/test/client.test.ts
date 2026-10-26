import { afterEach, describe, expect, it, vi } from "vitest";
import {
  computeBackoff,
  computeSha256,
  DaemonClient,
  DEFAULT_BACKOFF,
  exportAnalytics,
  exportAnalyticsCommand,
  listInbox,
  listInboxCommand,
  MAX_QUEUED_COMMANDS,
  mutateInbox,
  mutateInboxCommand,
  queryAnalytics,
  queryAnalyticsCommand,
  searchSessions,
  searchSessionsCommand,
  SessionSearchPager,
  type SocketLike,
} from "../src/client.js";
import { encodeEnvelope, FrameDecoder } from "../src/framing.js";
import { PROTOCOL_V1 } from "../src/version.js";
import type { Envelope, Payload } from "../src/envelope.js";
import type { Command, CommandBody } from "../src/commands.js";
import type { ServerHello } from "../src/handshake.js";
import type { SessionEvent } from "../src/events.js";
import type { InboxEntry, InboxListQuery, InboxMutation, InboxPage } from "../src/inbox.js";
import type {
  AnalyticsExportRequest,
  AnalyticsExportResult,
  AnalyticsPage,
  AnalyticsQuery,
} from "../src/analytics.js";
import type { SessionSearchPage, SessionSearchQuery } from "../src/session.js";

const flush = (): Promise<void> => new Promise((resolve) => setImmediate(resolve));

/** A controllable in-memory socket satisfying SocketLike. */
class FakeSocket implements SocketLike {
  readonly written: Uint8Array[] = [];
  destroyed = false;
  private readonly listeners = new Map<string, Set<Function>>();

  on(event: string, listener: Function): this {
    let set = this.listeners.get(event);
    if (!set) {
      set = new Set();
      this.listeners.set(event, set);
    }
    set.add(listener);
    return this;
  }

  emit(event: string, ...args: any[]): boolean {
    const set = this.listeners.get(event);
    if (!set) return false;
    for (const l of [...set]) l(...args);
    return true;
  }

  removeAllListeners(): this {
    this.listeners.clear();
    return this;
  }

  write(data: Uint8Array): boolean {
    this.written.push(new Uint8Array(data));
    return true;
  }

  destroy(): void {
    this.destroyed = true;
    this.emit("close", false);
  }

  sent(): Envelope[] {
    const decoder = new FrameDecoder();
    const out: Envelope[] = [];
    for (const chunk of this.written) {
      out.push(...decoder.push(chunk));
    }
    return out;
  }

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
      message_id: "00000000-0000-0000-0000-0000000000bb",
      correlation_id: correlationId,
      client_id: "00000000-0000-0000-0000-0000000000aa",
      payload,
    };
    this.emit("data", encodeEnvelope(envelope));
  }
}

function serverHelloPayload(resumeToken?: string): Payload {
  const hello: ServerHello = {
    selected_protocol: PROTOCOL_V1,
    daemon_version: "0.1.0",
    daemon_instance: "33333333-3333-3333-3333-333333333333",
    heartbeat_interval_ms: 15000,
    build_id: "0.1.0+a1b2c3d4e5f6",
    ...(resumeToken ? { resume_token: resumeToken } : {}),
  };
  return { type: "ServerHello", ...hello };
}

function eventPayload(sequence: number): Payload {
  const event: SessionEvent = {
    sequence,
    occurred_at: "2026-08-16T10:00:00Z",
    actor: { type: "System" },
    body: { type: "SessionClosed" },
  };
  return { type: "Event", ...event };
}

function catchupPayload(from = 1, through = 0, events: SessionEvent[] = []): Payload {
  return { type: "Catchup", catchup: { type: "Events", from, through, events } };
}

const SESSION_ID = "44444444-4444-4444-4444-444444444444";

describe("connection ownership and recovery", () => {
  const clients: DaemonClient[] = [];
  afterEach(() => { for (const client of clients.splice(0)) client.stop(); vi.useRealTimers(); });
  function setup(options: Partial<ConstructorParameters<typeof DaemonClient>[0]> = {}) {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({ sessionId: SESSION_ID,
      createConnection: () => { const socket = new FakeSocket(); sockets.push(socket); return socket; }, ...options });
    clients.push(client);
    client.start();
    const socket = sockets[0];
    socket.emit("connect"); socket.deliver(serverHelloPayload());
    return { client, socket, sockets };
  }

  it("does not resume past an event gap or expose that event to a consumer", () => {
    const { client, socket } = setup();
    socket.deliver(catchupPayload(1, 0));
    const events: number[] = []; client.on("event", event => events.push(event.sequence));
    socket.deliver(eventPayload(2));
    expect(client.sequenceCursor).toBe(0);
    expect(events).toEqual([]);
    expect(socket.destroyed).toBe(true);
  });

  it("rejects an invalid catch-up before announcing attachment or flushing queued commands", () => {
    const { client, socket } = setup();
    client.submitUserInput("queued work");
    socket.deliver(catchupPayload(1, 2, []));
    expect(client.connectionStatus).not.toBe("attached");
    expect(client.sequenceCursor).toBeUndefined();
    expect(socket.sent().some(frame => frame.payload.type === "Command" && frame.payload.body.type === "SubmitUserInput")).toBe(false);
  });

  it("resets cursor, pending requests and queued approvals when switching sessions", async () => {
    const { client, socket, sockets } = setup();
    socket.deliver(catchupPayload(1, 1, [{ sequence: 1, occurred_at: "", actor: { type: "System" }, body: { type: "SessionClosed" } }]));
    const pending = client.request({ type: "ListInbox" });
    client.attachSession("55555555-5555-5555-5555-555555555555");
    await expect(pending).rejects.toThrow("connection closed");
    expect(client.sequenceCursor).toBeUndefined();
    expect(client.store.getSnapshot().events).toEqual([]);
    expect(socket.destroyed).toBe(true);
    const next = sockets[1]; next.emit("connect"); next.deliver(serverHelloPayload());
    const attach = next.sent().find(frame => frame.payload.type === "Command");
    expect(attach?.payload).toMatchObject({ body: { type: "AttachSession", session_id: "55555555-5555-5555-5555-555555555555" } });
    expect(attach && "body" in attach.payload && "last_seen_sequence" in attach.payload.body).toBe(false);
  });

  it("emits pre-catch-up activity only after validation and in sequence order", () => {
    const { client, socket } = setup(); const seen: number[] = [];
    client.on("event", event => seen.push(event.sequence));
    socket.deliver(eventPayload(2)); socket.deliver(eventPayload(1)); socket.deliver(eventPayload(2));
    expect(seen).toEqual([]);
    socket.deliver(catchupPayload(1, 0));
    expect(seen).toEqual([1, 2]);
    expect(client.sequenceCursor).toBe(2);
  });

  it("rejects a daemon's incompatible negotiated version before attaching", () => {
    const socket = new FakeSocket();
    const client = new DaemonClient({ sessionId: SESSION_ID, createConnection: () => socket }); clients.push(client);
    const errors: string[] = []; client.on("protocolError", error => errors.push(error.code));
    client.start(); socket.emit("connect");
    socket.deliver({ ...serverHelloPayload(), selected_protocol: { major: 99, minor: 0 } } as Payload);
    expect(errors).toEqual(["protocol.incompatible-version"]);
    expect(client.connectionStatus).toBe("closed");
    expect(socket.sent().some(frame => frame.payload.type === "Command")).toBe(false);
  });

  it("bounds a silent connection and stop cancels the reconnect timer", async () => {
    vi.useFakeTimers();
    const socket = new FakeSocket();
    const client = new DaemonClient({ createConnection: () => socket, connectionTimeoutMs: 50 }); clients.push(client);
    client.start();
    await vi.advanceTimersByTimeAsync(51);
    expect(socket.destroyed).toBe(true);
    client.stop();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("never lets a previous reconnect loop create a second socket after stop/start", async () => {
    vi.useFakeTimers();
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({ createConnection: () => { const socket = new FakeSocket(); sockets.push(socket); return socket; } }); clients.push(client);
    client.start(); sockets[0].destroy(); await vi.advanceTimersByTimeAsync(1);
    client.stop(); client.start();
    await vi.advanceTimersByTimeAsync(1000);
    expect(sockets).toHaveLength(2);
  });
});

describe("Milestone 2 Task 2.3 Acceptance Tests", () => {
  it("fragmented_frames_reassemble_in_order", async () => {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/codypendent.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
    });

    const receivedHellos: any[] = [];
    const receivedCatchups: any[] = [];
    const receivedEvents: SessionEvent[] = [];

    client.on("serverHello", (h) => receivedHellos.push(h));
    client.on("catchup", (c) => receivedCatchups.push(c));
    client.on("event", (e) => receivedEvents.push(e));

    client.start();
    await flush();

    const socket = sockets[0];
    socket.emit("connect");

    const env1: Envelope = {
      protocol_version: PROTOCOL_V1,
      message_id: "msg-1",
      client_id: "client-1",
      payload: serverHelloPayload(),
    };
    const env2: Envelope = {
      protocol_version: PROTOCOL_V1,
      message_id: "msg-2",
      client_id: "client-1",
      payload: catchupPayload(1, 2, [
        { sequence: 1, occurred_at: "2026-08-16T10:00:00Z", actor: { type: "System" }, body: { type: "SessionClosed" } },
        { sequence: 2, occurred_at: "2026-08-16T10:00:01Z", actor: { type: "System" }, body: { type: "SessionClosed" } },
      ]),
    };
    const env3: Envelope = {
      protocol_version: PROTOCOL_V1,
      message_id: "msg-3",
      client_id: "client-1",
      payload: eventPayload(3),
    };

    const b1 = encodeEnvelope(env1);
    const b2 = encodeEnvelope(env2);
    const b3 = encodeEnvelope(env3);

    const totalLen = b1.length + b2.length + b3.length;
    const combined = new Uint8Array(totalLen);
    combined.set(b1, 0);
    combined.set(b2, b1.length);
    combined.set(b3, b1.length + b2.length);

    // Split across three arbitrary chunk boundaries
    const split1 = Math.floor(totalLen / 3);
    const split2 = Math.floor((2 * totalLen) / 3);

    const chunk1 = combined.subarray(0, split1);
    const chunk2 = combined.subarray(split1, split2);
    const chunk3 = combined.subarray(split2);

    socket.emit("data", chunk1);
    await flush();
    socket.emit("data", chunk2);
    await flush();
    socket.emit("data", chunk3);
    await flush();

    expect(receivedHellos.length).toBe(1);
    expect(receivedCatchups.length).toBe(1);
    expect(receivedEvents.length).toBe(1);
    expect(receivedEvents[0].sequence).toBe(3);

    client.stop();
  });

  it("catchup_overlap_is_deduplicated", async () => {
    const sockets: FakeSocket[] = [];
    const client = new DaemonClient({
      socketPath: "/tmp/codypendent.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
    });

    const receivedLiveEvents: SessionEvent[] = [];
    client.on("event", (e) => receivedLiveEvents.push(e));

    client.start();
    await flush();

    const socket = sockets[0];
    socket.emit("connect");
    socket.deliver(serverHelloPayload());
    await flush();

    // Catch-up delivers seq 1, seq 2, seq 3 through 3
    const catchupEvents: SessionEvent[] = [
      { sequence: 1, occurred_at: "2026-08-16T10:00:00Z", actor: { type: "System" }, body: { type: "SessionClosed" } },
      { sequence: 2, occurred_at: "2026-08-16T10:00:01Z", actor: { type: "System" }, body: { type: "SessionClosed" } },
      { sequence: 3, occurred_at: "2026-08-16T10:00:02Z", actor: { type: "System" }, body: { type: "SessionClosed" } },
    ];
    socket.deliver(catchupPayload(1, 3, catchupEvents));
    await flush();

    // Now deliver overlapping live events seq 2 and seq 3
    socket.deliver(eventPayload(2));
    socket.deliver(eventPayload(3));
    await flush();

    // Now deliver new live events seq 4 and seq 5
    socket.deliver(eventPayload(4));
    socket.deliver(eventPayload(5));
    await flush();

    // Verify live event emission skipped duplicates (seq 2, 3) and yielded seq 4, 5
    expect(receivedLiveEvents.map((e) => e.sequence)).toEqual([4, 5]);

    // Verify session store snapshot contains sequences 1, 2, 3, 4, 5 exactly once
    const snapshot = client.store.getSnapshot();
    expect(snapshot.events.map((e) => e.sequence)).toEqual([1, 2, 3, 4, 5]);
    expect(snapshot.cursor).toBe(5);

    client.stop();
  });

  it("requests_correlate_and_queue_is_bounded", async () => {
    const sockets: FakeSocket[] = [];
    let waitResolve: (() => void) | undefined;
    const client = new DaemonClient({
      socketPath: "/tmp/codypendent.sock",
      sessionId: SESSION_ID,
      createConnection: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
      wait: () => new Promise<void>((resolve) => {
        waitResolve = resolve;
      }),
    });

    client.start();
    await flush();

    const socket1 = sockets[0];
    socket1.emit("connect");
    socket1.deliver(serverHelloPayload("resume-token-123"));
    await flush();
    socket1.deliver(catchupPayload(1, 0, []));
    await flush();

    // 1. Interleaved request/reply correlation by correlation_id
    const p1 = client.listInbox({ limit: 10 });
    const p2 = client.queryAnalytics();
    const p3 = client.searchSessions({ query: "find me" });

    await flush();

    const sent = socket1.sent();
    const commandEnvelopes = sent.filter((e) => e.payload.type === "Command");
    expect(commandEnvelopes.length).toBe(4); // AttachSession + 3 requests

    const req1Env = commandEnvelopes.find((e) => (e.payload as any).body?.type === "ListInbox")!;
    const req2Env = commandEnvelopes.find((e) => (e.payload as any).body?.type === "QueryAnalytics")!;
    const req3Env = commandEnvelopes.find((e) => (e.payload as any).body?.type === "SearchSessions")!;

    expect(req1Env).toBeDefined();
    expect(req2Env).toBeDefined();
    expect(req3Env).toBeDefined();

    // Deliver replies out of order (2, then 3, then 1)
    socket1.deliverReply(req2Env.message_id, {
      type: "AnalyticsResults",
      command_id: "cmd-2",
      page: { items: [], next_cursor: null },
    });
    const r2 = await p2;
    expect(r2.items).toEqual([]);

    socket1.deliverReply(req3Env.message_id, {
      type: "SessionSearchResults",
      command_id: "cmd-3",
      page: { items: [], next_cursor: null },
    });
    const r3 = await p3;
    expect(r3.items).toEqual([]);

    socket1.deliverReply(req1Env.message_id, {
      type: "InboxPage",
      command_id: "cmd-1",
      page: { items: [], next_cursor: null },
    });
    const r1 = await p1;
    expect(r1.items).toEqual([]);

    // 2. Disconnect and test bounded offline queue
    socket1.destroy();
    await flush();

    // Sockets is now disconnected. Queue commands up to limit.
    const droppedApprovals: string[] = [];
    client.on("approvalDropped", (info) => droppedApprovals.push(info.approvalId));

    // Queue 250 regular commands
    for (let i = 0; i < 250; i++) {
      client.submitUserInput(`input-${i}`);
    }
    // Queue 6 approvals (total 256)
    for (let i = 0; i < 6; i++) {
      client.resolveApproval(`app-${i}`, "Approve");
    }

    // Adding more non-approvals evicts oldest non-approvals
    client.submitUserInput("extra-input");
    expect(droppedApprovals.length).toBe(0);

    // Fill the queue completely with 256 approvals
    for (let i = 6; i < 260; i++) {
      client.resolveApproval(`app-${i}`, "Approve");
    }

    // Now queue has 256 approvals. Adding an approval should drop and emit approvalDropped
    client.resolveApproval("app-overflow", "Approve");
    expect(droppedApprovals).toContain("app-overflow");

    // Adding non-approval when full of approvals is dropped quietly
    client.submitUserInput("dropped-input");

    // 3. Reconnect replays the resume token and flushes the bounded queue
    if (waitResolve) {
      waitResolve();
      waitResolve = undefined;
    }
    await flush();

    expect(sockets.length).toBe(2);
    const socket2 = sockets[1];
    socket2.emit("connect");

    await flush();

    // Check that next ClientHello presents the stored resume token
    const helloEnv = socket2.sent().find((e) => e.payload.type === "ClientHello");
    expect((helloEnv?.payload as any).resume_token).toBe("resume-token-123");

    // ServerHello + Catchup causes offline queue to flush
    socket2.deliver(serverHelloPayload("resume-token-456"));
    await flush();
    socket2.deliver(catchupPayload(1, 0, []));
    await flush();

    const flushedCommands = socket2
      .sent()
      .map((e) => e.payload)
      .filter((p): p is { type: "Command" } & Command => p.type === "Command");

    // Verify commands flushed (AttachSession + 256 queued approvals)
    expect(flushedCommands.length).toBe(257);

    client.stop();
  });
});

describe("Session search paging state", () => {
  it("resets cursor when query or filters change", async () => {
    const queries: SessionSearchQuery[] = [];
    const caller = async (body: CommandBody): Promise<Payload> => {
      if (body.type === "SearchSessions") {
        queries.push(body.query);
        return {
          type: "SessionSearchResults",
          command_id: "cmd-search",
          page: {
            items: [],
            next_cursor: `cursor-for-${body.query.query}`,
          },
        };
      }
      throw new Error(`unexpected command ${body.type}`);
    };

    const pager = new SessionSearchPager(caller);

    // Initial search
    const p1 = await pager.search({ query: "alpha" });
    expect(queries[0].cursor).toBeUndefined();
    expect(p1.next_cursor).toBe("cursor-for-alpha");

    // Next page with same query uses previous cursor
    const p2 = await pager.search({ query: "alpha" });
    expect(queries[1].cursor).toBe("cursor-for-alpha");

    // Query changes -> cursor is reset
    const p3 = await pager.search({ query: "beta" });
    expect(queries[2].cursor).toBeUndefined();
    expect(p3.next_cursor).toBe("cursor-for-beta");

    // Filter changes with same query string -> cursor is reset
    const p4 = await pager.search({ query: "beta", filters: { repository_ids: ["repo-1"] } });
    expect(queries[3].cursor).toBeUndefined();
  });
});

describe("protocol client helpers", () => {
  it("uses the Node SHA-256 fallback when Web Crypto is unavailable", async () => {
    const cryptoDescriptor = Object.getOwnPropertyDescriptor(globalThis, "crypto");
    try {
      Object.defineProperty(globalThis, "crypto", {
        configurable: true,
        value: undefined,
      });

      await expect(computeSha256(new Uint8Array())).resolves.toBe(
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
      );
    } finally {
      if (cryptoDescriptor) {
        Object.defineProperty(globalThis, "crypto", cryptoDescriptor);
      } else {
        delete (globalThis as { crypto?: Crypto }).crypto;
      }
    }
  });

  const sampleEntry: InboxEntry = {
    id: "entry-1",
    repository_id: "repo-1",
    kind: { type: "ApprovalRequest" },
    state: { type: "Unread" },
    title: "Approve migration",
    summary: "Pending DB migration",
    deep_link: { type: "Approval", approval_id: "app-1" },
    source: {
      dedup_key: "approval:app-1",
      identity: { type: "Approval", approval_id: "app-1" },
    },
    created_at: "2026-08-16T10:00:00Z",
  };

  const sampleInboxPage: InboxPage = {
    items: [sampleEntry],
    next_cursor: "cursor-123",
  };

  const sampleAnalyticsPage: AnalyticsPage = {
    items: [
      {
        dimensions: ["gemini-1.5-pro"],
        metrics: {
          input_tokens: 1500,
          output_tokens: 300,
          cost_micros: 2500,
          latency_ms: 450,
          coverage: {
            input_tokens: { measured: 1, total: 1 },
            output_tokens: { measured: 1, total: 1 },
            cached_tokens: { measured: 0, total: 1 },
            reasoning_tokens: { measured: 0, total: 1 },
            cost: { measured: 1, total: 1 },
            cost_per_successful_task: { measured: 1, total: 1 },
            latency: { measured: 1, total: 1 },
            retry_count: { measured: 1, total: 1 },
            escalation_count: { measured: 1, total: 1 },
            grader_score: { measured: 0, total: 1 },
            completion_count: { measured: 1, total: 1 },
          },
        },
      },
    ],
    next_cursor: null,
  };

  const sampleExportResult: AnalyticsExportResult = {
    artifact: {
      id: "art-1",
      byte_length: 1024,
      media_type: "application/json",
      sensitivity: { type: "Internal" },
      sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    },
    format: { type: "json" },
    generated_at: "2026-08-16T10:05:00Z",
    row_count: 10,
    truncated: false,
  };

  describe("command builders", () => {
    it("builds listInbox command with and without query", () => {
      expect(listInboxCommand()).toEqual({ type: "ListInbox" });
      const query: InboxListQuery = { limit: 20, filters: { states: [{ type: "Unread" }] } };
      expect(listInboxCommand(query)).toEqual({ type: "ListInbox", query });
    });

    it("builds mutateInbox command", () => {
      const mutation: InboxMutation = { type: "Acknowledge", entry_id: "entry-1" };
      expect(mutateInboxCommand(mutation)).toEqual({ type: "MutateInbox", mutation });
    });

    it("builds queryAnalytics command", () => {
      expect(queryAnalyticsCommand()).toEqual({ type: "QueryAnalytics" });
      const query: AnalyticsQuery = { group_by: [{ type: "model" }], limit: 50 };
      expect(queryAnalyticsCommand(query)).toEqual({ type: "QueryAnalytics", query });
    });

    it("builds exportAnalytics command", () => {
      const request: AnalyticsExportRequest = {
        format: { type: "json" },
        query: { limit: 100 },
      };
      expect(exportAnalyticsCommand(request)).toEqual({ type: "ExportAnalytics", request });
    });

    it("builds searchSessions command", () => {
      const query: SessionSearchQuery = { query: "auth" };
      expect(searchSessionsCommand(query)).toEqual({ type: "SearchSessions", query });
    });
  });

  describe("listInbox execution", () => {
    it("returns InboxPage when payload is InboxPage", async () => {
      const caller = async (body: CommandBody): Promise<Payload> => {
        expect(body.type).toBe("ListInbox");
        return { type: "InboxPage", command_id: "cmd-1", page: sampleInboxPage };
      };

      const result = await listInbox(caller, { limit: 10 });
      expect(result).toEqual(sampleInboxPage);
    });

    it("works with caller object implementing request method", async () => {
      const callerObj = {
        request: async (body: CommandBody): Promise<Payload> => {
          expect(body.type).toBe("ListInbox");
          return { type: "InboxPage", command_id: "cmd-1", page: sampleInboxPage };
        },
      };

      const result = await listInbox(callerObj);
      expect(result.items.length).toBe(1);
    });

    it("throws error when command is rejected", async () => {
      const caller = async (): Promise<Payload> => ({
        type: "CommandRejected",
        correlation_id: "cmd-1",
        code: "inbox.query-failed",
        message: "database is locked",
        retryable: true,
      });

      await expect(listInbox(caller)).rejects.toThrow("inbox.query-failed: database is locked");
    });
  });

  describe("mutateInbox execution", () => {
    it("returns mutated entry when payload is InboxEntryApplied", async () => {
      const mutation: InboxMutation = { type: "Acknowledge", entry_id: "entry-1" };
      const caller = async (body: CommandBody): Promise<Payload> => {
        expect(body).toEqual({ type: "MutateInbox", mutation });
        return {
          type: "InboxEntryApplied",
          command_id: "cmd-1",
          entry: { ...sampleEntry, state: { type: "Acknowledged" }, acknowledged_at: "2026-08-16T10:01:00Z" },
        };
      };

      const result = await mutateInbox(caller, mutation);
      expect(result.state).toEqual({ type: "Acknowledged" });
      expect(result.acknowledged_at).toBe("2026-08-16T10:01:00Z");
    });

    it("throws error for unexpected payload", async () => {
      const caller = async (): Promise<Payload> => ({ type: "Ping" });
      await expect(mutateInbox(caller, { type: "Dismiss", entry_id: "entry-1" })).rejects.toThrow(
        "unexpected response payload: Ping",
      );
    });
  });

  describe("queryAnalytics execution", () => {
    it("returns AnalyticsPage when payload is AnalyticsResults", async () => {
      const caller = async (body: CommandBody): Promise<Payload> => {
        expect(body.type).toBe("QueryAnalytics");
        return { type: "AnalyticsResults", command_id: "cmd-1", page: sampleAnalyticsPage };
      };

      const result = await queryAnalytics(caller, { group_by: [{ type: "model" }] });
      expect(result.items.length).toBe(1);
      expect(result.items[0].metrics.input_tokens).toBe(1500);
    });

    it("throws error when query analytics fails", async () => {
      const caller = async (): Promise<Payload> => ({
        type: "Error",
        code: "analytics.unsupported-grouping",
        message: "unknown grouping type",
        retryable: false,
      });

      await expect(queryAnalytics(caller)).rejects.toThrow("analytics.unsupported-grouping: unknown grouping type");
    });
  });

  describe("exportAnalytics execution", () => {
    it("returns export result when payload is AnalyticsExported", async () => {
      const request: AnalyticsExportRequest = { format: { type: "csv" }, query: {} };
      const caller = async (body: CommandBody): Promise<Payload> => {
        expect(body).toEqual({ type: "ExportAnalytics", request });
        return { type: "AnalyticsExported", command_id: "cmd-1", result: sampleExportResult };
      };

      const result = await exportAnalytics(caller, request);
      expect(result.artifact.id).toBe("art-1");
      expect(result.row_count).toBe(10);
    });
  });

  describe("computeBackoff", () => {
    it("computes backoff with exponential growth and clamp", () => {
      expect(computeBackoff(0, DEFAULT_BACKOFF)).toBe(500);
      expect(computeBackoff(1, DEFAULT_BACKOFF)).toBe(1000);
      expect(computeBackoff(2, DEFAULT_BACKOFF)).toBe(2000);
      expect(computeBackoff(3, DEFAULT_BACKOFF)).toBe(4000);
      expect(computeBackoff(10, DEFAULT_BACKOFF)).toBe(15000);
    });
  });
});
