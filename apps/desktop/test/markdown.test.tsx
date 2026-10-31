/**
 * The model writes Markdown. Showing it raw meant reading `##` and `**` as
 * literal characters, which is what the transcript did.
 *
 * The security property is the one to hold on to: this renders React elements
 * and never HTML, so model output arriving over a socket cannot inject into the
 * webview. The tests below check both halves — that Markdown becomes structure,
 * and that markup in the text stays text.
 */
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { renderMarkdown } from "../src/markdown.js";
import { Transcript } from "../src/components/Transcript.js";

function draw(text: string) {
  return render(<div data-testid="md">{renderMarkdown(text)}</div>);
}

describe("markdown rendering", () => {
  it("renders headings as structure, not as hashes", () => {
    draw("## Dead weight / hygiene");
    const root = screen.getByTestId("md");
    expect(root.textContent).toBe("Dead weight / hygiene");
    expect(root.textContent).not.toContain("##");
  });

  it("renders bold and inline code without their delimiters", () => {
    draw("**`main.py`** is a stub");
    const root = screen.getByTestId("md");
    expect(root.querySelector("strong")).toBeTruthy();
    expect(root.textContent).toBe("**`main.py`** is a stub".replace(/\*\*/g, "").replace(/`/g, ""));
  });

  it("renders a fenced code block verbatim, delimiters removed", () => {
    draw("before\n```rust\nlet x = 1;\n```\nafter");
    const root = screen.getByTestId("md");
    const pre = root.querySelector("pre");
    expect(pre?.textContent).toBe("let x = 1;");
    expect(root.textContent).not.toContain("```");
  });

  it("renders a bullet list as list items", () => {
    draw("- one\n- two\n- three");
    const items = screen.getByTestId("md").querySelectorAll("li");
    expect(items).toHaveLength(3);
    expect(items[1].textContent).toBe("two");
  });

  it("renders an ordered list as an ordered list", () => {
    const view = draw("1. first\n2. second");
    expect(view.container.querySelector("ol")).toBeTruthy();
    expect(view.container.querySelectorAll("li")).toHaveLength(2);
  });

  it("NEVER emits HTML from the model's text", () => {
    draw('<img src=x onerror="alert(1)"> and <b>bold</b>');
    const root = screen.getByTestId("md");
    // The markup survives as literal text...
    expect(root.textContent).toContain('<img src=x onerror="alert(1)">');
    // ...and produced no elements of its own.
    expect(root.querySelector("img")).toBeNull();
    expect(root.querySelector("b")).toBeNull();
  });

  it("keeps external destinations visible and copyable", () => {
    draw("see [the docs](https://example.com/x)");
    const root = screen.getByTestId("md");
    expect(root.querySelector("a")).toBeNull();
    expect(root.textContent).toBe("see the docs (https://example.com/x)");
    expect(screen.getByTitle("https://example.com/x")).toBeTruthy();
  });

  it("leaves unsupported syntax as the plain text it already was", () => {
    draw("a | table | row\n--- | --- | ---");
    expect(screen.getByTestId("md").textContent).toContain("a | table | row");
  });
});

describe("long system notes fold instead of walling the transcript", () => {
  it("folds the daemon's worktree-retention note behind a summary", () => {
    const note =
      "Kept the worktree /Users/x/codypendent-worktrees/s/run-abc and its branch `codypendent/run-abc`: " +
      "it held uncommitted changes, so nothing was deleted. Its diff is saved as artifact 01a0-e2a0.\n" +
      "Recover or discard it with `git worktree remove ...` and `git branch -D ...`.";
    render(
      <Transcript
        items={[{ id: "s1", type: "system", text: note, timestamp: "t" }]}
        connectionStatus="connected"
      />,
    );
    const details = document.querySelector("details");
    expect(details).toBeTruthy();
    // Folded by default, and the recovery text is kept rather than discarded.
    expect(details?.hasAttribute("open")).toBe(false);
    expect(details?.textContent).toContain("git branch -D");
  });

  it("leaves a short system line inline", () => {
    render(
      <Transcript
        items={[{ id: "s2", type: "system", text: "Run cancelled", timestamp: "t" }]}
        connectionStatus="connected"
      />,
    );
    expect(document.querySelector("details")).toBeNull();
    expect(screen.getByText("Run cancelled")).toBeTruthy();
  });
});

describe("fences and tables", () => {
  it("recognises a fence with an info string and labels the language", () => {
    // ```ts title="x" used to fail the fence regex entirely, so the code
    // rendered as literal backticks and text.
    render(<div>{renderMarkdown('```ts title="x"\nconst a = 1;\n```')}</div>);
    expect(document.querySelector("pre")?.textContent).toBe("const a = 1;");
    expect(screen.getByText("ts")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Copy" })).toBeTruthy();
  });

  it("renders a pipe table as a table", () => {
    render(
      <div>
        {renderMarkdown("| model | cost |\n|---|---|\n| gpt | low |\n| opus | high |")}
      </div>,
    );
    const table = document.querySelector("table");
    expect(table).toBeTruthy();
    expect(table?.querySelectorAll("th").length).toBe(2);
    expect(table?.querySelectorAll("td").length).toBe(4);
    expect(screen.getByText("opus")).toBeTruthy();
  });

  it("leaves a lone pipe line as the text it was", () => {
    // No separator row: not a table, and it must not vanish.
    render(<div>{renderMarkdown("| just some text |")}</div>);
    expect(screen.getByText("| just some text |")).toBeTruthy();
    expect(document.querySelector("table")).toBeNull();
  });
});

describe("source references", () => {
  it("opens angle-wrapped absolute paths with spaces through the source callback", async () => {
    const open = vi.fn().mockResolvedValue(undefined);
    const target = "/Users/daniel/My Project/codypendent-worktrees/triage/run-123/prompts/system_prompt.py:42";
    render(<div>{renderMarkdown(`See [triage prompt](<${target}>) and [agent](freshservice_triage_agent/agent.py:68).`, { onOpenSource: open })}</div>);
    const prompt = screen.getByRole("button", { name: "Open triage prompt at line 42" });
    expect(prompt.getAttribute("title")).toBe(`<${target}>`);
    expect(document.body.textContent).not.toContain("/Users/daniel");
    fireEvent.click(prompt);
    await waitFor(() => expect(open).toHaveBeenCalledWith(target.slice(0, -3), 42));
    fireEvent.click(screen.getByRole("button", { name: "Open agent at line 68" }));
    await waitFor(() => expect(open).toHaveBeenCalledWith("freshservice_triage_agent/agent.py", 68));
  });

  it("shortens path labels and supports bare filenames and encoded spaces", async () => {
    const open = vi.fn().mockResolvedValue(undefined);
    render(<div>{renderMarkdown("[docs/Project Notes.md:12](docs/Project%20Notes.md:12) and [README](README.md:91)", { onOpenSource: open })}</div>);
    expect(screen.getByRole("button", { name: "Open Project Notes.md:12" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Open Project Notes.md:12" }));
    await waitFor(() => expect(open).toHaveBeenCalledWith("docs/Project Notes.md", 12));
    fireEvent.click(screen.getByRole("button", { name: "Open README at line 91" }));
    await waitFor(() => expect(open).toHaveBeenCalledWith("README.md", 91));
  });

  it("preserves code literals and never turns unsafe destinations into navigation", () => {
    const open = vi.fn();
    render(<div>{renderMarkdown("`[source](src/main.py:2)`\n\n[script](javascript:alert) [encoded](javascript%3Aalert) [file](file:///etc/passwd) [data](data:text/plain,bad) [up](../secret.txt) [host](//example.com/a.py)", { onOpenSource: open })}</div>);
    expect(document.querySelector("code")?.textContent).toBe("[source](src/main.py:2)");
    expect(screen.queryAllByRole("button")).toHaveLength(0);
    expect(document.querySelector("a")).toBeNull();
    expect(open).not.toHaveBeenCalled();
  });

  it("does not attach a late open failure to a replacement source reference", async () => {
    let reject: (reason: Error) => void = () => undefined;
    const open = vi.fn().mockImplementation(() => new Promise<void>((_, fail) => { reject = fail; }));
    const view = render(<div>{renderMarkdown("[source](src/old.py:2)", { onOpenSource: open })}</div>);
    fireEvent.click(screen.getByRole("button", { name: "Open source at line 2" }));
    view.rerender(<div>{renderMarkdown("[source](src/new.py:4)", { onOpenSource: open })}</div>);
    await act(async () => reject(new Error("Old source unavailable")));
    await waitFor(() => expect(screen.getByRole("button", { name: "Open source at line 4" })).toBeTruthy());
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("shows rejected native source openings rather than dropping the failure", async () => {
    const open = vi.fn().mockRejectedValue(new Error("File is outside this checkout"));
    render(<div>{renderMarkdown("[source](/Users/elsewhere/src/main.py:2)", { onOpenSource: open })}</div>);
    fireEvent.click(screen.getByRole("button", { name: "Open source at line 2" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("File is outside this checkout"));
  });

  it("explains when an earlier run's files are no longer available", async () => {
    const open = vi.fn().mockRejectedValue(new Error("The run's worktree is no longer available: No such file or directory (os error 2)"));
    render(<div>{renderMarkdown("[agent](src/agent.py:68)", { onOpenSource: open })}</div>);
    fireEvent.click(screen.getByRole("button", { name: "Open agent at line 68" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("The files from this earlier run are no longer available."));
    expect(screen.getByRole("alert").textContent).not.toContain("os error");
  });
});
