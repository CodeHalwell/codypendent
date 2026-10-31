import { act, fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { App } from "../src/App.js";
import { KnowledgeSessionFixture } from "./fixtures/knowledgeSession.js";
import type { SkillCard } from "../src/components/knowledgeTransport.js";

function open(view: string) {
  if (!screen.queryByLabelText(`${view} View`)) {
    fireEvent.click(screen.getByLabelText("Workspace group"));
  }
  fireEvent.click(screen.getByLabelText(`${view} View`));
}

it("refreshes all scoped knowledge after switching sessions without reconnecting", async () => {
  const fixture = new KnowledgeSessionFixture();
  render(<App initialView="skills" makeTransport={() => fixture.transport} knowledge={fixture.knowledge} />);
  await screen.findByText("Skill workspace");
  fireEvent.click(await screen.findByLabelText("Session A"));
  open("Skills");
  await screen.findByText("Skill A");
  open("Memory");
  await screen.findByText("Memory A");
  fireEvent.click(screen.getByRole("button", { name: /^Learning review$/ }));
  await screen.findByText("Learning A");
  open("Documents");
  await screen.findAllByText("Document A");
  fireEvent.click(screen.getByLabelText("Session B"));
  open("Skills");
  await screen.findByText("Skill B");
  expect(screen.queryByText("Skill A")).toBeNull();
  open("Memory");
  await screen.findByText("Memory B");
  fireEvent.click(screen.getByRole("button", { name: /^Learning review$/ }));
  await screen.findByText("Learning B");
  expect(screen.queryByText("Memory A")).toBeNull();
  open("Documents");
  await screen.findAllByText("Document B");
  expect(screen.queryByText("Document A")).toBeNull();
});

it("discards a previous session's skill reply after the new session has loaded", async () => {
  const fixture = new KnowledgeSessionFixture();
  render(<App initialView="skills" makeTransport={() => fixture.transport} knowledge={fixture.knowledge} />);
  await screen.findByText("Skill workspace");
  fireEvent.click(await screen.findByLabelText("Session A"));
  await act(async () => undefined);
  let resolve!: (items: SkillCard[]) => void;
  fixture.skillsPending = new Promise((done) => { resolve = done; });
  open("Skills");
  const oldSkill = fixture.skill();
  fireEvent.click(screen.getByLabelText("Session B"));
  await act(async () => undefined);
  open("Skills");
  await screen.findByText("Skill B");
  await act(async () => { resolve([oldSkill]); });
  expect(screen.queryByText("Skill A")).toBeNull();
  expect(screen.getByText("Skill B")).toBeTruthy();
});

it("keeps a completed correction's notice in its originating session", async () => {
  const fixture = new KnowledgeSessionFixture();
  const correctMemory = vi.spyOn(fixture.knowledge, "correctMemory");
  render(<App initialView="memory" makeTransport={() => fixture.transport} knowledge={fixture.knowledge} />);
  await screen.findByText("Memory workspace");
  // Finish attaching before opening an editor: attachment clears the previous
  // knowledge scope, including any card briefly loaded during the transition.
  await act(async () => {
    fireEvent.click(await screen.findByLabelText("Session A"));
  });
  open("Memory");
  await screen.findByText("Memory A");
  let resolve!: (notice: string) => void;
  fixture.correctionPending = new Promise((done) => { resolve = done; });
  fireEvent.click(screen.getByRole("button", { name: /^Correct$/ }));
  const statement = await screen.findByRole("textbox", { name: "Corrected statement" });
  expect((statement as HTMLTextAreaElement).value).toBe("Memory A");
  fireEvent.change(statement, { target: { value: "Memory A corrected" } });
  fireEvent.click(screen.getByRole("button", { name: /^Send correction$/ }));
  expect(correctMemory).toHaveBeenCalledExactlyOnceWith("A", "Memory A corrected");
  await act(async () => {
    fireEvent.click(screen.getByLabelText("Session B"));
  });
  open("Memory");
  await screen.findByText("Memory B");
  await act(async () => { resolve("Corrected memory A"); });
  expect(screen.queryByText("Corrected memory A")).toBeNull();
  expect(screen.queryByText("Memory A")).toBeNull();
  expect(screen.getByText("Memory B")).toBeTruthy();
  expect(correctMemory).toHaveBeenCalledTimes(1);
});
