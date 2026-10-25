import { PageShell } from "./PageShell.js";
/**
 * Backtrack / Session Fork — the TUI's `Overlay::Backtrack(BacktrackState)`.
 *
 * # Where the checkpoints come from
 *
 * Nowhere but the session's own ledger. Every row below is a
 * `CheckpointRecorded` event the daemon appended
 * (`crates/protocol/src/events.rs`): its run, its 1-based turn `ordinal`, the
 * checkpoint object's `commit`, the `base_commit` the worktree was carved from,
 * and `kind` (`Stash` needs `git stash apply`, `Commit` is a plain reset
 * target). There is no client-side list of "recent turns" and no synthesised
 * checkpoint: a session the daemon recorded nothing for shows nothing.
 *
 * # Two different destructive shapes, kept apart
 *
 * **Fork** (`ForkSession`) copies this session's ledger up to — and excluding —
 * the checkpointed run into a NEW session. The source session is never
 * modified. The daemon enforces the cut rule itself: only an ordinal-1
 * (run-launch) checkpoint may be forked (`fork.mid-run-checkpoint`), which is
 * why the fork action is offered on those rows alone — that restriction is
 * read from `crates/daemon/src/forks.rs`, not invented here.
 *
 * **Restore** (`RestoreCheckpoint`) rewinds a run's real worktree with
 * `git reset --hard` + `git clean -fd`. It is approval-gated **by the daemon**:
 * the command returns as soon as the daemon has parked a
 * `ProposedAction::RestoreCheckpoint` approval carrying its own
 * `RiskLevel::High` reason, and nothing on disk changes until a human approves
 * that card. So this panel reports "restore requested — approval parked", never
 * "restored"; the truth of a restore is the `CheckpointRestored { restored }`
 * event, which arrives on the session stream either way.
 *
 * The daemon's refusals travel back verbatim (`checkpoint.run-active`,
 * `checkpoint.worktree-missing`, `checkpoint.not-found`,
 * `fork.mid-run-checkpoint`). This panel does not pre-empt them with a policy
 * of its own: the run's last known state is shown as a fact, not used as a
 * gate, because the daemon's settled/active test is the authoritative one.
 */
import React, { useMemo, useState } from "react";
import { relativeTime } from "../time.js";
import type { SessionEvent } from "@codypendent/protocol";
import type { DesktopTransport } from "../transport.js";
import { ConfirmPanel, Field, surfaceButton, surfaceStyles } from "./surfaceChrome.js";

export interface BacktrackViewProps {
  /** The attached session's durable ledger, exactly as the daemon replayed it. */
  events: SessionEvent[];
  /** The attached session, or `null` when none is. */
  activeSessionId: string | null;
  transport: DesktopTransport | null;
  /** Why there is no transport, shown instead of an empty list. */
  unavailable?: string | null;
  /** Open the fork the daemon just created. */
  onOpenSession?: (sessionId: string) => void;
}

/** One `CheckpointRecorded` event, plus what the ledger says about its run. */
export interface CheckpointRow {
  checkpointId: string;
  runId: string;
  /** 1-based user-turn ordinal: 1 at launch, +1 per applied steering turn. */
  ordinal: number;
  /** `Stash` / `Commit` / `Unknown` — the daemon's own tag. */
  kind: string;
  commit: string;
  baseCommit: string;
  occurredAt: string;
  /** The objective the run started with, when the ledger names it. */
  objective: string | null;
  /** The run's last recorded state, or `null` when the ledger never said. */
  runState: string | null;
}

/**
 * Fold the session ledger into checkpoint rows.
 *
 * Exported so the derivation is testable without a socket. It reads only what
 * the events carry: a field the ledger did not record stays `null` and is
 * rendered as "not recorded", never as a default.
 */
export function checkpointRows(events: SessionEvent[]): CheckpointRow[] {
  const objectives = new Map<string, string>();
  const states = new Map<string, string>();
  const rows: CheckpointRow[] = [];

  for (const event of events) {
    const body = event.body;
    switch (body.type) {
      case "RunStarted":
        objectives.set(body.run_id, body.objective);
        states.set(body.run_id, "Running");
        break;
      case "RunStateChanged":
        states.set(body.run_id, body.state.type);
        break;
      case "RunCompleted":
        states.set(body.run_id, body.disposition.type);
        break;
      case "CheckpointRecorded":
        rows.push({
          checkpointId: body.checkpoint_id,
          runId: body.run_id,
          ordinal: body.ordinal,
          kind: body.kind.type,
          commit: body.commit,
          baseCommit: body.base_commit,
          occurredAt: event.occurred_at,
          objective: null,
          runState: null,
        });
        break;
      default:
        break;
    }
  }

  // Resolved after the walk so a row picks up the run's FINAL state rather than
  // whatever it happened to be when the checkpoint was cut.
  return rows.map((row) => ({
    ...row,
    objective: objectives.get(row.runId) ?? null,
    runState: states.get(row.runId) ?? null,
  }));
}

/** The daemon forks at run-launch checkpoints only (`crates/daemon/src/forks.rs`). */
export function isForkable(row: CheckpointRow): boolean {
  return row.ordinal === 1;
}

type Pending =
  | { kind: "fork"; row: CheckpointRow }
  | { kind: "restore"; row: CheckpointRow };

function describe(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return String(error);
}

function short(sha: string): string {
  return sha.length > 12 ? `${sha.slice(0, 12)}…` : sha;
}

const banner = (tone: "warn" | "info"): React.CSSProperties => ({
  border: `1px solid ${tone === "warn" ? "var(--cody-warning-border)" : "var(--cody-border-strong)"}`,
  background: tone === "warn" ? "var(--cody-warning-bg)" : "var(--cody-panel-raised)",
  color: tone === "warn" ? "var(--cody-warning-text)" : "var(--cody-text-muted)",
  borderRadius: 8,
  padding: 12,
  fontSize: 12,
  lineHeight: 1.5,
  marginBottom: 12,
});

export const BacktrackView: React.FC<BacktrackViewProps> = ({
  events,
  activeSessionId,
  transport,
  unavailable,
  onOpenSession,
}) => {
  const rows = useMemo(() => checkpointRows(events), [events]);
  const [pending, setPending] = useState<Pending | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [refusal, setRefusal] = useState<string | null>(null);
  const [fork, setFork] = useState<string | null>(null);

  const canFork = Boolean(transport?.forkSession);
  const canRestore = Boolean(transport?.restoreCheckpoint);

  const runFork = async (row: CheckpointRow) => {
    if (!transport?.forkSession) return;
    setPending(null);
    setRefusal(null);
    setNotice(null);
    try {
      const sessionId = await transport.forkSession(row.checkpointId, null);
      setFork(sessionId);
      setNotice(`Forked at turn ${row.ordinal}. The source session was not modified.`);
    } catch (error) {
      // The daemon's refusal, verbatim — this client does not translate it.
      setRefusal(describe(error));
    }
  };

  const runRestore = async (row: CheckpointRow) => {
    if (!transport?.restoreCheckpoint) return;
    setPending(null);
    setRefusal(null);
    setNotice(null);
    setFork(null);
    try {
      await transport.restoreCheckpoint(row.runId, row.checkpointId);
      setNotice(
        "Restore requested. The daemon has parked a high-risk approval and has changed nothing " +
          "yet — approve or reject it on the approval card in Sessions. The restore only happens " +
          "if you approve it.",
      );
    } catch (error) {
      setRefusal(describe(error));
    }
  };

  return (
    <PageShell style={surfaceStyles.page}>
      <div className="cody-page-header" style={surfaceStyles.header}>
        <div>
          <h1 style={surfaceStyles.title}>Checkpoints</h1>
          <div style={surfaceStyles.subtitle}>
            Continue from an earlier point in this conversation or review a workspace restore.
          </div>
        </div>
      </div>

      <div className="cody-page-body" style={surfaceStyles.scroll}>
        {unavailable && (
          <div role="status" style={banner("warn")}>
            {unavailable}
          </div>
        )}

        {activeSessionId === null && (
          <div role="status" style={banner("warn")}>
            Open a chat to see its saved checkpoints.
          </div>
        )}

        {!canFork && !canRestore && !unavailable && (
          <div role="status" style={banner("warn")}>
            The shell exposes neither <code>fork_session</code> nor <code>restore_checkpoint</code>,
            so nothing here can be acted on. Run the desktop app rather than a browser tab.
          </div>
        )}

        {refusal && (
          <div role="alert" style={{ ...banner("warn"), color: "var(--cody-danger-text)", borderColor: "var(--cody-danger)", background: "var(--cody-danger-bg)" }}>
            The daemon refused: {refusal}
          </div>
        )}

        {notice && (
          <div role="status" style={banner("info")}>
            {notice}
            {fork && onOpenSession && (
              <div style={{ marginTop: 8 }}>
                <button style={surfaceButton("primary")} onClick={() => onOpenSession(fork)}>
                  Open the fork
                </button>
              </div>
            )}
            {fork && !onOpenSession && (
              <div style={{ ...surfaceStyles.mono, marginTop: 6, color: "var(--cody-text-secondary)" }}>{fork}</div>
            )}
          </div>
        )}

        {pending && (
          <ConfirmPanel
            title={
              pending.kind === "fork"
                ? `Fork this session at turn ${pending.row.ordinal}?`
                : `Ask the daemon to rewind this worktree to turn ${pending.row.ordinal}?`
            }
            evidence={evidence(pending.row)}
            confirmLabel={pending.kind === "fork" ? "Fork session" : "Request restore"}
            tone={pending.kind === "fork" ? "primary" : "danger"}
            onConfirm={() =>
              void (pending.kind === "fork" ? runFork(pending.row) : runRestore(pending.row))
            }
            onCancel={() => setPending(null)}
          />
        )}

        {pending?.kind === "fork" && (
          <div style={banner("info")}>
            The daemon copies this session's ledger up to — and excluding — the checkpointed run
            into a new session, remapping run ids into a fresh id space. The source session is
            never modified, and runs launched in the fork carve their worktrees from this
            checkpointed filesystem state.
          </div>
        )}
        {pending?.kind === "restore" && (
          <div style={banner("warn")}>
            This is destructive to the run's worktree. The daemon does not act on it directly: it
            parks its own high-risk approval and touches nothing until a human approves that card.
            The restore itself is transactional — the current state is captured behind a private
            ref first and re-applied if any step fails — and it is refused outright while the run
            is not settled, or when the recorded worktree no longer exists on disk.
          </div>
        )}

        {activeSessionId !== null && rows.length === 0 && (
          <div style={{ color: "var(--cody-text-faint)", fontSize: 13 }}>
            No checkpoints recorded for this session yet.
          </div>
        )}

        {rows.map((row) => (
          <div key={row.checkpointId} style={surfaceStyles.card}>
            <div style={{ display: "flex", justifyContent: "space-between", gap: 12 }}>
              <div style={{ fontSize: 13, color: "var(--cody-text)", fontWeight: 600 }}>
                Turn {row.ordinal}
                {row.ordinal === 1 ? " · run launch" : " · steering turn"}
              </div>
              <div style={{ fontSize: 11, color: "var(--cody-text-muted)" }} title={row.occurredAt}>
                {relativeTime(row.occurredAt)}
              </div>
            </div>
            <div style={{ fontSize: 12, color: "var(--cody-text-secondary)", marginTop: 4, whiteSpace: "pre-wrap" }}>
              {row.objective ?? "(the ledger records no objective for this run)"}
            </div>
            <div style={{ marginTop: 8 }}>
              <Field label="checkpoint" value={short(row.checkpointId)} />
              <Field label="kind" value={row.kind} />
              <Field label="commit" value={short(row.commit)} />
              <Field label="base" value={short(row.baseCommit)} />
              <Field label="run state" value={row.runState ?? "not recorded"} />
            </div>
            <div style={{ display: "flex", gap: 8, marginTop: 10, flexWrap: "wrap" }}>
              {isForkable(row) ? (
                <button
                  style={surfaceButton("primary")}
                  disabled={!canFork}
                  onClick={() => setPending({ kind: "fork", row })}
                >
                  Fork from here
                </button>
              ) : (
                <span style={{ fontSize: 11, color: "var(--cody-text-muted)", alignSelf: "center" }}>
                  Not forkable — the daemon forks run-launch checkpoints only.
                </span>
              )}
              <button
                style={surfaceButton("danger")}
                disabled={!canRestore}
                title="Roll the working tree back to this checkpoint's recorded state (asks first)"
                onClick={() => setPending({ kind: "restore", row })}
              >
                Restore worktree…
              </button>
            </div>
          </div>
        ))}
      </div>
    </PageShell>
  );
};

/** The checkpoint's own recorded fields, so the decisive moment keeps them. */
function evidence(row: CheckpointRow): string {
  return [
    `checkpoint  ${row.checkpointId}`,
    `run         ${row.runId}`,
    `ordinal     ${row.ordinal}`,
    `kind        ${row.kind}`,
    `commit      ${row.commit}`,
    `base_commit ${row.baseCommit}`,
    `recorded    ${row.occurredAt}`,
    `run state   ${row.runState ?? "not recorded"}`,
  ].join("\n");
}
