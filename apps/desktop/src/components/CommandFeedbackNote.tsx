import type { CommandFeedback } from "../useCommandActions.js";

export function CommandFeedbackNote({ feedback }: { feedback?: CommandFeedback }) {
  if (!feedback) return null;
  return <p role={feedback.phase === "error" ? "alert" : "status"}
    style={{ margin: "8px 0 0", fontSize: 13, overflowWrap: "anywhere", color: feedback.phase === "error" ? "var(--cody-danger-soft)" : "var(--cody-text-muted)" }}>
    {feedback.phase === "pending" ? "Sending…" : feedback.phase === "accepted"
      ? "Decision sent. Waiting for the daemon to confirm resolution."
      : feedback.detail}
  </p>;
}
