import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { GeneratedToolResult } from "../src/agui/GeneratedToolResult.js";
import { QuestionCard } from "../src/components/QuestionCard.js";
import { Transcript } from "../src/components/Transcript.js";
import { initialState, reduce } from "../src/daemonState.js";
import type { TranscriptItem } from "../src/types.js";
import type { ArtifactRef } from "@codypendent/protocol";

/** Text the reader sees before opening any disclosure. */
function defaultText(container: HTMLElement) {
  const copy = container.cloneNode(true) as HTMLElement;
  for (const details of copy.querySelectorAll("details:not([open])")) {
    const summary = details.querySelector("summary");
    details.replaceWith(summary?.textContent ?? "");
  }
  return copy.textContent;
}
const tool: TranscriptItem = { id: "tool", type: "tool_call", timestamp: "", text: "desktop.getWorkspace", toolName: "desktop.getWorkspace", status: "success" };

describe("readable chat actions", () => {
  it("summarises a generated helper approval while keeping its exact source and permissions reviewable", () => {
    const functionBody = `return await host.call('desktop.getWorkspace', {}); // ${"complete-source-".repeat(1000)}`;
    const args = JSON.stringify({ arguments: {}, generatedTool: {
      name: "desktop.custom.workspaceSummary", description: "Summarise the active project", functionBody,
      capabilities: ["desktop.getWorkspace"], version: 3, parameters: { type: "object" },
    } });
    const approve = vi.fn();
    const item: TranscriptItem = { id: "a", type: "approval", text: "McpToolCall: Desktop tool desktop.custom.workspaceSummary", timestamp: "", approvalId: "a",
      approvalAction: { type: "McpToolCall", server: "desktop", tool: "desktop.custom.workspaceSummary", summary: "Desktop tool desktop.custom.workspaceSummary", args } };
    const view = render(<Transcript items={[item]} connectionStatus="connected" onApprove={approve} />);
    expect(defaultText(view.container)).toContain("Workspace Summary");
    expect(defaultText(view.container)).toContain("Summarise the active project");
    expect(defaultText(view.container)).toContain("Read workspace context");
    expect(defaultText(view.container)).not.toContain("functionBody");
    expect(defaultText(view.container)).not.toContain("McpToolCall");
    const exact = screen.getByLabelText("Proposed action details");
    expect((exact.closest("details") as HTMLDetailsElement).open).toBe(false);
    expect(JSON.parse(exact.textContent!).args).toBe(args);
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(approve).toHaveBeenCalledExactlyOnceWith("a");
  });

  it("keeps command targets and the complete execution environment visible without dumping JSON", () => {
    const view = render(<Transcript connectionStatus="connected" items={[{ id: "a", type: "approval", timestamp: "", text: "ExecuteCommand: cargo test", approvalId: "a",
      approvalAction: { type: "ExecuteCommand", program: "cargo", args: ["test"], cwd: "/project", environment: [["RUSTC_WRAPPER", "/custom-wrapper"]] } }]} />);
    const text = defaultText(view.container);
    expect(text).toContain("Run a command");
    expect(text).toContain("cargo test");
    expect(text).toContain("/project");
    expect(text).toContain("RUSTC_WRAPPER=/custom-wrapper");
    expect(text).not.toContain('"environment"');
  });

  it("keeps tool arguments and structured output behind a closed disclosure", () => {
    const toolResult = JSON.stringify({ repository: "/project", sessionId: "internal-session", model: "codex" });
    const view = render(<Transcript connectionStatus="connected" items={[{ ...tool, toolArgs: { selectedFiles: ["file.ts"] }, toolResult }]} />);
    expect(defaultText(view.container)).toContain("Read workspace context");
    expect(defaultText(view.container)).toContain("Result ready.");
    expect(defaultText(view.container)).not.toContain("internal-session");
    expect(defaultText(view.container)).not.toContain("selectedFiles");
    expect(defaultText(view.container)).not.toContain("desktop.getWorkspace");
    const details = screen.getByText("Tool details").closest("details")!;
    expect(details.open).toBe(false);
    expect(details.textContent).toContain(toolResult);
    expect(details.textContent).toContain("file.ts");
  });

  it("summarises legacy JSON result rows and retains their full response", () => {
    const text = JSON.stringify({ items: [{ id: "private-record" }] });
    const view = render(<Transcript connectionStatus="connected" items={[{ id: "result", type: "tool_result", text, timestamp: "" }]} />);
    expect(defaultText(view.container)).toContain("Result ready.");
    expect(defaultText(view.container)).not.toContain("private-record");
    expect(screen.getByText(text).closest("details")?.open).toBe(false);
  });

  it("retains actionable plain tool failures", () => {
    render(<Transcript connectionStatus="connected" items={[{ ...tool, status: "error", toolResult: "File not found: src/missing.ts" }]} />);
    expect(screen.getAllByText("File not found: src/missing.ts")[0].closest("details")).toBeNull();
  });

  it("preserves requested assistant JSON and code verbatim", () => {
    const json = '{"answer":42,"items":["a","b"]}';
    const view = render(<Transcript connectionStatus="connected" items={[
      { id: "user", type: "user", text: "Return JSON and show the code.", timestamp: "" },
      { id: "answer", type: "assistant", text: `${json}\n\n\`\`\`json\n${json}\n\`\`\``, timestamp: "" },
    ]} />);
    expect(defaultText(view.container)).toContain(json);
    expect(view.container.querySelector("pre")?.textContent).toBe(json);
    expect(view.container.querySelector("pre")?.closest("details")).toBeNull();
  });

  it("explains an older provider error recorded after a warning prefix", () => {
    const message = "The gpt-6.1-sol model is not supported when using Codex with a ChatGPT account.";
    const text = `Warning: Skill descriptions were shortened. ${JSON.stringify({ type: "error", status: 400, error: { type: "invalid_request_error", message } })}`;
    const view = render(<Transcript connectionStatus="connected" items={[{ id: "warning", type: "system", text, timestamp: "" }]} />);
    expect(screen.getByRole("alert").textContent).toContain("Provider request failed");
    expect(defaultText(view.container)).toContain(message);
    expect(defaultText(view.container)).not.toContain('"status":400');
    expect(screen.getByText(text).closest("details")?.open).toBe(false);
  });

  it("leaves a user-requested provider error example intact in an assistant answer", () => {
    const text = JSON.stringify({ type: "error", status: 400, error: { type: "invalid_request_error", message: "Example error" } });
    const view = render(<Transcript connectionStatus="connected" items={[
      { id: "request", type: "user", text: "Show the provider error JSON.", timestamp: "" },
      { id: "answer", type: "assistant", text, timestamp: "" },
    ]} />);
    expect(defaultText(view.container)).toContain(text);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("explains an old completed ACP warning with its machine error envelope", () => {
    const text = 'Warning: Model metadata was unavailable. Warning: Skill descriptions were shortened. {"type":"error","status":400,"error":{"type":"invalid_request_error","message":"This model is unavailable for this account."}}';
    const view = render(<Transcript connectionStatus="connected" items={[{ id: "old", type: "assistant", text, timestamp: "" }]} />);
    expect(defaultText(view.container)).toContain("This model is unavailable for this account.");
    expect(defaultText(view.container)).not.toContain('"status":400');
    expect(screen.getByText(text).closest("details")?.open).toBe(false);
  });

  it("folds a streamed provider envelope only after its own run explicitly failed", () => {
    const text = JSON.stringify({ type: "error", status: 400, error: { type: "invalid_request_error", message: "Model unavailable" } });
    let state = reduce(initialState, { type: "frame", frame: { kind: "event", session_id: "s", event: {
      sequence: 1, actor: { type: "System" }, occurred_at: "", body: { type: "ModelStreamDelta", run_id: "failed", text, thought: false },
    } } });
    state = reduce(state, { type: "frame", frame: { kind: "event", session_id: "s", event: {
      sequence: 2, actor: { type: "System" }, occurred_at: "", body: { type: "RunCompleted", run_id: "failed", disposition: { type: "Failed", reason: "Model unavailable" },
        chronicle: { id: "chronicle", media_type: "application/json", byte_length: 1, sha256: "a".repeat(64), sensitivity: { type: "Public" } } },
    } } });
    const view = render(<Transcript connectionStatus="connected" items={[
      ...state.transcript,
      { id: "other-answer", type: "assistant", runId: "completed", text, timestamp: "" },
    ]} />);
    expect(screen.getByText("Provider error details").closest("details")?.open).toBe(false);
    expect(view.container.querySelector(".transcript-assistant")?.textContent).toContain(text);
    expect(screen.getAllByText("Provider error details")).toHaveLength(1);
    expect(state.transcript.every((item) => item.runId === "failed")).toBe(true);
  });

  it("preserves a fenced JSON example even when its run later fails", () => {
    const text = '```json\n{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"Example"}}\n```';
    const view = render(<Transcript connectionStatus="connected" items={[
      { id: "example", type: "assistant", runId: "r", text, timestamp: "" },
      { id: "failure", type: "failure", runId: "r", text: "Later action failed", timestamp: "" },
    ]} />);
    expect(view.container.querySelector(".transcript-assistant pre")?.textContent).toContain('"message":"Example"');
    expect(screen.queryByText("Provider error details")).toBeNull();
  });

  it.each(["none", "daemon", "retry"] as const)("offers only retry for a generic %s failure", (remedy) => {
    render(<Transcript connectionStatus="connected" onRetry={vi.fn()} onOpenView={vi.fn()} items={[
      { id: "failure", type: "failure", text: "ACP agent startup timed out", remedy, objective: "Do the task", timestamp: "" },
    ]} />);
    expect(screen.getByRole("button", { name: "Retry" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /models|keys|providers/i })).toBeNull();
  });

  it("sends an ACP sign-in failure to Providers and an HTTP key failure to API keys", () => {
    const open = vi.fn();
    const view = render(<Transcript connectionStatus="connected" onOpenView={open} items={[
      { id: "failure", type: "failure", text: "ACP agent requires authentication", remedy: "keys", timestamp: "" },
    ]} />);
    fireEvent.click(screen.getByRole("button", { name: "Open providers" }));
    expect(open).toHaveBeenLastCalledWith("providers");
    expect(screen.queryByRole("button", { name: "Open API keys" })).toBeNull();
    view.rerender(<Transcript connectionStatus="connected" onOpenView={open} items={[
      { id: "failure", type: "failure", text: "HTTP provider credential rejected", remedy: "keys", timestamp: "" },
    ]} />);
    fireEvent.click(screen.getByRole("button", { name: "Open API keys" }));
    expect(open).toHaveBeenLastCalledWith("keys");
  });

  it("offers model selection only when the failure names a model remedy", () => {
    render(<Transcript connectionStatus="connected" onOpenView={vi.fn()} items={[
      { id: "failure", type: "failure", text: "No usable model configured", remedy: "models", timestamp: "" },
    ]} />);
    expect(screen.getByRole("button", { name: "Choose a model" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /keys/i })).toBeNull();
  });

  it("renders question formatting while preserving an exact typed JSON answer", () => {
    const resolve = vi.fn();
    const view = render(<QuestionCard questionId="q" onResolve={resolve} prompts={[
      { header: "Output", question: "Choose the **format** for `report.json`.", options: [], custom: true, multiple: false },
    ]} />);
    expect(view.container.querySelector("strong")?.textContent).toBe("format");
    expect(view.container.querySelector("code")?.textContent).toBe("report.json");
    const answer = '{"format":"json"}';
    fireEvent.change(screen.getByLabelText("Your own answer to Output"), { target: { value: answer } });
    fireEvent.click(screen.getByRole("button", { name: "Answer" }));
    expect(resolve).toHaveBeenCalledExactlyOnceWith("q", { type: "Answered", answers: [[answer]] });
  });
});

describe("non-interactive desktop tool artifacts", () => {
  const artifact: ArtifactRef = { id: "result", sha256: "a".repeat(64), byte_length: 100, media_type: "application/vnd.codypendent.desktop-tool+json", sensitivity: { type: "Public" } };
  it("shows a saved-helper confirmation and keeps the original result available", async () => {
    const value = { name: "desktop.custom.workspaceSummary", registered: true, version: 2, execution: "sandbox" };
    const view = render(<GeneratedToolResult artifact={artifact} read={async () => new TextEncoder().encode(JSON.stringify(value))} />);
    await screen.findByText("Saved Workspace Summary for this chat.");
    expect(defaultText(view.container)).not.toContain('"registered"');
    const details = screen.getByText("Technical details").closest("details")!;
    expect(details.open).toBe(false);
    expect(JSON.parse(details.querySelector("pre")!.textContent!)).toEqual(value);
  });
});
