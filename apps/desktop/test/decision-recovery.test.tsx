import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { QuestionCard } from "../src/components/QuestionCard.js";
import { Transcript } from "../src/components/Transcript.js";
import { useCommandActions } from "../src/useCommandActions.js";
import type { QuestionCardProps } from "../src/components/QuestionCard.js";
import type { TranscriptItem } from "../src/types.js";

function pending() {
  let resolve!: () => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const prompts = [{ header: "Tests", question: "Which tests?", options: [{ label: "All" }], multiple: false, custom: true }];
const answer = () => screen.getByRole("button", { name: "Answer" }) as HTMLButtonElement;
const custom = () => screen.getByRole("textbox", { name: /Your own answer/ }) as HTMLInputElement;
const fill = () => fireEvent.change(custom(), { target: { value: "Only integration tests" } });

function Harness({ scope, visible, ...props }: QuestionCardProps & { scope: string; visible: boolean }) {
  const commandActions = useCommandActions(scope);
  return visible ? <QuestionCard {...props} commandActions={commandActions} /> : <p>Another view</p>;
}

describe("decision command recovery", () => {
  it("prevents duplicate or conflicting answers, preserves a refused draft, and waits for durable resolution", async () => {
    const first = pending();
    const resolve = vi.fn().mockReturnValueOnce(first.promise).mockResolvedValue(undefined);
    render(<QuestionCard questionId="q" prompts={prompts} onResolve={resolve} />);
    fill();
    answer().focus();
    act(() => { fireEvent.click(answer()); fireEvent.click(answer()); fireEvent.click(screen.getByRole("button", { name: "Reject" })); });
    expect(resolve).toHaveBeenCalledTimes(1);
    expect(document.activeElement).toBe(screen.getByTestId("question-card"));
    expect(custom().disabled).toBe(true);
    expect(screen.getByRole("status").textContent).toBe("Sending…");
    await act(async () => first.reject(new Error("Answer refused: temporarily unavailable")));
    expect(screen.getByRole("alert").textContent).toContain("Answer refused");
    expect(custom().value).toBe("Only integration tests");
    expect(answer().disabled).toBe(false);
    expect(document.activeElement).toBe(answer());
    await act(async () => fireEvent.click(answer()));
    expect(resolve).toHaveBeenLastCalledWith("q", { type: "Answered", answers: [["Only integration tests"]] });
    expect(screen.getByRole("status").textContent).toContain("Waiting for the daemon to confirm resolution");
    expect(answer().disabled).toBe(true);
    expect(screen.getByTestId("question-card")).toBeTruthy();
  });

  it("retains rejection feedback after refusal, including a synchronous bridge failure", async () => {
    const resolve = vi.fn().mockImplementationOnce(() => { throw new Error("Bridge refused"); }).mockResolvedValue(undefined);
    render(<QuestionCard questionId="q" prompts={prompts} onResolve={resolve} />);
    fireEvent.click(screen.getByRole("button", { name: "Reject" }));
    const feedback = screen.getByLabelText(/Why you are rejecting/) as HTMLInputElement;
    expect(document.activeElement).toBe(feedback);
    fireEvent.change(feedback, { target: { value: "Ask after the build" } });
    fireEvent.click(screen.getByRole("button", { name: "Send rejection" }));
    expect(screen.getByRole("alert").textContent).toContain("Bridge refused");
    expect(feedback.value).toBe("Ask after the build");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Send rejection" })));
    expect(resolve).toHaveBeenLastCalledWith("q", { type: "Rejected", feedback: "Ask after the build" });
    expect((screen.getByRole("button", { name: "Back" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("resets identical reissued prompts and ignores the old request's result", async () => {
    const old = pending();
    const resolve = vi.fn().mockReturnValueOnce(old.promise).mockResolvedValue(undefined);
    const { rerender } = render(<QuestionCard questionId="q" prompts={prompts} revision={1} onResolve={resolve} />);
    fill();
    fireEvent.click(answer());
    rerender(<QuestionCard questionId="q" prompts={prompts} revision={2} onResolve={resolve} />);
    expect(custom().value).toBe("");
    expect(custom().disabled).toBe(false);
    await act(async () => old.reject(new Error("Obsolete refusal")));
    expect(screen.queryByRole("alert")).toBeNull();
    fill();
    await act(async () => fireEvent.click(answer()));
    expect(resolve).toHaveBeenCalledTimes(2);
  });

  it("keeps an accepted decision locked across view changes", async () => {
    const resolve = vi.fn().mockResolvedValue(undefined);
    const props = { scope: "connected:1", questionId: "q", prompts, onResolve: resolve };
    const { rerender } = render(<Harness {...props} visible />);
    fill();
    await act(async () => fireEvent.click(answer()));
    rerender(<Harness {...props} visible={false} />);
    rerender(<Harness {...props} visible />);
    expect(answer().disabled).toBe(true);
    expect(screen.getByRole("status").textContent).toContain("Decision sent");
    fireEvent.click(answer());
    expect(resolve).toHaveBeenCalledTimes(1);
  });

  it("retires connection feedback even when a scope returns to the same name", async () => {
    const old = pending();
    const fresh = pending();
    const resolve = vi.fn().mockReturnValueOnce(old.promise).mockReturnValueOnce(fresh.promise);
    const props = { questionId: "q", prompts, onResolve: resolve, visible: true };
    const { rerender } = render(<Harness {...props} scope="A" />);
    fill();
    fireEvent.click(answer());
    rerender(<Harness {...props} scope="B" />);
    rerender(<Harness {...props} scope="A" />);
    fireEvent.click(answer());
    expect(resolve).toHaveBeenCalledTimes(2);
    await act(async () => old.reject(new Error("Retired connection")));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(answer().disabled).toBe(true);
    await act(async () => fresh.resolve());
    expect(screen.getByRole("status").textContent).toContain("Decision sent");
  });

  it("locks both approval choices while pending and permits retry after refusal", async () => {
    const command = pending();
    const approve = vi.fn().mockReturnValue(command.promise);
    const reject = vi.fn().mockResolvedValue(undefined);
    const item: TranscriptItem = { id: "a", type: "approval", text: "Run tests", timestamp: "", approvalId: "a",
      approvalAction: { type: "ExecuteCommand", program: "cargo", args: ["test"], cwd: null, environment: [] } };
    render(<Transcript items={[item]} connectionStatus="connected" onApprove={approve} onReject={reject} />);
    act(() => { fireEvent.click(screen.getByRole("button", { name: "Approve" })); fireEvent.click(screen.getByRole("button", { name: "Reject" })); });
    expect(reject).not.toHaveBeenCalled();
    await act(async () => command.reject(new Error("Approval refused")));
    expect(screen.getByRole("alert").textContent).toContain("Approval refused");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Reject" })));
    expect(reject).toHaveBeenCalledExactlyOnceWith("a");
    expect(screen.getByText("cargo test")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Approve" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("focuses a deep-linked request as its history arrives without stealing focus on later tokens", () => {
    const item: TranscriptItem = { id: "q", type: "question", text: "Tests", timestamp: "", questionId: "q", questionPrompts: prompts };
    const { rerender } = render(<Transcript items={[]} connectionStatus="connected" focusDecision="Question:q" />);
    rerender(<Transcript items={[item]} connectionStatus="connected" focusDecision="Question:q" onResolveQuestion={vi.fn()} />);
    expect(document.activeElement).toBe(screen.getByTestId("question-card"));
    custom().focus();
    rerender(<Transcript items={[item, { id: "token", type: "assistant", text: "More", timestamp: "" }]} connectionStatus="connected" focusDecision="Question:q" onResolveQuestion={vi.fn()} />);
    expect(document.activeElement).toBe(custom());
  });

  it("does not steal focus back when an operator leaves a pending decision", async () => {
    const command = pending();
    render(<><button>Elsewhere</button><QuestionCard questionId="q" prompts={prompts} onResolve={() => command.promise} /></>);
    fill();
    answer().focus();
    fireEvent.click(answer());
    const elsewhere = screen.getByRole("button", { name: "Elsewhere" });
    elsewhere.focus();
    await act(async () => command.reject(new Error("Refused")));
    expect(document.activeElement).toBe(elsewhere);
  });
});
