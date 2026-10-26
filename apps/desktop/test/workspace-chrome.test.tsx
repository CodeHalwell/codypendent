import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { App } from "../src/App.js";
import { WorkspaceLauncher } from "../src/components/WorkspaceLauncher.js";
import type { DesktopTransport } from "../src/transport.js";

const { invoke, native } = vi.hoisted(() => ({ invoke: vi.fn(), native: { dispatch: (_action: string) => {} } }));
vi.mock("../src/useNativeDesktopActions.js", () => ({ useNativeDesktopActions: (handler: (action: string) => void) => { native.dispatch = handler; } }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

beforeEach(() => { localStorage.clear(); delete document.documentElement.dataset.theme; invoke.mockReset(); });
afterEach(() => { localStorage.clear(); delete document.documentElement.dataset.theme; });

function connectedTransport(): DesktopTransport {
  const unused = async (): Promise<never> => { throw new Error("Unexpected command in focus test"); };
  return {
    socketPath: async () => "/tmp/focus-test.sock",
    connect: async () => ({ socket_path: "/tmp/focus-test.sock", protocol_version: "1.4", daemon_version: "0.14.0", daemon_instance: "focus-test", build_id: "focus-test" }),
    disconnect: async () => {}, listSessions: async () => [], attachSession: async () => {},
    listInbox: async () => ({ items: [], next_cursor: null }),
    startObjective: unused, cancelRun: unused, resolveApproval: unused,
    mutateInbox: unused, queryAnalytics: unused, exportAnalytics: unused,
  };
}

describe("native composer focus", () => {
  it.each(["sessions", "models", "palette", "shortcuts"] as const)("focuses after %s commits without an animation frame or losing the draft", async (origin) => {
    const animation = vi.spyOn(window, "requestAnimationFrame").mockReturnValue(1);
    try {
      const transport = connectedTransport();
      render(<App makeTransport={() => transport} />);
      await waitFor(() => expect((screen.getByRole("textbox", { name: "Objective" }) as HTMLTextAreaElement).disabled).toBe(false));
      fireEvent.change(screen.getByRole("textbox", { name: "Objective" }), { target: { value: "keep --top N draft" } });
      if (origin === "models") act(() => native.dispatch("settings"));
      else if (origin === "palette" || origin === "shortcuts") {
        screen.getByRole("button", { name: "Search commands" }).focus();
        fireEvent.click(screen.getByRole("button", { name: "Search commands" }));
        if (origin === "shortcuts") {
          const filter = screen.getByRole("combobox", { name: "Command palette filter" });
          fireEvent.change(filter, { target: { value: "Keyboard shortcuts" } });
          fireEvent.keyDown(filter, { key: "Enter" });
          expect(screen.getByRole("dialog", { name: "Keyboard shortcuts" })).toBeTruthy();
        }
      } else screen.getByRole("button", { name: "Search commands" }).focus();
      act(() => native.dispatch("focus-composer"));
      const area = screen.getByRole("textbox", { name: "Objective" }) as HTMLTextAreaElement;
      expect(document.activeElement).toBe(area);
      expect(area.value).toBe("keep --top N draft");
      expect(screen.queryByRole("dialog")).toBeNull();
      // A later native focus request must work too; the request flag is retired.
      screen.getByRole("button", { name: "Search commands" }).focus();
      act(() => native.dispatch("focus-composer"));
      expect(document.activeElement).toBe(area);
    } finally { animation.mockRestore(); }
  });

  it("does not move focus behind a blocking confirmation", async () => {
    const transport = connectedTransport();
    render(<App makeTransport={() => transport} />);
    await waitFor(() => expect((screen.getByRole("textbox", { name: "Objective" }) as HTMLTextAreaElement).disabled).toBe(false));
    const confirmation = document.createElement("div");
    confirmation.setAttribute("role", "dialog");
    confirmation.setAttribute("aria-modal", "true");
    confirmation.setAttribute("aria-label", "Delete model");
    const cancel = document.createElement("button");
    cancel.textContent = "Keep model";
    confirmation.append(cancel);
    document.body.append(confirmation);
    try {
      cancel.focus();
      act(() => native.dispatch("focus-composer"));
      expect(document.activeElement).toBe(cancel);
    } finally { confirmation.remove(); }
  });
});

describe("workspace presentation preferences", () => {
  it("restores a hidden sidebar while keeping its command bar available", () => {
    const app = render(<App />);
    fireEvent.keyDown(window, { key: "b", metaKey: true });
    expect(screen.queryByRole("complementary", { name: "Workspace sidebar" })).toBeNull();
    expect(screen.getByRole("button", { name: "Show sidebar" })).toBeTruthy();
    app.unmount();
    render(<App />);
    expect(screen.queryByRole("complementary", { name: "Workspace sidebar" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Show sidebar" }));
    expect(screen.getByLabelText("Workspace sidebar")).toBeTruthy();
  });

  it("restores the chosen appearance and can return to OS appearance", () => {
    const app = render(<App />);
    fireEvent.change(screen.getByRole("combobox", { name: "Appearance" }), { target: { value: "dark" } });
    expect(document.documentElement.dataset.theme).toBe("dark");
    app.unmount();
    render(<App />);
    expect((screen.getByRole("combobox", { name: "Appearance" }) as HTMLSelectElement).value).toBe("dark");
    fireEvent.change(screen.getByRole("combobox", { name: "Appearance" }), { target: { value: "system" } });
    expect(document.documentElement.dataset.theme).toBeUndefined();
  });

  it("changes appearance using the command palette and closes it", () => {
    render(<App />);
    fireEvent.click(screen.getByRole("button", { name: "Search commands" }));
    const filter = screen.getByRole("combobox", { name: "Command palette filter" });
    fireEvent.change(filter, { target: { value: "Appearance: light" } });
    fireEvent.keyDown(filter, { key: "Enter" });
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull();
  });

  it("dismisses the command palette when the native menu opens settings", () => {
    render(<App />);
    fireEvent.click(screen.getByRole("button", { name: "Search commands" }));
    expect(screen.getByRole("dialog", { name: "Command palette" })).toBeTruthy();
    act(() => native.dispatch("settings"));
    expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull();
    expect(screen.getByLabelText("Models View").getAttribute("aria-current")).toBe("page");
  });

  it("returns focus to the toolbar when the palette trigger's sidebar was hidden", () => {
    render(<App />);
    const trigger = screen.getByRole("button", { name: "Open command palette" });
    trigger.focus();
    fireEvent.click(trigger);
    fireEvent.keyDown(window, { key: "b", metaKey: true });
    fireEvent.keyDown(screen.getByRole("combobox", { name: "Command palette filter" }), { key: "Escape" });
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Show sidebar" }));
  });

  it("keeps native menu actions behind an active confirmation", () => {
    render(<App />);
    const confirmation = document.createElement("div");
    confirmation.setAttribute("role", "dialog");
    confirmation.setAttribute("aria-modal", "true");
    confirmation.setAttribute("aria-label", "Delete model");
    document.body.append(confirmation);
    try {
      act(() => native.dispatch("settings"));
      expect(screen.getByLabelText("Sessions View").getAttribute("aria-current")).toBe("page");
      expect(screen.queryByLabelText("Models View")).toBeNull();
    } finally { confirmation.remove(); }
  });

  it("ignores shortcut keys while an IME composition is active", () => {
    render(<App />);
    fireEvent.keyDown(window, { key: "b", metaKey: true, isComposing: true });
    expect(screen.getByLabelText("Workspace sidebar")).toBeTruthy();
  });
});

describe("native repository launchers", () => {
  const targets = [{ id: "zed", label: "Zed", kind: "editor" }, { id: "finder", label: "Finder", kind: "files" }];

  it("offers only installed applications and asks the shell to open the selected checkout", async () => {
    invoke.mockResolvedValueOnce(targets).mockResolvedValueOnce(undefined);
    render(<WorkspaceLauncher repository="/work/project" />);
    await screen.findByRole("option", { name: "Zed" });
    expect(screen.queryByRole("option", { name: "VS Code" })).toBeNull();
    fireEvent.change(screen.getByRole("combobox", { name: "Open repository in" }), { target: { value: "zed" } });
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("workspace_open", { target: "zed", expectedRepository: "/work/project", expectedSession: null }));
  });

  it("reports a launch failure and lets the operator retry", async () => {
    invoke.mockResolvedValueOnce(targets).mockRejectedValueOnce(new Error("Zed could not be opened")).mockResolvedValueOnce(undefined);
    render(<WorkspaceLauncher repository="/work/project" />);
    await screen.findByRole("option", { name: "Zed" });
    const select = screen.getByRole("combobox", { name: "Open repository in" });
    fireEvent.change(select, { target: { value: "zed" } });
    expect((await screen.findByRole("alert")).textContent).toContain("Zed could not be opened");
    fireEvent.change(select, { target: { value: "zed" } });
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
    expect(invoke).toHaveBeenCalledTimes(3);
  });

  it("cannot open applications without a selected repository", async () => {
    invoke.mockResolvedValueOnce(targets);
    render(<WorkspaceLauncher />);
    await screen.findByRole("option", { name: "Zed" });
    expect((screen.getByRole("combobox", { name: "Open repository in" }) as HTMLSelectElement).disabled).toBe(true);
  });
});
