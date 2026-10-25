import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { KanbanView } from "../src/components/KanbanView.js";
import type { DesktopTransport } from "../src/transport.js";

it("enables board creation only after a real repository board has loaded", async () => {
  let resolve!: (board: unknown) => void;
  const create = vi.fn(async () => ({ id: "task-1", workflow_run_id: "board:repo", kind: "task", payload: { title: "A task" }, author: {}, revision: 1, status: "todo" }));
  const transport = { watchBoard: () => new Promise((accept) => { resolve = accept; }), createBoardCard: create } as unknown as DesktopTransport;
  render(<KanbanView transport={transport} />);
  expect(screen.getByRole<HTMLButtonElement>("button", { name: "Add task" }).disabled).toBe(true);
  fireEvent.keyDown(screen.getByLabelText("New task title"), { key: "Enter" });
  expect(create).not.toHaveBeenCalled();
  resolve({ repository: "/repo", board_scope_id: "board:repo", cards: [] });
  await screen.findByTestId("board-column-todo");
  fireEvent.change(screen.getByLabelText("New task title"), { target: { value: "A task" } });
  expect(screen.getByRole<HTMLButtonElement>("button", { name: "Add task" }).disabled).toBe(false);
  fireEvent.click(screen.getByRole("button", { name: "Add task" }));
  await waitFor(() => expect(create).toHaveBeenCalledExactlyOnceWith("A task"));
});

it("offers the repository picker and disables mutations when no project is selected", async () => {
  const onOpenRepository = vi.fn();
  const create = vi.fn();
  const transport = { watchBoard: async () => { throw new Error("Choose a repository to use its task board."); }, createBoardCard: create } as unknown as DesktopTransport;
  render(<KanbanView transport={transport} onOpenRepository={onOpenRepository} />);
  await screen.findByText("Choose a project for your tasks");
  expect(screen.getByRole<HTMLButtonElement>("button", { name: "Add task" }).disabled).toBe(true);
  fireEvent.click(screen.getByRole("button", { name: "Choose repository" }));
  expect(onOpenRepository).toHaveBeenCalledTimes(1);
  expect(create).not.toHaveBeenCalled();
});

it("drops a board read from the previous connection when reconnecting", async () => {
  const replies: Array<(board: unknown) => void> = [];
  const transport = { watchBoard: () => new Promise((resolve) => replies.push(resolve)) } as unknown as DesktopTransport;
  const view = render(<KanbanView transport={transport} connectionEpoch={1} />);
  await waitFor(() => expect(replies).toHaveLength(1));
  view.rerender(<KanbanView transport={transport} connectionEpoch={2} />);
  await waitFor(() => expect(replies).toHaveLength(2));
  replies[1]({ repository: "/new-project", board_scope_id: "board:new", cards: [] });
  await screen.findByText("/new-project");
  replies[0]({ repository: "/old-project", board_scope_id: "board:old", cards: [] });
  await Promise.resolve();
  expect(screen.queryByText("/old-project")).toBeNull();
  expect(screen.getByText("/new-project")).toBeTruthy();
});
