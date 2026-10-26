/**
 * The Models page says whether each configured model can actually run, and
 * can ask the provider on demand.
 *
 * The desktop showed only credential PRESENCE; a mistyped key read as "setup
 * complete" until the first run failed. The shell now computes the TUI's
 * readiness badge for every row (`src-tauri/src/models.rs`), and a Test button
 * asks the provider over the network because the operator asked it to.
 */
import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { ModelPicker } from "../src/components/ModelPicker.js";
import type {
  LocalConfigClient,
  ModelReadinessRow,
  ModelsView,
} from "../src/components/localConfig.js";

const MODELS: ModelsView = {
  models: [
    {
      id: "openai/gpt-5",
      provider: "openai-compatible",
      base_url: "https://api.openai.com/v1",
      model: "gpt-5",
      provider_id: "openai",
      context_tokens: 200000,
      key: { state: "stored" },
    },
    {
      id: "ollama/qwen3",
      provider: "openai-compatible",
      base_url: "http://localhost:11434/v1",
      model: "qwen3:8b",
      provider_id: "ollama",
      context_tokens: null,
      key: { state: "missing" },
    },
  ],
  models_path: "/tmp/codypendent/models.toml",
  configured: true,
  warnings: [],
  pinned: null,
};

function client(readiness: ModelReadinessRow[], probe: (id: string) => ModelReadinessRow): LocalConfigClient {
  return {
    listModels: () => Promise.resolve(MODELS),
    setRunModel: () => Promise.resolve(),
    addModel: () => Promise.resolve(),
    removeModel: () => Promise.resolve(),
    listProviders: () => Promise.reject(new Error("not in this test")),
    listCatalogModels: () => Promise.reject(new Error("not in this test")),
    listApiKeys: () => Promise.reject(new Error("not in this test")),
    setApiKey: () => Promise.resolve(),
    removeApiKey: () => Promise.resolve(),
    listModes: () => Promise.resolve([]),
    runDefaults: () => Promise.resolve({ mode: { type: "Build" }, model: null }),
    setRunMode: () => Promise.resolve(),
    listModelReadiness: () => Promise.resolve(readiness),
    modelReadiness: (id) => Promise.resolve(probe(id)),
  };
}

describe("model readiness", () => {
  it("shows real readiness problems without default unverified badges or hosted probes", async () => {
    const api = client(
      [
        {
          id: "openai/gpt-5",
          readiness: { state: "unverified", detail: "credential resolves; Test asks the provider" },
          probed: false,
        },
        {
          id: "ollama/qwen3",
          readiness: { state: "unavailable", detail: "connection check to http://localhost:11434/v1 failed: connection refused" },
          probed: true,
        },
      ],
      () => {
        throw new Error("no probe expected");
      },
    );
    render(<ModelPicker client={api} />);
    await act(async () => undefined);
    await act(async () => undefined);
    expect(screen.queryByTestId("model-readiness-openai/gpt-5")).toBeNull();
    expect(screen.getByText("Not tested").closest("details")?.open).toBe(false);
    expect(screen.getByTestId("model-readiness-ollama/qwen3").textContent).toContain("unavailable");
    // An unavailable model says why, in the row itself.
    expect(screen.getByText(/connection refused/)).toBeTruthy();
  });

  it("asks the provider when Test is pressed and reports the verdict", async () => {
    const probes: string[] = [];
    const api = client(
      [
        {
          id: "openai/gpt-5",
          readiness: { state: "unverified", detail: "credential resolves; Test asks the provider" },
          probed: false,
        },
      ],
      (id) => {
        probes.push(id);
        return {
          id,
          readiness: { state: "ready", detail: "api.openai.com answered and lists gpt-5" },
          probed: true,
        };
      },
    );
    render(<ModelPicker client={api} />);
    await act(async () => undefined);
    await act(async () => undefined);
    const [test] = screen.getAllByRole("button", { name: "Test" });
    await act(async () => {
      fireEvent.click(test);
    });
    expect(probes).toEqual(["openai/gpt-5"]);
    expect(screen.getByTestId("model-readiness-openai/gpt-5").textContent).toContain("ready");
    expect(screen.getByRole("status").textContent).toContain("GPT-5: ready");
  });

  it("keeps a Test verdict when the slower bulk read lands after it", async () => {
    // The initial bulk read is deliberately not awaited, so the operator can
    // press Test while it is still in flight. The bulk answer only inspects
    // stored credentials; the Test asked the provider. Assigning the bulk map
    // wholesale on arrival reverted the proven verdict — Ready went back to
    // Unverified, beside a notice saying the test had succeeded.
    let releaseBulk: () => void = () => undefined;
    const bulkArrived = new Promise<void>((resolve) => {
      releaseBulk = resolve;
    });
    const api = client(
      [
        {
          id: "openai/gpt-5",
          readiness: { state: "unverified", detail: "credential resolves; Test asks the provider" },
          probed: false,
        },
      ],
      (id) => ({
        id,
        readiness: { state: "ready", detail: "api.openai.com answered and lists gpt-5" },
        probed: true,
      }),
    );
    api.listModelReadiness = () =>
      bulkArrived.then(() => [
        {
          id: "openai/gpt-5",
          readiness: {
            state: "unverified" as const,
            detail: "credential resolves; Test asks the provider",
          },
          probed: false,
        },
      ]);

    render(<ModelPicker client={api} />);
    await act(async () => undefined);
    await act(async () => undefined);

    // Test resolves while the bulk read is still held.
    const [test] = screen.getAllByRole("button", { name: "Test" });
    await act(async () => {
      fireEvent.click(test);
    });
    expect(screen.getByTestId("model-readiness-openai/gpt-5").textContent).toContain("ready");

    // Now the older, credential-only answer arrives. It must not win.
    await act(async () => {
      releaseBulk();
      await bulkArrived;
    });
    expect(screen.getByTestId("model-readiness-openai/gpt-5").textContent).toContain("ready");
  });

  it("draws no badge and no Test when the shell cannot compute readiness", async () => {
    const api = client([], () => {
      throw new Error("unused");
    });
    delete (api as Partial<LocalConfigClient>).listModelReadiness;
    delete (api as Partial<LocalConfigClient>).modelReadiness;
    render(<ModelPicker client={api} />);
    await act(async () => undefined);
    expect(screen.queryByTestId("model-readiness-openai/gpt-5")).toBeNull();
    expect(screen.queryByRole("button", { name: "Test" })).toBeNull();
  });
});
