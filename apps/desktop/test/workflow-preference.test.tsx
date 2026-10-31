import { act, fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import type { WorkflowRunSnapshot } from "@codypendent/protocol";
import type { DesktopTransport } from "../src/transport.js";
import { WorkflowView } from "../src/components/WorkflowView.js";
import { createWorkflowLaunchMemory } from "../src/components/workflowInputs.js";

type Start = NonNullable<DesktopTransport["startWorkflow"]>;
type Launcher = "catalog" | "manual";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(yes => { resolve = yes; });
  return { promise, resolve };
}

const snapshot = (id: string): WorkflowRunSnapshot => ({ workflow_run_id: id, phase: { type: "Running" },
  nodes: [{ workflow_run_id: id, node_id: `${id}-node`, state: { type: "Running" }, attempt: 1 }] });

function fixture() {
  const first = deferred<string>();
  const start = vi.fn<Start>().mockReturnValueOnce(first.promise).mockResolvedValueOnce("run-B");
  const watch = vi.fn(async (id: string) => ({ snapshot: snapshot(id), blackboard: [] }));
  const transport = {
    listWorkflows: vi.fn(async () => ({ repository: "/repo", items: [{ id: "review", version: 1,
      definition_digest: "digest", node_count: 1, source: { type: "BuiltIn" },
      inputs: { goal: { input_type: "string", required: true } } }], diagnostics: [], truncated: false })),
    startWorkflow: start, watchWorkflow: watch, readWorkflowRun: vi.fn(async (id: string) => snapshot(id)),
  } as unknown as DesktopTransport;
  return { first, start, watch, transport, memory: createWorkflowLaunchMemory() };
}

async function launch(kind: Launcher, goal: string) {
  if (kind === "catalog") {
    fireEvent.click(await screen.findByRole("button", { name: /^review / }));
    fireEvent.change(screen.getByRole("textbox", { name: /^goal/ }), { target: { value: JSON.stringify(goal) } });
    fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
  } else {
    fireEvent.click(screen.getByText("Start by Workflow ID"));
    fireEvent.change(screen.getByLabelText("Workflow id to start"), { target: { value: "review" } });
    fireEvent.change(screen.getByLabelText("Workflow inputs JSON"), { target: { value: JSON.stringify({ goal }) } });
    fireEvent.click(screen.getByRole("button", { name: "Start workflow" }));
  }
}

it.each<Launcher>(["catalog", "manual"])("keeps the newer %s launch across navigation and an older receipt", async kind => {
  const { first, start, watch, transport, memory } = fixture();
  const original = render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
  await launch(kind, "A");
  original.unmount();

  const current = render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
  await launch(kind, "B");
  await screen.findByTestId("workflow-node-run-B-node");
  expect(start.mock.calls[1][2]?.idempotencyKey).not.toBe(start.mock.calls[0][2]?.idempotencyKey);
  await act(async () => first.resolve("run-A"));
  expect(memory.runs.get("/repo")).toBe("run-B");
  expect(memory.keys.size).toBe(1);
  expect(screen.getByTestId("workflow-node-run-B-node")).toBeTruthy();
  current.unmount();

  render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
  await screen.findByTestId("workflow-node-run-B-node");
  expect(watch).toHaveBeenLastCalledWith("run-B");
});

it.each<Launcher>(["catalog", "manual"])("keeps an explicit run open over a pending %s launch", async kind => {
  const { first, transport, memory } = fixture();
  const current = render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
  await launch(kind, "A");
  fireEvent.click(screen.getByText("Open an existing run"));
  fireEvent.change(screen.getByLabelText("Workflow run id"), { target: { value: "run-B" } });
  fireEvent.click(screen.getByRole("button", { name: "Open run" }));
  await screen.findByTestId("workflow-node-run-B-node");
  await act(async () => first.resolve("run-A"));
  expect(memory.runs.get("/repo")).toBe("run-B");
  expect(screen.queryByTestId("workflow-node-run-A-node")).toBeNull();
  current.unmount();

  render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
  await screen.findByTestId("workflow-node-run-B-node");
});

it("does not let automatic restoration or reconnect outrank a pending background launch", async () => {
  const { first, transport, memory } = fixture();
  const original = render(<WorkflowView transport={transport} repository="/repo" initialRunId="previous" launchMemory={memory} connectionEpoch={1} />);
  await screen.findByTestId("workflow-node-previous-node");
  await launch("catalog", "A");
  original.unmount();

  const current = render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} connectionEpoch={1} />);
  await screen.findByTestId("workflow-node-previous-node");
  current.rerender(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} connectionEpoch={2} />);
  await screen.findByTestId("workflow-node-previous-node");
  await act(async () => first.resolve("run-A"));
  expect(memory.runs.get("/repo")).toBe("run-A");
  expect(memory.keys.size).toBe(1);
  expect(screen.queryByTestId("workflow-node-run-A-node")).toBeNull();
  current.unmount();

  render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} connectionEpoch={2} />);
  await screen.findByTestId("workflow-node-run-A-node");
});

it("keeps a newly requested deep link over an older launch receipt", async () => {
  const { first, transport, memory } = fixture();
  const current = render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
  await launch("catalog", "A");
  current.rerender(<WorkflowView transport={transport} repository="/repo" initialRunId="run-B" launchMemory={memory} />);
  await screen.findByTestId("workflow-node-run-B-node");
  await act(async () => first.resolve("run-A"));
  expect(memory.runs.get("/repo")).toBe("run-B");
  expect(screen.queryByTestId("workflow-node-run-A-node")).toBeNull();
});

it("treats the restored run becoming an explicit deep link as a newer choice", async () => {
  const { first, transport, memory } = fixture();
  const original = render(<WorkflowView transport={transport} repository="/repo" initialRunId="run-B" launchMemory={memory} />);
  await screen.findByTestId("workflow-node-run-B-node");
  original.unmount();

  const current = render(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />);
  await screen.findByTestId("workflow-node-run-B-node");
  await launch("catalog", "A");
  current.rerender(<WorkflowView transport={transport} repository="/repo" initialRunId="run-B" launchMemory={memory} />);
  await act(async () => first.resolve("run-A"));

  expect(memory.runs.get("/repo")).toBe("run-B");
  expect(screen.getByTestId("workflow-node-run-B-node")).toBeTruthy();
  expect(screen.queryByTestId("workflow-node-run-A-node")).toBeNull();
});

it("keeps a newer launch when a deep link clears and accepts a later explicit request for that run", async () => {
  const { first, watch, transport, memory } = fixture();
  const current = render(<WorkflowView transport={transport} repository="/repo" initialRunId="run-B" launchMemory={memory} />);
  await screen.findByTestId("workflow-node-run-B-node");
  await launch("catalog", "A");
  await act(async () => first.resolve("run-A"));
  await screen.findByTestId("workflow-node-run-A-node");
  const reads = watch.mock.calls.length;

  await act(async () => current.rerender(<WorkflowView transport={transport} repository="/repo" launchMemory={memory} />));
  expect(memory.runs.get("/repo")).toBe("run-A");
  expect(watch).toHaveBeenCalledTimes(reads);
  expect(screen.getByTestId("workflow-node-run-A-node")).toBeTruthy();

  current.rerender(<WorkflowView transport={transport} repository="/repo" initialRunId="run-B" launchMemory={memory} />);
  await screen.findByTestId("workflow-node-run-B-node");
  expect(memory.runs.get("/repo")).toBe("run-B");
});
