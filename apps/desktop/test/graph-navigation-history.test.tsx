import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { KnowledgeGraph } from "../src/components/codeGraph/KnowledgeGraph.js";
import { edge, node, page } from "./code-graph-fixtures.js";

const previous = () => screen.getByRole<HTMLButtonElement>("button", { name: "Previous symbol" });
const next = () => screen.getByRole<HTMLButtonElement>("button", { name: "Next symbol" });
const inspect = (id: string) => fireEvent.click(within(screen.getByRole("list", { name: "Loaded code symbols" })).getByRole("button", { name: new RegExp(`app::${id}`) }));
const selected = () => within(screen.getByRole("complementary", { name: "Symbol details" })).getByRole("heading", { level: 3 }).textContent;

describe("code graph symbol history", () => {
  it("retraces symbol exploration and replaces the forward branch after a new selection", () => {
    render(<KnowledgeGraph page={page([node("start"), node("work"), node("finish")])} />);
    fireEvent.click(screen.getByRole("button", { name: "List" }));
    expect(previous().disabled).toBe(true);
    inspect("start"); inspect("work"); inspect("work");
    fireEvent.click(previous());
    expect(selected()).toBe("start");
    fireEvent.click(next());
    expect(selected()).toBe("work");
    fireEvent.click(previous()); inspect("finish");
    expect(next().disabled).toBe(true);
    fireEvent.click(previous());
    expect(selected()).toBe("start");
  });

  it("restores a dismissed inspector and clears a focus/filter that would hide the previous symbol", () => {
    render(<KnowledgeGraph page={page([node("start"), node("work"), node("finish")])} />);
    fireEvent.click(screen.getByRole("button", { name: "List" }));
    inspect("start"); inspect("work");
    fireEvent.click(screen.getByRole("button", { name: "Focus" }));
    fireEvent.change(screen.getByLabelText("Filter connections"), { target: { value: "references" } });
    fireEvent.click(previous());
    expect(selected()).toBe("start");
    expect((screen.getByLabelText("Filter connections") as HTMLSelectElement).value).toBe("");
    expect(screen.getByRole("button", { name: "Inspect app::start" }).getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: "Close symbol details" }));
    expect(screen.queryByRole("complementary", { name: "Symbol details" })).toBeNull();
    fireEvent.click(previous());
    expect(selected()).toBe("start");
  });

  it("discards old navigation when a different repository query replaces the page", () => {
    const view = render(<KnowledgeGraph page={page()} />);
    fireEvent.click(screen.getByRole("button", { name: "List" }));
    inspect("start"); inspect("work"); fireEvent.click(previous());
    view.rerender(<KnowledgeGraph page={page([node("other")], [])} />);
    expect(previous().disabled).toBe(true);
    expect(next().disabled).toBe(true);
    expect(screen.queryByRole("complementary", { name: "Symbol details" })).toBeNull();
    expect(screen.queryByText("app::start")).toBeNull();
  });

  it("restores evicted symbol metadata across display modes and offers retry when reloading fails", async () => {
    let failRestore = false;
    const read = vi.fn(async (id: string) => {
      if (id === "s2" && failRestore) throw new Error("Recorded connections unavailable");
      const nextId = `s${Number(id.slice(1)) + 1}`;
      return page([node(id), node(nextId)], [edge(id, nextId)]);
    });
    render(<KnowledgeGraph page={page([node("s0")], [])} readNeighbors={read} />);
    fireEvent.click(screen.getByRole("button", { name: "Calls" }));
    fireEvent.click(screen.getByRole("button", { name: "List" }));
    for (let index = 1; index <= 15; index++) {
      await waitFor(() => expect(within(screen.getByRole("list", { name: "Loaded code symbols" })).getByRole("button", { name: new RegExp(`app::s${index} `) })).toBeTruthy());
      inspect(`s${index}`);
    }
    await waitFor(() => expect(read).toHaveBeenLastCalledWith("s15"));
    fireEvent.click(screen.getByRole("button", { name: "Structure" }));
    failRestore = true;
    for (let index = 14; index >= 2; index--) fireEvent.click(previous());
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("Recorded connections unavailable"));
    expect(selected()).toBe("s2");
    failRestore = false;
    fireEvent.click(screen.getByRole("button", { name: "Retry connections" }));
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
    expect(selected()).toBe("s2");
  });
});
