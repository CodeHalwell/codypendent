import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Transcript } from "../src/components/Transcript.js";
import { activityEntries } from "../src/components/activityEntries.js";
import type { TranscriptItem } from "../src/types.js";
import { initialState, reduce } from "../src/daemonState.js";
import type { SessionEvent } from "@codypendent/protocol";

const call = (index: number, status: TranscriptItem["status"] = "success"): TranscriptItem => ({
  id: `call-${index}`, type: "tool_call", timestamp: "", toolRunId: "run-a", toolName: "workspace.read_file",
  toolLabel: `src/file-${index}.ts`, text: `src/file-${index}.ts`, status,
  toolArgs: { path: `src/file-${index}.ts` }, ...(status === "error" ? { toolResult: "File could not be read" } : {}),
});
const open = async (summary: HTMLElement) => {
  await act(async () => { fireEvent.click(summary); });
  await waitFor(() => expect(summary.closest("details")?.open).toBe(true));
};

describe("tool activity history", () => {
  it("folds 50 calls into one lazy searchable history, without changing the stored items", async () => {
    const items = Object.freeze(Array.from({ length: 50 }, (_, i) => Object.freeze(call(i))));
    const view = render(<Transcript items={items as unknown as TranscriptItem[]} connectionStatus="connected" />);
    expect(view.container.querySelectorAll(".tool-activity")).toHaveLength(1);
    expect(view.container.querySelectorAll(".transcript-tool")).toHaveLength(0);
    expect(screen.getByText("· 50 calls")).toBeTruthy();
    const summary = view.container.querySelector<HTMLDetailsElement>(".tool-activity")!.querySelector("summary")!;
    await open(summary);
    expect(view.container.querySelectorAll(".transcript-tool")).toHaveLength(50);
    expect(screen.getByText("src/file-49.ts")).toBeTruthy();
    fireEvent.change(screen.getByRole("searchbox", { name: "Find an action" }), { target: { value: "file-27" } });
    expect(view.container.querySelectorAll(".transcript-tool")).toHaveLength(1);
    fireEvent.click(summary);
    await waitFor(() => expect(view.container.querySelectorAll(".transcript-tool")).toHaveLength(0));
    expect(items).toHaveLength(50);
    expect(items[27].toolArgs).toEqual({ path: "src/file-27.ts" });
  });

  it("shows running work and errors in the summary and preserves expansion during live changes", async () => {
    const items = [call(0), call(1, "error"), call(2, "finished"), call(3, "running")];
    const view = render(<Transcript items={items} connectionStatus="connected" />);
    expect(screen.getByText("1 failed")).toBeTruthy();
    expect(screen.getByText("1 unavailable")).toBeTruthy();
    expect(screen.getByText("1 running")).toBeTruthy();
    expect(screen.getByRole("status").textContent).toBe("src/file-3.ts");
    await open(view.container.querySelector(".tool-activity > summary")!);
    expect(screen.getAllByText("File could not be read").length).toBeGreaterThan(0);
    view.rerender(<Transcript items={[...items.slice(0, 3), call(3), call(4, "running")]} connectionStatus="connected" />);
    expect(view.container.querySelector<HTMLDetailsElement>(".tool-activity")!.open).toBe(true);
    expect(screen.getByText("src/file-4.ts")).toBeTruthy();
    expect(screen.getByText("finished · result unavailable")).toBeTruthy();
  });

  it("keeps approvals visible and focusable after 50 calls", () => {
    const approve = vi.fn();
    const items: TranscriptItem[] = [...Array.from({ length: 50 }, (_, i) => call(i)), {
      id: "approval", type: "approval", timestamp: "", text: "Run a command", approvalId: "pending",
      approvalAction: { type: "ExecuteCommand", program: "wc", args: ["-l", "src/a.ts"], cwd: null, environment: [] },
    }];
    render(<Transcript items={items} connectionStatus="connected" focusDecision="Approval:pending" onApprove={approve} />);
    const card = screen.getByRole("group", { name: "Approval required" });
    expect(document.activeElement).toBe(card);
    expect(card.closest(".tool-activity")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /^Approve$/ }));
    expect(approve).toHaveBeenCalledWith("pending");
  });

  it("preserves run boundaries, outputs and interactive artifacts", () => {
    const artifact = { id: "widget", media_type: "application/vnd.codypendent.desktop-tool+json" } as NonNullable<TranscriptItem["artifact"]>;
    const items: TranscriptItem[] = [call(0), call(1), { ...call(2), toolRunId: "run-b" }, { ...call(3), toolRunId: "run-b" },
      { ...call(4), artifact }, { id: "answer", type: "assistant", text: "Here is the result", timestamp: "" }, call(5), call(6)];
    expect(activityEntries(items).map((entry) => [entry.id, entry.grouped, entry.items.length])).toEqual([
      ["call-0", true, 2], ["call-2", true, 2], ["call-4", false, 1], ["answer", false, 1], ["call-5", true, 2],
    ]);
    const outputs: TranscriptItem[] = [call(0), { id: "out", type: "tool_result", timestamp: "", text: "Result", runId: "run-a" }];
    expect(activityEntries(outputs)[0].items).toHaveLength(2);
    expect(activityEntries([{ ...call(0), artifact: { ...artifact, media_type: "text/plain" } }, call(1)])[0].items).toHaveLength(2);
  });

  it("retains an open search when live calls move the 200-item window", async () => {
    const items = Array.from({ length: 200 }, (_, i) => call(i));
    const view = render(<Transcript items={items} connectionStatus="connected" />);
    await open(view.container.querySelector(".tool-activity > summary")!);
    fireEvent.change(screen.getByRole("searchbox", { name: "Find an action" }), { target: { value: "file-27" } });
    view.rerender(<Transcript items={[...items, call(200)]} connectionStatus="connected" />);
    expect(view.container.querySelector<HTMLDetailsElement>(".tool-activity")!.open).toBe(true);
    expect((screen.getByRole("searchbox", { name: "Find an action" }) as HTMLInputElement).value).toBe("file-27");
    expect(screen.getByText("src/file-27.ts")).toBeTruthy();
  });

  it("keeps blocked tool attempts in the same run history with a visible count", async () => {
    const event = { sequence: 1, occurred_at: "", actor: { type: "System" }, body: { type: "ToolDenied", run_id: "run-a",
      action: { type: "ExecuteCommand", program: "blocked", args: [], cwd: null, environment: [] }, reasons: ["Command is not allowed"] } } as SessionEvent;
    const state = reduce(initialState, { type: "frame", frame: { kind: "event", session_id: "s", event } });
    expect(state.transcript[0].runId).toBe("run-a");
    const view = render(<Transcript items={[call(0), ...state.transcript, call(1)]} connectionStatus="connected" />);
    expect(view.container.querySelectorAll(".tool-activity")).toHaveLength(1);
    expect(screen.getByText("2 calls", { exact: false })).toBeTruthy();
    expect(screen.getByText("1 blocked")).toBeTruthy();
    expect(screen.queryByText(/Command is not allowed/)).toBeNull();
    await open(view.container.querySelector(".tool-activity > summary")!);
    expect(screen.getByText(/Command is not allowed/)).toBeTruthy();
  });

  it("collects tools across inline progress without repeating activity rows", async () => {
    const items = Array.from({ length: 25 }, (_, i) => [
      { id: `progress-${i}`, type: "thought" as const, text: `Checking file ${i}`, timestamp: "", runId: "run-a" }, call(i),
    ]).flat();
    const view = render(<Transcript items={items} connectionStatus="connected" />);
    expect(view.container.querySelectorAll(".tool-activity")).toHaveLength(1);
    expect(view.container.querySelectorAll(".transcript-thought")).toHaveLength(25);
    expect(screen.getByText("· 25 calls")).toBeTruthy();
    await open(view.container.querySelector(".tool-activity > summary")!);
    expect(view.container.querySelectorAll(".transcript-tool")).toHaveLength(25);
    expect(screen.getByText("Checking file 24")).toBeTruthy();
  });

  it("keeps activity expanded when its inline progress leaves the current page", async () => {
    const progress: TranscriptItem = { id: "progress", type: "thought", text: "Checking source", timestamp: "", runId: "run-a" };
    const items = [progress, ...Array.from({ length: 199 }, (_, i) => call(i))];
    const view = render(<Transcript items={items} connectionStatus="connected" />);
    await open(view.container.querySelector(".tool-activity > summary")!);
    view.rerender(<Transcript items={[...items, call(200)]} connectionStatus="connected" />);
    expect(view.container.querySelector<HTMLDetailsElement>(".tool-activity")!.open).toBe(true);
  });
});
