import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Transcript } from "../src/components/Transcript.js";
import type { TranscriptItem } from "../src/types.js";

const tool: TranscriptItem = { id: "tool", type: "tool_call", timestamp: "", toolName: "acp:call-42",
  toolLabel: "git status --short", toolKind: "Terminal", text: "git status --short", status: "running" };

describe("transcript tool labels", () => {
  it("shows the human action instead of the ACP matching identifier", () => {
    render(<Transcript items={[tool]} connectionStatus="connected" />);
    expect(screen.getAllByText("git status --short")).toHaveLength(1);
    expect(screen.getByText("Terminal")).toBeTruthy();
    expect(screen.queryByText(/acp:call-42/)).toBeNull();
  });

  it("shows an unavailable result for old calls after the run ended", () => {
    render(<Transcript items={[{ ...tool, status: "finished" }]} connectionStatus="connected" />);
    expect(screen.getByText("finished · result unavailable")).toBeTruthy();
    expect(screen.queryByText("running")).toBeNull();
    expect(screen.queryByText("success")).toBeNull();
  });

  it.each([
    ["mcp.codypendent-desktop.cpd_desktop_defineTool_2749a2214202cee72b57", "Create a reusable helper"],
    ["mcp.codypendent-desktop.generateSandboxedUi", "Create an interactive answer"],
    ["mcp.codypendent-desktop.cpd_desktop_executeTool_0673826a29d7faf9af07", "Run a saved helper"],
    ["mcp.codypendent-desktop.cpd_graph_find_symbols_f5044150b32d1f48e89c", "Find code symbols"],
    ["mcp__codypendent-desktop__cpd_graph_callers_of_368c07f2364600495d82", "Find callers"],
  ])("presents trusted desktop MCP action %s without its transport name", (wire, title) => {
    const view = render(<Transcript items={[{ ...tool, text: wire, toolLabel: wire }]} connectionStatus="connected" />);
    expect(screen.getByText(title)).toBeTruthy();
    expect(screen.getByText("Workspace tool")).toBeTruthy();
    const details = screen.getByText(wire).closest("details")!;
    expect(details.open).toBe(false);
    const visible = view.container.cloneNode(true) as HTMLElement;
    for (const item of visible.querySelectorAll("details:not([open])")) item.remove();
    expect(visible.textContent).not.toContain(wire);
    expect(details.textContent).toContain("Terminal");
  });
});
