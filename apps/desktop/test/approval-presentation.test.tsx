import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Transcript } from "../src/components/Transcript.js";
import type { ApprovalAction } from "../src/approval.js";
import type { TranscriptItem } from "../src/types.js";

function defaultText(container: HTMLElement) {
  const copy = container.cloneNode(true) as HTMLElement;
  for (const details of copy.querySelectorAll("details:not([open])")) details.replaceWith(details.querySelector("summary")?.textContent ?? "");
  return copy.textContent ?? "";
}

const source = `return await host.call('desktop.getWorkspace', {}); // ${"source-only-in-details-".repeat(1000)}`;
const native: ApprovalAction = {
  type: "McpToolCall", server: "desktop", tool: "desktop.custom.workspaceCheck",
  summary: "Run the generated workspace helper", args: JSON.stringify({
    definition: { name: "desktop.custom.workspaceCheck", version: 2, source,
      description: "Check the active project", capabilities: ["desktop.getWorkspace"],
      parameters: { type: "object", properties: {} } }, args: {},
  }),
};
const item = (approvalAction?: ApprovalAction, text = "McpToolCall"): TranscriptItem => ({
  id: "approval", type: "approval", approvalId: "approval-id", timestamp: "", text, approvalAction,
});

describe("compact approval review", () => {
  it("explains native definition permissions without displaying its source or serialized action", () => {
    const approve = vi.fn();
    const view = render(<Transcript connectionStatus="connected" onApprove={approve} items={[item(native)]} />);
    const text = defaultText(view.container);
    expect(text).toContain("Workspace Check");
    expect(text).toContain("Check the active project");
    expect(text).toContain("Read workspace context");
    expect(text).not.toContain("McpToolCall");
    expect(text).not.toContain("source-only-in-details");
    expect(text).not.toContain('"parameters"');
    const exact = screen.getByLabelText("Proposed action details");
    expect(exact.closest("details")?.open).toBe(false);
    expect(JSON.parse(exact.textContent!).args).toBe(native.args);
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(approve).toHaveBeenCalledExactlyOnceWith("approval-id");
  });

  it.each(["title", "details"] as const)("explains native MCP data embedded in ACP %s without dumping JSON", (field) => {
    const action: ApprovalAction = { type: "AcpToolCall", agent: "codex-acp", title: "Run saved helper", details: "",
      [field]: `McpToolCall: ${JSON.stringify(native)}` };
    const view = render(<Transcript connectionStatus="connected" items={[item(action, action.title)]} />);
    expect(defaultText(view.container)).toContain("Workspace Check");
    expect(defaultText(view.container)).toContain("Read workspace context");
    expect(defaultText(view.container)).not.toContain("McpToolCall");
    expect(defaultText(view.container)).not.toContain("source-only-in-details");
    expect(JSON.parse(screen.getByLabelText("Proposed action details").textContent!)).toEqual(action);
  });

  it("folds a legacy action payload while retaining its rejection and original review gate", () => {
    const approve = vi.fn();
    const reject = vi.fn();
    const payload = `McpToolCall: ${JSON.stringify(native)}`;
    const view = render(<Transcript connectionStatus="connected" onApprove={approve} onReject={reject} items={[item(undefined, payload)]} />);
    expect(defaultText(view.container)).toContain("Workspace Check");
    expect(defaultText(view.container)).not.toContain("source-only-in-details");
    expect(screen.getByLabelText("Proposed action details").textContent).toBe(payload);
    expect((screen.getByRole("button", { name: "Approve" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Reject" }));
    expect(reject).toHaveBeenCalledExactlyOnceWith("approval-id");
    expect(approve).not.toHaveBeenCalled();
  });

  it("uses a readable prompt for a legacy entry containing only its native action type", () => {
    const view = render(<Transcript connectionStatus="connected" items={[item(undefined)]} />);
    expect(defaultText(view.container)).toContain("Review the proposed action");
    expect(defaultText(view.container)).not.toContain("McpToolCall");
    expect(screen.getByLabelText("Proposed action details").textContent).toBe("McpToolCall");
  });

  it("uses a plain summary for unparseable agent source and keeps the exact bytes reviewable", () => {
    const title = 'McpToolCall: {"functionBody":"incomplete';
    const action: ApprovalAction = { type: "AcpToolCall", agent: "codex-acp", title, details: source };
    const view = render(<Transcript connectionStatus="connected" items={[item(action, title)]} />);
    expect(defaultText(view.container)).toContain("Use an agent tool");
    expect(defaultText(view.container)).not.toContain("functionBody");
    expect(defaultText(view.container)).not.toContain("source-only-in-details");
    expect(JSON.parse(screen.getByLabelText("Proposed action details").textContent!)).toEqual(action);
  });

  it.each(["return await host.call('desktop.getWorkspace', {});", "const helper = () => 42;", "<button>Add one</button>"])("keeps source-only agent titles in details: %s", (title) => {
    const action: ApprovalAction = { type: "AcpToolCall", agent: "codex-acp", title, details: "" };
    const view = render(<Transcript connectionStatus="connected" items={[item(action, title)]} />);
    expect(defaultText(view.container)).toContain("Use an agent tool");
    expect(defaultText(view.container)).not.toContain(title);
    expect(JSON.parse(screen.getByLabelText("Proposed action details").textContent!).title).toBe(title);
  });

  it("keeps technical policy reasons closed while preserving readable risk reasons", () => {
    const reason = JSON.stringify({ source, policy: "Generated tool permissions" });
    const view = render(<Transcript connectionStatus="connected" items={[{ ...item(native), approvalRisk: {
      level: { type: "High" }, reasons: ["Reads the active workspace", reason],
    } }]} />);
    expect(defaultText(view.container)).toContain("Reads the active workspace");
    expect(defaultText(view.container)).not.toContain("source-only-in-details");
    const details = screen.getByText("Policy details").closest("details")!;
    expect(details.open).toBe(false);
    expect(JSON.parse(details.querySelector("pre")!.textContent!).reasons).toContain(reason);
  });

  it("keeps the risk and consequences visible while folding internal approval-routing reasons", () => {
    const reasons = ["Writes to a remote repository", "MCP tool calls to desktop require approval", "ACP transport requires approval", "Policy code: require_approval"];
    const risk = { level: { type: "High" as const }, reasons };
    const view = render(<Transcript connectionStatus="connected" items={[{ ...item(native), approvalRisk: risk }]} />);
    expect(defaultText(view.container)).toContain("Risk: High");
    expect(defaultText(view.container)).toContain("Writes to a remote repository");
    for (const reason of reasons.slice(1)) expect(defaultText(view.container)).not.toContain(reason);
    const details = screen.getByText("Policy details").closest("details")!;
    expect(details.open).toBe(false);
    expect(JSON.parse(details.querySelector("pre")!.textContent!)).toEqual(risk);
  });

  it("keeps large file scopes compact while retaining every proposed path", () => {
    const paths = Array.from({ length: 40 }, (_, index) => `src/file-${index}.ts`);
    const view = render(<Transcript connectionStatus="connected" items={[item({ type: "ReadFiles", paths })]} />);
    expect(defaultText(view.container)).toContain("Read project files");
    expect(defaultText(view.container)).toContain("src/file-0.ts");
    expect(defaultText(view.container)).toContain("36 more in technical details");
    expect(defaultText(view.container)).not.toContain("src/file-39.ts");
    expect(JSON.parse(screen.getByLabelText("Proposed action details").textContent!).paths).toEqual(paths);
  });
});
