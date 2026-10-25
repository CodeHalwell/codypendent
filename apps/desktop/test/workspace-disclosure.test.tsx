import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Navigation } from "../src/components/Navigation.js";

const preference = "codypendent.workspaceTools";
const props = {
  sessions: [], activeSessionId: null, connectionStatus: "connected" as const,
  onSelectSession: vi.fn(), onSelectView: vi.fn(),
};

beforeEach(() => localStorage.removeItem(preference));
afterEach(() => { vi.restoreAllMocks(); localStorage.removeItem(preference); });

describe("the whole workspace section", () => {
  it("starts expanded and leaves primary chat navigation available when collapsed", () => {
    render(<Navigation {...props} />);
    const toggle = screen.getByRole("button", { name: "Your workspace" });
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByRole("button", { name: "Workspace group" })).toBeTruthy();
    toggle.focus();
    fireEvent.click(toggle, { detail: 0 });
    expect(document.activeElement).toBe(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(document.getElementById(toggle.getAttribute("aria-controls")!)?.hidden).toBe(true);
    expect(screen.queryByRole("button", { name: "Workspace group" })).toBeNull();
    expect(screen.getByRole("button", { name: "Sessions View" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Inbox View" })).toBeTruthy();
  });

  it("remembers the collapsed preference across a fresh mount", () => {
    const first = render(<Navigation {...props} />);
    fireEvent.click(screen.getByRole("button", { name: "Your workspace" }));
    expect(localStorage.getItem(preference)).toBe("collapsed");
    first.unmount();
    render(<Navigation {...props} />);
    expect(screen.getByRole("button", { name: "Your workspace" }).getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByRole("button", { name: "Configuration group" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Your workspace" }));
    expect(localStorage.getItem(preference)).toBe("expanded");
  });

  it("retains subgroup expansion and does not force the parent open when a view changes", () => {
    const view = render(<Navigation {...props} />);
    fireEvent.click(screen.getByRole("button", { name: "Configuration group" }));
    expect(screen.getByRole("button", { name: "Models View" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Your workspace" }));
    view.rerender(<Navigation {...props} currentView="docs" />);
    expect(screen.getByRole("button", { name: "Your workspace" }).getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(screen.getByRole("button", { name: "Your workspace" }));
    expect(screen.getByRole("button", { name: "Models View" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Documents View" }).getAttribute("aria-current")).toBe("page");
  });

  it("remains usable when webview storage is unavailable", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("blocked"); });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("blocked"); });
    render(<Navigation {...props} />);
    const toggle = screen.getByRole("button", { name: "Your workspace" });
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
  });
});
