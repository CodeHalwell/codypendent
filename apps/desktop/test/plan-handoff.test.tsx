import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "../src/App.js";
import * as config from "../src/components/localConfig.js";
import type { DesktopTransport } from "../src/transport.js";
import type { SessionEvent } from "@codypendent/protocol";
import { ContinuityTransport } from "./fixtures/ContinuityTransport.js";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => undefined) }));

function deferred() {
  let resolve!: () => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

class PlanTransport extends ContinuityTransport {
  queued = vi.fn(async () => undefined);
  currentRepository = vi.fn(async () => ({ path: "/fixture/repo", name: "repo", branch: "main" }));
  constructor() {
    super();
    this.sessions.push({ session_id: "plan-session", title: "Retained plan", state: "open", created_at: "", updated_at: "" },
      { session_id: "other-session", title: "Other work", state: "open", created_at: "", updated_at: "" });
    this.histories.set("plan-session", [
      { type: "RunStarted", run_id: "plan-run", objective: "Plan the API migration", mode: { type: "Plan" } },
      { type: "ModelStreamDelta", run_id: "plan-run", text: "Preserve the API and add regression tests." },
      { type: "RunCompleted", run_id: "plan-run", disposition: { type: "Completed" } },
    ].map((body, index) => ({ sequence: index + 1, actor: { type: "System" }, occurred_at: "", body }) as SessionEvent));
  }
  async queuePrompt() { await this.queued(); }
}

async function openPlan() {
  const transport = new PlanTransport();
  render(<App makeTransport={() => transport as unknown as DesktopTransport} />);
  await screen.findByRole("button", { name: "Session Retained plan" });
  await act(async () => fireEvent.click(screen.getByRole("button", { name: "Session Retained plan" })));
  await screen.findByRole("button", { name: "Implement This Plan" });
  return transport;
}

beforeEach(() => {
  let stagedMode: config.AgentMode = { type: "Plan" };
  vi.spyOn(config, "shellAvailable").mockReturnValue(true);
  vi.spyOn(config.localConfigClient, "runDefaults").mockImplementation(async () => ({ mode: stagedMode, model: "chosen/model" }));
  vi.spyOn(config.localConfigClient, "listModels").mockResolvedValue({ models: [], configured: true, models_path: "", warnings: [], pinned: "chosen/model" });
  vi.spyOn(config.localConfigClient, "discoverModels").mockResolvedValue({ added: [], default_model: "chosen/model", detected: [], warnings: [], available_default_profiles: ["chosen/model"] });
  vi.spyOn(config.localConfigClient, "setRunMode").mockImplementation(async (mode) => { stagedMode = mode; });
  vi.spyOn(config.localConfigClient, "setRunModel").mockResolvedValue(undefined);
});
afterEach(() => vi.restoreAllMocks());

describe("desktop Plan to Build draft handoff", () => {
  it("preserves feedback and model/repository while preparing a draft, without starting or queueing work", async () => {
    const transport = await openPlan();
    const input = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "Keep the public API stable.\nAdd regression tests." } });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Implement This Plan" })));
    expect(input.value).toBe("Implement this plan.\n\nKeep the public API stable.\nAdd regression tests.");
    expect(document.activeElement).toBe(input);
    expect(config.localConfigClient.setRunMode).toHaveBeenCalledExactlyOnceWith({ type: "Build" });
    expect(screen.getByTitle("Choose the mode for your next run").textContent).toContain("Build");
    expect(config.localConfigClient.setRunModel).not.toHaveBeenCalled();
    expect(transport.starts).toEqual([]);
    expect(transport.queued).not.toHaveBeenCalled();
    expect(config.localConfigClient.runDefaults).toHaveBeenCalled();
    expect(transport.currentRepository).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("button", { name: "Implement This Plan" })).toBeNull();
  });

  it("invalidates delayed Plan defaults and refetches the saved Build mode without losing the selected model", async () => {
    let resolveOld!: (defaults: config.RunDefaults) => void;
    const oldDefaults = new Promise<config.RunDefaults>((resolve) => { resolveOld = resolve; });
    let saved = false;
    vi.mocked(config.localConfigClient.runDefaults).mockImplementation(() => saved
      ? Promise.resolve({ mode: { type: "Build" }, model: "chosen/model" }) : oldDefaults);
    vi.mocked(config.localConfigClient.setRunMode).mockImplementation(async () => { saved = true; });
    const transport = await openPlan();
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Keep my feedback" } });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Implement This Plan" })));
    const currentConfiguration = screen.getByLabelText("Run configuration").textContent;
    expect(currentConfiguration).toContain("Build");
    await act(async () => resolveOld({ mode: { type: "Plan" }, model: "obsolete/model" }));
    expect(screen.getByLabelText("Run configuration").textContent).toBe(currentConfiguration);
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("Implement this plan.\n\nKeep my feedback");
    expect(transport.starts).toEqual([]);
    expect(config.localConfigClient.setRunModel).not.toHaveBeenCalled();
  });

  it("keeps the draft and offers a retry if the mode cannot be saved", async () => {
    await openPlan();
    vi.mocked(config.localConfigClient.setRunMode).mockRejectedValueOnce(new Error("Synthetic config failure"));
    const input = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "Unsent feedback" } });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Implement This Plan" })));
    expect(input.value).toBe("Unsent feedback");
    expect(screen.getByRole("alert").textContent).toContain("Your draft was kept");
    expect(screen.getByRole("button", { name: "Implement This Plan" })).toBeTruthy();
  });

  it("keeps the draft and explains a saved mode whose authoritative refresh failed", async () => {
    await openPlan();
    vi.mocked(config.localConfigClient.runDefaults).mockRejectedValueOnce(new Error("Synthetic read failure"));
    const input = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "Unsent feedback" } });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Implement This Plan" })));
    expect(input.value).toBe("Unsent feedback");
    expect(screen.getByRole("alert").textContent).toContain("Build mode was saved");
    expect(screen.getByRole("button", { name: "Implement This Plan" })).toBeTruthy();
  });

  it("discards a delayed preparation after switching sessions", async () => {
    const transport = await openPlan();
    const save = deferred();
    vi.mocked(config.localConfigClient.setRunMode).mockReturnValueOnce(save.promise);
    const input = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "Keep this draft" } });
    fireEvent.click(screen.getByRole("button", { name: "Implement This Plan" }));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Session Other work" })));
    await act(async () => save.resolve());
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("Keep this draft");
    expect(transport.starts).toEqual([]);
    expect(transport.queued).not.toHaveBeenCalled();
  });

  it("hides the action while a new objective is awaiting its launch receipt", async () => {
    const transport = await openPlan();
    const launch = deferred();
    vi.spyOn(transport, "startObjective").mockImplementation(async (objective, sessionId) => {
      transport.starts.push({ objective, sessionId: sessionId ?? null });
      await launch.promise;
      return { session_id: "plan-session", run_id: "new-run" };
    });
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "A different objective" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(transport.starts).toHaveLength(1));
    expect(transport.histories.get("plan-session")).toHaveLength(3);
    expect(screen.queryByRole("button", { name: "Implement This Plan" })).toBeNull();
    expect(config.localConfigClient.setRunMode).not.toHaveBeenCalled();
    await act(async () => launch.resolve());
  });
});
