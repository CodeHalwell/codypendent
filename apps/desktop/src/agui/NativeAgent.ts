import { AbstractAgent, randomUUID, type AgentConfig, type AgentSubscriber, type RunAgentParameters, type RunAgentResult } from "@ag-ui/client";
import { contentToText, EventType, type BaseEvent, type RunAgentInput } from "@ag-ui/core";
import type { EventBody, SessionEvent } from "@codypendent/protocol";
import { Observable, type Subscriber } from "rxjs";
import { subscribeToFrames } from "../frameBus.js";
import type { DaemonFrame } from "../transport.js";

export interface NativeAgentConfig extends AgentConfig {
  submit: (objective: string) => Promise<boolean>;
  sessionId?: () => string | null;
}

class Acceptance {
  readonly promise: Promise<boolean>;
  settled = false;
  private resolve!: (accepted: boolean) => void;

  constructor() {
    this.promise = new Promise((resolve) => { this.resolve = resolve; });
  }

  settle(accepted: boolean) {
    if (this.settled) return;
    this.settled = true;
    this.resolve(accepted);
  }
}

export class NativeAgent extends AbstractAgent {
  private readonly submitNative: NativeAgentConfig["submit"];
  private readonly sessionId: () => string | null;
  private readonly sequences = new Map<string | null, number>();
  private readonly nativeRuns = new Set<string>();
  private unsubscribe: (() => void) | null = null;
  private active: NativeRun | null = null;
  private acceptance: Acceptance | null = null;
  private execution: Promise<RunAgentResult> | null = null;
  private generation = 0;
  private requestGeneration: number | null = null;

  constructor({ submit, sessionId, ...config }: NativeAgentConfig) {
    super({ agentId: "codypendent", description: "Codypendent native workspace agent", ...config });
    this.submitNative = submit;
    this.sessionId = sessionId ?? (() => null);
  }

  observeNative() { this.listen(); }

  submitObjective(objective: string): Promise<boolean> {
    if (this.execution || this.acceptance || !objective.trim()) return Promise.resolve(false);
    const acceptance = new Acceptance();
    this.acceptance = acceptance;
    this.addMessage({ id: randomUUID(), role: "user", content: objective });
    void this.runAgent().catch(() => acceptance.settle(false));
    return acceptance.promise.finally(() => {
      if (this.acceptance === acceptance) this.acceptance = null;
    });
  }

  override runAgent(parameters?: RunAgentParameters, subscriber?: AgentSubscriber): Promise<RunAgentResult> {
    if (this.execution) return Promise.reject(new Error("A native run is already active."));
    this.listen();
    this.requestGeneration = this.generation;
    const task = super.runAgent(parameters, subscriber);
    this.execution = task;
    const clear = () => { if (this.execution === task) { this.execution = null; this.requestGeneration = null; } };
    void task.then(clear, clear);
    return task;
  }

  run(input: RunAgentInput): Observable<BaseEvent> {
    return new Observable((observer) => {
      if (this.active || (this.requestGeneration !== null && this.requestGeneration !== this.generation)) {
        observer.next({ type: EventType.RUN_ERROR, message: "The native run was detached before submission." });
        observer.complete();
        return;
      }
      this.listen();
      const generation = this.generation;
      const user = [...input.messages].reverse().find((message) => message.role === "user");
      const objective = user ? contentToText(user.content) : "";
      const acceptance = this.acceptance ?? new Acceptance();
      const run = new NativeRun(input, objective, this.sessionId(), new Map(this.sequences), observer);
      this.active = run;
      if (!objective.trim()) {
        acceptance.settle(false);
        run.fail("Add a user objective before starting a native run.");
      } else {
        void Promise.resolve().then(() => generation !== this.generation || run.isClosed ? false : this.submitNative(objective)).then((accepted) => {
          acceptance.settle(accepted);
          if (!accepted) run.fail("The workspace did not accept the objective.");
        }, (error: unknown) => {
          acceptance.settle(false);
          run.fail(error instanceof Error ? error.message : String(error));
        });
      }
      return () => {
        if (this.active === run) this.active = null;
      };
    });
  }

  close() {
    this.generation++;
    this.unsubscribe?.();
    this.unsubscribe = null;
    this.acceptance?.settle(false);
    this.active?.fail("The native agent connection was closed.");
  }

  dispose() { this.close(); }

  private listen() {
    this.unsubscribe ??= subscribeToFrames((frame) => this.receive(frame));
  }

  private receive(frame: DaemonFrame) {
    if (frame.kind === "disconnected") {
      this.active?.fail(frame.reason);
      return;
    }
    if (frame.kind !== "event") return;
    const body = frame.event.body;
    const oldRun = body.type === "RunStarted" && this.nativeRuns.has(body.run_id);
    this.active?.receive(frame.session_id, frame.event, oldRun);
    this.sequences.set(frame.session_id, Math.max(this.sequences.get(frame.session_id) ?? -1, frame.event.sequence));
    if (body.type === "RunStarted") this.nativeRuns.add(body.run_id);
  }
}

class NativeRun {
  private nativeRunId: string | null = null;
  private nativeSessionId: string | null = null;
  private readonly seen = new Set<number>();
  private readonly tools = new Map<string, Array<{ id: string; complete: boolean }>>();
  private readonly approvals = new Set<string>();
  private readonly questions = new Set<string>();
  private message: { id: string; thought: boolean } | null = null;
  private messageCount = 0;
  private lastEvent: SessionEvent | null = null;

  constructor(
    private readonly input: RunAgentInput,
    private readonly objective: string,
    private readonly expectedSession: string | null,
    private readonly baseline: Map<string | null, number>,
    private readonly observer: Subscriber<BaseEvent>,
  ) {}

  get isClosed() { return this.observer.closed; }

  receive(sessionId: string | null, event: SessionEvent, oldRun: boolean) {
    if (this.observer.closed || event.sequence <= (this.baseline.get(sessionId) ?? -1)) return;
    if (this.expectedSession !== null && this.expectedSession !== sessionId) return;
    const body = event.body;
    if (this.nativeRunId === null) {
      if (body.type !== "RunStarted" || body.objective !== this.objective || oldRun) return;
      this.nativeRunId = body.run_id;
      this.nativeSessionId = sessionId;
      this.lastEvent = event;
      this.emit({ type: EventType.RUN_STARTED, threadId: this.input.threadId, runId: this.input.runId, input: this.input });
    }
    if (sessionId !== this.nativeSessionId || this.seen.has(event.sequence)) return;
    if ("run_id" in body && body.run_id !== this.nativeRunId) return;
    this.seen.add(event.sequence);
    this.lastEvent = event;
    this.apply(body, event.sequence);
  }

  fail(message: string) {
    if (this.observer.closed) return;
    this.closeMessage();
    this.emit({ type: EventType.RUN_ERROR, message });
    this.observer.complete();
  }

  private apply(body: EventBody, sequence: number) {
    switch (body.type) {
      case "ModelStreamDelta":
        if (!body.text) return;
        if (this.message && this.message.thought !== Boolean(body.thought)) this.closeMessage();
        if (!this.message) {
          this.message = { id: `${this.input.runId}:message:${++this.messageCount}`, thought: Boolean(body.thought) };
          this.emit({ type: body.thought ? EventType.REASONING_MESSAGE_START : EventType.TEXT_MESSAGE_START,
            messageId: this.message.id, role: body.thought ? "reasoning" : "assistant" });
        }
        this.emit({ type: body.thought ? EventType.REASONING_MESSAGE_CONTENT : EventType.TEXT_MESSAGE_CONTENT,
          messageId: this.message.id, delta: body.text });
        return;
      case "ToolStarted": {
        this.closeMessage();
        const calls = this.tools.get(body.tool) ?? [];
        if (body.tool.startsWith("acp:") && calls.length > 0) return this.custom(body);
        const call = { id: `${this.nativeRunId}:tool:${sequence}`, complete: false };
        this.tools.set(body.tool, [...calls, call]);
        this.emit({ type: EventType.TOOL_CALL_START, toolCallId: call.id, toolCallName: body.tool,
          metadata: { codypendent: { argsDigest: body.args_digest, label: body.label ?? null } } });
        this.emit({ type: EventType.TOOL_CALL_END, toolCallId: call.id });
        return;
      }
      case "ToolCompleted": {
        const call = [...(this.tools.get(body.tool) ?? [])].reverse().find((item) => !item.complete);
        if (!call) return this.custom(body);
        call.complete = true;
        this.emit({ type: EventType.TOOL_CALL_RESULT, toolCallId: call.id, messageId: `${call.id}:result`,
          content: JSON.stringify({ outcome: body.outcome, ...(body.artifact ? { artifact: body.artifact } : {}) }), role: "tool" });
        return;
      }
      case "ToolProposed": this.approvals.add(body.approval_id); return this.custom(body);
      case "ApprovalRequested":
      case "ApprovalResolved":
        if (this.approvals.has(body.approval_id)) this.custom(body);
        return;
      case "QuestionAsked": this.questions.add(body.question_id); return this.custom(body);
      case "QuestionResolved":
        if (this.questions.has(body.question_id)) this.custom(body);
        return;
      case "RunCompleted":
        return this.finish(body.disposition.type, body.disposition.type === "Failed" ? body.disposition.reason : undefined);
      case "RunStateChanged":
        if (["Completed", "Cancelled", "Failed"].includes(body.state.type)) return this.finish(body.state.type);
        return this.custom(body);
      default: this.custom(body);
    }
  }

  private finish(state: string, reason?: string) {
    this.closeMessage();
    if (state === "Failed" || state === "Unknown") return this.fail(reason ?? `Native run ended with state ${state}.`);
    this.emit({ type: EventType.RUN_FINISHED, threadId: this.input.threadId, runId: this.input.runId,
      outcome: state === "Cancelled" ? { type: "cancelled" } : { type: "success", pendingToolCallIds: [] } });
    this.observer.complete();
  }

  private closeMessage() {
    if (!this.message) return;
    this.emit({ type: this.message.thought ? EventType.REASONING_MESSAGE_END : EventType.TEXT_MESSAGE_END, messageId: this.message.id });
    this.message = null;
  }

  private custom(body: EventBody) { this.emit({ type: EventType.CUSTOM, name: `codypendent.${body.type}`, value: body }); }

  private emit(event: BaseEvent) {
    const native = (event.metadata?.codypendent ?? {}) as Record<string, unknown>;
    this.observer.next({ ...event, metadata: { ...event.metadata, codypendent: { ...native,
      sessionId: this.nativeSessionId, runId: this.nativeRunId, sequence: this.lastEvent?.sequence ?? null } } });
  }
}
