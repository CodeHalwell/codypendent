import React, { useEffect, useRef, useState, KeyboardEvent } from "react";
import type { RunActivity, RunUsage } from "../types.js";
import { describeActivity, usageLabel } from "../runActivity.js";
import { modelLabel as displayModelName, runDefaultsLabel } from "../modelPresentation.js";
import { ArrowUp, ChevronDown, Cpu, Layers3, ListOrdered, Pause, Play, Square, Waypoints } from "lucide-react";
import "./Composer.css";

interface ComposerProps {
  /**
   * Submit the objective. Resolving `true` means the daemon ACCEPTED the run,
   * and only then is the draft cleared: a refused submission — no model
   * configured, the most likely first-run failure — used to eat a carefully
   * typed objective and leave a red bar in its place. A plain `void` return
   * (older callers, tests) is treated as accepted.
   */
  onSend: (text: string) => void | boolean | Promise<void | boolean>;
  /**
   * What the status strip says while idle-and-connected — the STAGED run
   * defaults ("Plan mode · openai/gpt-5.4"), read from the shell. Absent, the
   * strip falls back to the old constant; it must never invent a mode the
   * shell did not report.
   */
  statusLabel?: string | null;
  onOpenModels?: () => void;
  onOpenMode?: () => void;
  /**
   * Lifted draft state. The composer unmounts on every trip to another view
   * (`App` conditionally mounts the session surface), so component-local
   * state lost the half-typed objective. When the caller supplies both, the
   * draft lives with it and survives the round trip.
   */
  draft?: string;
  onDraftChange?: (draft: string) => void;
  /** A lifted draft's owner compares against its current value, even if this
   * composer unmounts while the daemon is deciding. */
  onDraftAccepted?: (submittedDraft: string) => void;
  /**
   * Queue the text as a follow-up prompt instead of starting a run
   * (`QueuePrompt`, adoption 06).
   *
   * This is what the composer does while a run is LIVE. It used to do nothing
   * at all — the textarea was disabled outright, so there was nowhere to put
   * the next instruction and the only way to say anything was to cancel the
   * run. The TUI has never worked that way: submitting during an active run
   * pushes `Intent::QueuePrompt { delivery: Queue }`
   * (`crates/tui/src/reduce.rs`), and this is that behaviour, ported.
   */
  onQueue?: (text: string) => void | boolean | Promise<void | boolean>;
  /**
   * Whether a real `QueuePrompt` can be sent — connected, with a session the
   * shell is attached to and a bridge that offers the command. False keeps the
   * old behaviour (the textarea is disabled while a run is live), because a
   * composer that accepts text it cannot send anywhere is worse than one that
   * says it is closed.
   */
  canQueue?: boolean;
  /** How many prompts the daemon currently reports queued, for the toggle. */
  queuedCount?: number;
  /** Whether the queue panel is open (rendered by the caller). */
  queueOpen?: boolean;
  /** Open or close the queue panel. */
  onToggleQueue?: () => void;
  /**
   * REQUEST a cancellation — it opens the confirmation, it does not send
   * `CancelRun`. The command is only sent once the operator confirms, in
   * `ConfirmCancel.tsx`; this button used to fire it on the click, and a
   * misclick cost the whole run.
   */
  onRequestCancel?: () => void;
  isRunning: boolean;
  disabled?: boolean;
  /**
   * Whether a real `CancelRun` can be sent — i.e. the client is connected and
   * knows the run's id. When it cannot, the button is not offered at all.
   */
  canCancel?: boolean;
  /**
   * Whether a real `QueueSteering` can be sent — connected, with a live run
   * id. When it cannot, the Steer button is not offered at all, because there
   * is nothing it could truthfully do.
   */
  canSteer?: boolean;
  /** Whether the steering panel is currently open (rendered by the caller). */
  steeringOpen?: boolean;
  /** Open or close the steering panel. */
  onToggleSteering?: () => void;
  /**
   * Which run-lifecycle control the DAEMON would accept for the live run right
   * now, or `null` for neither.
   *
   * Supplied by `runLifecycleAffordance`, which transcribes
   * `validate_run_transition` (`crates/daemon/src/commands.rs`) — pause from
   * any live not-already-paused state, resume ONLY from `Paused`. `null` also
   * covers "this client does not know the run's state", which is a real case
   * after a compact catch-up: neither button is drawn, because offering one
   * would be a claim about a state nobody told us.
   */
  lifecycle?: "pause" | "resume" | null;
  /** Send a real `PauseRun`. */
  onPause?: () => void;
  /** Send a real `ResumeRun`. */
  onResume?: () => void;
  /**
   * What the live run is doing, for the status strip: `working…`,
   * `running shell.run…`, `retrying (2/5) · provider is overloaded · next
   * attempt in 8s`. The strip used to show only the staged defaults, so a
   * provider backoff looked exactly like a hang.
   */
  activity?: RunActivity;
  /** What the provider measured for the last run, once its `RunUsage` arrived. */
  usage?: RunUsage | null;
}

export const Composer: React.FC<ComposerProps> = ({
  onSend,
  statusLabel,
  onOpenModels,
  onOpenMode,
  draft,
  onDraftChange,
  onDraftAccepted,
  onQueue,
  canQueue,
  queuedCount,
  queueOpen,
  onToggleQueue,
  onRequestCancel,
  isRunning,
  disabled,
  canCancel,
  canSteer,
  steeringOpen,
  onToggleSteering,
  lifecycle,
  onPause,
  onResume,
  activity,
  usage,
}) => {
  const [localInput, setLocalInput] = useState("");
  // Controlled when the caller lifts the draft; local otherwise (tests and
  // older callers keep working unchanged).
  const input = draft !== undefined ? draft : localInput;
  const setInput = (next: string) => {
    if (onDraftChange) {
      onDraftChange(next);
    }
    if (draft === undefined) {
      setLocalInput(next);
    }
  };
  /**
   * The draft as it stands NOW, for a callback that resolves after the person
   * may have kept typing. `input` captured inside an async closure is the
   * value at submit time, which is exactly the value that must not be used to
   * decide whether the box still holds what was sent.
   */
  const latestInput = useRef(input);
  latestInput.current = input;

  /**
   * Auto-grow to the text, up to ~10 rows: a pasted stack trace used to
   * scroll inside a fixed 3-row box while the transcript above had the
   * space. Height is derived from `scrollHeight` after each change.
   */
  const areaRef = useRef<HTMLTextAreaElement | null>(null);
  useEffect(() => {
    const area = areaRef.current;
    if (!area) {
      return;
    }
    area.style.height = "auto";
    const line = 21; // ~14px font at 1.5 line height
    const max = line * 10 + 24;
    area.style.height = `${Math.min(area.scrollHeight, max)}px`;
  }, [input]);

  /**
   * While a run is live the composer QUEUES rather than starts a run — but only
   * when a real `QueuePrompt` can be sent. Without that it stays closed, as it
   * always was.
   */
  const queueing = Boolean(isRunning && canQueue && onQueue);
  /** Whether the textarea accepts text at all right now. */
  const open = !disabled && (!isRunning || queueing);

  /** True while a submission is awaiting the daemon's accept/refuse. */
  const [sending, setSending] = useState(false);
  const submissionPending = useRef(false);

  const submit = () => {
    // The RAW draft as well as the trimmed one: the box holds the raw text, so
    // the pending-edit guard below has to compare like with like.
    const draft = input;
    const text = draft.trim();
    if (!text || !open || submissionPending.current) {
      return;
    }
    // Run starts and queued follow-ups have the same acceptance contract.
    // Hold synchronously as well as in state, including before React renders.
    submissionPending.current = true;
    setSending(true);
    void Promise.resolve()
      .then(() => (queueing ? onQueue?.(text) : onSend(text)))
      .then((accepted) => {
        // Cleared only once the daemon accepted the run, AND only while the
        // box still holds what was sent. A refusal keeps the draft exactly as
        // typed, next to the banner that explains it. The textarea stays
        // editable during the round trip — `sending` blocks a second submit,
        // not typing — so an unconditional clear deleted the next objective
        // somebody had already started writing. Against the RAW draft: a
        // trailing newline or a padded paste differs from what was sent, and
        // comparing with the trimmed text left accepted work in the box.
        if (accepted !== false) {
          if (onDraftAccepted) {
            onDraftAccepted(draft);
          } else if (latestInput.current === draft) {
            setInput("");
          }
        }
      })
      .catch(() => undefined)
      .finally(() => {
        submissionPending.current = false;
        setSending(false);
      });
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter confirms the current IME candidate before it submits an objective.
    // Safari can clear isComposing on that final keydown but retains keyCode 229.
    if (e.nativeEvent.isComposing || e.nativeEvent.keyCode === 229) return;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  };

  const handleSend = () => submit();
  const activityText = activity ? describeActivity(activity) : null;
  const usageText = usageLabel(usage ?? null);

  const defaults = statusLabel?.split(" · ");
  const separatedDefaults = defaults?.length === 2;
  const modeLabel = separatedDefaults ? defaults[0] : "Mode";
  const modelLabel = separatedDefaults ? displayModelName(defaults[1]) : "Choose model";
  const submitLabel = queueing ? "Queue" : sending ? "Sending…" : "Send";

  return (
    <div className="workspace-composer">
      <div className="composer-surface" data-running={isRunning}>
        <textarea
          aria-label={queueing ? "Follow-up prompt" : "Objective"}
          autoCorrect="off"
          autoCapitalize="off"
          autoComplete="off"
          spellCheck={false}
          ref={areaRef}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder={
            disabled
              ? "Connect your workspace to start a coding task."
              : queueing
                ? "Queue a follow-up for this session (Enter to queue, Shift+Enter for newline)..."
                : isRunning
                  ? "Run in progress..."
                  : "Ask Codypendent or describe a task (Enter to send, Shift+Enter for newline)..."
          }
          disabled={!open}
          rows={3}
          className="composer-input"
        />
        <div className="composer-toolbar">
          <div className="composer-defaults" aria-label="Run configuration">
            {onOpenMode ? (
              <button type="button" className="composer-config" onClick={onOpenMode} title="Choose the mode for your next run">
                <Layers3 size={14} aria-hidden="true" /><span>{modeLabel}</span><ChevronDown size={12} aria-hidden="true" />
              </button>
            ) : separatedDefaults ? <span className="composer-config"><Layers3 size={14} aria-hidden="true" />{modeLabel}</span> : null}
            {onOpenModels ? (
              <button type="button" className="composer-config composer-model" onClick={onOpenModels} title="Choose the model for your next run">
                <Cpu size={14} aria-hidden="true" /><span>{modelLabel}</span><ChevronDown size={12} aria-hidden="true" />
              </button>
            ) : separatedDefaults ? <span className="composer-config composer-model"><Cpu size={14} aria-hidden="true" /><span>{modelLabel}</span></span> : null}
          </div>
          <div className="composer-controls">
            {lifecycle === "pause" && onPause && (
              <button type="button" onClick={onPause} data-testid="composer-pause" className="composer-control">
                <Pause size={13} aria-hidden="true" /><span>Pause Run</span>
              </button>
            )}
            {lifecycle === "resume" && onResume && (
              <button type="button" onClick={onResume} data-testid="composer-resume" className="composer-control composer-resume">
                <Play size={13} aria-hidden="true" /><span>Resume Run</span>
              </button>
            )}
            {onToggleQueue && (
              <button type="button" onClick={onToggleQueue} aria-expanded={Boolean(queueOpen)} data-testid="composer-queue" className="composer-control">
                <ListOrdered size={14} aria-hidden="true" /><span>Queue ({queuedCount ?? 0})</span>
              </button>
            )}
            {isRunning && canSteer && onToggleSteering && (
              <button type="button" onClick={onToggleSteering} aria-expanded={Boolean(steeringOpen)} data-testid="composer-steer" className="composer-control">
                <Waypoints size={14} aria-hidden="true" /><span>{steeringOpen ? "Hide steering" : "Steer Run"}</span>
              </button>
            )}
            {isRunning && canCancel && onRequestCancel && (
              <button type="button" onClick={onRequestCancel} className="composer-control composer-cancel">
                <Square size={12} aria-hidden="true" /><span>Cancel Run</span>
              </button>
            )}
            <button type="button" onClick={handleSend} disabled={!input.trim() || !open || sending} className="composer-submit" aria-label={submitLabel} title={`${submitLabel} · Enter`}>
              <ArrowUp size={19} strokeWidth={2.2} aria-hidden="true" />
            </button>
          </div>
        </div>
      </div>
      <div className="composer-status">
        <span className="composer-status-copy">
          {disabled ? "Not connected" : <>
            {isRunning ? "next run: " : ""}{runDefaultsLabel(statusLabel)}
            {queueing ? " · queueing follow-ups" : ""}
          </>}
        </span>
        {!disabled && activityText ? (
          <span data-testid="composer-activity" className="composer-activity">{activityText}</span>
        ) : !disabled && !isRunning && usageText ? (
          <span data-testid="composer-usage" className="composer-usage" title="What the provider measured for the last run">{usageText}</span>
        ) : <span className="composer-shortcut">Enter to send <span aria-hidden="true">·</span> Shift + Enter for a new line</span>}
      </div>
    </div>
  );
};
