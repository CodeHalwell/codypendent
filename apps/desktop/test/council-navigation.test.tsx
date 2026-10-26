import { act, fireEvent, render, renderHook, screen, waitFor, within } from "@testing-library/react";
import { StrictMode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "../src/App.js";
import { ContinuityTransport } from "./fixtures/ContinuityTransport.js";
import type { DesktopTransport } from "../src/transport.js";
import type { ModelsView } from "../src/components/localConfig.js";
import type { CouncilCard, CouncilProgressFrame, CouncilResultCard, CouncilRunReply, RepositorySelection } from "../src/localConfig.js";
import { useCouncilRun, type CouncilRunner } from "../src/useCouncilRun.js";

const config = vi.hoisted(() => ({ listModels: vi.fn(), discoverModels: vi.fn(), runDefaults: vi.fn(), listModelReadiness: vi.fn() }));
vi.mock("../src/components/localConfig", async (original) => ({
  ...await original<typeof import("../src/components/localConfig.js")>(),
  shellAvailable: () => true,
  localConfigClient: { listModels: config.listModels, discoverModels: config.discoverModels,
    runDefaults: config.runDefaults, listModelReadiness: config.listModelReadiness },
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn().mockResolvedValue(() => undefined) }));

const models = ["acp/codex-acp", "ollama/deepseek-v4.1-flash:cloud"];
const council: CouncilCard = { name: "code-review", description: "A second opinion", chair: models[0], rounds: 1,
  evidence: false, quorum: null, requiredQuorum: 2, chairIsMember: true,
  members: models.map((model) => ({ model, role: "member" })) };
const repository: RepositorySelection = { path: "/private/work/project", name: "project" };
const result: CouncilResultCard = { resultId: "council-result", council: council.name, status: "completed", objective: "Review this design",
  startedAt: "2026-10-19T10:00:00Z", finishedAt: "2026-10-19T10:01:00Z", repository: repository.path, evidence: false,
  warnings: [], rounds: [], synthesis: "The design is ready for review.", participants: [], costLine: "", reportMarkdown: "/private/report.md" };
function modelView(ids: string[]): ModelsView {
  return { models: ids.map((id) => ({ id, provider: "acp", provider_id: "acp", model: id,
    base_url: "", context_tokens: null, key: { state: "missing" as const } })),
  configured: true, models_path: "/private/models.toml", warnings: [], pinned: null };
}
class CouncilTransport extends ContinuityTransport {
  selection: RepositorySelection | null = repository;
  listCouncils = vi.fn(async () => [council]);
  listCouncilResults = vi.fn(async () => ({ results: [] as CouncilResultCard[], warnings: [] }));
  currentRepository = vi.fn(async () => this.selection);
  pickRepository = vi.fn(async () => { this.selection = repository; return repository; });
  runCouncil = vi.fn<NonNullable<DesktopTransport["runCouncil"]>>();
}
function back() { fireEvent.click(within(screen.getByRole("navigation", { name: "View navigation" })).getByRole("button", { name: "Back" })); }
async function openTeams(transport: CouncilTransport) {
  render(<App makeTransport={() => transport as unknown as DesktopTransport} />);
  await screen.findByLabelText("codypendentd connected");
  fireEvent.click(screen.getByLabelText("Councils group"));
  fireEvent.click(screen.getByLabelText("Councils View"));
  await screen.findByRole("heading", { name: "Code Review" });
}
async function openRun(transport: CouncilTransport) {
  await openTeams(transport);
  fireEvent.click(await screen.findByRole("button", { name: "Start council" }));
  await screen.findByLabelText("Question for the council");
}

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  config.listModels.mockResolvedValue(modelView(models));
  config.discoverModels.mockResolvedValue({ added: [], detected: [], warnings: [], default_model: null });
  config.runDefaults.mockResolvedValue({ mode: { type: "Build" }, model: null });
  config.listModelReadiness.mockResolvedValue([]);
});

describe("council page roundtrips", () => {
  it("returns from model discovery to the same builder draft with newly found model choices", async () => {
    config.listModels.mockResolvedValue(modelView([]));
    const transport = new CouncilTransport();
    await openTeams(transport);
    fireEvent.click(await screen.findByRole("button", { name: "New council" }));
    fireEvent.change(await screen.findByTestId("council-name"), { target: { value: "Architecture review" } });
    fireEvent.change(screen.getByTestId("council-description"), { target: { value: "Keep this purpose" } });
    fireEvent.click(screen.getByText("Roles and discussion settings"));
    fireEvent.change(screen.getByTestId("council-member-role-0"), { target: { value: "Architect" } });
    fireEvent.change(screen.getByTestId("council-rounds"), { target: { value: "2" } });
    fireEvent.click(await screen.findByRole("button", { name: "Find models" }));
    await screen.findByTestId("model-picker");
    config.listModels.mockResolvedValue(modelView(models));
    fireEvent.click(screen.getByRole("button", { name: "Scan again" }));
    await waitFor(() => expect(config.discoverModels).toHaveBeenCalledTimes(2));
    // Escape returns to the builder, rather than closing an off-screen builder flag.
    fireEvent.keyDown(window, { key: "Escape" });
    await screen.findByRole("heading", { name: "New council" });
    const member = screen.getByTestId("council-member-model-0");
    await waitFor(() => expect(member.querySelector(`option[value="${models[1]}"]`)).not.toBeNull());
    expect((screen.getByTestId("council-name") as HTMLInputElement).value).toBe("Architecture review");
    expect((screen.getByTestId("council-description") as HTMLTextAreaElement).value).toBe("Keep this purpose");
    expect((screen.getByTestId("council-member-role-0") as HTMLInputElement).value).toBe("Architect");
    expect((screen.getByTestId("council-rounds") as HTMLInputElement).value).toBe("2");
    expect(screen.queryByRole("button", { name: "Find models" })).toBeNull();
  });

  it("returns from repository selection to the same council question and then back to the team list", async () => {
    const transport = new CouncilTransport();
    transport.selection = null;
    await openRun(transport);
    fireEvent.change(screen.getByLabelText("Question for the council"), { target: { value: "Review this design" } });
    fireEvent.click(await screen.findByRole("button", { name: "Choose repository" }));
    fireEvent.click(await screen.findByTestId("repo-pick"));
    await screen.findByTestId("repo-selected");
    back();
    const question = await screen.findByLabelText("Question for the council");
    expect((question as HTMLTextAreaElement).value).toBe("Review this design");
    expect((screen.getByTestId("council-run-name") as HTMLSelectElement).value).toBe(council.name);
    expect((screen.getByTestId("council-run-submit") as HTMLButtonElement).disabled).toBe(false);
    back();
    await screen.findByRole("heading", { name: "Councils" });
    expect(screen.queryByLabelText("Question for the council")).toBeNull();
  });

  it("retains a native run, live progress and the start guard across navigation and completion off-screen", async () => {
    const transport = new CouncilTransport();
    let finish!: (reply: CouncilRunReply) => void;
    let progress!: (frame: CouncilProgressFrame) => void;
    transport.runCouncil.mockImplementation((_name, _objective, _options, callback) => {
      progress = callback!;
      return new Promise((resolve) => { finish = resolve; });
    });
    await openRun(transport);
    fireEvent.change(screen.getByLabelText("Question for the council"), { target: { value: result.objective } });
    fireEvent.click(screen.getByTestId("council-run-submit"));
    await waitFor(() => expect(transport.runCouncil).toHaveBeenCalledOnce());
    fireEvent.click(screen.getByLabelText("Inbox View"));
    act(() => progress({ council: council.name, resultId: result.resultId, phase: "chair-started", occurredAt: "", message: "The chair is working", activeSubagents: 1 }));
    fireEvent.click(screen.getByLabelText("Council Results View"));
    await screen.findByTestId("council-progress");
    expect(screen.getByTestId("council-progress").textContent).toContain("1 model is active");
    expect((screen.getByLabelText("Question for the council") as HTMLTextAreaElement).value).toBe(result.objective);
    expect((screen.getByTestId("council-run-submit") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.submit(screen.getByRole("form", { name: "Start a council" }));
    expect(transport.runCouncil).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByLabelText("Inbox View"));
    await act(async () => finish({ result }));
    fireEvent.click(screen.getByLabelText("Council Results View"));
    await screen.findByText(result.synthesis);
    expect(screen.queryByTestId("council-progress")).toBeNull();
    expect((screen.getByLabelText("Question for the council") as HTMLTextAreaElement).value).toBe(result.objective);
    expect(transport.runCouncil).toHaveBeenCalledOnce();
    expect(transport.runCouncil.mock.calls[0].slice(0, 3)).toEqual([council.name, result.objective, { repository: repository.path, sessionId: null }]);
  });

  it("guards same-tick starts in StrictMode and ignores callbacks from a finished attempt", async () => {
    const callbacks: Array<(frame: CouncilProgressFrame) => void> = [];
    const completions: Array<(reply: CouncilRunReply) => void> = [];
    const run = vi.fn<CouncilRunner>((_name, _objective, callback) => {
      callbacks.push(callback);
      return new Promise((resolve) => completions.push(resolve));
    });
    const controller = renderHook(() => useCouncilRun(run), { wrapper: StrictMode });
    act(() => {
      controller.result.current.setCouncil(council.name);
      controller.result.current.setObjective(result.objective);
      void controller.result.current.start(repository.path);
      void controller.result.current.start(repository.path);
      controller.result.current.setObjective("Do not retarget an active run");
      controller.result.current.setCouncil("another-team");
    });
    expect(run).toHaveBeenCalledOnce();
    expect(controller.result.current.objective).toBe(result.objective);
    expect(controller.result.current.council).toBe(council.name);
    await act(async () => completions[0]({ result }));
    act(() => {
      controller.result.current.setObjective("A different question");
      void controller.result.current.start(repository.path);
      callbacks[0]({ council: council.name, resultId: result.resultId, phase: "warning", occurredAt: "", message: "Late old event", activeSubagents: 0 });
    });
    expect(run).toHaveBeenCalledTimes(2);
    expect(controller.result.current.progress).toEqual([]);
    await act(async () => completions[1]({ failure: "The provider stopped responding" }));
    expect(controller.result.current.failure).toBe("The provider stopped responding");
    expect(controller.result.current.running).toBe(false);
    expect(controller.result.current.completion).toBe(2);
  });
});
