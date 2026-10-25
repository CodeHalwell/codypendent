import { PageShell } from "./PageShell.js";
/**
 * Memory + Journey — the TUI's `Overlay::Memory` and `Overlay::Journey`.
 *
 * Two lists, two data paths, and they are NOT interchangeable:
 *
 * - Memories are read from the daemon's SQLite (there is no `ListMemories` on
 *   the wire) but corrected and forgotten over the PROTOCOL
 *   (`CorrectMemory` / `ForgetMemory`, both keyed by `{ id, repository }`).
 * - Learnings are read AND mutated in SQLite with an optimistic revision;
 *   the store answers `Conflict` / `Duplicate`, which are surfaced as
 *   outcomes rather than swallowed.
 *
 * "Open source" does no I/O: it reveals the evidence string already loaded
 * with the card, exactly as `state.rs` describes.
 */
import React, { useState } from "react";
import type {
  Loaded,
  LearningCard,
  LearningMutation,
  MemoryCard,
} from "./knowledgeTransport.js";
import { ConfirmPanel, Field, SurfaceBody, surfaceButton, surfaceStyles } from "./surfaceChrome.js";

export interface MemoryViewProps {
  memories: Loaded<MemoryCard>;
  learnings: Loaded<LearningCard>;
  /** Present only when the shell can actually serve the read. */
  onRefresh?: () => void;
  /** `CorrectMemory`. Absent when the bridge command does not exist. */
  onCorrectMemory?: (memoryId: string, statement: string) => void;
  /** `ForgetMemory`. Absent when the bridge command does not exist. */
  onForgetMemory?: (memoryId: string) => void;
  /** Optimistic-revision learning mutation. */
  onMutateLearning?: (id: string, revision: number, mutation: LearningMutation) => void;
  /** The last outcome sentence from a mutation, shown verbatim. */
  notice?: string | null;
}

type Pending =
  | { kind: "forget"; memoryId: string; statement: string }
  | { kind: "deleteLearning"; id: string; revision: number; statement: string };

export const MemoryView: React.FC<MemoryViewProps> = ({
  memories,
  learnings,
  onRefresh,
  onCorrectMemory,
  onForgetMemory,
  onMutateLearning,
  notice,
}) => {
  const [tab, setTab] = useState<"memories" | "journey">("memories");
  const [openSource, setOpenSource] = useState<string | null>(null);
  const [correcting, setCorrecting] = useState<{ id: string; buffer: string } | null>(null);
  const [editing, setEditing] = useState<{ id: string; revision: number; buffer: string } | null>(
    null,
  );
  const [pending, setPending] = useState<Pending | null>(null);
  const [refusal, setRefusal] = useState<string | null>(null);

  const tabStyle = (active: boolean): React.CSSProperties => ({
    ...surfaceButton(),
    background: active ? "var(--cody-panel-hover)" : "var(--cody-inset)",
    borderColor: active ? "var(--cody-accent)" : "var(--cody-border-strong)",
    fontWeight: active ? 600 : 400,
  });

  return (
    <PageShell style={surfaceStyles.page}>
      <div className="cody-page-header" style={surfaceStyles.header}>
        <div>
          <h1 style={surfaceStyles.title}>Memory</h1>
          <div style={surfaceStyles.subtitle}>
            What your agents remember, where it came from and what needs your review.
          </div>
        </div>
        <div style={{ display: "flex", gap: 8 }}>
          <button style={tabStyle(tab === "memories")} onClick={() => setTab("memories")}>
            Memories
          </button>
          <button style={tabStyle(tab === "journey")} onClick={() => setTab("journey")}>
            Learning review
          </button>
          {onRefresh && (
            <button style={surfaceButton()} onClick={onRefresh}>
              Refresh
            </button>
          )}
        </div>
      </div>

      <div className="cody-page-body" style={surfaceStyles.scroll}>
        {notice && (
          <div role="status" style={{ ...surfaceStyles.card, color: "var(--cody-success-text)", borderColor: "var(--cody-success-strong)" }}>
            {notice}
          </div>
        )}
        {refusal && (
          <div role="alert" style={{ ...surfaceStyles.card, color: "var(--cody-danger-text)", borderColor: "var(--cody-danger)" }}>
            {refusal}
          </div>
        )}

        {pending?.kind === "forget" && (
          <ConfirmPanel
            title="Forget this memory?"
            evidence={pending.statement}
            confirmLabel="Forget"
            onConfirm={() => {
              onForgetMemory?.(pending.memoryId);
              setPending(null);
            }}
            onCancel={() => setPending(null)}
          />
        )}
        {pending?.kind === "deleteLearning" && (
          <ConfirmPanel
            title="Permanently delete this learning?"
            evidence={pending.statement}
            confirmLabel="Delete"
            onConfirm={() => {
              onMutateLearning?.(pending.id, pending.revision, { type: "Delete" });
              setPending(null);
            }}
            onCancel={() => setPending(null)}
          />
        )}

        {tab === "memories" ? (
          <SurfaceBody
          onRetry={onRefresh}
            status={memories.status}
            detail={memories.detail}
            count={memories.items.length}
            emptyTitle="A workspace that learns with you"
            emptyMessage="Useful context from your runs appears here, ready for you to review. Memory extraction needs a chat-capable model."
          >
            {memories.items.map((memory) => (
              <div key={memory.id} style={surfaceStyles.card}>
                <div style={{ fontSize: 13, color: "var(--cody-text)" }}>{memory.statement}</div>
                <div style={{ marginTop: 8 }}>
                  <Field label="class" value={memory.class} />
                  <Field label="scope" value={memory.scope} />
                  <Field label="from" value={memory.revision} />
                  <Field label="observed" value={memory.observed} />
                  <Field label="confidence" value={memory.confidence.toFixed(2)} />
                </div>
                <div style={{ display: "flex", gap: 8, marginTop: 10, flexWrap: "wrap" }}>
                  <button
                    style={surfaceButton()}
                    title="Reveal the evidence this memory was curated from — already loaded, no file is opened"
                    onClick={() => setOpenSource(openSource === memory.id ? null : memory.id)}
                  >
                    {openSource === memory.id ? "Hide source" : "Open source"}
                  </button>
                  {onCorrectMemory && (
                    <button
                      style={surfaceButton()}
                      onClick={() =>
                        setCorrecting({ id: memory.id, buffer: memory.statement })
                      }
                    >
                      Correct
                    </button>
                  )}
                  {onForgetMemory && (
                    <button
                      style={surfaceButton("danger")}
                      onClick={() =>
                        setPending({
                          kind: "forget",
                          memoryId: memory.id,
                          statement: memory.statement,
                        })
                      }
                    >
                      Forget
                    </button>
                  )}
                </div>
                {openSource === memory.id && (
                  <pre
                    style={{
                      ...surfaceStyles.mono,
                      whiteSpace: "pre-wrap",
                      wordBreak: "break-word",
                      color: "var(--cody-text-muted)",
                      marginTop: 10,
                      marginBottom: 0,
                    }}
                  >
                    {memory.source}
                  </pre>
                )}
                {correcting?.id === memory.id && onCorrectMemory && (
                  <div style={{ marginTop: 10 }}>
                    <textarea
                      value={correcting.buffer}
                      aria-label="Corrected statement"
                      rows={3}
                      onChange={(event) =>
                        setCorrecting({ id: memory.id, buffer: event.target.value })
                      }
                      style={{
                        width: "100%",
                        boxSizing: "border-box",
                        background: "var(--cody-canvas)",
                        border: "1px solid var(--cody-border-strong)",
                        borderRadius: 6,
                        color: "var(--cody-text)",
                        fontSize: 12,
                        padding: 8,
                      }}
                    />
                    <div style={{ display: "flex", gap: 8, marginTop: 6 }}>
                      <button
                        style={surfaceButton("primary")}
                        onClick={() => {
                          const statement = correcting.buffer.trim();
                          if (statement.length === 0) {
                            // Blank refuses; nothing is sent and the prompt stays open.
                            setRefusal("a corrected statement must not be empty");
                            return;
                          }
                          setRefusal(null);
                          onCorrectMemory(memory.id, statement);
                          setCorrecting(null);
                        }}
                      >
                        Send correction
                      </button>
                      <button style={surfaceButton()} onClick={() => setCorrecting(null)}>
                        Cancel
                      </button>
                    </div>
                  </div>
                )}
              </div>
            ))}
          </SurfaceBody>
        ) : (
          <SurfaceBody
          onRetry={onRefresh}
            status={learnings.status}
            detail={learnings.detail}
            count={learnings.items.length}
            emptyTitle="Learning review is clear"
            emptyMessage="No proposed or active learnings for this repository."
          >
            {learnings.items.map((learning) => (
              <div key={learning.id} style={surfaceStyles.card}>
                <div style={{ fontSize: 13, color: "var(--cody-text)" }}>{learning.statement}</div>
                <div style={{ marginTop: 8 }}>
                  <Field label="kind" value={learning.kind} />
                  <Field label="state" value={learning.state} />
                  <Field label="scope" value={learning.scope} />
                  <Field label="provenance" value={learning.provenance} />
                  <Field label="confidence" value={learning.confidence.toFixed(2)} />
                  <Field label="rev" value={learning.revision} />
                  {learning.pinned && <Field label="" value="pinned" />}
                </div>
                {onMutateLearning && (
                  <div style={{ display: "flex", gap: 8, marginTop: 10, flexWrap: "wrap" }}>
                    <button
                      style={surfaceButton()}
                      onClick={() =>
                        onMutateLearning(learning.id, learning.revision, { type: "Activate" })
                      }
                    >
                      Activate
                    </button>
                    <button
                      style={surfaceButton()}
                      onClick={() =>
                        onMutateLearning(learning.id, learning.revision, { type: "Reject" })
                      }
                    >
                      Reject
                    </button>
                    <button
                      style={surfaceButton()}
                      onClick={() =>
                        onMutateLearning(learning.id, learning.revision, {
                          type: "SetPinned",
                          pinned: !learning.pinned,
                        })
                      }
                    >
                      {learning.pinned ? "Unpin" : "Pin"}
                    </button>
                    <button
                      style={surfaceButton()}
                      onClick={() =>
                        setEditing({
                          id: learning.id,
                          revision: learning.revision,
                          buffer: learning.statement,
                        })
                      }
                    >
                      Edit
                    </button>
                    <button
                      style={surfaceButton("danger")}
                      onClick={() =>
                        setPending({
                          kind: "deleteLearning",
                          id: learning.id,
                          revision: learning.revision,
                          statement: learning.statement,
                        })
                      }
                    >
                      Delete
                    </button>
                  </div>
                )}
                {editing?.id === learning.id && onMutateLearning && (
                  <div style={{ marginTop: 10 }}>
                    <textarea
                      value={editing.buffer}
                      aria-label="Edited learning statement"
                      rows={3}
                      onChange={(event) =>
                        setEditing({ ...editing, buffer: event.target.value })
                      }
                      style={{
                        width: "100%",
                        boxSizing: "border-box",
                        background: "var(--cody-canvas)",
                        border: "1px solid var(--cody-border-strong)",
                        borderRadius: 6,
                        color: "var(--cody-text)",
                        fontSize: 12,
                        padding: 8,
                      }}
                    />
                    <div style={{ display: "flex", gap: 8, marginTop: 6 }}>
                      <button
                        style={surfaceButton("primary")}
                        onClick={() => {
                          const statement = editing.buffer.trim();
                          if (statement.length === 0) {
                            setRefusal("a learning statement must not be empty");
                            return;
                          }
                          setRefusal(null);
                          onMutateLearning(editing.id, editing.revision, {
                            type: "EditStatement",
                            statement,
                          });
                          setEditing(null);
                        }}
                      >
                        Save
                      </button>
                      <button style={surfaceButton()} onClick={() => setEditing(null)}>
                        Cancel
                      </button>
                    </div>
                  </div>
                )}
              </div>
            ))}
          </SurfaceBody>
        )}
      </div>
    </PageShell>
  );
};
