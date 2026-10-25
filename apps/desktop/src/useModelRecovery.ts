import { useCallback, useEffect, useRef, useState } from "react";
import type { EventBody } from "@codypendent/protocol";
import type { ModelDiscovery, ModelsView } from "./components/localConfig.js";
import { subscribeToFrames } from "./frameBus.js";
import type { DaemonFrame } from "./transport.js";
import { modelLabel } from "./modelPresentation.js";

export type RecoverySnapshot = {
  connected: boolean; epoch: number; sessionId: string | null; attached: boolean;
  attaching: boolean; running: boolean; draftRevision: number; modelRevision: number;
  model: string | null; repository: string | null; mode: string; view: string;
};
export type ModelRecoveryOptions = {
  snapshot: () => RecoverySnapshot;
  submit: (objective: string) => Promise<boolean>;
  discover: () => Promise<ModelDiscovery | undefined>;
  models: () => Promise<ModelsView>;
  stage: (model: string, stillCurrent: () => boolean) => Promise<void>;
  status: (message: string | null) => void;
  recovered?: (runIds: string[]) => void;
};
type Chain = {
  objective: string; scope: RecoverySnapshot; session: string | null; run: string | null;
  accepted: boolean | null; failure: Extract<EventBody, { type: "RunCompleted" }> | null;
  effects: boolean; recovering: boolean; attempts: number; tried: Set<string>; seen: Set<number>; retriedRuns: Set<string>;
};

/** Only provider/model availability failures qualify; task and policy failures do not. */
export function recoverableModelFailure(body: Extract<EventBody, { type: "RunCompleted" }>): boolean {
  if (body.disposition.type !== "Failed") return false;
  const { reason, error } = body.disposition;
  const code = error?.code ?? "";
  if (/budget|policy|cancel/i.test(code) || /budget|approval.{0,40}(denied|reject)|classification.{0,40}(ceiling|restrict)|policy.{0,40}(denied|reject)|cancel/i.test(reason)) return false;
  if (error?.user_action?.type === "ReconfigureModel" || error?.user_action?.type === "Reauthenticate") return true;
  if (/^model\.(not-runnable|not-found|invalid-auth|unavailable|rate-limited|timeout|connection)/.test(code)) return true;
  return /\b(model|provider|acp)\b/i.test(reason) && /unavailable|not available|not supported|not found|not configured|unknown model|does not support chat|credential|unauthorized|authentication|sign.?in|login|api.?key|handshake|spawn|start.*timed out|timeout|timed out|connection refused|rate.?limit|overloaded|payment.required|quota|\b(401|402|403|429|503)\b/i.test(reason);
}

function friendlyModel(id: string): string {
  return modelLabel(id);
}

/** One live user submission and at most two alternatives, never a history observer. */
export class ModelRecovery {
  private chain: Chain | null = null;
  private readonly sequences = new Map<string | null, number>();
  private readonly knownRuns = new Set<string>();

  constructor(private readonly options: ModelRecoveryOptions) {}

  stop(notify = true) {
    this.chain = null;
    if (notify) this.options.status(null);
  }

  refreshScope() { if (this.chain && !this.valid(this.chain)) this.stop(); }

  async submit(objective: string): Promise<boolean> {
    this.stop();
    const scope = { ...this.options.snapshot() };
    const chain: Chain = { objective, scope, session: scope.sessionId, run: null, accepted: null, failure: null,
      effects: false, recovering: false, attempts: 0, tried: new Set(scope.model ? [scope.model] : []), seen: new Set(), retriedRuns: new Set() };
    if (scope.connected && !scope.attaching && scope.view === "sessions") this.chain = chain;
    try {
      const accepted = await this.options.submit(objective);
      if (this.chain === chain) {
        chain.accepted = accepted;
        if (!accepted) this.stop();
        else if (chain.failure) void this.recover(chain);
      }
      return accepted;
    } catch (error) {
      if (this.chain === chain) this.stop();
      throw error;
    }
  }

  receive(frame: DaemonFrame) {
    if (frame.kind === "disconnected") { this.stop(); return; }
    if (frame.kind !== "event") return;
    const baseline = this.sequences.get(frame.session_id) ?? -1;
    this.sequences.set(frame.session_id, Math.max(baseline, frame.event.sequence));
    const body = frame.event.body;
    const alreadyKnown = body.type === "RunStarted" && this.knownRuns.has(body.run_id);
    if (body.type === "RunStarted") this.knownRuns.add(body.run_id);
    const chain = this.chain;
    if (!chain || frame.event.sequence <= baseline || !this.valid(chain)) return;
    if (chain.run === null) {
      if (body.type !== "RunStarted" || body.objective !== chain.objective || alreadyKnown) return;
      if (chain.session !== null && chain.session !== frame.session_id) return;
      chain.session = frame.session_id;
      chain.run = body.run_id;
    }
    if (chain.session !== frame.session_id || chain.seen.has(frame.event.sequence)) return;
    if ("run_id" in body && body.run_id !== chain.run) return;
    chain.seen.add(frame.event.sequence);
    switch (body.type) {
      case "RunStarted": case "ModelRetrying": case "BudgetWarning": case "ContextUsage":
      case "RunUsage": case "NoteAppended": case "CheckpointRecorded": case "LearningsCaptured": case "ClientPresenceChanged": return;
      case "ModelStreamDelta":
        if (body.text && !body.thought) this.options.status(null);
        return;
      case "RunStateChanged":
        if (body.state.type === "Completed") { this.complete(chain); return; }
        if (["WaitingForApproval", "WaitingForUserInput"].includes(body.state.type)) { chain.effects = true; this.options.status(null); return; }
        if (["Paused", "Cancelled", "Unknown"].includes(body.state.type)) this.stop();
        return;
      case "RunCompleted":
        if (body.disposition.type === "Completed") { this.complete(chain); return; }
        chain.failure = body;
        if (chain.accepted === true) void this.recover(chain);
        return;
      default:
        // Tool attempts, approvals, questions, patches, steering and unknown
        // events are an effect boundary, even when an operation was denied.
        chain.effects = true;
        this.options.status(null);
    }
  }

  private complete(chain: Chain) {
    const recovered = [...chain.retriedRuns];
    this.stop();
    if (recovered.length) {
      this.options.recovered?.(recovered);
      const model = this.options.snapshot().model;
      this.options.status(model ? `Connected using ${friendlyModel(model)}.` : "Your task is back on track.");
    }
  }

  private valid(chain: Chain, confirmed = false): boolean {
    if (this.chain !== chain) return false;
    const current = this.options.snapshot();
    const sameSession = current.sessionId === chain.session || (!confirmed && chain.scope.sessionId === null && current.sessionId === null);
    return current.connected && current.epoch === chain.scope.epoch && !current.attaching && sameSession
      && current.repository === chain.scope.repository && current.mode === chain.scope.mode && current.view === "sessions"
      && current.draftRevision === chain.scope.draftRevision && current.modelRevision === chain.scope.modelRevision
      && (!confirmed || (current.attached && !current.running && current.sessionId !== null));
  }

  private async recover(chain: Chain) {
    if (chain.recovering || !chain.failure) return;
    if (chain.effects || chain.attempts >= 2 || !recoverableModelFailure(chain.failure) || !this.valid(chain)) { this.stop(); return; }
    chain.recovering = true;
    let finished = false;
    this.options.status("Trying another available model…");
    try {
      const before = await this.options.models();
      if (before.pinned) chain.tried.add(before.pinned);
      if (!this.valid(chain)) return;
      const discovered = await this.options.discover();
      if (!this.valid(chain)) return;
      const current = await this.options.models();
      if (!this.valid(chain, true) || chain.effects) return;
      const configured = new Set(current.models.map((model) => model.id));
      const candidate = discovered?.available_default_profiles?.find((id) => configured.has(id) && !chain.tried.has(id));
      if (!candidate) return;
      chain.tried.add(candidate);
      chain.attempts++;
      await this.options.stage(candidate, () => this.valid(chain, true) && !chain.effects);
      if (!this.valid(chain, true) || chain.effects) return;
      this.options.status(`Trying ${friendlyModel(candidate)}…`);
      if (chain.run) chain.retriedRuns.add(chain.run);
      chain.run = null;
      chain.failure = null;
      chain.accepted = null;
      chain.seen.clear();
      const accepted = await this.options.submit(chain.objective);
      if (this.chain !== chain) return;
      finished = true;
      chain.accepted = accepted;
      chain.recovering = false;
      if (!accepted) this.stop();
      else if (chain.failure) void this.recover(chain);
      return;
    } catch {
      // Keep the existing failure and its real recovery controls visible.
    } finally {
      if (this.chain === chain && !finished) this.stop();
    }
  }
}

export function useModelRecovery(options: Omit<ModelRecoveryOptions, "status" | "recovered">) {
  const latest = useRef(options);
  latest.current = options;
  const [status, setStatus] = useState<string | null>(null);
  const [recoveredRuns, setRecoveredRuns] = useState<ReadonlySet<string>>(() => new Set());
  const [controller] = useState(() => new ModelRecovery({
    snapshot: () => latest.current.snapshot(), submit: (objective) => latest.current.submit(objective),
    discover: () => latest.current.discover(), models: () => latest.current.models(),
    stage: (model, current) => latest.current.stage(model, current), status: setStatus,
    recovered: (runs) => setRecoveredRuns((previous) => new Set([...previous, ...runs])),
  }));
  useEffect(() => {
    const unsubscribe = subscribeToFrames((frame) => controller.receive(frame));
    return () => { unsubscribe(); controller.stop(false); };
  }, [controller]);
  useEffect(() => { controller.refreshScope(); });
  return { status, recoveredRuns, submit: useCallback((objective: string) => controller.submit(objective), [controller]),
    cancel: useCallback(() => controller.stop(), [controller]) };
}
