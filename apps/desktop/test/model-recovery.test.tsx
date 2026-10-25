import { StrictMode } from "react";
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import type { EventBody } from "@codypendent/protocol";
import type { ModelsView } from "../src/components/localConfig.js";
import { publishFrame, resetFrameBus } from "../src/frameBus.js";
import { ModelRecovery, useModelRecovery, type RecoverySnapshot } from "../src/useModelRecovery.js";

afterEach(resetFrameBus);
const chronicle = { id: "a", sha256: "a".repeat(64), byte_length: 1, media_type: "application/json", sensitivity: { type: "Public" as const } };
function setup() {
  const snapshot: RecoverySnapshot = { connected: true, epoch: 1, sessionId: "chat", attached: true, attaching: false,
    running: false, draftRevision: 0, modelRevision: 0, model: "broken", repository: "/repo", mode: "Build", view: "sessions" };
  const models: ModelsView = { models: ["broken", "acp/codex", "ollama/coder"].map((id) => ({ id, provider: "acp", base_url: "", model: id, provider_id: null, context_tokens: null, key: { state: "missing" } })), pinned: "broken", configured: true, models_path: "", warnings: [] };
  const options = {
    snapshot: () => snapshot,
    submit: vi.fn(async (_objective: string) => true),
    models: vi.fn(async () => models),
    discover: vi.fn(async () => ({ added: [], default_model: null, detected: [], warnings: [], available_default_profiles: ["broken", "acp/codex", "ollama/coder"] })),
    stage: vi.fn(async (model: string, current: () => boolean) => { if (current()) { snapshot.model = model; models.pinned = model; } }),
    status: vi.fn(), recovered: vi.fn(),
  };
  const recovery = new ModelRecovery(options);
  let sequence = 0;
  const frame = (body: EventBody, session_id = "chat") => ({ kind: "event" as const, session_id, event: { sequence: ++sequence, body, actor: { type: "System" as const }, occurred_at: "" } });
  const event = (body: EventBody, session = "chat") => recovery.receive(frame(body, session));
  const started = (run = "r1", session = "chat") => event({ type: "RunStarted", run_id: run, objective: "Do the task", mode: { type: "Build" } }, session);
  const failed = (run = "r1", reason = "ACP agent startup timed out") => event({ type: "RunCompleted", run_id: run, disposition: { type: "Failed", reason }, chronicle });
  return { recovery, options, snapshot, models, frame, event, started, failed };
}

it("retries the same accepted objective with at most two untried available defaults", async () => {
  const s = setup();
  await s.recovery.submit("Do the task");
  s.started(); s.failed();
  await waitFor(() => expect(s.options.submit).toHaveBeenCalledTimes(2));
  expect(s.options.stage.mock.calls[0][0]).toBe("acp/codex");
  s.started("r2"); s.failed("r2");
  await waitFor(() => expect(s.options.submit).toHaveBeenCalledTimes(3));
  expect(s.options.stage.mock.calls[1][0]).toBe("ollama/coder");
  s.started("r3"); s.failed("r3");
  await Promise.resolve();
  expect(s.options.submit.mock.calls.map(([objective]) => objective)).toEqual(["Do the task", "Do the task", "Do the task"]);
  expect(s.options.stage).toHaveBeenCalledTimes(2);
});

it("waits for native acceptance when a failure arrives before the submit reply", async () => {
  const s = setup();
  let accept!: (value: boolean) => void;
  s.options.submit.mockImplementationOnce(() => new Promise<boolean>((resolve) => { accept = resolve; }));
  const sent = s.recovery.submit("Do the task");
  s.started(); s.failed();
  expect(s.options.models).not.toHaveBeenCalled();
  accept(true); await sent;
  await waitFor(() => expect(s.options.submit).toHaveBeenCalledTimes(2));
});

it.each(["ToolStarted", "ToolProposed", "PatchProposed", "QuestionAsked", "ApprovalRequested"])("never retries after %s", async (type) => {
  const s = setup();
  await s.recovery.submit("Do the task"); s.started();
  s.event({ type, run_id: "r1", tool: "shell.run", args_digest: "digest" } as unknown as EventBody);
  s.failed(); await Promise.resolve();
  expect(s.options.discover).not.toHaveBeenCalled();
  expect(s.options.submit).toHaveBeenCalledTimes(1);
});

it.each(["Paused", "Cancelled"] as const)("stops recovery after %s", async (type) => {
  const s = setup();
  await s.recovery.submit("Do the task"); s.started();
  s.event({ type: "RunStateChanged", run_id: "r1", state: { type } }); s.failed();
  expect(s.options.discover).not.toHaveBeenCalled();
});

it("does not treat ordinary task failure or historical events as a model failure", async () => {
  const s = setup();
  s.started("old"); s.failed("old");
  await s.recovery.submit("Do the task");
  s.recovery.receive({ kind: "history", session_id: "chat", through: 10, events: [s.frame({ type: "RunStarted", run_id: "history", objective: "Do the task", mode: { type: "Build" } }).event] });
  s.failed("history");
  s.started(); s.failed("r1", "The task's expected output did not match");
  await Promise.resolve();
  expect(s.options.submit).toHaveBeenCalledTimes(1);
  expect(s.options.discover).not.toHaveBeenCalled();
});

it("tries an available route after provider billing refusal", async () => {
  const s = setup();
  await s.recovery.submit("Do the task"); s.started();
  s.failed("r1", "model driver error: API error 402 Payment Required: provider billing required");
  await waitFor(() => expect(s.options.submit).toHaveBeenCalledTimes(2));
  expect(s.options.stage.mock.calls[0][0]).toBe("acp/codex");
});

it.each(["model request denied by policy: token budget exceeded", "model ContextBudgetExceeded", "provider denied by classification ceiling", "ACP approval rejected"])("does not bypass a user limit: %s", async (reason) => {
  const s = setup();
  await s.recovery.submit("Do the task"); s.started(); s.failed("r1", reason);
  await Promise.resolve();
  expect(s.options.discover).not.toHaveBeenCalled();
  expect(s.options.submit).toHaveBeenCalledTimes(1);
});

it.each(["draftRevision", "modelRevision", "epoch"] as const)("discards discovery after %s changes", async (field) => {
  const s = setup();
  let discover!: (report: Awaited<ReturnType<typeof s.options.discover>>) => void;
  s.options.discover.mockImplementationOnce(() => new Promise((resolve) => { discover = resolve; }));
  await s.recovery.submit("Do the task"); s.started(); s.failed();
  await waitFor(() => expect(s.options.discover).toHaveBeenCalledOnce());
  s.snapshot[field]++;
  discover({ added: [], default_model: null, detected: [], warnings: [], available_default_profiles: ["acp/codex"] });
  await Promise.resolve(); await Promise.resolve();
  expect(s.options.stage).not.toHaveBeenCalled();
  expect(s.options.submit).toHaveBeenCalledTimes(1);
});

it("does not submit a staged fallback after the user changes chats", async () => {
  const s = setup();
  let finish!: () => void;
  s.options.stage.mockImplementationOnce(() => new Promise<void>((resolve) => { finish = resolve; }));
  await s.recovery.submit("Do the task"); s.started(); s.failed();
  await waitFor(() => expect(s.options.stage).toHaveBeenCalledOnce());
  s.snapshot.sessionId = "other"; finish();
  await Promise.resolve(); await Promise.resolve();
  expect(s.options.submit).toHaveBeenCalledTimes(1);
});

it("leaves the failure visible when discovery finds no confirmed alternative", async () => {
  const s = setup(); s.options.discover.mockResolvedValueOnce({ added: [], default_model: null, detected: [], warnings: [], available_default_profiles: [] });
  await s.recovery.submit("Do the task"); s.started(); s.failed();
  await waitFor(() => expect(s.options.status).toHaveBeenLastCalledWith(null));
  expect(s.options.stage).not.toHaveBeenCalled();
  expect(s.options.submit).toHaveBeenCalledTimes(1);
});

it("folds earlier failure evidence only after the replacement run succeeds", async () => {
  const s = setup();
  await s.recovery.submit("Do the task"); s.started(); s.failed();
  await waitFor(() => expect(s.options.submit).toHaveBeenCalledTimes(2));
  expect(s.options.recovered).not.toHaveBeenCalled();
  s.started("r2"); s.event({ type: "RunStateChanged", run_id: "r2", state: { type: "Completed" } });
  expect(s.options.recovered).toHaveBeenCalledExactlyOnceWith(["r1"]);
  expect(s.options.status).toHaveBeenLastCalledWith("Connected using Codex · Default.");
});

it("keeps one live listener through StrictMode and cancels an in-flight recovery on unmount", async () => {
  const s = setup();
  let discover!: (report: Awaited<ReturnType<typeof s.options.discover>>) => void;
  s.options.discover.mockImplementationOnce(() => new Promise((resolve) => { discover = resolve; }));
  const hook = renderHook(() => useModelRecovery(s.options), { wrapper: StrictMode });
  await act(async () => { await hook.result.current.submit("Do the task"); });
  act(() => {
    publishFrame(s.frame({ type: "RunStarted", run_id: "live", objective: "Do the task", mode: { type: "Build" } }));
    publishFrame(s.frame({ type: "RunCompleted", run_id: "live", disposition: { type: "Failed", reason: "ACP handshake timed out" }, chronicle }));
  });
  await waitFor(() => expect(s.options.discover).toHaveBeenCalledOnce());
  hook.unmount();
  discover({ added: [], default_model: null, detected: [], warnings: [], available_default_profiles: ["acp/codex"] });
  await Promise.resolve(); await Promise.resolve();
  expect(s.options.stage).not.toHaveBeenCalled();
  expect(s.options.submit).toHaveBeenCalledTimes(1);
});
