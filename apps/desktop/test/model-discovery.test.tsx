import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ModelDiscoveryPanel } from "../src/components/ModelDiscoveryPanel.js";
import { ProviderPicker } from "../src/components/ProviderPicker.js";
import type { LocalConfigClient, ModelDiscovery, ProviderRow } from "../src/components/localConfig.js";

const report: ModelDiscovery = {
  added: ["acp/claude-acp"], default_model: "acp/claude-acp", warnings: [],
  detected: [{ id: "claude-acp", name: "Claude Code", kind: "agent", state: "ready", detail: "Installed agent connected." }],
};

describe("automatic model discovery", () => {
  it("rescans on request, keeps one scan pending, and refreshes the models after persistence", async () => {
    let finish!: (value: ModelDiscovery) => void;
    const discoverModels = vi.fn(() => new Promise<ModelDiscovery>((resolve) => { finish = resolve; }));
    const refreshed = vi.fn();
    render(<ModelDiscoveryPanel client={{ discoverModels } as unknown as LocalConfigClient} onDiscovered={refreshed} />);
    expect(discoverModels).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Scan again" }));
    expect((screen.getByRole("button", { name: "Finding models…" }) as HTMLButtonElement).disabled).toBe(true);
    expect(discoverModels).toHaveBeenCalledOnce();
    await act(async () => finish(report));
    expect(refreshed).toHaveBeenCalledOnce();
    expect(screen.getByText(/1 model added/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Scan again" })).toBeTruthy();
  });

  it("retains a failed discovery as an error and allows a retry", async () => {
    const discoverModels = vi.fn().mockRejectedValueOnce(new Error("configuration is not writable")).mockResolvedValueOnce(report);
    render(<ModelDiscoveryPanel client={{ discoverModels } as unknown as LocalConfigClient} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Scan again" })));
    expect(screen.getByRole("alert").textContent).toContain("not writable");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Scan again" })));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(discoverModels).toHaveBeenCalledTimes(2);
  });

  it("opens existing agent models without repeating agent setup", async () => {
    const agent: ProviderRow = { id: "claude-acp", name: "Claude Code", protocol: "acp", auth: "agent sign-in", local: true,
      requires_key: false, can_list_models: true, available: true, catalog_models: 0, has_key: false,
      unusable_reason: null, community_consent_required: false, community_consent_detail: null };
    const connectAcp = vi.fn();
    const onConnected = vi.fn();
    const api = { connectAcp,
      listProviders: vi.fn().mockResolvedValue({ providers: [agent], warnings: [], acp_unavailable: "" }),
      listModels: vi.fn().mockResolvedValue({ models: [{ provider: "acp", model: "claude-acp@1.0.0" }] }),
    } as unknown as LocalConfigClient;
    render(<ProviderPicker client={api} onConnected={onConnected} />);
    fireEvent.click(await screen.findByRole("button", { name: /Claude Code/ }));
    expect(onConnected).toHaveBeenCalledOnce();
    expect(connectAcp).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});
