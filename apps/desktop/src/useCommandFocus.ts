import { useCallback, useLayoutEffect, useRef } from "react";
import type { CommandFeedback } from "./useCommandActions.js";

/** Keep keyboard focus in a decision card while its controls are disabled. */
export function useCommandFocus(feedback?: CommandFeedback) {
  const ref = useRef<HTMLDivElement>(null);
  const returnTo = useRef<HTMLElement | null>(null);
  const preserveFocus = useCallback(() => {
    const active = document.activeElement;
    if (active instanceof HTMLElement && ref.current?.contains(active) && active !== ref.current) {
      returnTo.current = active;
      ref.current.focus({ preventScroll: true });
    }
  }, []);
  useLayoutEffect(() => {
    if (feedback?.phase === "pending" || feedback?.phase === "accepted") return;
    const target = returnTo.current;
    returnTo.current = null;
    // Never pull focus back from somewhere the operator deliberately moved.
    if (target?.isConnected && document.activeElement === ref.current) target.focus({ preventScroll: true });
  }, [feedback]);
  return { ref, preserveFocus };
}
