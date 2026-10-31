import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { WorkflowCatalog } from "@codypendent/protocol";
import type { DesktopTransport } from "../src/transport.js";
import { WorkflowLauncher } from "../src/components/WorkflowLauncher.js";
import { createWorkflowLaunchMemory } from "../src/components/workflowInputs.js";

type Entry = WorkflowCatalog["items"][number];
type Start = NonNullable<DesktopTransport["startWorkflow"]>;
type List = NonNullable<DesktopTransport["listWorkflows"]>;
const entry = (id = "review", overrides: Partial<Entry> = {}): Entry => ({
  id, version: 2, definition_digest: "digest-one", node_count: 3, description: "Review a proposed change.", source: { type: "BuiltIn" },
  inputs: { goal: { input_type: "UserGoal", required: true }, context: { input_type: "ReviewContext", required: false } }, ...overrides,
});
const catalog = (overrides: Partial<WorkflowCatalog> = {}): WorkflowCatalog => ({ repository: "/repo", items: [entry()], diagnostics: [], truncated: false, ...overrides });
function deferred<T>() {
  let resolve!: (value: T) => void, reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const mockStart = (implementation: Start = async () => "workflow-run") => vi.fn<Start>(implementation);
const bridge = (list: List = vi.fn(async () => catalog()), start: Start = mockStart()) => ({ listWorkflows: list, startWorkflow: start } as unknown as DesktopTransport);
const select = async (id = "review") => fireEvent.click(await screen.findByRole("button", { name: new RegExp(`^${id} `) }));
const goal = () => screen.getByRole<HTMLTextAreaElement>("textbox", { name: /^goal/ });
const fill = (text = '"Keep the public API"') => fireEvent.change(goal(), { target: { value: text } });

describe("desktop workflow library", () => {
  it("shows effective definition metadata and filters loaded items without launching on selection", async () => {
    const list = vi.fn(async () => catalog({ items: [entry(), entry("publish", { source: { type: "Repository" }, version: 4 })] }));
    const start = mockStart(async () => "run");
    render(<WorkflowLauncher transport={bridge(list, start)} repository="/repo" onStarted={vi.fn()} />);
    await select();
    expect(list).toHaveBeenCalledExactlyOnceWith("/repo");
    expect(screen.getByText("Built-in definition · Version 2 · 3 steps")).toBeTruthy();
    expect(goal().getAttribute("aria-required")).toBe("true");
    expect(screen.getByText("Required · UserGoal")).toBeTruthy();
    fireEvent.change(screen.getByRole("textbox", { name: "Filter Loaded Workflows" }), { target: { value: "Repository" } });
    expect(within(screen.getByRole("list", { name: "Available Workflows" })).getAllByRole("button")).toHaveLength(1);
    expect(start).not.toHaveBeenCalled();
  });

  it("requires repository selection and distinguishes unavailable discovery from an empty catalog", async () => {
    const list = vi.fn(async () => catalog());
    const chooseRepository = vi.fn();
    const view = render(<WorkflowLauncher transport={bridge(list)} onStarted={vi.fn()} onOpenRepository={chooseRepository} />);
    fireEvent.click(screen.getByRole("button", { name: "Choose Repository" }));
    expect(chooseRepository).toHaveBeenCalledOnce();
    expect(list).not.toHaveBeenCalled();
    view.rerender(<WorkflowLauncher transport={{} as DesktopTransport} repository="/repo" onStarted={vi.fn()} />);
    expect(screen.getByText("Workflow Library Unavailable")).toBeTruthy();
    expect(screen.queryByText("No Workflow Definitions")).toBeNull();
  });

  it("renders a failed read with retry, and keeps partial/invalid sibling diagnostics separate", async () => {
    const list = vi.fn().mockRejectedValueOnce(new Error("Catalog offline")).mockResolvedValue(catalog({
      items: [], truncated: true, diagnostics: [{ code: "invalid-definition", message: "Broken sibling definition", workflow_id: "broken" }],
    }));
    render(<WorkflowLauncher transport={bridge(list)} repository="/repo" onStarted={vi.fn()} />);
    expect((await screen.findByRole("alert")).textContent).toContain("Catalog offline");
    expect(screen.queryByText("No Workflow Definitions")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Try Again" }));
    await screen.findByText("No Workflow Definitions");
    expect(screen.getByText(/This library is partial/)).toBeTruthy();
    expect(screen.getByText(/Some Workflow Definitions Could Not Be Loaded/)).toBeTruthy();
    expect(screen.getByText("Broken sibling definition")).toBeTruthy();
  });

  it("requires canonical repository confirmation before starting", async () => {
    const start = mockStart(async () => "run");
    render(<WorkflowLauncher transport={bridge(vi.fn(async () => catalog({ repository: "/different" })), start)} repository="/repo" onStarted={vi.fn()} />);
    await select(); fill();
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Start Workflow" }).disabled).toBe(true);
    fireEvent.submit(screen.getByRole("form", { name: "Start review" }));
    expect(start).not.toHaveBeenCalled();
  });

  it("focuses invalid required/JSON input and preserves per-definition drafts across selection and refresh", async () => {
    const list = vi.fn(async () => catalog({ items: [entry(), entry("other")] }));
    const start = mockStart(async () => "run");
    render(<WorkflowLauncher transport={bridge(list, start)} repository="/repo" onStarted={vi.fn()} />);
    await select();
    fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    expect(document.activeElement).toBe(goal());
    expect(goal().getAttribute("aria-invalid")).toBe("true");
    fill("unquoted text");
    fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    expect(screen.getByText(/goal must be valid JSON/)).toBeTruthy();
    fill('"original feedback"');
    await select("other"); fill('"other feedback"'); await select();
    expect(goal().value).toBe('"original feedback"');
    fireEvent.click(screen.getByRole("button", { name: "Refresh Library" }));
    await screen.findByRole("button", { name: "Start Workflow" });
    expect(goal().value).toBe('"original feedback"');
    expect(start).not.toHaveBeenCalled();
  });

  it("submits explicit null, omits blank optionals and guards duplicate clicks until receipt", async () => {
    const response = deferred<string>();
    const start = mockStart(() => response.promise), started = vi.fn();
    render(<WorkflowLauncher transport={bridge(undefined, start)} repository="/repo" onStarted={started} />);
    await select(); fill("null");
    const form = screen.getByRole("form", { name: "Start review" });
    fireEvent.submit(form); fireEvent.submit(form);
    expect(start).toHaveBeenCalledTimes(1);
    expect(start.mock.calls[0]).toEqual(["review", { goal: null }, { expectedRepository: "/repo", expectedDefinitionDigest: "digest-one", idempotencyKey: expect.any(String) }]);
    expect(started).not.toHaveBeenCalled();
    await act(async () => response.resolve("accepted-run"));
    expect(started).toHaveBeenCalledExactlyOnceWith("accepted-run");
  });

  it("retries an unacknowledged intent with the same key after JSON reformatting, without automatic replay", async () => {
    const start = vi.fn<Start>().mockRejectedValueOnce(new Error("Lost receipt")).mockResolvedValue("accepted-run");
    render(<WorkflowLauncher transport={bridge(undefined, start)} repository="/repo" onStarted={vi.fn()} />);
    await select(); fill('{"a":1,"b":2}');
    fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    await screen.findByRole("button", { name: "Retry Start" });
    expect(start).toHaveBeenCalledTimes(1);
    expect(document.activeElement).toBe(screen.getByRole("alert"));
    fill('{ "b": 2, "a": 1 }');
    fireEvent.click(screen.getByRole("button", { name: "Retry Start" }));
    await screen.findByText("Workflow started: accepted-run");
    expect(start.mock.calls[1][2]?.idempotencyKey).toBe(start.mock.calls[0][2]?.idempotencyKey);
    fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    await waitFor(() => expect(start).toHaveBeenCalledTimes(3));
    expect(start.mock.calls[2][2]?.idempotencyKey).not.toBe(start.mock.calls[1][2]?.idempotencyKey);
  });

  it("discards a late catalog from a previous repository", async () => {
    const old = deferred<WorkflowCatalog>();
    const list = vi.fn().mockReturnValueOnce(old.promise).mockResolvedValue(catalog({ repository: "/new", items: [entry("current")] }));
    const transport = bridge(list);
    const view = render(<WorkflowLauncher transport={transport} repository="/old" onStarted={vi.fn()} />);
    view.rerender(<WorkflowLauncher transport={transport} repository="/new" onStarted={vi.fn()} />);
    await select("current");
    await act(async () => old.resolve(catalog({ repository: "/old", items: [entry("stale")] })));
    expect(screen.queryByRole("button", { name: /^stale / })).toBeNull();
    expect(screen.getByRole("form", { name: "Start current" })).toBeTruthy();
  });

  it("retains an unresolved request key across reconnect while discarding stale start results", async () => {
    const first = deferred<string>(), second = deferred<string>();
    const start = vi.fn<Start>().mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
    const transport = bridge(undefined, start), started = vi.fn();
    const view = render(<WorkflowLauncher transport={transport} repository="/repo" connectionEpoch={1} onStarted={started} />);
    await select(); fill(); fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    view.rerender(<WorkflowLauncher transport={transport} repository="/repo" connectionEpoch={2} onStarted={started} />);
    await select(); fill();
    expect(start).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Retry Start" }));
    expect(start.mock.calls[1][2]?.idempotencyKey).toBe(start.mock.calls[0][2]?.idempotencyKey);
    await act(async () => first.resolve("stale-run"));
    expect(started).not.toHaveBeenCalled();
    await act(async () => second.resolve("recovered-run"));
    expect(started).toHaveBeenCalledExactlyOnceWith("recovered-run");
  });

  it("does not reuse an unresolved request identity with a different transport owner", async () => {
    const firstStart = vi.fn<Start>().mockRejectedValue(new Error("Lost receipt"));
    const secondStart = mockStart(async () => "new-owner-run");
    const view = render(<WorkflowLauncher transport={bridge(undefined, firstStart)} repository="/repo" onStarted={vi.fn()} />);
    await select(); fill(); fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    await screen.findByRole("button", { name: "Retry Start" });
    view.rerender(<WorkflowLauncher transport={bridge(undefined, secondStart)} repository="/repo" onStarted={vi.fn()} />);
    await select(); fill(); fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    await waitFor(() => expect(secondStart).toHaveBeenCalledOnce());
    expect(secondStart.mock.calls[0][2]?.idempotencyKey).not.toBe(firstStart.mock.calls[0][2]?.idempotencyKey);
  });

  it("pins the selected definition digest and requires a new explicit start after a refreshed definition", async () => {
    const list = vi.fn().mockResolvedValueOnce(catalog()).mockResolvedValue(catalog({ items: [entry("review", { definition_digest: "digest-two" })] }));
    const start = vi.fn<Start>().mockRejectedValueOnce(new Error("Lost receipt")).mockResolvedValue("new-definition-run");
    render(<WorkflowLauncher transport={bridge(list, start)} repository="/repo" onStarted={vi.fn()} />);
    await select(); fill(); fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    await screen.findByRole("button", { name: "Retry Start" });
    fireEvent.click(screen.getByRole("button", { name: "Refresh Library" }));
    await screen.findByRole("button", { name: "Start Workflow" });
    expect(goal().value).toBe('"Keep the public API"');
    expect(start).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    await waitFor(() => expect(start).toHaveBeenCalledTimes(2));
    expect(start.mock.calls[1][2]?.expectedDefinitionDigest).toBe("digest-two");
    expect(start.mock.calls[1][2]?.idempotencyKey).not.toBe(start.mock.calls[0][2]?.idempotencyKey);
  });

  it("ignores a start receipt after the launcher unmounts", async () => {
    const receipt = deferred<string>(), started = vi.fn();
    const view = render(<WorkflowLauncher transport={bridge(undefined, vi.fn(() => receipt.promise))} repository="/repo" onStarted={started} />);
    await select(); fill(); fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    view.unmount();
    await act(async () => receipt.resolve("late-run"));
    expect(started).not.toHaveBeenCalled();
  });

  it("keeps disconnected inputs read-only and retries a lost receipt with the same identity after reconnect", async () => {
    const receipt = deferred<string>();
    const start = vi.fn<Start>().mockReturnValueOnce(receipt.promise).mockResolvedValue("recovered-run");
    const list = vi.fn(async () => catalog()), transport = bridge(list, start), started = vi.fn();
    const view = render(<WorkflowLauncher transport={transport} repository="/repo" onStarted={started} />);
    await select(); fill(); fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    view.rerender(<WorkflowLauncher transport={transport} repository="/repo" unavailable="Connection closed" onStarted={started} />);
    expect(goal().value).toBe('"Keep the public API"');
    expect(goal().disabled).toBe(true);
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Retry Start" }).disabled).toBe(true);
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Refresh Library" }).disabled).toBe(true);
    expect(screen.getByText(/Workflows are unavailable: Connection closed/)).toBeTruthy();
    await act(async () => receipt.reject(new Error("Connection closed before acknowledgement")));
    expect(started).not.toHaveBeenCalled();
    view.rerender(<WorkflowLauncher transport={transport} repository="/repo" onStarted={started} />);
    await screen.findByRole("button", { name: "Retry Start" });
    expect(goal().disabled).toBe(false);
    expect(goal().value).toBe('"Keep the public API"');
    expect(start).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Retry Start" }));
    await waitFor(() => expect(started).toHaveBeenCalledExactlyOnceWith("recovered-run"));
    expect(start.mock.calls[1][2]?.idempotencyKey).toBe(start.mock.calls[0][2]?.idempotencyKey);
  });

  it("does not discover or start workflows while the runtime is unavailable", () => {
    const list = vi.fn(async () => catalog()), start = mockStart();
    render(<WorkflowLauncher transport={bridge(list, start)} repository="/repo" unavailable="Reconnecting" onStarted={vi.fn()} />);
    expect(screen.getByText("Reconnect to Browse Workflows")).toBeTruthy();
    expect(list).not.toHaveBeenCalled();
    expect(start).not.toHaveBeenCalled();
  });

  it("focuses an oversized multi-byte input and does not bypass the native payload budget", async () => {
    const start = mockStart();
    render(<WorkflowLauncher transport={bridge(undefined, start)} repository="/repo" onStarted={vi.fn()} />);
    await select(); fill(JSON.stringify("界".repeat(22_000)));
    fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    expect(goal().getAttribute("aria-invalid")).toBe("true");
    expect(document.activeElement).toBe(goal());
    expect(screen.getByText(/goal exceeds the 65,536-byte JSON input limit/)).toBeTruthy();
    expect(start).not.toHaveBeenCalled();
  });

  it("restores App-owned drafts and an unresolved launch identity after navigating away", async () => {
    const first = deferred<string>(), retry = deferred<string>();
    const start = vi.fn<Start>().mockReturnValueOnce(first.promise).mockReturnValueOnce(retry.promise);
    const memory = createWorkflowLaunchMemory(), transport = bridge(undefined, start), started = vi.fn();
    const original = render(<WorkflowLauncher transport={transport} repository="/repo" launchMemory={memory} onStarted={started} />);
    await select(); fill('"Keep feedback across navigation"');
    fireEvent.change(screen.getByRole("textbox", { name: "Filter Loaded Workflows" }), { target: { value: "review" } });
    fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    original.unmount();
    render(<WorkflowLauncher transport={transport} repository="/repo" launchMemory={memory} connectionEpoch={2} onStarted={started} />);
    await screen.findByRole("button", { name: "Retry Start" });
    expect(goal().value).toBe('"Keep feedback across navigation"');
    expect((screen.getByRole("textbox", { name: "Filter Loaded Workflows" }) as HTMLInputElement).value).toBe("review");
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Retry Start" }).disabled).toBe(false);
    expect(start).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Retry Start" }));
    expect(start.mock.calls[1][2]?.idempotencyKey).toBe(start.mock.calls[0][2]?.idempotencyKey);
    await act(async () => first.resolve("owned-run"));
    expect(started).not.toHaveBeenCalled();
    await act(async () => retry.resolve("owned-run"));
    expect(started).toHaveBeenCalledExactlyOnceWith("owned-run");
    expect(memory.runs.get("/repo")).toBe("owned-run");
  });

  it("remembers an owned receipt after unmount without restoring stale errors or invoking navigation", async () => {
    const receipt = deferred<string>(), started = vi.fn(), memory = createWorkflowLaunchMemory();
    const transport = bridge(undefined, mockStart(() => receipt.promise));
    const view = render(<WorkflowLauncher transport={transport} repository="/repo" launchMemory={memory} onStarted={started} />);
    await select(); fill(); fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    view.unmount();
    await act(async () => receipt.resolve("owned-background-run"));
    expect(started).not.toHaveBeenCalled();
    expect(memory.runs.get("/repo")).toBe("owned-background-run");
    expect(memory.keys.size).toBe(1);
  });

  it("keeps Retry idempotent when a background receipt arrives after remount but before retry", async () => {
    const receipt = deferred<string>();
    const start = vi.fn<Start>().mockReturnValueOnce(receipt.promise).mockResolvedValue("owned-run");
    const memory = createWorkflowLaunchMemory(), transport = bridge(undefined, start), started = vi.fn();
    const original = render(<WorkflowLauncher transport={transport} repository="/repo" launchMemory={memory} onStarted={started} />);
    await select(); fill(); fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    original.unmount();
    render(<WorkflowLauncher transport={transport} repository="/repo" launchMemory={memory} onStarted={started} />);
    await screen.findByRole("button", { name: "Retry Start" });
    await act(async () => receipt.resolve("owned-run"));
    expect(started).not.toHaveBeenCalled();
    expect(memory.runs.get("/repo")).toBe("owned-run");
    fireEvent.click(screen.getByRole("button", { name: "Retry Start" }));
    await waitFor(() => expect(started).toHaveBeenCalledExactlyOnceWith("owned-run"));
    expect(start.mock.calls[1]).toEqual(start.mock.calls[0]);
    expect(memory.keys.size).toBe(0);
  });

  it("clears all App-owned workflow memory when another transport takes ownership", async () => {
    const memory = createWorkflowLaunchMemory();
    const oldReceipt = deferred<string>(), firstStart = mockStart(() => oldReceipt.promise);
    const original = render(<WorkflowLauncher transport={bridge(undefined, firstStart)} repository="/repo" launchMemory={memory} onStarted={vi.fn()} />);
    await select(); fill(); fireEvent.click(screen.getByRole("button", { name: "Start Workflow" }));
    expect(firstStart).toHaveBeenCalledOnce();
    memory.manual.set("/repo", { workflowId: "old-owner", inputs: "{}" }); memory.runs.set("/repo", "old-owner-run");
    original.unmount();
    render(<WorkflowLauncher transport={bridge()} repository="/repo" launchMemory={memory} onStarted={vi.fn()} />);
    await screen.findByRole("button", { name: /^review / });
    expect(screen.queryByRole("form", { name: "Start review" })).toBeNull();
    expect([memory.keys.size, memory.editors.size, memory.manual.size, memory.runs.size]).toEqual([0, 0, 0, 0]);
    await act(async () => oldReceipt.resolve("old-owner-receipt"));
    expect(memory.runs.size).toBe(0);
  });
});
