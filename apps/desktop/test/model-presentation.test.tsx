import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ModelPicker } from "../src/components/ModelPicker.js";
import { Composer } from "../src/components/Composer.js";
import type { LocalConfigClient, ModelRow } from "../src/components/localConfig.js";
import { modelLabel, modelProviderLabel } from "../src/modelPresentation.js";

const row = (id: string): ModelRow => ({ id, provider: id.startsWith("acp/") ? "acp" : "openai-compatible",
  model: id.replace(/^acp\//, ""), base_url: "", provider_id: null, context_tokens: null, key: { state: "missing" } });

describe("readable model choices", () => {
  it("shows clean labels, searches them, and sends the original model ID when selected", async () => {
    const models = [row("acp/cline#openai/gpt-5.6-sol"), row("acp/claude-acp#opus[1m]"), row("ollama/deepseek-v4.1-flash:cloud")];
    const setRunModel = vi.fn().mockResolvedValue(undefined);
    const onModelPinned = vi.fn();
    const client = { listModels: vi.fn().mockResolvedValue({ models, configured: true, models_path: "/fixture/models.toml", warnings: [], pinned: null }), setRunModel } as unknown as LocalConfigClient;
    render(<ModelPicker client={client} onModelPinned={onModelPinned} />);
    const heading = await screen.findByText("Claude · Opus · 1M context");
    const choice = heading.closest("li")!;
    const details = within(choice).getByText("Connection details").closest("details")!;
    expect(details.open).toBe(false);
    expect(within(details).getByText("acp/claude-acp#opus[1m]")).toBeTruthy();
    expect(screen.queryByText("unverified")).toBeNull();
    expect(screen.getByText("Cline · GPT-5.6 Sol · OpenAI")).toBeTruthy();
    expect(screen.getByText("DeepSeek V4.1 Flash · Cloud")).toBeTruthy();
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "Claude Opus" } });
    // Friendly labels are searchable even when the user omits punctuation.
    expect(screen.queryByText("Cline · GPT-5.6 Sol · OpenAI")).toBeNull();
    await act(async () => fireEvent.click(within(choice).getByRole("button", { name: "Use" })));
    expect(setRunModel).toHaveBeenCalledWith("acp/claude-acp#opus[1m]");
    expect(onModelPinned).toHaveBeenCalledWith("acp/claude-acp#opus[1m]");
  });

  it("uses the same names in the composer without changing custom names or model variants", () => {
    render(<Composer isRunning={false} onSend={() => true} onOpenModels={() => {}} statusLabel="Build mode · acp/codex-acp#gpt-6.1-sol" />);
    expect(screen.getByRole("button", { name: "Codex · GPT-6.1 Sol" })).toBeTruthy();
    expect(screen.queryByText(/acp\/codex-acp/)).toBeNull();
    expect(modelLabel("My trusted model")).toBe("My trusted model");
    expect(modelLabel("ollama/qwen3:8b")).not.toBe(modelLabel("ollama/qwen3:32b"));
    expect(modelLabel("acp/zcode#account:zai-individual-coding-plan/GLM-5.3-Flash")).toBe("ZCode · GLM-5.3 Flash");
    expect(modelProviderLabel("acp/zcode#account:zai-individual-coding-plan/GLM-5.3-Flash")).toBe("Z.AI");
  });

  it("keeps identical models routed through different agent providers distinct", () => {
    expect(modelLabel("acp/opencode#cerebras/gpt-oss-120b")).toBe("OpenCode · GPT OSS 120B · Cerebras");
    expect(modelLabel("acp/opencode#nebius/openai/gpt-oss-120b")).toBe("OpenCode · GPT OSS 120B · Nebius");
    expect(modelLabel("acp/cline#openai/gpt-5.6-sol")).toBe("Cline · GPT-5.6 Sol · OpenAI");
    expect(modelLabel("acp/cline#openrouter/openai/gpt-5.6-sol")).toBe("Cline · GPT-5.6 Sol · OpenRouter");
  });
});
