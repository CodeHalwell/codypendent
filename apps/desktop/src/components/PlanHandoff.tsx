import { ArrowRight, ClipboardCheck } from "lucide-react";
import "./PlanHandoff.css";

/** This prepares an editable draft. It has no run or queue submission capability. */
export function PlanHandoff({ preparing, error, onPrepare }: {
  preparing: boolean;
  error: string | null;
  onPrepare: () => void;
}) {
  return <section className="plan-handoff" aria-label="Plan ready">
    <ClipboardCheck size={19} aria-hidden="true" className="plan-handoff-icon" />
    <div className="plan-handoff-copy">
      <strong>Plan ready to build</strong>
      <p>Prepare a Build draft, keep your feedback, and review before sending. Nothing starts yet.</p>
      {error && <p role="alert">{error}</p>}
    </div>
    <button type="button" className="plan-handoff-action" disabled={preparing} onClick={onPrepare}>
      {preparing ? "Preparing Build draft…" : "Implement This Plan"}<ArrowRight size={15} aria-hidden="true" />
    </button>
  </section>;
}
