import { PageSection, PageShell } from "./PageShell.js";
import React, { useCallback, useState } from "react";
import { useLoadOnMount } from "../useLoadOnMount.js";
import type { RepositorySelection } from "../localConfig.js";

/**
 * Choosing which repository the desktop client works in.
 *
 * The shell had no answer to this: `daemon_connect` sent
 * `std::env::current_dir()` as the `repository` on every `CreateSession`,
 * `AttachSession` and `StartRun`. For a bundled `.app` that is the launch
 * directory, and the daemon indexes whatever it is handed — a code graph once
 * reached 510,904 nodes, 76% of it editor cache, because the path it received
 * was a home directory.
 *
 * So this view can only ever ASK for a folder; it cannot assert one. The shell
 * runs the native picker, resolves the git toplevel with the repository-location
 * environment variables stripped, and refuses `$HOME`, an account root, or
 * anything without a `.git`. The three outcomes are kept distinct here:
 *
 * - **selected** — a validated checkout, shown with the path the daemon will get.
 * - **dismissed** — the operator closed the dialog. Nothing changes, no error.
 * - **refused** — the folder was rejected, and the shell's reason is shown
 *   verbatim. There is deliberately no "use it anyway".
 *
 * Changing the selection needs a reconnect, because the repository is bound
 * into the connection's `CreateSession`/`AttachSession`/`StartRun` commands.
 * That is stated rather than done silently.
 */
export interface RepoPickerProps {
  /** Read the current selection. Rejecting means we do not know — not "none". */
  onLoad: () => Promise<RepositorySelection | null>;
  /** Open the native folder picker. Resolves `null` when dismissed. */
  onPick: () => Promise<RepositorySelection | null>;
  /** Select a typed path, through the same gate. */
  onSetPath?: (path: string) => Promise<RepositorySelection>;
  /** Forget the selection. */
  onClear?: () => Promise<void>;
  /**
   * Re-establish the daemon connection so the new repository is bound into it.
   * Omitted when the client is not connected, in which case the view says the
   * selection applies on the next connection.
   */
  onReconnect?: () => Promise<void>;
  /** Whether a daemon connection is currently held. */
  connected?: boolean;
}

type Status =
  | { kind: "loading" }
  /** We asked and got an answer — possibly "none selected", which is real. */
  | { kind: "loaded"; selection: RepositorySelection | null }
  /** We could not find out. NOT the same as "none selected". */
  | { kind: "unavailable"; detail: string };

const panel: React.CSSProperties = {
  padding: 16,
  background: "var(--cody-panel-raised)",
  border: "1px solid var(--cody-border-strong)",
  borderRadius: 8,
};

export const RepoPicker: React.FC<RepoPickerProps> = ({
  onLoad,
  onPick,
  onSetPath,
  onClear,
  onReconnect,
  connected = false,
}) => {
  const [status, setStatus] = useState<Status>({ kind: "loading" });
  const [refusal, setRefusal] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [typedPath, setTypedPath] = useState("");
  const [pendingReconnect, setPendingReconnect] = useState(false);

  const load = useCallback(async () => {
    setStatus({ kind: "loading" });
    try {
      setStatus({ kind: "loaded", selection: await onLoad() });
    } catch (error) {
      setStatus({ kind: "unavailable", detail: describe(error) });
    }
  }, [onLoad]);

  useLoadOnMount(load);

  const adopt = (selection: RepositorySelection | null, dismissedNote: string) => {
    if (selection === null) {
      // Dismissing the dialog is not a failure and must not look like one.
      setNote(dismissedNote);
      return;
    }
    setStatus({ kind: "loaded", selection });
    setRefusal(null);
    setNote(null);
    setPendingReconnect(true);
  };

  const pick = async () => {
    setBusy(true);
    setRefusal(null);
    setNote(null);
    try {
      adopt(await onPick(), "No folder chosen — the repository is unchanged.");
    } catch (error) {
      setRefusal(describe(error));
    } finally {
      setBusy(false);
    }
  };

  const submitTyped = async () => {
    if (!onSetPath || typedPath.trim().length === 0) {
      return;
    }
    setBusy(true);
    setRefusal(null);
    setNote(null);
    try {
      adopt(await onSetPath(typedPath.trim()), "");
      setTypedPath("");
    } catch (error) {
      setRefusal(describe(error));
    } finally {
      setBusy(false);
    }
  };

  const clear = async () => {
    if (!onClear) {
      return;
    }
    setBusy(true);
    setRefusal(null);
    try {
      await onClear();
      setStatus({ kind: "loaded", selection: null });
      setPendingReconnect(true);
    } catch (error) {
      setRefusal(describe(error));
    } finally {
      setBusy(false);
    }
  };

  const reconnect = async () => {
    if (!onReconnect) {
      return;
    }
    setBusy(true);
    try {
      await onReconnect();
      setPendingReconnect(false);
      setNote("Reconnected — new sessions will use this repository.");
    } catch (error) {
      setRefusal(describe(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <PageShell style={{ padding: 24, overflowY: "auto", color: "var(--cody-text)" }}>
      <h2 style={{ margin: "0 0 4px", fontSize: 18, fontWeight: 600 }}>Repository</h2>
      <p style={{ margin: "0 0 20px", fontSize: 13, color: "var(--cody-text-muted)", maxWidth: 720 }}>
        Choose the Git project your chats, agents and councils work in.
      </p>

      {status.kind === "loading" && (
        <div data-testid="repo-loading" role="status" style={{ ...panel, color: "var(--cody-text-muted)" }}>
          Reading the current selection…
        </div>
      )}

      {status.kind === "unavailable" && (
        /* We could not read the selection. Saying "none selected" here would
           assert a fact we do not have. */
        <div
          data-testid="repo-unavailable"
          role="status"
          style={{ ...panel, color: "var(--cody-warning)" }}
        >
          Repository selection unavailable — {status.detail}
        </div>
      )}

      {status.kind === "loaded" && status.selection === null && (
        <div data-testid="repo-none" style={{ ...panel, color: "var(--cody-text-muted)" }}>
          <strong style={{ color: "var(--cody-text)" }}>No repository selected.</strong>
          <div style={{ marginTop: 6, fontSize: 13 }}>
            Choose a project folder to use its files, tasks and code graph.
          </div>
        </div>
      )}

      {status.kind === "loaded" && status.selection !== null && (
        <div data-testid="repo-selected" style={panel}>
          <div style={{ fontSize: 15, fontWeight: 600 }}>{status.selection.name}</div>
          <code
            data-testid="repo-path"
            style={{ display: "block", marginTop: 6, fontSize: 12, color: "var(--cody-text-muted)", wordBreak: "break-all" }}
          >
            {status.selection.path}
          </code>
          {status.selection.picked && (
            /* The operator picked a subdirectory. Say so, rather than letting
               the displayed path silently disagree with what they clicked. */
            <div data-testid="repo-anchored" style={{ marginTop: 8, fontSize: 12, color: "var(--cody-text-muted)" }}>
              Anchored up from <code>{status.selection.picked}</code> to the checkout root, so
              board and knowledge scopes match every other client.
            </div>
          )}
        </div>
      )}

      {refusal && (
        <div
          data-testid="repo-refusal"
          role="alert"
          style={{
            ...panel,
            marginTop: 12,
            background: "var(--cody-danger-bg)",
            borderColor: "var(--cody-danger)",
            color: "var(--cody-danger-text)",
            fontSize: 13,
            whiteSpace: "pre-wrap",
          }}
        >
          {refusal}
        </div>
      )}

      {note && (
        <div data-testid="repo-note" role="status" style={{ marginTop: 12, fontSize: 13, color: "var(--cody-text-muted)" }}>
          {note}
        </div>
      )}

      {pendingReconnect && (
        <div
          data-testid="repo-reconnect"
          role="status"
          style={{
            ...panel,
            marginTop: 12,
            background: "var(--cody-warning-bg)",
            borderColor: "var(--cody-warning-border)",
            color: "var(--cody-warning)",
            fontSize: 13,
          }}
        >
          Reconnect to apply this project to new chats and runs.
          {connected && onReconnect ? (
            <button onClick={() => void reconnect()} disabled={busy} style={buttonStyle} data-testid="repo-reconnect-now">
              Reconnect now
            </button>
          ) : (
            <span> It will apply on the next connection.</span>
          )}
        </div>
      )}

      <div style={{ display: "flex", gap: 8, marginTop: 16, flexWrap: "wrap" }}>
        <button onClick={() => void pick()} disabled={busy} style={primaryButtonStyle} data-testid="repo-pick">
          Choose folder…
        </button>
        <button onClick={() => void load()} disabled={busy} style={buttonStyle} data-testid="repo-refresh">
          Refresh
        </button>
        {onClear && status.kind === "loaded" && status.selection !== null && (
          <button onClick={() => void clear()} disabled={busy} style={buttonStyle} data-testid="repo-clear">
            Clear selection
          </button>
        )}
      </div>

      {onSetPath && (
        <PageSection title="Enter a folder path" description="An alternative to Choose folder">
          <label htmlFor="repo-path-input" style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
            Project folder path
          </label>
          <div style={{ display: "flex", gap: 8, marginTop: 6 }}>
            <input
              id="repo-path-input"
              data-testid="repo-path-input"
              value={typedPath}
              onChange={(event) => setTypedPath(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  void submitTyped();
                }
              }}
              placeholder="/Users/you/code/project"
              style={inputStyle}
            />
            <button
              onClick={() => void submitTyped()}
              disabled={busy || typedPath.trim().length === 0}
              style={buttonStyle}
              data-testid="repo-path-submit"
            >
              Select
            </button>
          </div>
        </PageSection>
      )}
    </PageShell>
  );
};

const buttonStyle: React.CSSProperties = {
  padding: "6px 12px",
  borderRadius: 6,
  border: "1px solid var(--cody-border-strong)",
  background: "var(--cody-inset)",
  color: "var(--cody-text)",
  fontSize: 13,
  cursor: "pointer",
};

const primaryButtonStyle: React.CSSProperties = {
  ...buttonStyle,
  background: "var(--cody-success-strong)",
  borderColor: "var(--cody-success)",
};

const inputStyle: React.CSSProperties = {
  flex: 1,
  padding: "6px 10px",
  borderRadius: 6,
  border: "1px solid var(--cody-border-strong)",
  background: "var(--cody-canvas)",
  color: "var(--cody-text)",
  fontSize: 13,
};

/** The shell's own message, unchanged. It names the reason a folder was refused. */
function describe(error: unknown): string {
  if (typeof error === "string") {
    return error;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}
