import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ProviderPicker } from "../src/components/ProviderPicker.js";
import type { LocalConfigClient, ProviderRow } from "../src/components/localConfig.js";
const agent: ProviderRow = { id: "claude-code", name: "Claude Code", protocol: "acp", auth: "agent sign-in", local: true,
  requires_key: false, can_list_models: true, available: true, catalog_models: 0, has_key: false,
  unusable_reason: null, community_consent_required: false, community_consent_detail: null };
function client(row = agent, connectAcp = vi.fn().mockResolvedValue({ profileId: "acp/claude-code", coordinate: "claude-code@1", models: ["model-a"] })) {
  return { connectAcp, listProviders: vi.fn().mockResolvedValue({ providers: [row], providers_path: "/config/providers.toml", warnings: [], acp_unavailable: "" }) } as unknown as LocalConfigClient;
}
describe("desktop ACP onboarding", () => {
  it("waits for an explicit connect action and keeps a failed handshake retryable", async () => {
    const connectAcp = vi.fn().mockRejectedValueOnce(new Error("agent sign-in required")).mockResolvedValueOnce({ profileId: "acp/claude-code", models: [] });
    const onConnected = vi.fn();
    render(<ProviderPicker client={client(agent, connectAcp)} repositoryPath="/repo" onConnected={onConnected} />);
    fireEvent.click(await screen.findByRole("button", { name: /Claude Code/ }));
    expect(connectAcp).not.toHaveBeenCalled();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Connect agent" })));
    expect(screen.getByRole("alert").textContent).toContain("agent sign-in required");
    expect(onConnected).not.toHaveBeenCalled();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Connect agent" })));
    expect(connectAcp).toHaveBeenLastCalledWith("claude-code", "/repo", false);
    expect(onConnected).toHaveBeenCalledOnce();
    expect(screen.queryByRole("dialog")).toBeNull();
  });
  it("requires explicit community-risk acceptance and preserves it across failed attempts", async () => {
    const api = client({ ...agent, id: "antigravity-acp", community_consent_required: true, community_consent_detail: "This community adapter may breach account terms." });
    render(<ProviderPicker client={api} repositoryPath="/repo" />);
    fireEvent.click(await screen.findByRole("button", { name: /Claude Code/ }));
    const connect = screen.getByRole("button", { name: "Connect agent" }) as HTMLButtonElement;
    expect(connect.disabled).toBe(true);
    fireEvent.click(screen.getByRole("checkbox"));
    await act(async () => fireEvent.click(connect));
    expect(api.connectAcp).toHaveBeenCalledWith("antigravity-acp", "/repo", true);
  });
  it("does not start an agent without a selected repository", async () => {
    const api = client();
    render(<ProviderPicker client={api} />);
    fireEvent.click(await screen.findByRole("button", { name: /Claude Code/ }));
    expect((screen.getByRole("button", { name: "Connect agent" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(api.connectAcp).not.toHaveBeenCalled();
  });
});
