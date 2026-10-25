import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ProposedAction } from "@codypendent/protocol";
import { ApprovalConsent } from "../src/components/ApprovalConsent.js";
import type { ApprovalAction } from "../src/approval.js";
import { initialState, reduce } from "../src/daemonState.js";

const action: ProposedAction = { type: "ExecuteCommand", program: "git", args: ["status", "--short"], environment: [], cwd: "/project" };
const risk = { level: { type: "Medium" as const }, reasons: [] };

describe("always allow approval scopes", () => {
  it.each([
    ["This action · this run", "Run"],
    ["Similar commands · this run", "Pattern"],
    ["Similar commands · this repository", "Repository"],
  ])("sends the explicit scope for %s", (label, scope) => {
    const choose = vi.fn();
    render(<ApprovalConsent action={action} pattern="git status *" onChoose={choose} />);
    fireEvent.click(screen.getByRole("button", { name: "Always allow" }));
    expect(screen.getByText("git status *")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: new RegExp(label) }));
    expect(choose).toHaveBeenCalledExactlyOnceWith(scope);
    expect(screen.queryByRole("group", { name: "Always allow scope" })).toBeNull();
  });

  it("only offers identical actions for agent tools and commands without a daemon pattern", () => {
    const tool: ApprovalAction = { type: "McpToolCall", server: "desktop", tool: "getWorkspace", args: "{}", summary: "Read workspace" };
    const view = render(<ApprovalConsent action={tool} pattern="forged *" onChoose={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "Always allow" }));
    expect(screen.queryByText(/this repository/)).toBeNull();
    expect(screen.getByText(/Only this exact action/)).toBeTruthy();
    view.rerender(<ApprovalConsent action={{ ...action, program: "python3" }} onChoose={vi.fn()} />);
    expect(screen.queryByText(/Similar commands/)).toBeNull();
  });

  it("disables all scopes while sending or disconnected and blocks unknown actions", async () => {
    const choose = vi.fn();
    const view = render(<ApprovalConsent action={action} pattern="git status *" onChoose={choose} />);
    fireEvent.click(screen.getByRole("button", { name: "Always allow" }));
    await act(async () => view.rerender(<ApprovalConsent action={action} pattern="git status *" disabled onChoose={choose} />));
    expect(screen.queryByRole("group")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Always allow" }));
    expect(choose).not.toHaveBeenCalled();
    view.rerender(<ApprovalConsent action={{ type: "Unknown" }} onChoose={choose} />);
    expect((screen.getByRole("button", { name: "Always allow" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("preserves the same daemon pattern in live and restored approvals", () => {
    const state = reduce(initialState, { type: "frame", frame: {
      kind: "event", session_id: "session", event: {
        sequence: 1, actor: { type: "System" }, occurred_at: "2026-10-19T00:00:00Z",
        body: { type: "ApprovalRequested", approval_id: "approval", action, risk, pattern: "git status *" },
      },
    } });
    expect(state.transcript[0].approvalPattern).toBe("git status *");
    const restored = reduce(initialState, { type: "frame", frame: {
      kind: "catchup", session_id: "session", snapshot: { type: "Snapshot", through: 1, projection: {
        session_id: "session", title: "Test", last_sequence: 1, closed: false,
        pending_approvals: [{ approval_id: "approval", run_id: "run", action, risk, pattern: "git status *" }],
      } },
    } });
    expect(restored.transcript[0].approvalPattern).toBe("git status *");
  });
});
