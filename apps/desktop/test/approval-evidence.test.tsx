import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ProposedAction, Risk } from "@codypendent/protocol";
import { Transcript } from "../src/components/Transcript.js";
import { initialState, reduce } from "../src/daemonState.js";

const risk: Risk = { level: { type: "High" }, reasons: ["Writes to a remote repository"] };
const action: ProposedAction = {
  type: "ExecuteCommand", program: "cargo", args: ["test"],
  environment: [["RUSTC_WRAPPER", "/tmp/custom-wrapper"]], cwd: "/tmp/project",
};

describe("approval evidence", () => {
  it("preserves every action field and the risk for live approvals", () => {
    const state = reduce(initialState, { type: "frame", frame: {
      kind: "event", session_id: "session-1", event: {
        sequence: 1, actor: { type: "System" }, occurred_at: "2026-09-08T00:00:00Z",
        body: { type: "ApprovalRequested", approval_id: "approval-1", action, risk },
      },
    } });
    const approve = vi.fn();
    render(<Transcript items={state.transcript} connectionStatus="connected" onApprove={approve} />);
    expect(screen.getByLabelText("Proposed action details").textContent).toContain("RUSTC_WRAPPER");
    expect(screen.getByLabelText("Proposed action details").textContent).toContain("/tmp/custom-wrapper");
    expect(screen.getByLabelText("Proposed action details").textContent).toContain("/tmp/project");
    expect(screen.getByText("Writes to a remote repository")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(approve).toHaveBeenCalledWith("approval-1");
  });

  it("blocks unknown actions while retaining a working rejection", () => {
    const approve = vi.fn();
    const reject = vi.fn();
    render(<Transcript connectionStatus="connected" onApprove={approve} onReject={reject}
      items={[{ id: "a", type: "approval", text: "Unknown", timestamp: "", approvalId: "a", approvalAction: { type: "Unknown" } }]} />);
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(approve).not.toHaveBeenCalled();
    expect(screen.getByRole("status").textContent).toContain("cannot review");
    fireEvent.click(screen.getByRole("button", { name: "Reject" }));
    expect(reject).toHaveBeenCalledWith("a");
  });

  it("does not present disconnected approval buttons as working controls", () => {
    render(<Transcript connectionStatus="disconnected"
      items={[{ id: "a", type: "approval", text: "command", timestamp: "", approvalId: "a", approvalAction: action }]} />);
    expect((screen.getByRole("button", { name: "Approve" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Reject" }) as HTMLButtonElement).disabled).toBe(true);
  });
});
