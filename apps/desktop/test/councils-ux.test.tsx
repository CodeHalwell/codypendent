import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { CouncilBrowser } from "../src/components/CouncilBrowser.js";
import { CouncilBuilder } from "../src/components/CouncilBuilder.js";
import { CouncilResults } from "../src/components/CouncilResults.js";
import type { CouncilCard, CouncilProgressFrame, CouncilResultCard, CouncilRunReply } from "../src/localConfig.js";

const models = ["acp/codex-acp", "ollama/deepseek-v4.1-flash:cloud", "acp/claude-acp#opus[1m]"];
const council: CouncilCard = { name: "code-review", description: "A careful review before shipping", chair: models[2], rounds: 2,
  evidence: false, quorum: null, requiredQuorum: 2, chairIsMember: false,
  members: [{ model: models[0], role: "architecture-reviewer" }, { model: models[1], role: "security-reviewer" }] };
const result: CouncilResultCard = { resultId: "private-result-id", council: council.name, status: "completed", objective: "Review the proposed design",
  startedAt: "2026-10-19T09:00:00Z", finishedAt: "2026-10-19T09:05:00Z", repository: "/private/work/project", evidence: false,
  warnings: [], synthesis: "## Recommendation\n\nKeep the **small change**.\n\n```json\n{\"requested\":true}\n```", participants: ["raw-private-participant-run-id"], costLine: "",
  reportMarkdown: "/private/reports/report.md", rounds: [{ round: 1, failures: [], members: [{ model: models[0], role: "reviewer", runId: "private-member-run", sessionId: "private-chat", response: "A **useful** perspective." }] }] };
function defaultText(container: HTMLElement) {
  const copy = container.cloneNode(true) as HTMLElement;
  for (const details of copy.querySelectorAll("details:not([open])")) details.replaceWith(details.querySelector("summary")?.textContent ?? "");
  return copy.textContent ?? "";
}

describe("council teams", () => {
  it("shows readable names, grouped participants and a distinct chair while keeping canonical actions", async () => {
    const run = vi.fn();
    const view = render(<CouncilBrowser onLoad={async () => [council]} onRun={run} />);
    await screen.findByRole("heading", { name: "Code Review" });
    expect(within(screen.getByLabelText("Council members")).getByText("Codex · Default")).toBeTruthy();
    expect(within(screen.getByLabelText("Council chair")).getByText("Claude · Opus · 1M context")).toBeTruthy();
    expect(defaultText(view.container)).not.toContain("acp/claude-acp");
    expect(defaultText(view.container)).not.toContain("code-review");
    expect(screen.getByText("Council settings").closest("details")?.open).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Start council" }));
    expect(run).toHaveBeenCalledExactlyOnceWith("code-review");
  });

  it("searches friendly council and model labels", async () => {
    render(<CouncilBrowser onLoad={async () => [council, { ...council, name: "writing-team", description: "Writing", chair: "other", members: [] }]} />);
    await screen.findByRole("heading", { name: "Code Review" });
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "DeepSeek" } });
    expect(screen.getByRole("heading", { name: "Code Review" })).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "Writing Team" })).toBeNull();
  });

  it("confirms deletion with focus on Cancel and retains canonical IDs", async () => {
    const remove = vi.fn().mockResolvedValue(undefined);
    render(<CouncilBrowser onLoad={async () => [council]} onDelete={remove} />);
    await screen.findByRole("heading", { name: "Code Review" });
    fireEvent.click(screen.getByText("Council settings"));
    fireEvent.click(screen.getByRole("button", { name: "Delete council" }));
    const dialog = screen.getByRole("alertdialog");
    expect(dialog.textContent).toContain("Previous council results are kept");
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Delete" }));
    await act(async () => {});
    expect(remove).toHaveBeenCalledExactlyOnceWith("code-review");
  });

  it("distinguishes a failed read from an empty list and allows retry", async () => {
    const load = vi.fn().mockRejectedValueOnce(new Error("The saved teams could not be read")).mockResolvedValue([]);
    render(<CouncilBrowser onLoad={load} />);
    await screen.findByText("Could not load councils");
    expect(screen.queryByTestId("council-empty")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByTestId("council-empty");
  });
});

describe("creating a council", () => {
  it("accepts a natural name, keeps model IDs exact, and saves optional roles and rounds", async () => {
    const create = vi.fn().mockImplementation(async (draft) => ({ ...council, ...draft }));
    const view = render(<CouncilBuilder configuredModels={models} onCreate={create} />);
    const advanced = screen.getByText("Roles and discussion settings").closest("details")!;
    expect(advanced.open).toBe(false);
    fireEvent.change(screen.getByTestId("council-name"), { target: { value: "Architecture review" } });
    fireEvent.change(screen.getByTestId("council-member-model-0"), { target: { value: models[0] } });
    fireEvent.change(screen.getByTestId("council-member-model-1"), { target: { value: models[1] } });
    fireEvent.change(screen.getByTestId("council-chair"), { target: { value: models[2] } });
    expect(defaultText(view.container)).not.toContain("architecture-review");
    fireEvent.click(screen.getByText("Roles and discussion settings"));
    fireEvent.change(screen.getByTestId("council-member-role-0"), { target: { value: "Architect" } });
    fireEvent.change(screen.getByTestId("council-rounds"), { target: { value: "3" } });
    fireEvent.click(screen.getByRole("button", { name: "Create council" }));
    await screen.findByTestId("council-builder-created");
    expect(create).toHaveBeenCalledExactlyOnceWith({ name: "architecture-review", description: "", chair: models[2], rounds: 3,
      members: [{ model: models[0], role: "Architect" }, { model: models[1], role: "member" }] });
  });

  it("prevents duplicate models and exposes the actual refusal without losing the form", async () => {
    const create = vi.fn().mockRejectedValue(new Error("This council name is already in use"));
    render(<CouncilBuilder onCreate={create} />);
    fireEvent.change(screen.getByTestId("council-name"), { target: { value: "review" } });
    for (const index of [0, 1]) fireEvent.change(screen.getByTestId(`council-member-model-${index}`), { target: { value: models[0] } });
    fireEvent.change(screen.getByTestId("council-chair"), { target: { value: models[2] } });
    expect((screen.getByRole("button", { name: "Create council" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByTestId("council-builder-blocked").textContent).toContain("different model");
    fireEvent.change(screen.getByTestId("council-member-model-1"), { target: { value: models[1] } });
    fireEvent.click(screen.getByRole("button", { name: "Create council" }));
    await screen.findByRole("alert");
    expect(screen.getByRole("alert").textContent).toContain("already in use");
    expect((screen.getByTestId("council-name") as HTMLInputElement).value).toBe("review");
  });

  it("offers a route to models when none are configured", () => {
    const open = vi.fn();
    render(<CouncilBuilder configuredModels={[]} onCreate={vi.fn()} onOpenModels={open} />);
    fireEvent.click(screen.getByRole("button", { name: "Find models" }));
    expect(open).toHaveBeenCalledOnce();
    expect((screen.getByRole("button", { name: "Create council" }) as HTMLButtonElement).disabled).toBe(true);
  });
});

describe("council answers", () => {
  it("keeps an actual independent review folded, without inventing one for older reports", async () => {
    const withReview = { ...result, review: { model: models[1], role: "independent-reviewer", sessionId: "private-review-chat", runId: "private-review-run",
      response: "Check the **migration** before shipping.", tokens: 9246 } };
    const view = render(<CouncilResults onLoad={async () => ({ results: [withReview, { ...result, resultId: "older-result" }], warnings: [] })} />);
    const review = await screen.findByTestId(`council-review-${result.resultId}`);
    expect((review as HTMLDetailsElement).open).toBe(false);
    expect(screen.queryByTestId("council-review-older-result")).toBeNull();
    expect(defaultText(view.container)).not.toContain("Check the migration");
    expect(defaultText(view.container)).not.toContain(models[1]);
    expect(defaultText(view.container)).not.toContain("private-review-run");
    fireEvent.click(within(review).getByText("Independent review"));
    expect(review.querySelector("strong")?.textContent).toBe("DeepSeek V4.1 Flash · Cloud");
    expect(review.querySelector(".council-member-body strong")?.textContent).toBe("migration");
    expect(within(review).getByText("9,246 tokens measured")).toBeTruthy();
  });

  it("uses a readable chair warning and keeps the exact recorded warning inside report details", async () => {
    const warning = `chair \`${models[1]}\` is also a council member; its synthesis will weigh its own report`;
    const view = render(<CouncilResults onLoad={async () => ({ results: [{ ...result, warnings: [warning] }], warnings: [] })} />);
    await screen.findByText(/DeepSeek V4.1 Flash · Cloud also contributes a member response/);
    expect(defaultText(view.container)).not.toContain(models[1]);
    expect(defaultText(view.container)).not.toContain("its synthesis will weigh its own report");
    const report = screen.getByText("Report details").closest("details")!;
    expect(report.open).toBe(false);
    expect(within(report).getByText(warning)).toBeTruthy();
  });

  it("folds the completed question form, retains its draft, and restores the normal form for another live run", async () => {
    const completions: Array<(reply: CouncilRunReply) => void> = [];
    const run = vi.fn(() => new Promise<CouncilRunReply>((resolve) => completions.push(resolve)));
    const view = render(<CouncilResults onLoad={async () => ({ results: [], warnings: [] })} onRun={run} initialCouncil={council.name} repository={result.repository} />);
    fireEvent.change(screen.getByLabelText("Question for the council"), { target: { value: "Review the plan" } });
    fireEvent.click(screen.getByTestId("council-run-submit"));
    await act(async () => completions[0]({ result }));
    const disclosure = screen.getByText("Ask another question").closest("details")!;
    expect(disclosure.open).toBe(false);
    expect(defaultText(view.container)).not.toContain("What would you like a second opinion on?");
    expect(screen.getByRole("heading", { name: "The chair’s answer" })).toBeTruthy();
    fireEvent.click(screen.getByText("Ask another question"));
    const question = await screen.findByRole("textbox", { name: "Question for the council" });
    expect((question as HTMLTextAreaElement).value).toBe("Review the plan");
    fireEvent.change(question, { target: { value: "Now review the implementation" } });
    fireEvent.click(screen.getByTestId("council-run-submit"));
    expect(run).toHaveBeenCalledTimes(2);
    expect(screen.queryByText("Ask another question")).toBeNull();
    expect((screen.getByRole("textbox", { name: "Question for the council" }) as HTMLTextAreaElement).disabled).toBe(true);
    expect(screen.getByTestId("council-progress")).toBeTruthy();
    await act(async () => completions[1]({ result: { ...result, resultId: "next-result" } }));
    expect(screen.getByText("Ask another question").closest("details")?.open).toBe(false);
  });

  it("leads with a formatted chair answer and keeps report IDs and individual responses folded", async () => {
    const view = render(<CouncilResults onLoad={async () => ({ results: [result], warnings: [] })} />);
    await screen.findByRole("heading", { name: "Code Review" });
    expect(view.container.querySelector("strong")?.textContent).toBe("small change");
    expect(view.container.querySelector("pre")?.textContent).toBe('{"requested":true}');
    expect(defaultText(view.container)).not.toContain("private-result-id");
    expect(defaultText(view.container)).not.toContain("raw-private-participant-run-id");
    expect(defaultText(view.container)).not.toContain("A useful perspective");
    expect(screen.queryByTestId(`council-cost-${result.resultId}`)).toBeNull();
    expect(screen.getByText("Report details").closest("details")?.open).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Explore member responses" }));
    expect(screen.getByText("Codex · Default")).toBeTruthy();
    expect(defaultText(view.container)).not.toContain("A useful perspective");
    fireEvent.click(screen.getByText("Codex · Default"));
    expect(view.container.querySelector(".council-member-body strong")?.textContent).toBe("useful");
    expect(screen.queryByText(/tokens measured/)).toBeNull();
  });

  it("waits for a real repository selection and provides its action", async () => {
    const open = vi.fn();
    const view = render(<CouncilResults onLoad={async () => ({ results: [], warnings: [] })} onRun={vi.fn()} initialCouncil="code-review" repository={undefined} onOpenRepository={open} />);
    fireEvent.change(screen.getByLabelText("Question for the council"), { target: { value: "Review this" } });
    expect((screen.getByTestId("council-run-submit") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByRole("button", { name: "Choose repository" })).toBeNull();
    view.rerender(<CouncilResults onLoad={async () => ({ results: [], warnings: [] })} onRun={vi.fn()} initialCouncil="code-review" repository={null} onOpenRepository={open} />);
    fireEvent.click(screen.getByRole("button", { name: "Choose repository" }));
    expect(open).toHaveBeenCalledOnce();
    expect((screen.getByTestId("council-run-submit") as HTMLButtonElement).disabled).toBe(true);
    await act(async () => {});
  });

  it("shows measured live progress, prevents duplicate starts and retains partial failures", async () => {
    let finish!: (reply: CouncilRunReply) => void;
    let progress!: (frame: CouncilProgressFrame) => void;
    const run = vi.fn((_name, _objective, callback) => { progress = callback; return new Promise<CouncilRunReply>((resolve) => { finish = resolve; }); });
    render(<CouncilResults onLoad={async () => ({ results: [], warnings: [] })} onRun={run} initialCouncil="code-review" councilNames={["code-review"]} repository="/private/work/project" />);
    fireEvent.change(screen.getByLabelText("Question for the council"), { target: { value: "Review the plan" } });
    fireEvent.click(screen.getByTestId("council-run-submit"));
    fireEvent.click(screen.getByTestId("council-run-submit"));
    expect(run).toHaveBeenCalledOnce();
    expect(screen.getByTestId("council-progress").textContent).not.toContain("0 active");
    act(() => progress({ council: "code-review", resultId: "r", phase: "chair-started", occurredAt: "", message: "raw private activity session-id", activeSubagents: 1 }));
    expect(screen.getByTestId("council-progress").textContent).toContain("1 model is active");
    expect(screen.getByText("Discussion activity").closest("details")?.open).toBe(false);
    await act(async () => finish({ failure: "Not enough members completed", result: { ...result, status: "quorum-failed", failure: "Not enough members completed", synthesis: "" } }));
    expect(screen.getByTestId("council-run-failure").textContent).toContain("Not enough members completed");
    expect(screen.getByTestId(`council-result-status-${result.resultId}`).textContent).toBe("Not enough responses");
    expect(screen.getByTestId(`council-no-synthesis-${result.resultId}`)).toBeTruthy();
    expect(run.mock.calls[0].slice(0, 2)).toEqual(["code-review", "Review the plan"]);
  });
});
