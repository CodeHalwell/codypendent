import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { WorkflowRunSnapshot } from "@codypendent/protocol";
import { WorkflowView } from "../src/components/WorkflowView.js";
import type { DesktopTransport } from "../src/transport.js";
import { createWorkflowLaunchMemory } from "../src/components/workflowInputs.js";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

const snapshot = (runId: string, label: string): WorkflowRunSnapshot => ({ workflow_run_id: runId, phase: { type: "Running" },
  nodes: [{ workflow_run_id: runId, node_id: label, state: { type: "Running" }, attempt: 1 }] });
const bridge = (overrides: Partial<DesktopTransport>) => ({ readWorkflowRun: vi.fn(async (id: string) => snapshot(id, "current-node")), ...overrides } as unknown as DesktopTransport);

describe("workflow view recovery", () => {
  it.each([
    ['{"amount":1e999}', /Workflow inputs must use finite numbers/],
    [JSON.stringify({ text: "é".repeat(40_000) }), /Workflow inputs must fit within 64 KiB/],
  ])("explains invalid manual input without dispatching a start (%#)", async (value, notice) => {
    const start = vi.fn();
    render(<WorkflowView transport={bridge({ startWorkflow: start })} repository="/repo" />);
    fireEvent.click(screen.getByText("Start by Workflow ID"));
    fireEvent.change(screen.getByLabelText("Workflow id to start"), { target: { value: "review" } });
    fireEvent.change(screen.getByLabelText("Workflow inputs JSON"), { target: { value } });
    fireEvent.click(screen.getByRole("button", { name: "Start workflow" }));
    await screen.findByText(notice);
    expect(start).not.toHaveBeenCalled();
  });

  it("rejects a snapshot belonging to a different run", async () => {
    render(<WorkflowView transport={bridge({ watchWorkflow: vi.fn(async () => ({ snapshot: snapshot("foreign", "wrong-node"), blackboard: [] })) })} initialRunId="requested" />);
    await screen.findByText(/snapshot for another workflow run/);
    expect(screen.queryByTestId("workflow-node-wrong-node")).toBeNull();
  });

  it("opens a deep-linked run exactly once after the initial connection becomes available", async () => {
    const watch = vi.fn(async (id: string) => ({ snapshot: snapshot(id, "connected-node"), blackboard: [] }));
    const transport = bridge({ watchWorkflow: watch });
    const view = render(<WorkflowView transport={transport} initialRunId="run" unavailable="Connecting" connectionEpoch={0} />);
    expect(watch).not.toHaveBeenCalled();
    view.rerender(<WorkflowView transport={transport} initialRunId="run" unavailable={null} connectionEpoch={1} />);
    await screen.findByTestId("workflow-node-connected-node");
    expect(watch).toHaveBeenCalledExactlyOnceWith("run");
  });

  it("reopens a pending watch after reconnect and ignores an older same-run snapshot", async () => {
    const old = deferred<{ snapshot: WorkflowRunSnapshot; blackboard: [] }>();
    const fresh = deferred<{ snapshot: WorkflowRunSnapshot; blackboard: [] }>();
    const watch = vi.fn().mockReturnValueOnce(old.promise).mockReturnValueOnce(fresh.promise);
    const transport = bridge({ watchWorkflow: watch });
    const view = render(<WorkflowView transport={transport} initialRunId="run" connectionEpoch={1} />);
    view.rerender(<WorkflowView transport={transport} initialRunId="run" connectionEpoch={2} />);
    await waitFor(() => expect(watch).toHaveBeenCalledTimes(2));
    await act(async () => fresh.resolve({ snapshot: snapshot("run", "fresh-node"), blackboard: [] }));
    await act(async () => old.resolve({ snapshot: snapshot("run", "old-node"), blackboard: [] }));
    expect(screen.getByTestId("workflow-node-fresh-node")).toBeTruthy();
    expect(screen.queryByTestId("workflow-node-old-node")).toBeNull();
  });

  it("drops an old repository's late snapshot instead of drawing it in the new workspace", async () => {
    const old = deferred<{ snapshot: WorkflowRunSnapshot; blackboard: [] }>();
    const transport = bridge({ watchWorkflow: vi.fn(() => old.promise) });
    const view = render(<WorkflowView transport={transport} repository="/old" initialRunId="run" />);
    view.rerender(<WorkflowView transport={transport} repository="/new" />);
    await act(async () => old.resolve({ snapshot: snapshot("run", "old-repository-node"), blackboard: [] }));
    expect(screen.queryByTestId("workflow-node-old-repository-node")).toBeNull();
    expect(screen.getByTestId("workflow-idle")).toBeTruthy();
  });

  it("serializes manual starts and reuses the original launch identity after a lost acknowledgement", async () => {
    const first = deferred<string>();
    const start = vi.fn().mockReturnValueOnce(first.promise).mockResolvedValueOnce("receipt");
    const watch = vi.fn(async (id: string) => ({ snapshot: snapshot(id, "accepted-node"), blackboard: [] }));
    render(<WorkflowView transport={bridge({ startWorkflow: start, watchWorkflow: watch })} repository="/repo" />);
    fireEvent.click(screen.getByText("Start by Workflow ID"));
    fireEvent.change(screen.getByLabelText("Workflow id to start"), { target: { value: "review" } });
    fireEvent.change(screen.getByLabelText("Workflow inputs JSON"), { target: { value: '{"objective":"review"}' } });
    fireEvent.click(screen.getByRole("button", { name: "Start workflow" }));
    fireEvent.keyDown(screen.getByLabelText("Workflow id to start"), { key: "Enter" });
    expect(start).toHaveBeenCalledTimes(1);
    await act(async () => first.reject(new Error("Reply lost")));
    fireEvent.click(screen.getByRole("button", { name: "Start workflow" }));
    await waitFor(() => expect(screen.getByTestId("workflow-node-accepted-node")).toBeTruthy());
    expect(start.mock.calls[1]).toEqual(start.mock.calls[0]);
    expect(start.mock.calls[0][2]).toEqual({ expectedRepository: "/repo", idempotencyKey: expect.any(String) });
  });

  it("keeps a paused run's delayed refresh out of a newly opened run", async () => {
    const oldRead = deferred<WorkflowRunSnapshot>();
    const read = vi.fn(() => oldRead.promise);
    const watch = vi.fn(async (id: string) => ({ snapshot: snapshot(id, `${id}-node`), blackboard: [] }));
    render(<WorkflowView transport={bridge({ watchWorkflow: watch, pauseWorkflow: vi.fn(async () => undefined), readWorkflowRun: read })} initialRunId="first" />);
    await screen.findByTestId("workflow-node-first-node");
    fireEvent.click(screen.getByRole("button", { name: "Pause" }));
    await waitFor(() => expect(read).toHaveBeenCalledWith("first"));
    fireEvent.click(screen.getByText("Open an existing run"));
    fireEvent.change(screen.getByLabelText("Workflow run id"), { target: { value: "second" } });
    fireEvent.click(screen.getByRole("button", { name: "Open run" }));
    await screen.findByTestId("workflow-node-second-node");
    await act(async () => oldRead.resolve(snapshot("first", "stale-refresh")));
    expect(screen.queryByTestId("workflow-node-stale-refresh")).toBeNull();
    expect(screen.getByTestId("workflow-node-second-node")).toBeTruthy();
  });

  it("keeps a control's older refresh from overwriting a newer snapshot of the same run", async () => {
    const oldRead = deferred<WorkflowRunSnapshot>();
    const read = vi.fn(() => oldRead.promise);
    const watch = vi.fn().mockResolvedValueOnce({ snapshot: snapshot("run", "initial-node"), blackboard: [] })
      .mockResolvedValueOnce({ snapshot: snapshot("run", "fresh-node"), blackboard: [] });
    render(<WorkflowView transport={bridge({ watchWorkflow: watch, pauseWorkflow: vi.fn(async () => undefined), readWorkflowRun: read })} initialRunId="run" />);
    await screen.findByTestId("workflow-node-initial-node");
    fireEvent.click(screen.getByRole("button", { name: "Pause" }));
    await waitFor(() => expect(read).toHaveBeenCalledWith("run"));
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await screen.findByTestId("workflow-node-fresh-node");
    await act(async () => oldRead.resolve(snapshot("run", "older-control-read")));
    expect(screen.queryByTestId("workflow-node-older-control-read")).toBeNull();
    expect(screen.getByTestId("workflow-node-fresh-node")).toBeTruthy();
  });

  it.each([false, true])("retains a manual launch identity across navigation when the old receipt arrives before retry=%s", async (acknowledgedBeforeRetry) => {
    const oldReceipt = deferred<string>();
    const start = vi.fn().mockReturnValueOnce(oldReceipt.promise).mockResolvedValueOnce("owned-run");
    const watch = vi.fn(async (id: string) => ({ snapshot: snapshot(id, "restored-node"), blackboard: [] }));
    const transport = bridge({ startWorkflow: start, watchWorkflow: watch });
    const memory = createWorkflowLaunchMemory();
    const first = render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
    fireEvent.click(screen.getByText("Start by Workflow ID"));
    fireEvent.change(screen.getByLabelText("Workflow id to start"), { target: { value: "review" } });
    fireEvent.change(screen.getByLabelText("Workflow inputs JSON"), { target: { value: '{"objective":"keep my feedback"}' } });
    fireEvent.click(screen.getByRole("button", { name: "Start workflow" }));
    first.unmount();
    const second = render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
    fireEvent.click(screen.getByText("Start by Workflow ID"));
    expect((screen.getByLabelText("Workflow inputs JSON") as HTMLInputElement).value).toContain("keep my feedback");
    if (acknowledgedBeforeRetry) await act(async () => oldReceipt.resolve("owned-run"));
    fireEvent.click(screen.getByRole("button", { name: "Start workflow" }));
    await screen.findByTestId("workflow-node-restored-node");
    expect(start.mock.calls[1]).toEqual(start.mock.calls[0]);
    if (!acknowledgedBeforeRetry) await act(async () => oldReceipt.resolve("owned-run"));
    second.unmount();
    render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
    await screen.findByTestId("workflow-node-restored-node");
    expect(start).toHaveBeenCalledTimes(2);
    expect(watch).toHaveBeenLastCalledWith("owned-run");
  });
});
