import { useEffect, useRef, useState } from "react";
import { canReviewApproval, reusableApprovalPattern, type ApprovalAction, type ApprovalGrant } from "../approval.js";
import "./ApprovalConsent.css";

export interface ApprovalConsentProps {
  action?: ApprovalAction;
  pattern?: string;
  disabled?: boolean;
  onChoose: (scope: ApprovalGrant) => void;
}

export function ApprovalConsent({ action, pattern, disabled, onChoose }: ApprovalConsentProps) {
  const [open, setOpen] = useState(false);
  const container = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const locked = disabled || !canReviewApproval(action);
  const reusable = reusableApprovalPattern(action, pattern);
  useEffect(() => { if (locked) setOpen(false); }, [locked]);
  useEffect(() => {
    if (!open) return;
    const close = (event: PointerEvent) => { if (!container.current?.contains(event.target as Node)) setOpen(false); };
    document.addEventListener("pointerdown", close);
    return () => document.removeEventListener("pointerdown", close);
  }, [open]);
  const choose = (scope: ApprovalGrant) => {
    if (locked) return;
    setOpen(false);
    trigger.current?.focus({ preventScroll: true });
    onChoose(scope);
  };
  return <div ref={container} className="approval-consent" onKeyDown={(event) => {
    if (event.key === "Escape") { event.stopPropagation(); setOpen(false); trigger.current?.focus({ preventScroll: true }); }
  }}>
    <button ref={trigger} type="button" className="approval-consent-trigger" disabled={locked} aria-expanded={open}
      onClick={() => setOpen(!open)}>Always allow <span aria-hidden="true">⌄</span></button>
    {open && <div className="approval-consent-options" role="group" aria-label="Always allow scope">
      <button type="button" onClick={() => choose("Run")}>
        <strong>This action · this run</strong><span>Allow identical requests until this run ends.</span>
      </button>
      {reusable ? <>
        <div className="approval-consent-pattern">Similar commands <code>{reusable}</code></div>
        <button type="button" onClick={() => choose("Pattern")}>
          <strong>Similar commands · this run</strong><span>Allow this command pattern until this run ends.</span>
        </button>
        <button type="button" onClick={() => choose("Repository")}>
          <strong>Similar commands · this repository</strong><span>Remember this command pattern for future runs here.</span>
        </button>
      </> : <p>Only this exact action can be remembered for the current run.</p>}
    </div>}
  </div>;
}
