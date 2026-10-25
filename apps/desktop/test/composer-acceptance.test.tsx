import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Composer } from "../src/components/Composer.js";
import { useState } from "react";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

describe("composer acceptance", () => {
  it("lets a lifted draft owner protect edits after a view unmounts", async () => {
    const outcome = deferred<boolean>();
    function Host() {
      const [visible, setVisible] = useState(true);
      const [draft, setDraft] = useState("first");
      return <>
        <button onClick={() => setVisible(!visible)}>Toggle view</button>
        {visible && <Composer draft={draft} onDraftChange={setDraft}
          onDraftAccepted={(submitted) => setDraft((current) => current === submitted ? "" : current)}
          onSend={() => outcome.promise} isRunning={false} />}
      </>;
    }
    render(<Host />);
    await act(async () => { fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter" }); });
    fireEvent.click(screen.getByRole("button", { name: "Toggle view" }));
    fireEvent.click(screen.getByRole("button", { name: "Toggle view" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "new draft after returning" } });
    await act(async () => outcome.resolve(true));
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("new draft after returning");
  });
  it.each([false, true])("preserves a refused draft while queueing=%s", async (isRunning) => {
    const result = deferred<boolean>();
    const send = vi.fn(() => result.promise);
    render(<Composer onSend={send} onQueue={send} canQueue isRunning={isRunning} />);
    const input = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "  keep this draft\n" } });
    await act(async () => { fireEvent.keyDown(input, { key: "Enter" }); });
    expect(input.value).toBe("  keep this draft\n");
    fireEvent.keyDown(input, { key: "Enter" });
    expect(send).toHaveBeenCalledTimes(1);
    await act(async () => result.resolve(false));
    expect(input.value).toBe("  keep this draft\n");
  });

  it("clears an accepted queued draft without deleting edits made during the request", async () => {
    const first = deferred<boolean>();
    const second = deferred<boolean>();
    const queue = vi.fn().mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
    render(<Composer onSend={vi.fn()} onQueue={queue} canQueue isRunning />);
    const input = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "first\n" } });
    await act(async () => { fireEvent.keyDown(input, { key: "Enter" }); });
    fireEvent.change(input, { target: { value: "next objective" } });
    await act(async () => first.resolve(true));
    expect(input.value).toBe("next objective");
    await act(async () => { fireEvent.keyDown(input, { key: "Enter" }); });
    await act(async () => second.resolve(true));
    expect(input.value).toBe("");
  });

  it("preserves drafts when a queue transport rejects", async () => {
    render(<Composer onSend={vi.fn()} onQueue={() => Promise.reject(new Error("disconnected"))} canQueue isRunning />);
    const input = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "recover me" } });
    await act(async () => { fireEvent.keyDown(input, { key: "Enter" }); });
    expect(input.value).toBe("recover me");
  });

  it.each([{ isComposing: true }, { keyCode: 229 }])("does not submit an IME confirmation: %j", async (native) => {
    const send = vi.fn();
    render(<Composer onSend={send} isRunning={false} />);
    const input = screen.getByRole("textbox");
    fireEvent.change(input, { target: { value: "日本語" } });
    await act(async () => { fireEvent.keyDown(input, { key: "Enter", ...native }); });
    expect(send).not.toHaveBeenCalled();
  });
});
