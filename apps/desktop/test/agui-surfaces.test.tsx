import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { AgentToolsView } from "../src/agui/AgentToolsView.js";
import { GeneratedToolResult, generatedUi } from "../src/agui/GeneratedToolResult.js";
import type { DesktopTool } from "../src/agui/types.js";
import type { ArtifactRef } from "@codypendent/protocol";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), convertFileSrc: vi.fn() }));
vi.mock("../src/components/localConfig.js", () => ({ shellAvailable: () => true }));
beforeEach(() => {
  vi.mocked(invoke).mockReset();
  vi.mocked(convertFileSrc).mockReset().mockReturnValue("cody-sandbox://localhost/");
});
afterEach(() => { vi.restoreAllMocks(); });

function tool(description: string, version = 1): DesktopTool {
  return { name: "desktop.custom.helper", description, version, capabilities: [], parameters: { type: "object" } };
}
function artifact(id: string): ArtifactRef {
  return { id, sha256: id.repeat(64).slice(0, 64), byte_length: 100, media_type: "application/json", sensitivity: { type: "Public" } };
}
function bytes(value: unknown) { return new TextEncoder().encode(JSON.stringify(value)); }
function initialiseNativeFrame(frame: HTMLIFrameElement) {
  const post = vi.spyOn(frame.contentWindow!, "postMessage");
  fireEvent(window, new MessageEvent("message", { source: frame.contentWindow, data: { kind: "cody-sandbox-ready" } }));
  expect(post).toHaveBeenCalledTimes(1);
  const init = post.mock.calls[0][0] as { kind: string; source: string };
  expect(init.kind).toBe("cody-sandbox-init");
  const token = init.source.match(/"token":"([^"]+)"/)![1];
  return { post, token };
}
function message(frame: HTMLIFrameElement, token: string, kind: string, value: unknown) {
  fireEvent(window, new MessageEvent("message", { source: frame.contentWindow, data: { token, kind, value } }));
}

it("keeps an old chat’s slow tool-list reply out of the new chat", async () => {
  const replies = new Map<string, (tools: DesktopTool[]) => void>();
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "desktop_tools_list") return new Promise((resolve) => replies.set((args as { sessionId: string }).sessionId, resolve)) as never;
    return null;
  });
  const view = render(<AgentToolsView sessionId="old" connected error={null} onCompose={vi.fn()} />);
  await waitFor(() => expect(replies.has("old")).toBe(true));
  view.rerender(<AgentToolsView sessionId="new" connected error={null} onCompose={vi.fn()} />);
  await waitFor(() => expect(replies.has("new")).toBe(true));
  replies.get("new")!([tool("New chat tool")]);
  await screen.findByText("New chat tool");
  replies.get("old")!([tool("Old chat tool")]);
  await Promise.resolve();
  expect(screen.queryByText("Old chat tool")).toBeNull();
  expect(screen.getByText("New chat tool")).toBeTruthy();
});

it("does not apply an old chat’s removal failure or refresh to a different chat", async () => {
  let rejectRemoval!: (reason: Error) => void;
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "desktop_tools_list") return [tool((args as { sessionId: string }).sessionId === "old" ? "Old chat tool" : "New chat tool") ] as never;
    if (command === "desktop_tools_remove") return new Promise((resolve, reject) => { void resolve; rejectRemoval = reject; }) as never;
    return null;
  });
  const view = render(<AgentToolsView sessionId="old" connected error={null} onCompose={vi.fn()} />);
  await screen.findByText("Old chat tool");
  fireEvent.click(screen.getByRole("button", { name: "Remove desktop.custom.helper" }));
  fireEvent.click(screen.getByRole("button", { name: "Remove" }));
  await waitFor(() => expect(rejectRemoval).toBeTypeOf("function"));
  view.rerender(<AgentToolsView sessionId="new" connected error={null} onCompose={vi.fn()} />);
  await screen.findByText("New chat tool");
  rejectRemoval(new Error("Old chat removal failed"));
  await Promise.resolve();
  expect(screen.queryByRole("alert")).toBeNull();
  expect(vi.mocked(invoke).mock.calls.filter(([command]) => command === "desktop_tools_list")).toHaveLength(2);
  expect(screen.getByText("New chat tool")).toBeTruthy();
});

describe("interactive result validation", () => {
  it.each([
    { jsExpressions: "not a list" }, { jsExpressions: [42] }, { jsFunctions: {} }, { css: [] },
    { placeholderMessages: [false] }, { initialHeight: "320" }, { initialHeight: Number.POSITIVE_INFINITY },
  ])("rejects invalid optional widget data %j", (field) => {
    expect(() => generatedUi({ type: "open-generative-ui", html: "<p>Hello</p>", ...field })).toThrow("Interactive result");
  });

  it("shows malformed widget data as a recoverable error before mounting an iframe", async () => {
    render(<GeneratedToolResult artifact={artifact("a")} read={async () => bytes({ type: "open-generative-ui", html: "Hello", jsExpressions: {} })} />);
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("jsExpressions must be a list"));
    expect(screen.queryByTitle("Interactive agent result")).toBeNull();
    expect(screen.getByRole("button", { name: "Retry" })).toBeTruthy();
  });

  it("initialises the native sandbox once and only accepts its authenticated widget messages", async () => {
    vi.mocked(convertFileSrc).mockReturnValue("http://cody-sandbox.localhost/");
    render(<GeneratedToolResult artifact={artifact("a")} read={async () => bytes({ type: "open-generative-ui", html: "Current result" })} />);
    const frame = await screen.findByTitle<HTMLIFrameElement>("Interactive agent result");
    expect(convertFileSrc).toHaveBeenCalledWith("", "cody-sandbox");
    expect(frame.src).toBe("http://cody-sandbox.localhost/");
    expect(frame.style.opacity).toBe("0");
    const wrongFramePost = vi.spyOn(frame.contentWindow!, "postMessage");
    fireEvent(window, new MessageEvent("message", { source: window, data: { kind: "cody-sandbox-ready" } }));
    expect(wrongFramePost).not.toHaveBeenCalled();
    wrongFramePost.mockRestore();

    const { post, token } = initialiseNativeFrame(frame);
    fireEvent(window, new MessageEvent("message", { source: frame.contentWindow, data: { kind: "cody-sandbox-ready" } }));
    expect(post).toHaveBeenCalledTimes(1);
    fireEvent(window, new MessageEvent("message", { source: window, data: { token, kind: "error", value: "Wrong frame" } }));
    message(frame, "wrong-token", "height", 1100);
    message(frame, "wrong-token", "ready", null);
    expect(screen.queryByRole("alert")).toBeNull();
    expect(frame.style.height).toBe("320px");
    expect(frame.style.opacity).toBe("0");

    message(frame, token, "ready", null);
    message(frame, token, "height", 640);
    expect(frame.style.opacity).toBe("1");
    expect(frame.style.height).toBe("640px");
    expect(screen.queryByText("Opening interactive result…")).toBeNull();
  });

  it("ignores a superseded artifact read and resets widget height and errors for the next result", async () => {
    const pending = new Map<string, (value: Uint8Array) => void>();
    const read = vi.fn((ref: ArtifactRef) => new Promise<Uint8Array>((resolve) => pending.set(ref.id, resolve)));
    const view = render(<GeneratedToolResult artifact={artifact("a")} read={read} />);
    await waitFor(() => expect(pending.has("a")).toBe(true));
    view.rerender(<GeneratedToolResult artifact={artifact("b")} read={read} />);
    await waitFor(() => expect(pending.has("b")).toBe(true));
    pending.get("b")!(bytes({ type: "open-generative-ui", html: "Current result", initialHeight: 400 }));
    const frame = await screen.findByTitle<HTMLIFrameElement>("Interactive agent result");
    const { token } = initialiseNativeFrame(frame);
    message(frame, token, "height", 1100);
    message(frame, token, "error", "An old widget error");
    await screen.findByRole("alert");
    pending.get("a")!(bytes({ type: "open-generative-ui", html: "Old result" }));
    await Promise.resolve();
    expect(screen.getByTitle("Interactive agent result")).toBe(frame);
    expect(frame.style.height).toBe("1100px");
    view.rerender(<GeneratedToolResult artifact={artifact("c")} read={read} />);
    expect(screen.queryByTitle("Interactive agent result")).toBeNull();
    await waitFor(() => expect(pending.has("c")).toBe(true));
    pending.get("c")!(bytes({ type: "open-generative-ui", html: "Next result" }));
    const next = await screen.findByTitle<HTMLIFrameElement>("Interactive agent result");
    expect(next.style.height).toBe("320px");
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
