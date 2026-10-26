import { EmptyState, PageSection, PageShell } from "./PageShell.js";
/**
 * A durable workflow run: its graph, its live node transitions, and the four
 * lifecycle controls the protocol exposes.
 *
 * The split this view sits on is real and worth naming. The **list** of
 * available workflows has no wire command at all — the TUI reads
 * `<repo>/.codypendent/workflows/*.yaml` and overlays the last durable run from
 * SQLite (`crates/cli/src/tui.rs::load_workflows`). This shell has neither the
 * repository walker nor the database open, so it does not guess: there is no
 * workflow picker here, only a field for a workflow id the daemon resolves
 * itself, and the panel says so rather than showing a hardcoded menu.
 *
 * **Control** is entirely protocol, and that half is complete: `StartWorkflow`,
 * `ReadWorkflowRun`, `PauseWorkflow`, `ResumeWorkflow`, `CancelWorkflow`,
 * `RetryWorkflowNode`.
 *
 * Two merge rules the daemon's contract requires:
 *
 * * A live `NodeTransitioned` carries the node's **full** state, so it merges
 *   by `node_id` as an overwrite — but it omits `depends_on`, because the graph
 *   shape is static per run. Overwriting the edges with an empty list would
 *   erase the DAG mid-run, so the merge keeps the edges the snapshot taught it.
 * * Measured cost is `None` until a node completes an attempt. It renders as
 *   `—`. A node that has not been measured is not a node that cost zero.
 */
import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useFocusTrap } from "../useFocusTrap.js";
import type { WorkflowNodeView, WorkflowRunSnapshot } from "@codypendent/protocol";
import type { DesktopTransport } from "../transport.js";
import { subscribeToFrames } from "../frameBus.js";

export interface WorkflowViewProps {
  transport: DesktopTransport | null;
  /** Why there is no transport, shown instead of an empty graph. */
  unavailable?: string | null;
  /** Open a run immediately (e.g. from an inbox deep link). */
  initialRunId?: string;
  /** Show a run's blackboard; the Blackboard panel is a separate surface. */
  onOpenBlackboard?: (workflowRunId: string) => void;
  /**
   * How many connections have been established. A watch belongs to the
   * connection that grew it, so a change here means this panel's live stream is
   * gone and must be re-established. Zero means nothing has connected yet.
   */
  connectionEpoch?: number;
}

type Watch =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "watching"; runId: string; snapshot: WorkflowRunSnapshot }
  | { status: "failed"; detail: string };

const button: React.CSSProperties = {
  padding: "5px 12px",
  background: "var(--cody-inset)",
  border: "1px solid var(--cody-border-strong)",
  borderRadius: 6,
  color: "var(--cody-text-secondary)",
  fontSize: 12,
  cursor: "pointer",
};

const field: React.CSSProperties = {
  padding: "7px 10px",
  background: "var(--cody-canvas)",
  border: "1px solid var(--cody-border-strong)",
  borderRadius: 6,
  color: "var(--cody-text)",
  fontSize: 13,
};

const STATE_TONE: Record<string, string> = {
  Pending: "var(--cody-text-faint)",
  Running: "var(--cody-accent-strong)",
  WaitingApproval: "var(--cody-warning)",
  Blocked: "var(--cody-warning)",
  Completed: "var(--cody-success-strong)",
  Failed: "var(--cody-danger)",
  Skipped: "var(--cody-text-faint)",
  Unknown: "var(--cody-text-faint)",
};

export const WorkflowView: React.FC<WorkflowViewProps> = ({
  transport,
  unavailable,
  initialRunId,
  onOpenBlackboard,
  connectionEpoch = 0,
}) => {
  const [runIdInput, setRunIdInput] = useState(initialRunId ?? "");
  const [watch, setWatch] = useState<Watch>({ status: "idle" });
  const [notice, setNotice] = useState<string | null>(null);
  const [startId, setStartId] = useState("");
  const [startInputs, setStartInputs] = useState("");
  const [confirmCancel, setConfirmCancel] = useState<string | null>(null);
  // The cancel confirmation takes focus and owns Escape (`useFocusTrap`).
  const cancelDialogRef = useRef<HTMLDivElement | null>(null);
  useFocusTrap(cancelDialogRef, {
    active: confirmCancel !== null,
    onEscape: () => setConfirmCancel(null),
  });

  /** The run whose open may still be drawn. Two rapid opens race, and only the
   * most recent request's answer may paint — never the last to RESOLVE. */
  const liveOpen = useRef<string | null>(null);

  const open = useCallback(
    async (runId: string) => {
      const trimmed = runId.trim();
      if (!transport?.watchWorkflow || trimmed.length === 0) {
        return;
      }
      liveOpen.current = trimmed;
      setWatch({ status: "loading" });
      try {
        const result = await transport.watchWorkflow(trimmed);
        if (liveOpen.current !== trimmed) {
          // A newer open is in flight; its own answer sets the state.
          return;
        }
        setWatch({ status: "watching", runId: trimmed, snapshot: result.snapshot });
      } catch (error) {
        if (liveOpen.current !== trimmed) {
          return;
        }
        // A refused read is an outcome. The graph area says the run could not
        // be read rather than drawing an empty DAG, which would read as "this
        // workflow has no steps".
        setWatch({ status: "failed", detail: describe(error) });
      }
    },
    [transport],
  );

  useEffect(() => {
    if (initialRunId) {
      void open(initialRunId);
    }
  }, [initialRunId, open]);

  // A watch belongs to the connection that asked for it. A reconnect builds a
  // new client, so the daemon is no longer streaming this run to anyone and the
  // graph would sit at its last transition looking merely idle. Re-open on a
  // new connection, which re-subscribes and re-reads the baseline.
  const watchedRunId = watch.status === "watching" ? watch.runId : null;
  useEffect(() => {
    if (connectionEpoch > 0 && watchedRunId) {
      void open(watchedRunId);
    }
    // Deliberately keyed on the epoch alone: this is "the connection changed",
    // not "the run changed", which the effect above already handles.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionEpoch]);

  // Live node transitions and phase changes for the run currently open. Frames
  // for any other run are ignored: each carries its own `workflow_run_id`, so
  // there is never a need to guess which graph a transition belongs to.
  useEffect(() => {
    if (watch.status !== "watching") {
      return;
    }
    const runId = watch.runId;
    return subscribeToFrames((frame) => {
      if (frame.kind !== "workflow_event") {
        return;
      }
      const event = frame.event;
      if (event.type === "NodeTransitioned") {
        if (event.workflow_run_id !== runId) {
          return;
        }
        setWatch((current) =>
          current.status === "watching" && current.runId === runId
            ? { ...current, snapshot: mergeNode(current.snapshot, event) }
            : current,
        );
      } else if (event.type === "RunPhaseChanged") {
        if (event.workflow_run_id !== runId) {
          return;
        }
        const phase = event.phase;
        setWatch((current) =>
          current.status === "watching" && current.runId === runId
            ? { ...current, snapshot: { ...current.snapshot, phase } }
            : current,
        );
      }
    });
  }, [watch.status, watch.status === "watching" ? watch.runId : null]);

  const control = useCallback(
    async (action: () => Promise<void> | undefined, label: string, runId: string) => {
      try {
        await action();
        setNotice(`${label} accepted`);
        // The daemon is the authority on what the command did, so the panel
        // re-reads its snapshot rather than predicting the new phase.
        if (transport?.readWorkflowRun) {
          const snapshot = await transport.readWorkflowRun(runId);
          setWatch((current) =>
            current.status === "watching" && current.runId === runId
              ? { ...current, snapshot }
              : current,
          );
        }
      } catch (error) {
        setNotice(describe(error));
      }
    },
    [transport],
  );

  const startWorkflow = useCallback(async () => {
    if (!transport?.startWorkflow) {
      return;
    }
    const id = startId.trim();
    if (id.length === 0) {
      setNotice("a workflow id is required");
      return;
    }
    // Blank means "no inputs", which is the empty object — not null, and not a
    // guess at what the manifest wanted.
    const raw = startInputs.trim();
    let inputs: Record<string, unknown> = {};
    if (raw.length > 0) {
      let parsed: unknown;
      try {
        parsed = JSON.parse(raw);
      } catch (error) {
        setNotice(`invalid workflow input JSON: ${describe(error)}`);
        return;
      }
      // A manifest's typed inputs are named fields. A valid JSON scalar or
      // array is refused here, exactly as the TUI reducer refuses it.
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
        setNotice("workflow inputs must be a JSON object");
        return;
      }
      inputs = parsed as Record<string, unknown>;
    }
    try {
      const runId = await transport.startWorkflow(id, inputs);
      setNotice(`workflow run ${runId} started`);
      setRunIdInput(runId);
      await open(runId);
    } catch (error) {
      setNotice(describe(error));
    }
  }, [transport, startId, startInputs, open]);

  const nodes = watch.status === "watching" ? watch.snapshot.nodes : [];
  const phase = watch.status === "watching" ? watch.snapshot.phase : null;

  const graph = useMemo(() => {
    if (unavailable) {
      return (
        <div data-testid="workflow-unavailable" role="status" style={note("var(--cody-warning)")}>
          Workflow runs unavailable — {unavailable}
        </div>
      );
    }
    if (watch.status === "failed") {
      return (
        <div data-testid="workflow-failed" role="status" style={note("var(--cody-danger-soft)")}>
          Could not read that workflow run — {watch.detail}
        </div>
      );
    }
    if (watch.status === "loading") {
      return (
        <div role="status" style={note("var(--cody-text-muted)")}>
          Reading the run…
        </div>
      );
    }
    if (watch.status === "idle") {
      return (
        <div data-testid="workflow-idle">
          <EmptyState title="Bring a workflow to life" description="Start a workflow from this repository, or open an existing run to follow its progress." />
        </div>
      );
    }
    if (nodes.length === 0) {
      return (
        <div data-testid="workflow-empty" style={note("var(--cody-text-muted)")}>
          The daemon reported this run with no nodes.
        </div>
      );
    }
    return (
      <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        {nodes.map((node) => (
          <NodeRow
            key={node.node_id}
            node={node}
            onRetry={() =>
              void control(
                () => transport?.retryWorkflowNode?.(node.workflow_run_id, node.node_id),
                `retry from ${node.node_id}`,
                node.workflow_run_id,
              )
            }
          />
        ))}
      </div>
    );
  }, [unavailable, watch, nodes, control, transport]);

  return (
    <PageShell role="region" aria-label="Workflow" style={{ flex: 1, display: "flex", flexDirection: "column", height: "100%", background: "var(--cody-canvas)", color: "var(--cody-text)", overflowY: "auto" }}>
      <div className="cody-page-header" style={{ padding: "20px 24px 14px", borderBottom: "1px solid var(--cody-inset)" }}>
        <h1 style={{ margin: 0, fontSize: 20, fontWeight: 600 }}>Workflows</h1>
        <p style={{ margin: "4px 0 14px", fontSize: 13, color: "var(--cody-text-muted)" }}>
          Follow work across agents, review progress and keep each step moving.
        </p>

        <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
          <input aria-label="Workflow id to start" value={startId}
            onChange={(event) => setStartId(event.target.value)}
            onKeyDown={(event) => event.key === "Enter" && startId.trim() && void startWorkflow()}
            placeholder="Workflow name or ID" style={{ ...field, minWidth: 240, flex: 1 }} />
          <button style={{ ...button, background: "var(--cody-accent-strong)", color: "var(--cody-on-accent)" }}
            disabled={!transport?.startWorkflow || !startId.trim()} onClick={() => void startWorkflow()}>
            Start workflow
          </button>
        </div>
        <PageSection title="Workflow inputs" description="Optional values for this run">
          <label htmlFor="workflow-inputs" style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>Input values as JSON</label>
          <input id="workflow-inputs" aria-label="Workflow inputs JSON" value={startInputs}
            onChange={(event) => setStartInputs(event.target.value)}
            onKeyDown={(event) => event.key === "Enter" && void startWorkflow()}
            placeholder='e.g. {"branch":"main"}' style={{ ...field, width: "100%", marginTop: 6 }} />
        </PageSection>
        <PageSection title="Open an existing run" description="Continue from a workflow run ID" defaultOpen={Boolean(initialRunId)}>
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
            <input aria-label="Workflow run id" value={runIdInput}
              onChange={(event) => setRunIdInput(event.target.value)}
              onKeyDown={(event) => event.key === "Enter" && void open(runIdInput)}
              placeholder="Paste a workflow run ID" style={{ ...field, minWidth: 0, flex: 1 }} />
            <button style={button} disabled={!transport?.readWorkflowRun || !runIdInput.trim()} onClick={() => void open(runIdInput)}>Open run</button>
            {watch.status === "watching" && <button style={button} onClick={() => void open(watch.runId)}>Refresh</button>}
          </div>
        </PageSection>
      </div>

      {watch.status === "watching" && (
        <div
          style={{
            padding: "12px 24px",
            borderBottom: "1px solid var(--cody-inset)",
            background: "var(--cody-panel-raised)",
            display: "flex",
            gap: 8,
            alignItems: "center",
            flexWrap: "wrap",
          }}
        >
          <span style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>run {watch.runId}</span>
          {phase && <Tone tone={STATE_TONE[phase.type] ?? "var(--cody-text-faint)"}>{phase.type}</Tone>}
          <div style={{ marginLeft: "auto", display: "flex", gap: 8, flexWrap: "wrap" }}>
            <button
              style={button}
              onClick={() =>
                void control(() => transport?.pauseWorkflow?.(watch.runId), "pause", watch.runId)
              }
            >
              Pause
            </button>
            <button
              style={button}
              onClick={() =>
                void control(() => transport?.resumeWorkflow?.(watch.runId), "resume", watch.runId)
              }
            >
              Resume
            </button>
            <button
              style={{ ...button, color: "var(--cody-danger-soft)", borderColor: "var(--cody-danger-soft)" }}
              onClick={() => setConfirmCancel(watch.runId)}
            >
              Cancel run
            </button>
            {onOpenBlackboard && (
              <button style={button} onClick={() => onOpenBlackboard(watch.runId)}>
                Blackboard
              </button>
            )}
          </div>
        </div>
      )}

      {notice && (
        <div role="status" style={{ padding: "8px 24px", fontSize: 12, color: "var(--cody-warning)" }}>
          {notice}
        </div>
      )}

      <div style={{ flex: 1, padding: "16px 24px" }}>{graph}</div>

      {confirmCancel !== null && (
        <div
          ref={cancelDialogRef}
          role="dialog"
          aria-modal="true"
          aria-label="Cancel this workflow run?"
          style={{ position: "fixed", inset: 0, background: "rgba(1,4,9,0.72)", display: "flex", alignItems: "center", justifyContent: "center" }}
        >
          <div style={{ width: 460, padding: 20, background: "var(--cody-panel-raised)", border: "1px solid var(--cody-border-strong)", borderRadius: 10 }}>
            <h2 style={{ margin: "0 0 10px", fontSize: 16 }}>Cancel this workflow run?</h2>
            <p style={{ margin: "0 0 14px", fontSize: 13, color: "var(--cody-text-muted)" }}>
              {confirmCancel}
              <br />
              <br />
              Cancellation is a cooperative drain and <strong>terminal</strong>: the in-flight
              node&rsquo;s run is interrupted, every still-pending node becomes Skipped, and the run
              lands Cancelled. There is no resume from Cancelled.
            </p>
            <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
              <button style={button} onClick={() => setConfirmCancel(null)}>
                Keep running
              </button>
              <button
                style={{ ...button, background: "var(--cody-danger)", borderColor: "var(--cody-danger-soft)", color: "var(--cody-on-accent)" }}
                onClick={() => {
                  const runId = confirmCancel;
                  setConfirmCancel(null);
                  void control(() => transport?.cancelWorkflow?.(runId), "cancel", runId);
                }}
              >
                Cancel the run
              </button>
            </div>
          </div>
        </div>
      )}
    </PageShell>
  );
};

const NodeRow: React.FC<{ node: WorkflowNodeView; onRetry: () => void }> = ({ node, onRetry }) => (
  <div
    data-testid={`workflow-node-${node.node_id}`}
    style={{
      padding: 12,
      background: "var(--cody-panel-raised)",
      border: "1px solid var(--cody-border-strong)",
      borderRadius: 8,
      display: "flex",
      flexDirection: "column",
      gap: 6,
    }}
  >
    <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
      <strong style={{ fontSize: 14 }}>{node.node_id}</strong>
      <Tone tone={STATE_TONE[node.state.type] ?? "var(--cody-text-faint)"}>{node.state.type}</Tone>
      <span style={{ fontSize: 11, color: "var(--cody-text-muted)" }}>
        {node.attempt > 0 ? `attempt ${node.attempt}` : "not yet attempted"}
      </span>
      <button
        style={{ ...button, marginLeft: "auto" }}
        title="Re-drive the run from this node; its dependents reset and re-run with it"
        onClick={onRetry}
      >
        Retry from here
      </button>
    </div>

    {node.depends_on && node.depends_on.length > 0 && (
      <span style={{ fontSize: 11, color: "var(--cody-text-faint)" }}>after: {node.depends_on.join(", ")}</span>
    )}

    <span style={{ fontSize: 11, color: "var(--cody-text-muted)" }}>
      {/* Absent measurements stay absent. A node with no recorded cost has not
          been measured; it has not cost nothing. */}
      cost: {node.cost === undefined || node.cost === null ? "—" : JSON.stringify(node.cost)}
    </span>

    {node.error && (
      <div style={{ fontSize: 12, color: "var(--cody-danger-soft)", whiteSpace: "pre-wrap" }}>{node.error}</div>
    )}
    {node.warnings?.map((warning) => (
      <div key={warning} style={{ fontSize: 12, color: "var(--cody-warning)" }}>
        {warning}
      </div>
    ))}
  </div>
);

const Tone: React.FC<{ tone: string; children: React.ReactNode }> = ({ tone, children }) => (
  <span style={{ padding: "1px 8px", borderRadius: 10, fontSize: 11, background: tone, color: "var(--cody-on-accent)" }}>
    {children}
  </span>
);

/**
 * Merge one live transition into the snapshot.
 *
 * The overwrite is by `node_id` and is idempotent — each transition is
 * full-state, so an overlap between the snapshot baseline and the stream is a
 * harmless re-write and no watermark is needed. The single exception is
 * `depends_on`: a live event omits it because the graph shape is static per
 * run, so the previously-known edges are carried forward. Blanking them would
 * silently flatten the DAG the moment the first node moved.
 *
 * A node id the snapshot never mentioned is appended rather than dropped: a
 * transition the client cannot place is still work that happened.
 */
export function mergeNode(
  snapshot: WorkflowRunSnapshot,
  incoming: WorkflowNodeView,
): WorkflowRunSnapshot {
  let found = false;
  const nodes = snapshot.nodes.map((node) => {
    if (node.node_id !== incoming.node_id) {
      return node;
    }
    found = true;
    const edges = incoming.depends_on?.length ? incoming.depends_on : node.depends_on;
    return { ...incoming, depends_on: edges };
  });
  return { ...snapshot, nodes: found ? nodes : [...nodes, incoming] };
}

function note(color: string): React.CSSProperties {
  return { padding: "48px 24px", textAlign: "center", color, fontSize: 14 };
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
