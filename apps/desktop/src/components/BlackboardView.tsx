import { PageShell } from "./PageShell.js";
/**
 * A workflow run's blackboard: the typed artifacts its agents posted, their
 * evidence, and their supersession history.
 *
 * Agents in a multi-agent workflow communicate **only** through this board and
 * their declared node outputs, which is why the one thing an operator may post
 * here is an **open question**. A question deliberately carries no unverified
 * factual claim; a human-authored `finding` or `decision` would enter the
 * agents' evidence channel indistinguishable from something they had verified
 * themselves. There is no affordance for those kinds in this panel and the
 * bridge exposes no command that would create one.
 *
 * Everything else is display discipline: the author is whatever the daemon
 * built from the posting connection (never a client-supplied identity), the
 * payload and evidence are opaque JSON rendered as-is, and a superseded
 * revision is shown as superseded rather than deleted — a correction's history
 * is the point of the board.
 */
import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { BlackboardItemView } from "@codypendent/protocol";
import type { DesktopTransport } from "../transport.js";
import { subscribeToFrames } from "../frameBus.js";

export interface BlackboardViewProps {
  transport: DesktopTransport | null;
  /** The run whose board to show. Empty means no run has been chosen yet. */
  workflowRunId?: string;
  /** Why there is no transport, shown instead of an empty board. */
  unavailable?: string | null;
  /**
   * How many connections have been established. A watch belongs to the
   * connection that grew it, so a change here means this panel's live stream is
   * gone and must be re-established. Zero means nothing has connected yet.
   */
  connectionEpoch?: number;
}

type Board =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "loaded"; runId: string; items: BlackboardItemView[] }
  /** Read failed. Not the same fact as a run that has posted nothing. */
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

const KIND_TONE: Record<string, string> = {
  finding: "var(--cody-accent-strong)",
  decision: "var(--cody-success-strong)",
  hypothesis: "var(--cody-purple)",
  open_question: "var(--cody-warning)",
  task: "var(--cody-text-faint)",
};

export const BlackboardView: React.FC<BlackboardViewProps> = ({
  transport,
  workflowRunId,
  unavailable,
  connectionEpoch = 0,
}) => {
  const [runInput, setRunInput] = useState(workflowRunId ?? "");
  const [board, setBoard] = useState<Board>({ status: "idle" });
  const [question, setQuestion] = useState("");
  const [notice, setNotice] = useState<string | null>(null);

  // Which load is current. Reads are not cancellable, so a superseded one is
  // ignored on arrival instead: switching runs quickly leaves two in flight and
  // whichever the daemon answers last would otherwise win. That put the older
  // run's board on screen under the newer run's selection, and — because the
  // live subscription below keys off the board's own run id — subscribed the
  // panel to the run the operator had just navigated away from.
  const loadGeneration = useRef(0);

  const load = useCallback(
    async (runId: string) => {
      const trimmed = runId.trim();
      if (!transport?.readBlackboard || trimmed.length === 0) {
        return;
      }
      const generation = (loadGeneration.current += 1);
      const current = () => loadGeneration.current === generation;
      setBoard({ status: "loading" });
      try {
        const items = await transport.readBlackboard(trimmed);
        if (current()) {
          setBoard({ status: "loaded", runId: trimmed, items });
        }
      } catch (error) {
        if (current()) {
          setBoard({ status: "failed", detail: describe(error) });
        }
      }
    },
    [transport],
  );

  useEffect(() => {
    if (workflowRunId) {
      setRunInput(workflowRunId);
      void load(workflowRunId);
    }
  }, [workflowRunId, load]);

  // The live posts below ride a subscription belonging to the connection that
  // grew it, so a reconnect leaves this board frozen at its last read while
  // still looking current. Re-read on a new connection; the Workflow panel
  // re-establishes the subscription itself on the same signal.
  const loadedRunId = board.status === "loaded" ? board.runId : null;
  useEffect(() => {
    if (connectionEpoch > 0 && loadedRunId) {
      void load(loadedRunId);
    }
    // Keyed on the epoch alone: "the connection changed", not "the run
    // changed", which the effect above already handles.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionEpoch]);

  // Live posts. The subscription itself is established by `watch_workflow`
  // (the Workflow panel); this read is the baseline it folds onto, and a run
  // opened only here will still show its live posts once that watch exists.
  useEffect(() => {
    if (board.status !== "loaded") {
      return;
    }
    const runId = board.runId;
    return subscribeToFrames((frame) => {
      if (frame.kind !== "blackboard_posted" || frame.item.workflow_run_id !== runId) {
        return;
      }
      const item = frame.item;
      setBoard((current) =>
        current.status === "loaded" && current.runId === runId
          ? { ...current, items: mergeItem(current.items, item) }
          : current,
      );
    });
  }, [board.status, board.status === "loaded" ? board.runId : null]);

  const post = useCallback(async () => {
    if (!transport?.postBlackboardQuestion || board.status !== "loaded") {
      return;
    }
    const text = question.trim();
    if (text.length === 0) {
      setNotice("question must not be empty");
      return;
    }
    try {
      const item = await transport.postBlackboardQuestion(board.runId, text);
      setQuestion("");
      setNotice(null);
      setBoard((current) =>
        current.status === "loaded" ? { ...current, items: mergeItem(current.items, item) } : current,
      );
    } catch (error) {
      setNotice(describe(error));
    }
  }, [transport, board, question]);

  const items = board.status === "loaded" ? board.items : [];
  const ordered = useMemo(
    () => [...items].sort((left, right) => left.id.localeCompare(right.id)),
    [items],
  );

  return (
    <PageShell
      role="region"
      aria-label="Blackboard"
      style={{ flex: 1, display: "flex", flexDirection: "column", height: "100%", background: "var(--cody-canvas)", color: "var(--cody-text)", overflowY: "auto" }}
    >
      <div className="cody-page-header" style={{ padding: "20px 24px 14px", borderBottom: "1px solid var(--cody-inset)" }}>
        <h1 style={{ margin: 0, fontSize: 20, fontWeight: 600 }}>Blackboard</h1>
        <p style={{ margin: "4px 0 12px", fontSize: 13, color: "var(--cody-text-muted)" }}>
          Shared findings, evidence and questions from a workflow’s agents.
        </p>
        <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
          <input
            aria-label="Workflow run id"
            value={runInput}
            onChange={(event) => setRunInput(event.target.value)}
            onKeyDown={(event) => event.key === "Enter" && void load(runInput)}
            placeholder="Paste a workflow run ID"
            style={{ flex: 1, minWidth: 260, padding: "7px 10px", background: "var(--cody-canvas)", border: "1px solid var(--cody-border-strong)", borderRadius: 6, color: "var(--cody-text)", fontSize: 13 }}
          />
          <button style={button} onClick={() => void load(runInput)}>
            Read board
          </button>
        </div>
      </div>

      {board.status === "loaded" && (
        <div style={{ padding: "12px 24px", borderBottom: "1px solid var(--cody-inset)", background: "var(--cody-panel-raised)" }}>
          <label htmlFor="blackboard-question" style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
            Ask the agents a question
          </label>
          <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
            <input
              id="blackboard-question"
              aria-label="Open question"
              value={question}
              onChange={(event) => setQuestion(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  void post();
                }
              }}
              placeholder="What should the run establish before continuing?"
              style={{ flex: 1, padding: "7px 10px", background: "var(--cody-canvas)", border: "1px solid var(--cody-border-strong)", borderRadius: 6, color: "var(--cody-text)", fontSize: 13 }}
            />
            <button style={{ ...button, background: "var(--cody-success-strong)", color: "var(--cody-on-accent)" }} onClick={() => void post()}>
              Post question
            </button>
          </div>
        </div>
      )}

      {notice && (
        <div role="status" style={{ padding: "8px 24px", fontSize: 12, color: "var(--cody-warning)" }}>
          {notice}
        </div>
      )}

      <div style={{ flex: 1, padding: "16px 24px" }}>
        {unavailable ? (
          <div data-testid="blackboard-unavailable" role="status" style={note("var(--cody-warning)")}>
            Blackboard unavailable — {unavailable}
          </div>
        ) : board.status === "failed" ? (
          <div data-testid="blackboard-failed" role="status" style={note("var(--cody-danger-soft)")}>
            The board could not be read — {board.detail}
            <div style={{ marginTop: 8, fontSize: 12, color: "var(--cody-text-muted)" }}>
              This is not an empty board: nothing was read.
            </div>
          </div>
        ) : board.status === "loading" ? (
          <div role="status" style={note("var(--cody-text-muted)")}>
            Reading the board…
          </div>
        ) : board.status === "idle" ? (
          <div data-testid="blackboard-idle" style={note("var(--cody-text-muted)")}>
            Name a workflow run to read its board.
          </div>
        ) : ordered.length === 0 ? (
          <div data-testid="blackboard-empty" style={note("var(--cody-text-muted)")}>
            This run has posted nothing to its board yet.
          </div>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
            {ordered.map((item) => (
              <ItemCard key={item.id} item={item} />
            ))}
          </div>
        )}
      </div>
    </PageShell>
  );
};

const ItemCard: React.FC<{ item: BlackboardItemView }> = ({ item }) => {
  const superseded = Boolean(item.superseded_by);
  return (
    <div
      data-testid={`blackboard-item-${item.id}`}
      style={{
        padding: 14,
        background: "var(--cody-panel-raised)",
        border: `1px solid ${superseded ? "var(--cody-border-strong)" : "var(--cody-border-strong)"}`,
        borderRadius: 8,
        opacity: superseded ? 0.62 : 1,
        display: "flex",
        flexDirection: "column",
        gap: 8,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
        <span style={{ padding: "1px 8px", borderRadius: 10, fontSize: 11, background: KIND_TONE[item.kind] ?? "var(--cody-border-strong)", color: "var(--cody-on-accent)" }}>
          {item.kind}
        </span>
        <span style={{ fontSize: 11, color: "var(--cody-text-muted)" }}>revision {item.revision}</span>
        {/* Confidence is the author's own, and only when the author gave one.
            An absent confidence is not zero confidence. */}
        {item.confidence !== undefined && item.confidence !== null && (
          <span style={{ fontSize: 11, color: "var(--cody-text-muted)" }}>
            confidence {item.confidence.toFixed(2)}
          </span>
        )}
        {superseded && (
          <span style={{ fontSize: 11, color: "var(--cody-warning)" }}>superseded by {item.superseded_by}</span>
        )}
      </div>

      <pre style={preStyle}>{stringify(item.payload)}</pre>

      <div style={{ fontSize: 11, color: "var(--cody-text-muted)" }}>
        {/* Attribution the daemon built from the posting connection — never
            model-supplied identity. */}
        author: <code>{stringify(item.author)}</code>
      </div>

      {item.evidence && item.evidence.length > 0 && (
        <details>
          <summary style={{ fontSize: 12, color: "var(--cody-text-muted)", cursor: "pointer" }}>
            evidence ({item.evidence.length})
          </summary>
          <pre style={preStyle}>{item.evidence.map((entry) => stringify(entry)).join("\n")}</pre>
        </details>
      )}
    </div>
  );
};

const preStyle: React.CSSProperties = {
  margin: 0,
  padding: 8,
  background: "var(--cody-canvas)",
  border: "1px solid var(--cody-inset)",
  borderRadius: 6,
  fontSize: 12,
  color: "var(--cody-text-secondary)",
  whiteSpace: "pre-wrap",
  wordBreak: "break-word",
  overflowX: "auto",
  // Bounded: one long payload used to push every sibling card off screen.
  // The full text stays reachable by scrolling within the block.
  maxHeight: 260,
  overflowY: "auto",
};

/**
 * Fold one delivery into the item list.
 *
 * Unlike the task board, a superseded revision is **kept**: the run's board is
 * read with `include_superseded`, and a correction's predecessor is the record
 * of what was corrected. So the predecessor is only stamped, never removed.
 */
export function mergeItem(
  items: BlackboardItemView[],
  incoming: BlackboardItemView,
): BlackboardItemView[] {
  let replaced = false;
  const folded = items.map((item) => {
    if (item.id !== incoming.id) {
      return item;
    }
    replaced = true;
    return incoming;
  });
  return replaced ? folded : [...folded, incoming];
}

function stringify(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    return String(value);
  }
}

function note(color: string): React.CSSProperties {
  return { padding: "48px 24px", textAlign: "center", color, fontSize: 14 };
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
