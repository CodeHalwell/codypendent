import { PageShell } from "./PageShell.js";
/**
 * The Session Library: ranked, cursor-paged search over `SearchSessions`, plus
 * the lifecycle mutations `MutateSessionLifecycle` carries.
 *
 * Four things this view is careful about, each because getting them wrong is
 * silent:
 *
 * 1. **A failed search is not an empty one.** A rejection stops the "searching"
 *    state and says the search failed; it never falls through to "no sessions
 *    matched", which asserts the daemon looked.
 * 2. **A stale page is discarded.** Two searches can be in flight; the bridge
 *    returns the query each page answers and a page for a query the operator
 *    has typed past is dropped, not re-headed.
 * 3. **A cut page says so.** `next_cursor` present means there is more; the
 *    count is labelled as loaded-so-far rather than as the result set.
 * 4. **Delete is the daemon's policy.** The confirmation says the daemon
 *    applies its retention policy and may tombstone rather than purge, and the
 *    receipt afterwards reports which it did — the client predicts neither.
 *
 * An unauthorized session and an absent one are deliberately indistinguishable
 * on the wire: the daemon answers a generic not-found for both so the command
 * cannot enumerate other people's sessions. Its refusal is rendered verbatim
 * and nothing here adds a word about which case it was.
 */
import React, { useCallback, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ArtifactSaveButton } from "./ArtifactActions.js";
import type { ArtifactRef } from "@codypendent/protocol";
import { useFocusTrap } from "../useFocusTrap.js";
import { relativeTime } from "../time.js";
import type { PageCursor, SessionLifecycleAction, SessionSearchResult } from "@codypendent/protocol";
import type { DesktopTransport, SessionLifecycleOutcome } from "../transport.js";

export interface SessionLibraryProps {
  /** The shell bridge, or `null` outside the shell. */
  transport: DesktopTransport | null;
  /** Why there is no transport, shown instead of an empty library. */
  unavailable?: string | null;
  /** Resume a session: the caller attaches and switches to the transcript. */
  onOpenSession?: (sessionId: string) => void;
}

type SearchState =
  | { status: "idle" }
  | { status: "searching" }
  | { status: "loaded" }
  /** An outcome, not an absence — this is why "searching…" stops. */
  | { status: "failed"; detail: string; continuation: boolean };

type Pending =
  | { kind: "rename"; sessionId: string; title: string; draft: string }
  | { kind: "delete"; sessionId: string; title: string };

const SEARCH_DEBOUNCE_MS = 180;

const panel: React.CSSProperties = {
  flex: 1,
  display: "flex",
  flexDirection: "column",
  height: "100%",
  background: "var(--cody-canvas)",
  color: "var(--cody-text)",
  overflowY: "auto",
};

const button: React.CSSProperties = {
  padding: "4px 10px",
  background: "var(--cody-inset)",
  border: "1px solid var(--cody-border-strong)",
  borderRadius: 6,
  color: "var(--cody-text-secondary)",
  fontSize: 12,
  cursor: "pointer",
};

/**
 * A title must be non-empty and on one line — the reducer's rule, verbatim
 * (`crates/tui/src/reduce.rs`: `title.is_empty() || title.chars().any(char::is_control)`).
 * A control character in a session title reaches every renderer that ever shows
 * it, so it is refused where it is typed.
 */
function validTitle(candidate: string): boolean {
  const trimmed = candidate.trim();
  if (trimmed.length === 0) {
    return false;
  }
  for (const character of trimmed) {
    const code = character.codePointAt(0) ?? 0;
    if (code < 0x20 || (code >= 0x7f && code <= 0x9f)) {
      return false;
    }
  }
  return true;
}

/** A closed session cannot be resumed. The library ranks more rows; it does not
 *  relax what can be resumed. */
function resumable(state: string): boolean {
  return state.toLowerCase() !== "closed";
}

export const SessionLibrary: React.FC<SessionLibraryProps> = ({
  transport,
  unavailable,
  onOpenSession,
}) => {
  const [query, setQuery] = useState("");
  const searchInput = useRef<HTMLInputElement | null>(null);
  const [results, setResults] = useState<SessionSearchResult[]>([]);
  const [nextCursor, setNextCursor] = useState<PageCursor | null>(null);
  const [search, setSearch] = useState<SearchState>({ status: "idle" });
  const [exported, setExported] = useState<ArtifactRef | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const [pendingError, setPendingError] = useState<string | null>(null);
  const [busySessions, setBusySessions] = useState<ReadonlySet<string>>(new Set());
  const operations = useRef(new Map<string, symbol>());
  const mutationGeneration = useRef(0);
  const dialogSession = useRef<string | null>(null);
  dialogSession.current = pending?.sessionId ?? null;

  useLayoutEffect(() => {
    ++mutationGeneration.current;
    operations.current.clear();
    setBusySessions(new Set());
    setPending(null);
    setPendingError(null);
    setNotice(null);
    setExported(null);
    return () => { ++mutationGeneration.current; operations.current.clear(); };
  }, [transport, unavailable]);

  /** What is in the box right now. A page answering anything else is dropped. */
  const searchGeneration = useRef(0);
  const liveQuery = useRef(query);
  liveQuery.current = query;

  const runSearch = useCallback(
    async (text: string, cursor: PageCursor | null) => {
      if (!transport?.searchSessions || unavailable) {
        return;
      }
      const generation = ++searchGeneration.current;
      setSearch({ status: "searching" });
      try {
        const answer = await transport.searchSessions(text, cursor);
        if (generation !== searchGeneration.current || answer.query !== liveQuery.current) {
          // The operator has typed past this query. Showing its page under the
          // current heading would be a lie about what was searched for, so it
          // is discarded — and the newer search's own answer sets the state.
          return;
        }
        setResults((previous) =>
          answer.cursor === null ? answer.page.items : [...previous, ...answer.page.items],
        );
        setNextCursor(answer.page.next_cursor ?? null);
        setSearch({ status: "loaded" });
      } catch (error) {
        if (generation !== searchGeneration.current || liveQuery.current !== text) {
          return;
        }
        setSearch({ status: "failed", detail: describe(error), continuation: cursor !== null });
      }
    },
    [transport, unavailable],
  );

  // Debounced first page. Every keystroke restarts from no cursor: a cursor is
  // only meaningful for the query that issued it.
  useLayoutEffect(() => {
    ++searchGeneration.current;
    setResults([]);
    setNextCursor(null);
    setSearch({ status: "idle" });
    if (!transport?.searchSessions || unavailable) {
      return;
    }
    setSearch({ status: "searching" });
    const timer = setTimeout(() => {
      setResults([]);
      setNextCursor(null);
      void runSearch(query, null);
    }, SEARCH_DEBOUNCE_MS);
    return () => {
      ++searchGeneration.current;
      clearTimeout(timer);
    };
  }, [query, transport, unavailable, runSearch]);

  const mutate = useCallback(
    async (sessionId: string, action: SessionLifecycleAction): Promise<boolean> => {
      if (!transport?.mutateSession || unavailable || operations.current.has(sessionId)) return false;
      const operation = Symbol(sessionId);
      const generation = mutationGeneration.current;
      operations.current.set(sessionId, operation);
      setBusySessions(new Set(operations.current.keys()));
      setNotice(null);
      if (action.type === "Export") setExported(null);
      try {
        const outcome = await transport.mutateSession(sessionId, action);
        if (generation !== mutationGeneration.current) return false;
        setNotice(describeOutcome(outcome));
        if (outcome.outcome === "exported") {
          setExported(outcome.artifact);
        } else {
          applyOutcome(outcome, setResults);
          // A lifecycle change can alter search membership, ranking, and cursor
          // validity. Requery from the first page and retire older responses.
          setNextCursor(null);
          void runSearch(liveQuery.current, null);
        }
        return true;
      } catch (error) {
        if (generation !== mutationGeneration.current) return false;
        const detail = describe(error);
        setNotice(detail);
        if (dialogSession.current === sessionId) setPendingError(detail);
        return false;
      } finally {
        if (operations.current.get(sessionId) === operation) {
          operations.current.delete(sessionId);
          setBusySessions(new Set(operations.current.keys()));
        }
      }
    },
    [transport, unavailable, runSearch],
  );

  const confirmPending = async () => {
    const request = pending;
    if (!request || busySessions.has(request.sessionId)) return;
    setPendingError(null);
    if (request.kind === "rename" && !validTitle(request.draft)) {
      setPendingError("A session title must be non-empty and on one line.");
      return;
    }
    const accepted = await mutate(request.sessionId, request.kind === "rename"
      ? { type: "Rename", title: request.draft.trim() }
      : { type: "Delete", mode: { type: "RetentionPolicy" } });
    if (accepted) setPending((current) => current === request ? null : current);
  };

  const total = results.length;
  const truncated = nextCursor !== null;

  const body = useMemo(() => {
    if (unavailable) {
      return (
        <div data-testid="library-unavailable" role="status" style={notePanel("var(--cody-warning)")}>
          Session Library unavailable — {unavailable}
        </div>
      );
    }
    // A failure with rows already on screen is a failed CONTINUATION: the rows
    // stay (they were really returned) and the banner says the next page never
    // arrived, so the list is not mistaken for the whole set.
    if (search.status === "failed" && total === 0) {
      return (
        <div data-testid="library-failed" role="status" style={notePanel("var(--cody-danger-soft)")}>
          Search failed — {search.detail}
          <div style={{ marginTop: 8, fontSize: 12, color: "var(--cody-text-muted)" }}>
            Your chats will appear when the workspace answers. Try again to reload them.
          </div>
          <button type="button" style={{ ...button, marginTop: 12 }} onClick={() => void runSearch(query, null)}>Retry search</button>
        </div>
      );
    }
    if (search.status === "searching" && total === 0) {
      return (
        <div data-testid="library-searching" role="status" style={notePanel("var(--cody-text-muted)")}>
          Searching…
        </div>
      );
    }
    if (search.status === "idle") {
      return (
        <div data-testid="library-idle" style={notePanel("var(--cody-text-muted)")}>
          Search by conversation, file, change or phrase.
        </div>
      );
    }
    if (total === 0) {
      return (
        <div data-testid="library-empty" style={notePanel("var(--cody-text-muted)")}>
          No sessions matched “{query}”.
        </div>
      );
    }
    return (
      <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
        {search.status === "failed" && (
          <div data-testid="library-page-failed" role="status" style={{ padding: 12, border: "1px solid var(--cody-danger-soft)", borderRadius: 8, color: "var(--cody-danger-soft)", fontSize: 13 }}>
            {search.continuation ? "The next page failed" : "Refresh failed"} — {search.detail}. Previously loaded rows are retained.
            {!search.continuation && <button type="button" style={{ ...button, marginLeft: 8 }} onClick={() => void runSearch(query, null)}>Retry search</button>}
          </div>
        )}
        {results.map((result) => (
          <SessionRow
            key={result.stable_identity}
            result={result}
            onOpen={onOpenSession}
            onRefuse={setNotice}
            busy={busySessions.has(result.session.session_id)}
            canMutate={Boolean(transport?.mutateSession)}
            onMutate={(id, action) => { void mutate(id, action); }}
            onAskRename={(sessionId, title) => {
              setPendingError(null);
              setPending({ kind: "rename", sessionId, title, draft: title });
            }}
            onAskDelete={(sessionId, title) => { setPendingError(null); setPending({ kind: "delete", sessionId, title }); }}
          />
        ))}
      </div>
    );
  }, [unavailable, search, total, query, results, onOpenSession, mutate, busySessions, transport, runSearch]);

  return (
    <PageShell role="region" aria-label="Session Library" style={panel}>
      <div className="cody-page-header" style={{ padding: "20px 24px 12px", borderBottom: "1px solid var(--cody-inset)" }}>
        <h1 style={{ margin: 0, fontSize: 20, fontWeight: 600 }}>All chats</h1>
        <p style={{ margin: "4px 0 12px", fontSize: 13, color: "var(--cody-text-muted)" }}>
          Pick up a previous conversation or find work across your chats.
        </p>
        <input
          aria-label="Search sessions"
          ref={searchInput}
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search sessions…"
          style={{
            width: "100%",
            padding: "8px 10px",
            background: "var(--cody-canvas)",
            border: "1px solid var(--cody-border-strong)",
            borderRadius: 6,
            color: "var(--cody-text)",
            fontSize: 13,
          }}
        />
        {search.status !== "idle" && total > 0 && (
          <div data-testid="library-count" style={{ marginTop: 8, fontSize: 12, color: "var(--cody-text-muted)" }}>
            {truncated
              ? `${total} chats loaded · more available`
              : `${total} ${total === 1 ? "result" : "results"}.`}
          </div>
        )}
      </div>

      {exported && <ArtifactSaveButton key={exported.id} artifact={exported}
        onSave={transport?.saveArtifact ? (artifact) => transport.saveArtifact!(artifact) : undefined} />}
      {notice && (
        <div role="status" style={{ padding: "8px 24px", fontSize: 12, color: "var(--cody-warning)" }}>
          {notice}
        </div>
      )}

      <div style={{ flex: 1, padding: "16px 24px" }}>{body}</div>

      {truncated && (
        <div style={{ padding: "0 24px 20px" }}>
          <button
            style={{ ...button, padding: "6px 14px" }}
            disabled={search.status === "searching"}
            onClick={() => void runSearch(query, nextCursor)}
          >
            {search.status === "searching" ? "Loading…" : "Load the next page"}
          </button>
        </div>
      )}

      {pending?.kind === "rename" && (
        <Prompt
          title="Rename session"
          restoreFocusFallback={searchInput}
          value={pending.draft}
          onChange={(draft) => setPending({ ...pending, draft })}
          confirmLabel="Rename"
          onCancel={() => setPending(null)}
          busy={busySessions.has(pending.sessionId)}
          error={pendingError}
          onConfirm={() => void confirmPending()}
        />
      )}

      {pending?.kind === "delete" && (
        <Prompt
          title="Delete this session?"
          restoreFocusFallback={searchInput}
          detail={`${pending.title}\n\nThe daemon applies its retention policy; it may tombstone rather than purge. This cannot be undone from the client.`}
          confirmLabel="Delete"
          destructive
          onCancel={() => setPending(null)}
          busy={busySessions.has(pending.sessionId)}
          error={pendingError}
          onConfirm={() => void confirmPending()}
        />
      )}
    </PageShell>
  );
};

interface SessionRowProps {
  result: SessionSearchResult;
  onOpen?: (sessionId: string) => void;
  busy: boolean;
  canMutate: boolean;
  onRefuse: (message: string) => void;
  onMutate: (sessionId: string, action: SessionLifecycleAction) => void | Promise<void>;
  onAskRename: (sessionId: string, title: string) => void;
  onAskDelete: (sessionId: string, title: string) => void;
}

const SessionRow: React.FC<SessionRowProps> = ({
  result,
  busy,
  canMutate,
  onOpen,
  onRefuse,
  onMutate,
  onAskRename,
  onAskDelete,
}) => {
  const session = result.session;
  const id = session.session_id;
  const archived = session.archived_at !== null && session.archived_at !== undefined;
  const canResume = resumable(session.state);

  return (
    <div
      data-testid={`library-row-${id}`}
      aria-busy={busy}
      style={{
        padding: 14,
        background: "var(--cody-panel-raised)",
        border: "1px solid var(--cody-border-strong)",
        borderRadius: 8,
        display: "flex",
        flexDirection: "column",
        gap: 8,
      }}
    >
      <div style={{ display: "flex", justifyContent: "space-between", gap: 12, flexWrap: "wrap" }}>
        <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
          <strong style={{ fontSize: 15 }}>{session.title || "Untitled Session"}</strong>
          {session.pinned && <Badge tone="var(--cody-accent-strong)">pinned</Badge>}
          {archived && <Badge tone="var(--cody-text-faint)">archived</Badge>}
          <Badge tone={canResume ? "var(--cody-success-strong)" : "var(--cody-text-faint)"}>{session.state}</Badge>
          <Badge tone="var(--cody-border-strong)">{result.source.type}</Badge>
          <Badge tone="var(--cody-border-strong)">scope: {result.scope.type}</Badge>
        </div>
        <span style={{ fontSize: 11, color: "var(--cody-text-muted)" }} title={session.updated_at}>
          {relativeTime(session.updated_at)}
        </span>
      </div>

      {result.excerpt && (
        <p
          style={{
            margin: 0,
            fontSize: 12,
            color: "var(--cody-text-muted)",
            whiteSpace: "pre-wrap",
            // Clamped: a long transcript excerpt used to make one hit fill
            // the whole viewport. Ranked hits are a scan, not a read.
            maxHeight: 72,
            overflow: "hidden",
          }}
        >
          {result.excerpt}
        </p>
      )}
      {session.repository && (
        <span style={{ fontSize: 11, color: "var(--cody-text-faint)" }}>repo: {session.repository}</span>
      )}

      <div style={{ display: "flex", gap: 8, flexWrap: "wrap", paddingTop: 6, borderTop: "1px solid var(--cody-inset)" }}>
        <button
          style={{ ...button, background: canResume ? "var(--cody-success-strong)" : "var(--cody-inset)", color: canResume ? "var(--cody-on-accent)" : "var(--cody-text-muted)" }}
          aria-label={`Open ${session.title}`}
          disabled={busy || !canResume || !onOpen}
          title={!canResume ? "Closed sessions cannot be resumed" : undefined}
          onClick={() => {
            if (!canResume) {
              onRefuse("cannot resume a closed session");
              return;
            }
            onOpen?.(id);
          }}
        >
          Open
        </button>
        <button style={button} disabled={busy || !canMutate} onClick={() => onAskRename(id, session.title)}>
          Rename
        </button>
        <button
          style={button}
          disabled={busy || !canMutate}
          onClick={() => void onMutate(id, session.pinned ? { type: "Unpin" } : { type: "Pin" })}
        >
          {session.pinned ? "Unpin" : "Pin"}
        </button>
        <button
          style={button}
          disabled={busy || !canMutate}
          onClick={() => void onMutate(id, archived ? { type: "Restore" } : { type: "Archive" })}
        >
          {archived ? "Restore" : "Archive"}
        </button>
        <button
          style={button}
          disabled={busy || !canMutate}
          onClick={() =>
            void onMutate(id, {
              type: "Export",
              options: { format: { type: "Markdown" } },
            })
          }
        >
          Export
        </button>
        <button
          disabled={busy || !canMutate}
          style={{ ...button, color: "var(--cody-danger-soft)", borderColor: "var(--cody-danger-soft)" }}
          onClick={() => onAskDelete(id, session.title)}
        >
          Delete
        </button>
        {busy && <span role="status" style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>Updating session…</span>}
      </div>
    </div>
  );
};

const Badge: React.FC<{ tone: string; children: React.ReactNode }> = ({ tone, children }) => (
  <span
    style={{
      padding: "1px 7px",
      borderRadius: 10,
      fontSize: 11,
      background: tone,
      color: "var(--cody-text)",
    }}
  >
    {children}
  </span>
);

interface PromptProps {
  title: string;
  detail?: string;
  value?: string;
  confirmLabel: string;
  destructive?: boolean;
  busy?: boolean;
  error?: string | null;
  restoreFocusFallback?: React.RefObject<HTMLElement | null>;
  onChange?: (value: string) => void;
  onConfirm: () => void;
  onCancel: () => void;
}

const Prompt: React.FC<PromptProps> = ({
  title,
  detail,
  value,
  confirmLabel,
  destructive,
  busy = false,
  error,
  restoreFocusFallback,
  onChange,
  onConfirm,
  onCancel,
}) => {
  const dialogRef = React.useRef<HTMLDivElement | null>(null);
  const inputRef = React.useRef<HTMLInputElement | null>(null);
  const cancelRef = React.useRef<HTMLButtonElement | null>(null);
  useFocusTrap(dialogRef, {
    active: true,
    initialFocus: onChange ? inputRef : cancelRef,
    restoreFocusFallback,
    onEscape: () => { if (!busy) onCancel(); },
  });
  const wasBusy = React.useRef(busy);
  React.useLayoutEffect(() => {
    if (wasBusy.current === busy) return;
    wasBusy.current = busy;
    // Browsers blur a focused control when it becomes disabled. Keep keys in
    // the pending dialog, then return to the draft (or safe Cancel) on refusal.
    if (busy) dialogRef.current?.focus();
    else (inputRef.current ?? cancelRef.current)?.focus();
  }, [busy]);
  return (
  <div
    ref={dialogRef}
    tabIndex={-1}
    role="dialog"
    aria-label={title}
    aria-modal="true"
    aria-busy={busy}
    // The dialog owns its own keys: Enter confirms, Escape dismisses it —
    // and stops there, so the app-level Escape handler (which cannot see
    // this overlay) does not ALSO navigate the view away underneath it.
    onKeyDown={(event) => {
      if (event.nativeEvent.isComposing || event.keyCode === 229) { event.stopPropagation(); return; }
      if (busy) { event.stopPropagation(); return; }
      if (event.key === "Escape") {
        event.stopPropagation();
        onCancel();
        return;
      }
      if (event.key !== "Enter") {
        return;
      }
      if (event.target instanceof HTMLButtonElement) {
        // A focused Cancel or confirm button already turns its own Enter
        // into a click; confirming here too would fire onConfirm alongside
        // Cancel, or fire it twice alongside Confirm.
        event.stopPropagation();
        return;
      }
      event.stopPropagation();
      onConfirm();
    }}
    style={{
      position: "fixed",
      inset: 0,
      background: "var(--cody-scrim)",
      zIndex: 100,
      display: "flex",
      alignItems: "center",
      justifyContent: "center",
    }}
  >
    <div
      style={{
        width: 460,
        maxWidth: "calc(100vw - 64px)",
        maxHeight: "calc(100vh - 64px)",
        overflowY: "auto",
        padding: 20,
        background: "var(--cody-panel-raised)",
        border: "1px solid var(--cody-border-strong)",
        borderRadius: 10,
      }}
    >
      <h2 style={{ margin: "0 0 10px", fontSize: 16 }}>{title}</h2>
      {detail && (
        <p style={{ margin: "0 0 14px", fontSize: 13, color: "var(--cody-text-muted)", whiteSpace: "pre-wrap" }}>
          {detail}
        </p>
      )}
      {onChange && (
        <input
          aria-label={title}
          ref={inputRef}
          disabled={busy}
          value={value ?? ""}
          onChange={(event) => onChange(event.target.value)}
          style={{
            width: "100%",
            boxSizing: "border-box",
            padding: "8px 10px",
            marginBottom: 14,
            background: "var(--cody-canvas)",
            border: "1px solid var(--cody-border-strong)",
            borderRadius: 6,
            color: "var(--cody-text)",
          }}
        />
      )}
      {error && <p role="alert" style={{ color: "var(--cody-danger-text)" }}>{error}</p>}
      {busy && <p role="status">Waiting for the daemon…</p>}
      <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
        <button ref={cancelRef} type="button" style={button} disabled={busy} onClick={onCancel}>
          Cancel
        </button>
        <button
          style={{
            ...button,
            background: destructive ? "var(--cody-danger)" : "var(--cody-accent-strong)",
            borderColor: destructive ? "var(--cody-danger-soft)" : "var(--cody-accent)",
            color: "var(--cody-on-accent)",
          }}
          type="button"
          disabled={busy}
          onClick={onConfirm}
        >
          {confirmLabel}
        </button>
      </div>
    </div>
  </div>
  );
};

function notePanel(color: string): React.CSSProperties {
  return { padding: "48px 24px", textAlign: "center", color, fontSize: 14 };
}

/** The daemon's own words for what it did. `tombstoned` is its decision. */
function describeOutcome(outcome: SessionLifecycleOutcome): string {
  switch (outcome.outcome) {
    case "applied":
      return `session “${outcome.session.title}” updated`;
    case "deleted":
      return outcome.tombstoned
        ? `session ${outcome.session_id} tombstoned by the daemon's retention policy`
        : `session ${outcome.session_id} deleted`;
    case "exported":
      return `exported to artifact ${outcome.artifact.id} (${outcome.artifact.byte_length} bytes)`;
  }
}

/**
 * Fold the daemon's projection back into the loaded page. A mutation's reply is
 * authoritative; nothing here toggles a local flag and calls that the state.
 */
function applyOutcome(
  outcome: SessionLifecycleOutcome,
  setResults: React.Dispatch<React.SetStateAction<SessionSearchResult[]>>,
): void {
  if (outcome.outcome === "applied") {
    const updated = outcome.session;
    setResults((previous) =>
      previous.map((result) =>
        result.session.session_id === updated.session_id ? { ...result, session: updated } : result,
      ),
    );
  } else if (outcome.outcome === "deleted") {
    setResults((previous) =>
      previous.filter((result) => result.session.session_id !== outcome.session_id),
    );
  }
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
