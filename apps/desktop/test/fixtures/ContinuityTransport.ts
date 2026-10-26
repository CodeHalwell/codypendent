import type { SessionEvent, SessionSummary } from "@codypendent/protocol";
import type { ConnectionInfo, DaemonFrame, RunHandle } from "../../src/transport.js";

/** Synthetic transport for conversation acceptance. No native calls or providers. */
export class ContinuityTransport {
  readonly starts: Array<{ objective: string; sessionId: string | null }> = [];
  readonly histories = new Map<string, SessionEvent[]>();
  readonly sessions: SessionSummary[] = [];
  holdNext = false;
  failNext = false;
  private frames: ((frame: DaemonFrame) => void) | null = null;
  private serial = 0;
  private runSerial = 0;
  private accept: (() => void) | null = null;

  async connect(onFrame: (frame: DaemonFrame) => void): Promise<ConnectionInfo> {
    this.frames = onFrame;
    return { socket_path: "/synthetic/daemon.sock", protocol_version: "1.10", daemon_version: "0.14.0", daemon_instance: "continuity-fixture", build_id: "synthetic" };
  }
  async disconnect() { this.frames = null; }
  async socketPath() { return "/synthetic/daemon.sock"; }
  async listSessions() { return this.sessions; }
  async listInbox() { return { items: [], next_cursor: null }; }
  async queryAnalytics() { return { items: [], next_cursor: null }; }
  async attachSession(sessionId: string) {
    const events = this.histories.get(sessionId) ?? [];
    this.frames?.({ kind: "history", session_id: sessionId, through: events.length, events });
  }
  emit(sessionId: string, body: object) {
    const history = this.histories.get(sessionId) ?? [];
    const event = { sequence: history.length + 1, actor: { type: "System" }, occurred_at: "2026-10-04T22:00:00Z", body } as SessionEvent;
    this.histories.set(sessionId, [...history, event]);
    this.frames?.({ kind: "event", session_id: sessionId, event });
  }
  async startObjective(objective: string, sessionId: string | null = null): Promise<RunHandle> {
    this.starts.push({ objective, sessionId });
    if (this.failNext) { this.failNext = false; throw new Error("Synthetic objective refused"); }
    const session = sessionId ?? `session-${++this.serial}`;
    const run = `run-${++this.runSerial}`;
    if (!this.sessions.some((item) => item.session_id === session)) {
      this.sessions.push({ session_id: session, title: objective, state: "open", created_at: "2026-10-04T22:00:00Z", updated_at: "2026-10-04T22:00:00Z" });
    }
    this.emit(session, { type: "RunStarted", run_id: run, objective, mode: { type: "Build" } });
    if (this.holdNext) {
      this.holdNext = false;
      await new Promise<void>((resolve) => { this.accept = resolve; });
    }
    this.emit(session, { type: "ModelStreamDelta", run_id: run, text: `Reply to: ${objective}` });
    this.emit(session, { type: "RunStateChanged", run_id: run, state: { type: "Completed" } });
    this.emit(session, { type: "RunCompleted", run_id: run, disposition: { type: "Completed" } });
    return { session_id: session, run_id: run };
  }
  release() { this.accept?.(); this.accept = null; }
}
