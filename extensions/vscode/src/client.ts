/**
 * DaemonClient: a thin, reconnecting client for the Codypendent daemon over a
 * Unix domain socket.
 *
 * Lifecycle of one connection:
 *   connect -> send `ClientHello` -> receive `ServerHello`
 *           -> send `Command(AttachSession { requested_role: Approver })`
 *           -> receive `Catchup` and a live stream of `Event`s.
 *
 * The client holds NO session state beyond its live connection and
 * `lastSeenSequence`. On disconnect it reconnects with exponential backoff and
 * re-attaches with `last_seen_sequence` set, so the daemon resumes the event
 * stream from where the client left off (a kill/reload recovers purely via
 * attach-resume). This module imports NOTHING from `vscode`, so it is fully
 * unit-testable in plain Node.
 */
import { EventEmitter } from "node:events";
import * as net from "node:net";
import { randomUUID } from "node:crypto";

import { encodeEnvelope, FrameDecoder } from "./protocol/frame.js";
import {
  IDE_CAPABILITIES,
  PROTOCOL_V1,
  type ApprovalDecision,
  type ApprovalScope,
  type AgentMode,
  type Catchup,
  type ClientRole,
  type CodypendentError,
  type Command,
  type CommandBody,
  type Envelope,
  type IdeContextUpdate,
  type Payload,
  type ProtocolError,
  type ServerHello,
  type SessionEvent,
  type Subscription,
  type Uuid,
} from "./protocol/types.js";

/** Minimal duplex-stream surface the client needs (net.Socket satisfies it). */
export interface SocketLike {
  write(data: Uint8Array): boolean;
  on(event: "data", listener: (chunk: Buffer) => void): this;
  on(event: "connect", listener: () => void): this;
  on(event: "close", listener: (hadError: boolean) => void): this;
  on(event: "error", listener: (err: Error) => void): this;
  removeAllListeners(): this;
  destroy(error?: Error): void;
}

/** Factory that opens a connection to a socket path. */
export type ConnectionFactory = (socketPath: string) => SocketLike;

export interface BackoffConfig {
  /** First reconnect delay in ms. */
  initialMs: number;
  /** Ceiling for the delay in ms. */
  maxMs: number;
  /** Multiplier applied per attempt. */
  factor: number;
}

export const DEFAULT_BACKOFF: BackoffConfig = {
  initialMs: 500,
  maxMs: 15_000,
  factor: 2,
};

/**
 * Exponential backoff for reconnect `attempt` (0-based).
 * `delay(attempt) = min(maxMs, initialMs * factor^attempt)`.
 */
export function computeBackoff(attempt: number, config: BackoffConfig = DEFAULT_BACKOFF): number {
  const raw = config.initialMs * Math.pow(config.factor, Math.max(0, attempt));
  return Math.min(config.maxMs, Math.round(raw));
}

export interface DaemonClientOptions {
  socketPath: string;
  sessionId: Uuid;
  /** Stable client identity for the connection lifetime. Generated if absent. */
  clientId?: Uuid;
  clientName?: string;
  clientVersion?: string;
  subscriptions?: Subscription[];
  role?: ClientRole;
  backoff?: BackoffConfig;
  /** Injectable for tests; defaults to `net.createConnection`. */
  createConnection?: ConnectionFactory;
  /** Injectable delay; defaults to `setTimeout`. Tests resolve it immediately. */
  wait?: (ms: number) => Promise<void>;
}

/** Strongly-typed event map the client emits. */
export interface DaemonClientEvents {
  status: (status: ConnectionStatus) => void;
  serverHello: (hello: ServerHello) => void;
  catchup: (catchup: Catchup) => void;
  event: (event: SessionEvent) => void;
  commandAccepted: (info: { command_id: Uuid; sequence?: number }) => void;
  commandRejected: (error: CodypendentError) => void;
  protocolError: (error: ProtocolError) => void;
  error: (error: Error) => void;
}

export type ConnectionStatus =
  | "connecting"
  | "handshaking"
  | "attaching"
  | "attached"
  | "reconnecting"
  | "closed";

// Typed EventEmitter surface without pulling in a dependency.
export interface DaemonClient {
  on<E extends keyof DaemonClientEvents>(event: E, listener: DaemonClientEvents[E]): this;
  once<E extends keyof DaemonClientEvents>(event: E, listener: DaemonClientEvents[E]): this;
  off<E extends keyof DaemonClientEvents>(event: E, listener: DaemonClientEvents[E]): this;
  emit<E extends keyof DaemonClientEvents>(event: E, ...args: Parameters<DaemonClientEvents[E]>): boolean;
}

export class DaemonClient extends EventEmitter {
  private readonly socketPath: string;
  private readonly sessionId: Uuid;
  private readonly clientId: Uuid;
  private readonly clientName: string;
  private readonly clientVersion: string;
  private readonly subscriptions: Subscription[];
  private readonly role: ClientRole;
  private readonly backoff: BackoffConfig;
  private readonly connect: ConnectionFactory;
  private readonly wait: (ms: number) => Promise<void>;

  // The ONLY retained state beyond the live connection: the highest ledger
  // sequence observed. Presented on re-attach so the daemon resumes the stream.
  private lastSeenSequence: number | undefined;

  private socket: SocketLike | undefined;
  private decoder = new FrameDecoder();
  private stopped = false;
  private running = false;
  private status: ConnectionStatus = "closed";

  constructor(options: DaemonClientOptions) {
    super();
    this.socketPath = options.socketPath;
    this.sessionId = options.sessionId;
    this.clientId = options.clientId ?? randomUUID();
    this.clientName = options.clientName ?? "codypendent-vscode";
    this.clientVersion = options.clientVersion ?? "0.1.0";
    this.subscriptions = options.subscriptions ?? [
      { type: "SessionSummary" },
      { type: "AgentActivity" },
    ];
    // Approver, not Contributor: the extension both starts runs AND resolves the
    // approvals it surfaces as native prompts. The daemon gates `ResolveApproval`
    // to Approver/Controller, and Approver is a superset of Contributor's
    // start/submit permissions, so a Contributor default would have every
    // approval response rejected with `protocol.role-denied`.
    this.role = options.role ?? { type: "Approver" };
    this.backoff = options.backoff ?? DEFAULT_BACKOFF;
    this.connect =
      options.createConnection ?? ((p: string) => net.createConnection({ path: p }) as SocketLike);
    this.wait = options.wait ?? ((ms: number) => new Promise((resolve) => setTimeout(resolve, ms)));
  }

  /** The highest ledger sequence observed so far (the resume cursor). */
  get sequenceCursor(): number | undefined {
    return this.lastSeenSequence;
  }

  get connectionStatus(): ConnectionStatus {
    return this.status;
  }

  /** Begin the connect/handshake/attach/reconnect loop. Idempotent. */
  start(): void {
    if (this.running) {
      return;
    }
    this.running = true;
    this.stopped = false;
    void this.runLoop();
  }

  /** Stop for good: close the socket and do not reconnect. */
  stop(): void {
    this.stopped = true;
    this.running = false;
    this.teardownSocket();
    this.setStatus("closed");
  }

  // --- command helpers ------------------------------------------------------

  /** Resolve an approval. Decision `Approve`/`Reject`, default scope `Once`. */
  resolveApproval(
    approvalId: Uuid,
    decision: ApprovalDecision["type"],
    scope: ApprovalScope["type"] = "Once",
  ): void {
    this.sendCommand({
      type: "ResolveApproval",
      approval_id: approvalId,
      decision: { type: decision },
      scope: { type: scope },
    });
  }

  /** Start a run in the attached session. */
  startRun(objective: string, mode: AgentMode["type"] = "Build", repository?: string): void {
    const body: CommandBody = {
      type: "StartRun",
      session_id: this.sessionId,
      objective,
      mode: { type: mode },
    };
    if (repository !== undefined) {
      body.repository = repository;
    }
    this.sendCommand(body);
  }

  /** Submit steering / user input into the attached session. */
  submitUserInput(text: string, mode: AgentMode["type"] = "Build"): void {
    this.sendCommand({
      type: "SubmitUserInput",
      session_id: this.sessionId,
      text,
      mode: { type: mode },
    });
  }

  /** Push a debounced IDE context snapshot (STEP 3.4/3.5 `UpdateIdeContext`). */
  sendIdeContext(update: IdeContextUpdate): void {
    this.sendCommand({
      type: "UpdateIdeContext",
      session_id: this.sessionId,
      update,
    });
  }

  // --- connection loop ------------------------------------------------------

  private async runLoop(): Promise<void> {
    let attempt = 0;
    while (!this.stopped) {
      try {
        await this.connectOnce(() => {
          // A successful attach means the connection is healthy — reset backoff.
          attempt = 0;
        });
      } catch (err) {
        this.emit("error", err instanceof Error ? err : new Error(String(err)));
      }
      if (this.stopped) {
        break;
      }
      const delay = computeBackoff(attempt, this.backoff);
      attempt += 1;
      this.setStatus("reconnecting");
      await this.wait(delay);
    }
    this.running = false;
  }

  /**
   * Open one connection and resolve when it closes. Runs the full handshake and
   * attach; `onAttached` fires once the attach command has been sent.
   */
  private connectOnce(onAttached: () => void): Promise<void> {
    return new Promise<void>((resolve) => {
      this.decoder = new FrameDecoder();
      this.setStatus("connecting");

      let settled = false;
      const settle = (): void => {
        if (settled) {
          return;
        }
        settled = true;
        resolve();
      };

      let socket: SocketLike;
      try {
        socket = this.connect(this.socketPath);
      } catch (err) {
        this.emit("error", err instanceof Error ? err : new Error(String(err)));
        settle();
        return;
      }
      this.socket = socket;

      socket.on("connect", () => {
        this.setStatus("handshaking");
        this.sendClientHello();
      });

      socket.on("data", (chunk: Buffer) => {
        let envelopes: Envelope[];
        try {
          envelopes = this.decoder.push(chunk);
        } catch (err) {
          // A framing violation is fatal for this connection: tear it down and
          // let the reconnect loop resume.
          this.emit("error", err instanceof Error ? err : new Error(String(err)));
          socket.destroy();
          return;
        }
        for (const envelope of envelopes) {
          this.handlePayload(envelope.payload, onAttached);
        }
      });

      socket.on("error", (err: Error) => {
        this.emit("error", err);
      });

      socket.on("close", () => {
        if (this.socket === socket) {
          this.socket = undefined;
        }
        settle();
      });
    });
  }

  private handlePayload(payload: Payload, onAttached: () => void): void {
    switch (payload.type) {
      case "ServerHello": {
        const hello = payload as { type: "ServerHello" } & ServerHello;
        this.emit("serverHello", {
          selected_protocol: hello.selected_protocol,
          daemon_version: hello.daemon_version,
          daemon_instance: hello.daemon_instance,
          heartbeat_interval_ms: hello.heartbeat_interval_ms,
        });
        // Send the attach, but do NOT claim "attached" yet: the daemon proves a
        // successful attach by replying with a `Catchup`. Marking attached (and
        // resetting reconnect backoff) only on that reply avoids showing a live
        // panel for an attach the daemon rejected.
        this.setStatus("attaching");
        this.sendAttach();
        break;
      }
      case "Ping": {
        // Answer the daemon's liveness probe. The daemon stamps liveness only on
        // frames it reads from us and drops a client that is silent past three
        // heartbeat intervals, so an idle panel MUST reply or it is disconnected
        // on a fixed cycle. (Mirrors the Rust clients' Pong reply.)
        this.sendEnvelope(this.buildEnvelope({ type: "Pong" }, { withSession: false }));
        break;
      }
      case "Catchup": {
        const catchup = (payload as { type: "Catchup"; catchup: Catchup }).catchup;
        // A Catchup is the daemon's acknowledgement of a successful attach — only
        // now is the connection truly attached and healthy, so reset backoff here
        // rather than on attach-sent.
        this.setStatus("attached");
        onAttached();
        this.applyCatchup(catchup);
        this.emit("catchup", catchup);
        break;
      }
      case "Event": {
        const event = payload as { type: "Event" } & SessionEvent;
        const sessionEvent: SessionEvent = {
          sequence: event.sequence,
          occurred_at: event.occurred_at,
          causation_id: event.causation_id,
          correlation_id: event.correlation_id,
          actor: event.actor,
          body: event.body,
        };
        this.advanceCursor(sessionEvent.sequence);
        this.emit("event", sessionEvent);
        break;
      }
      case "CommandAccepted": {
        const accepted = payload as { command_id: Uuid; sequence?: number };
        this.emit("commandAccepted", {
          command_id: accepted.command_id,
          sequence: accepted.sequence,
        });
        break;
      }
      case "CommandRejected": {
        this.emit("commandRejected", payload as { type: "CommandRejected" } & CodypendentError);
        break;
      }
      case "Error": {
        this.emit("protocolError", payload as { type: "Error" } & ProtocolError);
        break;
      }
      default:
        // Unknown / unhandled payload tag (Ping/Pong/etc. or a future variant):
        // ignore structurally, exactly as the Rust `Unknown` fallback does.
        break;
    }
  }

  private applyCatchup(catchup: Catchup): void {
    if (catchup.type === "Events") {
      this.advanceCursor(catchup.through);
      for (const event of catchup.events) {
        this.advanceCursor(event.sequence);
      }
    } else if (catchup.type === "Snapshot") {
      this.advanceCursor(catchup.through);
    }
  }

  private advanceCursor(sequence: number): void {
    if (typeof sequence !== "number") {
      return;
    }
    if (this.lastSeenSequence === undefined || sequence > this.lastSeenSequence) {
      this.lastSeenSequence = sequence;
    }
  }

  // --- outbound framing -----------------------------------------------------

  private sendClientHello(): void {
    const payload: Payload = {
      type: "ClientHello",
      client_name: this.clientName,
      client_version: this.clientVersion,
      supported_protocols: [PROTOCOL_V1],
      capabilities: IDE_CAPABILITIES,
      // resume_token omitted (null in Rust) — absent on the wire.
    };
    this.sendEnvelope(this.buildEnvelope(payload, { withSession: false }));
  }

  private sendAttach(): void {
    const body: CommandBody = {
      type: "AttachSession",
      session_id: this.sessionId,
      subscriptions: this.subscriptions,
      requested_role: this.role,
    };
    if (this.lastSeenSequence !== undefined) {
      body.last_seen_sequence = this.lastSeenSequence;
    }
    this.sendCommand(body);
  }

  private sendCommand(body: CommandBody): void {
    const command: Command = {
      command_id: randomUUID(),
      idempotency_key: randomUUID(),
      body,
    };
    const payload: Payload = { type: "Command", ...command };
    this.sendEnvelope(this.buildEnvelope(payload, { withSession: true }));
  }

  private buildEnvelope(payload: Payload, opts: { withSession: boolean }): Envelope {
    const envelope: Envelope = {
      protocol_version: PROTOCOL_V1,
      message_id: randomUUID(),
      client_id: this.clientId,
      payload,
    };
    if (opts.withSession) {
      envelope.session_id = this.sessionId;
    }
    return envelope;
  }

  private sendEnvelope(envelope: Envelope): void {
    if (!this.socket) {
      return;
    }
    try {
      this.socket.write(encodeEnvelope(envelope));
    } catch (err) {
      this.emit("error", err instanceof Error ? err : new Error(String(err)));
    }
  }

  private teardownSocket(): void {
    const socket = this.socket;
    if (socket) {
      // Clear our reference first, then destroy WITHOUT removing listeners, so
      // the socket's own `close` handler still fires and settles the in-flight
      // `connectOnce` promise. Removing listeners here would strand that promise
      // and leak the run loop, so a later `start()` would spawn a second loop
      // beside the zombie.
      this.socket = undefined;
      socket.destroy();
    }
  }

  private setStatus(status: ConnectionStatus): void {
    if (this.status !== status) {
      this.status = status;
      this.emit("status", status);
    }
  }
}
