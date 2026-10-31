/**
 * Keyboard focus around dialogs, and the way back from a secondary view.
 *
 * Every overlay declared `aria-modal="true"` and none confined focus; the
 * inline confirmations never moved it; and secondary views had no on-screen
 * way back. These pin the new behaviour: focus moves into a dialog, Tab wraps
 * inside it, Escape closes only the dialog, focus returns to the trigger, and
 * every non-session view carries a Back control.
 */
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { App } from "../src/App.js";
import { ApiKeys } from "../src/components/ApiKeys.js";
import type { KeysView, LocalConfigClient } from "../src/components/localConfig.js";

function keysClient(): LocalConfigClient {
  const view: KeysView = {
    keys: [
      {
        target: { kind: "model", id: "openai/gpt-5" },
        label: "openai/gpt-5",
        detail: "openai-compatible",
        status: { state: "stored" },
      },
    ],
    auth_path: "/tmp/codypendent/auth.json",
    warnings: [],
    unavailable: "",
  };
  return {
    listModels: () => Promise.reject(new Error("unused")),
    setRunModel: () => Promise.resolve(),
    addModel: () => Promise.resolve(),
    removeModel: () => Promise.resolve(),
    listProviders: () => Promise.reject(new Error("unused")),
    listCatalogModels: () => Promise.reject(new Error("unused")),
    listApiKeys: () => Promise.resolve(view),
    setApiKey: () => Promise.resolve(),
    removeApiKey: () => Promise.resolve(),
    listModes: () => Promise.resolve([]),
    runDefaults: () => Promise.resolve({ mode: { type: "Build" }, model: null }),
    setRunMode: () => Promise.resolve(),
  };
}

describe("dialog focus", () => {
  it("moves focus into the palette, wraps Tab inside it, and gives it back on close", async () => {
    render(<App />);
    const trigger = screen.getByRole("button", { name: "Open command palette" });
    trigger.focus();
    await act(async () => {
      fireEvent.click(trigger);
    });
    const dialog = screen.getByRole("dialog", { name: "Command palette" });
    const input = dialog.querySelector("input") as HTMLInputElement;
    expect(document.activeElement).toBe(input);

    // Shift-Tab from the first focusable wraps to the last one inside the
    // dialog, never out to the sidebar behind it.
    await act(async () => {
      fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
    });
    expect(dialog.contains(document.activeElement)).toBe(true);
    expect(document.activeElement).not.toBe(input);

    await act(async () => {
      fireEvent.keyDown(input, { key: "Escape" });
    });
    expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("focuses an inline confirmation on its safe button and lets Escape close only the dialog", async () => {
    render(<ApiKeys client={keysClient()} />);
    await act(async () => undefined);
    const remove = screen.getByRole("button", { name: /Remove/ });
    remove.focus();
    await act(async () => {
      fireEvent.click(remove);
    });
    const dialog = screen.getByTestId("api-key-remove-confirm");
    const cancel = dialog.querySelector('button[type="button"]:last-of-type') as HTMLButtonElement;
    expect(cancel.textContent).toBe("Cancel");
    expect(document.activeElement).toBe(cancel);

    // Escape closes the confirmation, and nothing else.
    await act(async () => {
      fireEvent.keyDown(cancel, { key: "Escape" });
    });
    expect(screen.queryByTestId("api-key-remove-confirm")).toBeNull();
    expect(document.activeElement).toBe(remove);
  });
});

describe("the way back", () => {
  it("shows a Back control on every view but Sessions, and it walks the history", async () => {
    render(<App />);
    await act(async () => undefined);
    expect(screen.queryByTestId("view-bar")).toBeNull();

    // The Configuration group starts collapsed; open it to reach Models.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Configuration group" }));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Models View" }));
    });
    const bar = screen.getByTestId("view-bar");
    expect(bar.textContent).toContain("Models");

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "API Keys View" }));
    });
    expect(screen.getByTestId("view-bar").textContent).toContain("API Keys");

    await act(async () => {
      fireEvent.click(within(screen.getByTestId("view-bar")).getByRole("button", { name: "Back" }));
    });
    expect(screen.getByTestId("view-bar").textContent).toContain("Models");

    await act(async () => {
      fireEvent.click(within(screen.getByTestId("view-bar")).getByRole("button", { name: "Back" }));
    });
    expect(screen.queryByTestId("view-bar")).toBeNull();
  });
});
