/**
 * The banner across the top of the main pane while the daemon is not
 * connected.
 *
 * "Not connected" is not a detail for a corner: it changes what every view
 * below can be trusted to mean. This used to be a sentence and the raw socket
 * error — `No such file or directory (os error 2)` — with nothing to click and
 * no instruction, on top of a reconnect loop that could never succeed because
 * nobody had started a daemon. Now it says what is missing, offers to start it
 * (`launcher.rs`, the CLI's own `ensure_daemon` flow), offers an immediate
 * retry, and names the terminal command for the case where the shell cannot.
 */
import React, { useEffect, useState } from "react";

import type { ConnectionStatus } from "../types.js";
import type { DaemonLaunchStatus } from "../transport.js";
import { useConnectionPresentation } from "../useConnectionPresentation.js";

export interface ConnectionBannerProps {
  status: ConnectionStatus;
  /** The daemon's (or shell's) own reason, verbatim. */
  detail: string;
  /** Whether there is a transport at all — outside the shell there is none. */
  hasTransport: boolean;
  /** Whether the shell offers `startDaemon`. */
  canStart: boolean;
  onStart: () => Promise<{ started: boolean; detail: string }>;
  onRetry: () => void;
  /** Reads what the shell would launch; absent on an older shell. */
  launchStatus?: () => Promise<DaemonLaunchStatus>;
}

const BANNER: React.CSSProperties = {
  padding: "10px 24px",
  fontSize: 12,
  lineHeight: 1.5,
  display: "flex",
  flexDirection: "column",
  gap: 6,
};
const DISCONNECTED: React.CSSProperties = {
  ...BANNER,
  background: "var(--cody-warning-bg)",
  borderBottom: "1px solid var(--cody-warning-border)",
  color: "var(--cody-warning-text)",
};
const CONNECTING: React.CSSProperties = {
  ...BANNER,
  background: "var(--cody-canvas)",
  borderBottom: "1px solid var(--cody-border)",
  color: "var(--cody-text-muted)",
};
const GUIDANCE: React.CSSProperties = { color: "var(--cody-text)" };
const RAW: React.CSSProperties = {
  fontFamily: "var(--cody-mono, ui-monospace, SFMono-Regular, Menlo, monospace)",
  fontSize: 11,
  opacity: 0.9,
  overflowWrap: "anywhere",
};
const ACTIONS: React.CSSProperties = { display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" };
const PRIMARY: React.CSSProperties = {
  background: "var(--cody-success-strong)",
  border: "none",
  color: "var(--cody-on-accent)",
  padding: "5px 12px",
  borderRadius: 6,
  fontSize: 12,
  cursor: "pointer",
  fontWeight: 600,
};
const SECONDARY: React.CSSProperties = {
  background: "var(--cody-inset)",
  border: "1px solid var(--cody-border-strong)",
  color: "var(--cody-text)",
  padding: "5px 12px",
  borderRadius: 6,
  fontSize: 12,
  cursor: "pointer",
  fontWeight: 600,
};
const CODE: React.CSSProperties = {
  fontFamily: "var(--cody-mono, ui-monospace, SFMono-Regular, Menlo, monospace)",
  background: "var(--cody-canvas)",
  border: "1px solid var(--cody-border-strong)",
  borderRadius: 4,
  padding: "0 4px",
};

export const ConnectionBanner: React.FC<ConnectionBannerProps> = ({
  status,
  detail,
  hasTransport,
  canStart,
  onStart,
  onRetry,
  launchStatus,
}) => {
  const [launch, setLaunch] = useState<DaemonLaunchStatus | null>(null);
  const [starting, setStarting] = useState(false);
  const [outcome, setOutcome] = useState<{ started: boolean; detail: string } | null>(null);
  const presentation = useConnectionPresentation(status, starting);

  // What the shell would launch, read whenever we land in the disconnected
  // state. A failed read leaves the banner with the manual command only.
  useEffect(() => {
    if (presentation !== "disconnected" || !launchStatus) {
      return;
    }
    let cancelled = false;
    void launchStatus()
      .then((state) => {
        if (!cancelled) {
          setLaunch(state);
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [presentation, launchStatus]);

  // A fresh disconnect retires the previous attempt's report.
  useEffect(() => {
    if (status === "connected") {
      setOutcome(null);
    }
  }, [status]);

  if (status === "connected") {
    return null;
  }

  if (presentation === "connecting") {
    return (
      <div data-testid="connection-banner" role="status" style={CONNECTING}>
        <strong>Connecting your workspace…</strong>
        <details><summary style={{ cursor: "pointer" }}>Connection details</summary><span style={RAW}>{starting ? "Starting the local service. Your workspace will connect when it is ready." : detail}</span></details>
      </div>
    );
  }

  const manual = launch?.manualCommand ?? "codypendent daemon start";
  const found = launch?.invocation?.program ?? null;
  const start = async () => {
    setStarting(true);
    setOutcome(null);
    try {
      setOutcome(await onStart());
    } finally {
      setStarting(false);
    }
  };

  return (
    <div data-testid="connection-banner" role="alert" style={DISCONNECTED}>
      <div>
        <strong>
          {hasTransport ? "Workspace offline · reconnecting" : "Open Codypendent to connect"}
        </strong>
      </div>
      <div style={GUIDANCE}>
        {hasTransport
          ? "Your sessions are saved. Codypendent is restoring the connection automatically."
          : "This browser preview has no connection to your local workspace. Use the desktop app to run coding tasks."}
      </div>
      <details>
        <summary style={{ cursor: "pointer", fontSize: 11 }}>Connection details</summary>
        <div style={{ ...RAW, marginTop: 6 }}>{detail}</div>
      {hasTransport && (
        <div style={{ ...GUIDANCE, marginTop: 6 }}>
          {launch && !launch.listening ? (
            found ? (
              <>
                No daemon is listening on <span style={CODE}>{launch.socketPath}</span>. Start it
                here — the shell runs <span style={CODE}>{found}</span> for you — or run{" "}
                <span style={CODE}>{manual}</span> in a terminal.
              </>
            ) : (
              <>
                No daemon is listening, and the <span style={CODE}>codypendent</span> program was
                not found on this machine (looked on PATH and in the usual install directories).
                Install Codypendent, then run <span style={CODE}>{manual}</span> in a terminal, or
                set <span style={CODE}>CODYPENDENT_BINARY</span> to the binary and start it here.
              </>
            )
          ) : (
            <>
              The daemon keeps its own process: a run in flight is safe, and the app reconnects the
              moment it answers. If nothing is running, start it here or run{" "}
              <span style={CODE}>{manual}</span> in a terminal.
            </>
          )}
        </div>
      )}
      </details>
      {hasTransport && (
        <div style={ACTIONS}>
          {canStart && (
            <button
              type="button"
              style={PRIMARY}
              disabled={starting}
              onClick={() => void start()}
              data-testid="start-daemon"
            >
              {starting ? "Connecting…" : "Connect workspace"}
            </button>
          )}
          <button type="button" style={SECONDARY} onClick={onRetry} data-testid="retry-connect">
            Retry now
          </button>
          {outcome && (
            <span
              role="status"
              data-testid="start-daemon-outcome"
              style={{ color: outcome.started ? "var(--cody-text)" : "inherit" }}
            >
              {outcome.detail}
            </span>
          )}
        </div>
      )}
    </div>
  );
};
