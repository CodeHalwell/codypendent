/**
 * First-run setup — the desktop's port of `Overlay::Onboard`.
 *
 * The TUI opens onboarding after boot when the authoritative runnable-model
 * projection is empty and the operator has not chosen to skip
 * (`crates/cli/src/tui.rs::apply_post_boot_onboard_gate`), and it owns only the
 * small screens: triage, skip confirmation, and the validation wait. Provider
 * and model browsing are handed off to the overlays that already exist
 * (`crates/tui/src/state.rs`: "Provider/model browsing is handed off to the
 * existing discovery overlays instead of being copied into this enum").
 *
 * This surface keeps both halves of that shape:
 *
 * - It DETECTS rather than assumes. Every step is answered by
 *   `onboarding_status` in `src-tauri/src/bridge.rs`, which re-reads
 *   `models.toml`, `auth.json`, the provider catalog, the process environment
 *   and the stored repository preference on every call. A step is never marked
 *   done because it was done once, and a read that could not answer renders as
 *   UNKNOWN — never as "not done", which would be a setup wizard shown to
 *   somebody already set up.
 * - It LINKS rather than reimplements. `ProviderPicker`, `ModelPicker`,
 *   `ApiKeys` and `RepoPicker` are the surfaces that do the work; this one
 *   routes to them and re-reads afterwards.
 *
 * What is deliberately NOT ported: the TUI's `OnboardStep::Validating` wait.
 * That step exists because the TUI can ask the host for an authoritative
 * runnable-model refresh and hold setup open until the model can start a run.
 * The shell has no such projection — it configures models, the daemon runs
 * them — so this surface reports CONFIGURATION, and says so, rather than
 * showing a validation screen whose result nothing here can measure. That is
 * the same reason `ModelPicker` carries no readiness badge.
 *
 * The one piece of persisted state is the skip preference, mirroring the TUI's
 * `SessionStore::onboard_skipped`: only the explicit "stop opening this" choice
 * is remembered. Completion is derived from the reads above every time, so
 * there is no second, staler source of truth about whether setup is done.
 */
import React, { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import type { DesktopView } from "./Navigation.js";
import { describeError, localConfigClient, shellAvailable, NO_SHELL } from "./localConfig";
import { surfaceStyles, surfaceButton } from "./surfaceChrome.js";
import { ModelDiscoveryPanel } from "./ModelDiscoveryPanel.js";

/**
 * One first-run condition, exactly as `bridge::OnboardCheck` serializes it.
 *
 * `unknown` is the point of the type: a failed or undecidable read is an
 * outcome, and it is neither "done" nor "not done".
 */
export type OnboardCheck =
  | { state: "satisfied"; detail: string }
  | { state: "unsatisfied"; detail: string }
  | { state: "unknown"; reason: string };

/** What the shell's `onboarding_status` command answers with. */
export type OnboardingStatus = {
  /** At least one `[[model]]` entry in `models.toml`. */
  model: OnboardCheck;
  /** A credential for at least one configured model that RESOLVES NOW. */
  credential: OnboardCheck;
  /** A validated git checkout is selected. */
  repository: OnboardCheck;
  /** The file the first two checks read. */
  models_path: string;
  warnings: string[];
};

/** Read the three conditions from the shell. Rejects when it cannot read them. */
export function readOnboardingStatus(): Promise<OnboardingStatus> {
  return invoke<OnboardingStatus>("onboarding_status");
}

/**
 * Whether first-run setup should open ITSELF.
 *
 * Only a PROVEN blocker opens it: a `models.toml` with no entries, or
 * configured models none of whose credentials resolve. `unknown` never opens
 * it — the TUI's gate has an authoritative projection to consult and this one
 * does not, so anything ambiguous fails closed by staying quiet. The surface
 * remains in the sidebar either way, so nothing is hidden by that choice.
 *
 * The repository step deliberately does not open it: a session can be created
 * without a repository (`StartRun.repository` is optional), so a missing
 * checkout is a recommendation, not a blocker.
 */
export function shouldOpenOnboarding(status: OnboardingStatus): boolean {
  return status.model.state === "unsatisfied" || status.credential.state === "unsatisfied";
}

/** The browser-local key holding the "stop opening this" preference. */
const SKIP_KEY = "codypendent.onboarding.skipped";

/**
 * Whether the operator asked never to be shown setup automatically again.
 *
 * Client-local UI preference, not daemon state and not configuration — the
 * same role `SessionStore::onboard_skipped` plays for the TUI. A storage that
 * refuses to answer is treated as "not skipped": the failure mode is one extra
 * setup screen, not a silently suppressed one.
 */
export function onboardingSkipped(): boolean {
  try {
    return window.localStorage.getItem(SKIP_KEY) === "true";
  } catch {
    return false;
  }
}

/** Record (or clear) the skip preference. Failure to persist is not fatal. */
export function setOnboardingSkipped(skipped: boolean): void {
  try {
    if (skipped) {
      window.localStorage.setItem(SKIP_KEY, "true");
    } else {
      window.localStorage.removeItem(SKIP_KEY);
    }
  } catch {
    // A webview with storage disabled simply asks again next launch.
  }
}

type Load =
  | { kind: "loading" }
  | { kind: "loaded"; status: OnboardingStatus }
  /** We could not find out. NOT "nothing is configured". */
  | { kind: "unavailable"; detail: string };

const BADGE: Record<OnboardCheck["state"], { label: string; fg: string; bg: string; border: string }> = {
  satisfied: { label: "done", fg: "var(--cody-success)", bg: "var(--cody-success-bg)", border: "var(--cody-success-strong)" },
  unsatisfied: { label: "not done", fg: "var(--cody-warning-text)", bg: "var(--cody-warning-bg)", border: "var(--cody-warning-border)" },
  unknown: { label: "unknown", fg: "var(--cody-purple-text)", bg: "var(--cody-purple-bg)", border: "var(--cody-purple)" },
};

/** The evidence sentence a check carries, whichever variant it is. */
function evidence(check: OnboardCheck): string {
  return check.state === "unknown" ? check.reason : check.detail;
}

interface StepProps {
  index: number;
  title: string;
  /** What this step establishes, and what it cannot. */
  summary: string;
  check: OnboardCheck;
  /** The surfaces that actually do the work, in the order to try them. */
  actions: ReadonlyArray<{ label: string; view: DesktopView }>;
  onOpen: (view: DesktopView) => void;
}

const Step: React.FC<StepProps> = ({ index, title, summary, check, actions, onOpen }) => {
  const badge = BADGE[check.state];
  return (
    <section
      aria-label={title}
      data-testid={`onboarding-step-${index}`}
      style={{ ...surfaceStyles.card, display: "flex", flexDirection: "column", gap: 8 }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
        <span style={{ ...surfaceStyles.mono, color: "var(--cody-text-faint)" }}>{index}</span>
        <span style={{ fontSize: 14, fontWeight: 600, color: "var(--cody-text)", flex: 1 }}>{title}</span>
        <span
          data-testid={`onboarding-state-${index}`}
          style={{
            fontSize: 11,
            padding: "1px 8px",
            borderRadius: 10,
            color: badge.fg,
            background: badge.bg,
            border: `1px solid ${badge.border}`,
          }}
        >
          {badge.label}
        </span>
      </div>

      <div style={{ fontSize: 12, color: "var(--cody-text-muted)", lineHeight: 1.5 }}>{summary}</div>

      {/*
        The read's own words, verbatim. A step that says "done" has to be able
        to show what it saw, and a step that says "unknown" has to show why —
        otherwise the badge is an assertion with nothing behind it.
      */}
      <div
        style={{
          ...surfaceStyles.mono,
          color: "var(--cody-text-secondary)",
          background: "var(--cody-canvas)",
          border: "1px solid var(--cody-border-strong)",
          borderRadius: 6,
          padding: "8px 10px",
          whiteSpace: "pre-wrap",
          wordBreak: "break-word",
        }}
      >
        {evidence(check)}
      </div>

      <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
        {actions.map((action) => (
          <button
            key={action.view}
            style={surfaceButton(check.state === "satisfied" ? "neutral" : "primary")}
            onClick={() => onOpen(action.view)}
          >
            {action.label}
          </button>
        ))}
      </div>
    </section>
  );
};

export interface OnboardingProps {
  /**
   * Reads the three conditions. Defaults to the shell command; a test injects
   * its own so the surface can be driven without a Tauri shell.
   */
  read?: () => Promise<OnboardingStatus>;
  /** Open one of the surfaces that does the work. */
  onOpen: (view: DesktopView) => void;
  /**
   * Record "stop opening this automatically". Omitted when the caller does not
   * hold the preference, in which case no skip button is offered — a button
   * that cannot persist its decision is worse than none.
   */
  onSkip?: (skipped: boolean) => void;
  /** Whether the skip preference is currently set. */
  skipped?: boolean;
  /**
   * Whether the daemon is reachable right now. Setup can be complete on disk
   * while nothing is listening on the socket — in which case "Go to Sessions"
   * lands on a disabled composer — so the complete banner says so.
   */
  connected?: boolean;
}

export const Onboarding: React.FC<OnboardingProps> = ({
  read,
  onOpen,
  onSkip,
  skipped = false,
  connected,
}) => {
  const [load, setLoad] = useState<Load>({ kind: "loading" });

  const refresh = useCallback(async () => {
    if (!read && !shellAvailable()) {
      // No shell means no `models.toml`, no `auth.json` and no repository
      // preference to read. Saying "nothing is set up" here would be a claim
      // about files this page never opened.
      setLoad({ kind: "unavailable", detail: NO_SHELL });
      return;
    }
    setLoad({ kind: "loading" });
    try {
      setLoad({ kind: "loaded", status: await (read ?? readOnboardingStatus)() });
    } catch (error) {
      setLoad({ kind: "unavailable", detail: describeError(error) });
    }
  }, [read]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const status = load.kind === "loaded" ? load.status : null;
  const allDone =
    status !== null &&
    status.model.state === "satisfied" &&
    status.credential.state === "satisfied" &&
    status.repository.state === "satisfied";

  return (
    <div style={surfaceStyles.page} data-testid="onboarding">
      <header style={surfaceStyles.header}>
        <div>
          <div style={surfaceStyles.title}>{allDone ? "Your workspace is ready" : "Your workspace is getting ready"}</div>
          <div style={surfaceStyles.subtitle}>
            Codypendent uses the tools and sign-ins already on your computer.
          </div>
        </div>
        <div style={{ display: "flex", gap: 8 }}>
          <button style={surfaceButton()} onClick={() => void refresh()}>
            Re-check
          </button>
          {onSkip && (
            <button
              style={surfaceButton()}
              onClick={() => onSkip(!skipped)}
              title={
                skipped
                  ? "Open this automatically again when a model or credential is missing"
                  : "Stop opening this automatically; it stays in the sidebar"
              }
            >
              {skipped ? "Open automatically again" : "Don't open this automatically"}
            </button>
          )}
        </div>
      </header>

      {shellAvailable() && <ModelDiscoveryPanel client={localConfigClient} onDiscovered={() => { void refresh(); }} />}

      <div style={surfaceStyles.scroll}>
        {load.kind === "loading" && (
          <div role="status" style={{ color: "var(--cody-text-muted)", fontSize: 13 }}>
            Reading local configuration…
          </div>
        )}

        {load.kind === "unavailable" && (
          <div
            role="status"
            style={{
              border: "1px solid var(--cody-warning-border)",
              background: "var(--cody-warning-bg)",
              color: "var(--cody-warning-text)",
              borderRadius: 8,
              padding: 14,
              fontSize: 13,
              lineHeight: 1.5,
            }}
          >
            <strong>Setup state could not be read.</strong> {load.detail}
            <div style={{ marginTop: 8, color: "var(--cody-text-secondary)" }}>
              No step below is shown, because none of them could be answered. This is not the same
              thing as an unconfigured install.
            </div>
          </div>
        )}

        {status && (
          <>
            {allDone && (
              <div
                role="status"
                data-testid="onboarding-complete"
                style={{
                  border: "1px solid var(--cody-success-strong)",
                  background: "var(--cody-success-bg)",
                  color: "var(--cody-success)",
                  borderRadius: 8,
                  padding: 14,
                  fontSize: 13,
                  lineHeight: 1.5,
                  marginBottom: 12,
                }}
              >
                <strong>Setup is complete.</strong> A model is configured, its credential resolves,
                and a repository is selected. Runs are submitted from the composer in Sessions.
                {connected === false && (
                  <div style={{ marginTop: 6, color: "var(--cody-warning-text)" }} data-testid="onboarding-daemon-down">
                    Your workspace is reconnecting automatically. Your saved models and chats will be ready when it returns.
                  </div>
                )}
                <div style={{ marginTop: 8 }}>
                  <button style={surfaceButton("primary")} onClick={() => onOpen("sessions")}>
                    Go to Sessions
                  </button>
                </div>
              </div>
            )}

            <p style={{ fontSize: 12, color: "var(--cody-text-muted)", lineHeight: 1.6, margin: "0 0 12px" }}>
              Installed agents and local models are found automatically. You can also connect a provider or choose a repository below.
            </p>

            <Step
              index={1}
              title="Choose your model"
              summary="Use an installed agent, a local model, or a provider you already have access to. Models found on this computer are added for you."
              check={status.model}
              actions={[
                { label: "Browse providers", view: "providers" },
                { label: "Add a model", view: "models" },
              ]}
              onOpen={onOpen}
            />

            <Step
              index={2}
              title="Use your existing sign-in"
              summary="Installed agents keep their own sign-in. Codypendent uses existing provider keys when available, and local models need no key."
              check={status.credential}
              actions={[{ label: "Manage API keys", view: "keys" }]}
              onOpen={onOpen}
            />

            <Step
              index={3}
              title="Choose a repository"
              summary="Choose the project you want to work on. You can start a conversation first and add a repository later."
              check={status.repository}
              actions={[{ label: "Choose a repository", view: "repository" }]}
              onOpen={onOpen}
            />

            {status.warnings.length > 0 && (
              <div
                role="status"
                style={{
                  border: "1px solid var(--cody-warning-border)",
                  background: "var(--cody-warning-bg)",
                  color: "var(--cody-warning-text)",
                  borderRadius: 8,
                  padding: 12,
                  fontSize: 12,
                  lineHeight: 1.5,
                }}
              >
                <div style={{ fontWeight: 600, marginBottom: 6 }}>
                  Degradations during the read — the steps above may be incomplete:
                </div>
                <ul style={{ margin: 0, paddingLeft: 18 }}>
                  {status.warnings.map((warning) => (
                    <li key={warning}>{warning}</li>
                  ))}
                </ul>
              </div>
            )}

            {skipped && (
              <div style={{ fontSize: 12, color: "var(--cody-text-faint)", marginTop: 12, lineHeight: 1.5 }}>
                This page will not open on its own. It stays in the sidebar, and the steps above are
                re-read every time you open it.
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
};
