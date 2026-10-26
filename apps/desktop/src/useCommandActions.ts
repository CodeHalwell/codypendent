import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { sanitizeFailureText } from "./failure.js";

export type CommandFeedback =
  | { phase: "pending" }
  | { phase: "accepted" }
  | { phase: "error"; detail: string };

export interface CommandActions {
  feedback: Readonly<Record<string, CommandFeedback>>;
  isLocked: (key: string) => boolean;
  run: (key: string, command: () => void | Promise<void>, retainAccepted?: boolean) => void;
}

/**
 * One command per resource at a time. An accepted decision stays locked until
 * its durable event retires the card; acceptance alone never resolves it.
 * Keep this hook above navigation to retain that lock when a view remounts.
 */
export function useCommandActions(scope: string): CommandActions {
  const [state, setState] = useState<{ scope: string; feedback: Record<string, CommandFeedback> }>(
    () => ({ scope, feedback: {} }),
  );
  const current = useRef(state);
  if (state.scope !== scope) {
    const next = { scope, feedback: {} };
    current.current = next;
    setState(next);
  }
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  const isLocked = useCallback((key: string) => {
    const feedback = current.current.feedback[key];
    return feedback?.phase === "pending" || feedback?.phase === "accepted";
  }, []);
  const run = useCallback((key: string, command: () => void | Promise<void>, retainAccepted = false) => {
    if (isLocked(key)) return;
    const pending: CommandFeedback = { phase: "pending" };
    const update = (feedback: Record<string, CommandFeedback>) => {
      current.current = { scope: current.current.scope, feedback };
      setState(current.current);
    };
    update({ ...current.current.feedback, [key]: pending });
    const settle = (feedback?: CommandFeedback) => {
      // Identity, rather than a string comparison, handles A -> B -> A scopes
      // and a late reply after a retry or a retired connection.
      if (!mounted.current || current.current.feedback[key] !== pending) return;
      const next = { ...current.current.feedback };
      if (feedback) next[key] = feedback;
      else delete next[key];
      update(next);
    };
    try {
      const result = command();
      void Promise.resolve(result).then(
        () => settle(retainAccepted ? { phase: "accepted" } : undefined),
        (error: unknown) => settle({ phase: "error", detail: sanitizeFailureText(error instanceof Error ? error.message : String(error)) }),
      );
    } catch (error) {
      settle({ phase: "error", detail: sanitizeFailureText(error instanceof Error ? error.message : String(error)) });
    }
  }, [isLocked]);
  return useMemo(() => ({ feedback: state.scope === scope ? state.feedback : {}, isLocked, run }), [state, scope, isLocked, run]);
}
