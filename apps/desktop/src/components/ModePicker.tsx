import { PageShell } from "./PageShell.js";
/**
 * Mode selection.
 *
 * The mode is a FIELD on the next run, not a command of its own: picking one
 * sends nothing, exactly as `Overlay::ModePicker` sets `AppState::default_mode`
 * and stops (`crates/tui/src/reduce.rs`). It rides on the next `StartRun`.
 *
 * The five cards' labels and summaries are the TUI's own
 * (`crates/tui/src/state.rs::MODE_CARDS`), read out of the shell rather than
 * retyped here, so the two clients cannot describe the same mode differently.
 */
import React, { useCallback, useState } from "react";
import { useLoadOnMount } from "../useLoadOnMount.js";

import {
  describeError,
  noticeStyle,
  type Notice,
  localConfigClient,
  shellAvailable,
  NO_SHELL,
  type AgentMode,
  type LocalConfigClient,
  type ModeCard,
} from "./localConfig";

export interface ModePickerProps {
  /** Substituted in tests; the real shell client otherwise. */
  client?: LocalConfigClient;
  /** Told the new mode after the shell accepted it, for a status line. */
  onModeChanged?: (mode: AgentMode, label: string) => void;
}

const PANEL: React.CSSProperties = {
  background: "var(--cody-canvas)",
  color: "var(--cody-text-secondary)",
  fontSize: 13,
  display: "flex",
  flexDirection: "column",
  height: "100%",
};

export const ModePicker: React.FC<ModePickerProps> = ({ client, onModeChanged }) => {
  const api = client ?? localConfigClient;
  const [cards, setCards] = useState<ModeCard[] | null>(null);
  const [current, setCurrent] = useState<AgentMode | null>(null);
  const [unavailable, setUnavailable] = useState<string | null>(
    shellAvailable() || client ? null : NO_SHELL,
  );
  const [notice, setNotice] = useState<Notice | null>(null);

  const load = useCallback(async () => {
    if (!shellAvailable() && !client) {
      setUnavailable(NO_SHELL);
      return;
    }
    try {
      const [modes, defaults] = await Promise.all([api.listModes(), api.runDefaults()]);
      setCards(modes);
      setCurrent(defaults.mode);
      setUnavailable(null);
    } catch (error) {
      // Not an empty list: we never learned what the modes are.
      setCards(null);
      setCurrent(null);
      setUnavailable(describeError(error));
    }
  }, [api, client]);

  useLoadOnMount(load);

  const choose = async (card: ModeCard) => {
    try {
      await api.setRunMode(card.mode);
      setCurrent(card.mode);
      setNotice({ tone: "ok", text: `mode set to ${card.label} — applies to your next run` });
      onModeChanged?.(card.mode, card.label);
    } catch (error) {
      // The staged mode is unchanged, so the highlight must not move.
      setNotice({ tone: "error", text: `could not set mode: ${describeError(error)}` });
    }
  };

  return (
    <PageShell style={PANEL} data-testid="mode-picker">
      <header className="cody-page-header" style={{ padding: "16px 24px", borderBottom: "1px solid var(--cody-inset)" }}>
        <h2 style={{ margin: 0, fontSize: 16, fontWeight: 600 }}>Mode</h2>
        <p style={{ margin: "4px 0 0", color: "var(--cody-text-muted)", fontSize: 12 }}>
          Choose how much your agent can do. This applies to the next run.
        </p>
      </header>

      {unavailable ? (
        <div
          role="status"
          data-testid="mode-picker-unavailable"
          style={{ padding: "32px 24px", color: "var(--cody-warning)" }}
        >
          Modes unavailable — {unavailable}
        </div>
      ) : cards === null ? (
        <div role="status" style={{ padding: "32px 24px", color: "var(--cody-text-muted)" }}>
          Reading modes&hellip;
        </div>
      ) : (
        <div style={{ padding: "12px 16px", display: "flex", flexDirection: "column", gap: 8 }}>
          {cards.map((card) => {
            const selected = current?.type === card.mode.type;
            return (
              <button
                key={card.mode.type}
                type="button"
                aria-pressed={selected}
                onClick={() => void choose(card)}
                style={{
                  textAlign: "left",
                  padding: "10px 12px",
                  borderRadius: 6,
                  cursor: "pointer",
                  background: selected ? "var(--cody-info-bg)" : "var(--cody-panel-raised)",
                  border: `1px solid ${selected ? "var(--cody-link)" : "var(--cody-inset)"}`,
                  color: "var(--cody-text-secondary)",
                  font: "inherit",
                }}
              >
                <span style={{ fontWeight: 600 }}>{card.label}</span>
                {selected && (
                  <span style={{ marginLeft: 8, color: "var(--cody-link)", fontSize: 11 }}>next run</span>
                )}
                <div style={{ color: "var(--cody-text-muted)", fontSize: 12, marginTop: 2 }}>{card.summary}</div>
              </button>
            );
          })}
        </div>
      )}

      {notice && (
        <div role={notice.tone === "error" ? "alert" : "status"} style={noticeStyle(notice)}>
          {notice.text}
        </div>
      )}
    </PageShell>
  );
};

export default ModePicker;
