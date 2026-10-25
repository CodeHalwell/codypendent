import { useState } from "react";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { PendingPromptView } from "@codypendent/protocol";
import { PromptQueue } from "../src/components/PromptQueue.js";
import { usePromptQueueActions } from "../src/usePromptQueueActions.js";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const prompts: PendingPromptView[] = ["first", "second"].map((text, i) => ({
  id: `p${i}`, text, mode: { type: "Build" }, delivery: { type: "Queue" },
}));
const accepted = async () => true;
function Harness({ scope = "a", sessionId = "session", canMutate = true, items = prompts,
  onEdit = accepted, onPromote = accepted, onDelete = accepted,
}: {
  scope?: string; sessionId?: string; canMutate?: boolean; items?: PendingPromptView[];
  onEdit?: (id: string, text: string) => Promise<boolean>;
  onPromote?: (id: string) => Promise<boolean>; onDelete?: (id: string) => Promise<boolean>;
}) {
  const actions = usePromptQueueActions({ scope, sessionId, canMutate, prompts: items, onEdit, onPromote, onDelete });
  const [open, setOpen] = useState(true);
  return <>
    <button onClick={() => setOpen(true)}>Open queue</button>
    <button>Elsewhere</button>
    {open && <PromptQueue prompts={items} canMutate={canMutate} actions={actions} onClose={() => setOpen(false)} />}
  </>;
}
const row = (n = 1) => screen.getByRole("group", { name: `Queued prompt ${n}` });
const button = (name: string, n = 1) => within(row(n)).getByRole("button", { name });
const draft = () => screen.getByRole("textbox", { name: "Queued prompt text" }) as HTMLTextAreaElement;
function edit(text = "changed", n = 1) {
  fireEvent.click(button("Edit", n));
  fireEvent.change(draft(), { target: { value: text } });
}

describe("queue draft and command recovery", () => {
  it("sends only one Save and keeps text edited during acceptance, including A → B → A", async () => {
    const request = deferred<boolean>();
    const onEdit = vi.fn(() => request.promise);
    render(<Harness onEdit={onEdit} />);
    edit("first draft");
    const save = button("Save");
    act(() => { fireEvent.click(save); fireEvent.click(save); });
    expect(onEdit).toHaveBeenCalledExactlyOnceWith("p0", "first draft");
    fireEvent.change(draft(), { target: { value: "new draft" } });
    fireEvent.change(draft(), { target: { value: "first draft" } });
    await act(async () => { request.resolve(true); });
    expect(draft().value).toBe("first draft");
    expect((button("Save") as HTMLButtonElement).disabled).toBe(false);
  });

  it("does not close another row's editor when an older Save succeeds", async () => {
    const request = deferred<boolean>();
    render(<Harness onEdit={() => request.promise} />);
    edit();
    fireEvent.click(button("Save"));
    fireEvent.click(button("Cancel"));
    edit("second draft", 2);
    await act(async () => { request.resolve(true); });
    expect(draft().value).toBe("second draft");
    expect(within(row(2)).getByRole("textbox")).toBe(draft());
  });

  it("preserves a refused draft and keyboard focus, then supports a successful retry", async () => {
    const request = deferred<boolean>();
    const onEdit = vi.fn().mockReturnValueOnce(request.promise).mockResolvedValue(true);
    render(<Harness onEdit={onEdit} />);
    edit();
    expect(document.activeElement).toBe(draft());
    const save = button("Save");
    save.focus();
    fireEvent.click(save);
    expect(document.activeElement).toBe(row());
    await act(async () => { request.reject(new Error("disk refused")); });
    expect(draft().value).toBe("changed");
    expect(within(row()).getByRole("alert").textContent).toContain("disk refused");
    expect(document.activeElement).toBe(save);
    await act(async () => { fireEvent.click(save); });
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(document.activeElement).toBe(button("Edit"));
    // Acceptance changes the editor, never the daemon's displayed queue.
    expect(within(row()).getByText("first")).toBeTruthy();
  });

  it("keeps drafts and command locks when the panel closes and reopens", async () => {
    const request = deferred<boolean>();
    const onEdit = vi.fn(() => request.promise);
    render(<Harness onEdit={onEdit} />);
    edit();
    fireEvent.click(button("Save"));
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    fireEvent.click(screen.getByRole("button", { name: "Open queue" }));
    expect(draft().value).toBe("changed");
    expect((button("Save") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(button("Save"));
    expect(onEdit).toHaveBeenCalledTimes(1);
    await act(async () => { request.resolve(false); });
    expect(draft().value).toBe("changed");
    expect(within(row()).getByRole("alert").textContent).toContain("not accepted");
  });

  it("serializes conflicting row commands while allowing another row to proceed", async () => {
    const request = deferred<boolean>();
    const onPromote = vi.fn(() => request.promise);
    const onDelete = vi.fn(accepted);
    render(<Harness onPromote={onPromote} onDelete={onDelete} />);
    const promote = button("Send next");
    const remove = button("Remove");
    act(() => { fireEvent.click(promote); fireEvent.click(remove); fireEvent.click(promote); });
    expect(onPromote).toHaveBeenCalledExactlyOnceWith("p0");
    expect(onDelete).not.toHaveBeenCalled();
    await act(async () => { fireEvent.click(button("Remove", 2)); });
    expect(onDelete).toHaveBeenCalledExactlyOnceWith("p1");
    await act(async () => { request.resolve(true); });
    expect(screen.getAllByTestId("prompt-queue-row")).toHaveLength(2);
  });

  it("ignores old acceptance after A → B → A connection scopes", async () => {
    const request = deferred<boolean>();
    const onEdit = vi.fn(() => request.promise);
    const view = render(<Harness scope="a" onEdit={onEdit} />);
    edit();
    fireEvent.click(button("Save"));
    view.rerender(<Harness scope="b" canMutate={false} onEdit={onEdit} />);
    view.rerender(<Harness scope="a" onEdit={onEdit} />);
    await act(async () => { request.resolve(true); });
    expect(draft().value).toBe("changed");
    expect(screen.queryByRole("alert")).toBeNull();
    expect((button("Save") as HTMLButtonElement).disabled).toBe(false);
  });

  it("retires old errors across sessions and drops a draft when its prompt is removed", async () => {
    const request = deferred<boolean>();
    const onEdit = vi.fn(() => request.promise);
    const view = render(<Harness onEdit={onEdit} />);
    edit("old session");
    fireEvent.click(button("Save"));
    view.rerender(<Harness scope="b" sessionId="second session" onEdit={onEdit} />);
    edit("new session");
    await act(async () => { request.reject(new Error("retired refusal")); });
    expect(draft().value).toBe("new session");
    expect(screen.queryByRole("alert")).toBeNull();
    view.rerender(<Harness scope="b" sessionId="second session" items={[prompts[1]]} onEdit={onEdit} />);
    view.rerender(<Harness scope="b" sessionId="second session" onEdit={onEdit} />);
    expect(screen.queryByRole("textbox")).toBeNull();
  });

  it("offers IME-safe keyboard save and cancel without submitting blank text", async () => {
    const onEdit = vi.fn(accepted);
    render(<Harness onEdit={onEdit} />);
    edit("  ");
    fireEvent.keyDown(draft(), { key: "Enter", ctrlKey: true });
    expect(onEdit).not.toHaveBeenCalled();
    fireEvent.change(draft(), { target: { value: "new" } });
    fireEvent.keyDown(draft(), { key: "Enter", metaKey: true, isComposing: true });
    fireEvent.keyDown(draft(), { key: "Escape", keyCode: 229 });
    expect(draft().value).toBe("new");
    expect(onEdit).not.toHaveBeenCalled();
    await act(async () => { fireEvent.keyDown(draft(), { key: "Enter", ctrlKey: true }); });
    expect(onEdit).toHaveBeenCalledExactlyOnceWith("p0", "new");
    edit("cancel this");
    fireEvent.keyDown(draft(), { key: "Escape" });
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(document.activeElement).toBe(button("Edit"));
  });

  it("keeps focus elsewhere on failure and catches synchronous command throws", async () => {
    const request = deferred<boolean>();
    const onDelete = vi.fn().mockReturnValueOnce(request.promise).mockImplementation(() => { throw new Error("bridge unavailable"); });
    render(<Harness onDelete={onDelete} />);
    button("Remove").focus();
    fireEvent.click(button("Remove"));
    const elsewhere = screen.getByRole("button", { name: "Elsewhere" });
    elsewhere.focus();
    await act(async () => { request.reject(new Error("refused")); });
    expect(document.activeElement).toBe(elsewhere);
    await act(async () => { fireEvent.click(button("Remove")); });
    expect(within(row()).getByRole("alert").textContent).toContain("bridge unavailable");
  });
});
