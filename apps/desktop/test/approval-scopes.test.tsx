import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { createTransport } from "../src/transport.js";
import { Transcript } from "../src/components/Transcript.js";
import type { TranscriptItem } from "../src/types.js";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
afterEach(() => { delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__; vi.mocked(invoke).mockReset(); });

const item: TranscriptItem = {
  id: "a", type: "approval", text: "Run git status", timestamp: "", approvalId: "approval", approvalPattern: "git status *",
  approvalAction: { type: "ExecuteCommand", program: "git", args: ["status"], environment: [], cwd: "/project" },
};

describe("approval scope delivery", () => {
  it("sends one native ResolveApproval request with the chosen atomic scope", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    vi.mocked(invoke).mockResolvedValue(undefined);
    const transport = createTransport()!;
    for (const scope of ["Once", "Run", "Pattern", "Repository"] as const) {
      await transport.resolveApproval("approval", "approve", scope);
      expect(invoke).toHaveBeenLastCalledWith("resolve_approval", { approvalId: "approval", approved: true, scope: { type: scope } });
    }
    await transport.resolveApproval("approval", "reject");
    expect(invoke).toHaveBeenLastCalledWith("resolve_approval", { approvalId: "approval", approved: false, scope: { type: "Once" } });
    expect(invoke).toHaveBeenCalledTimes(5);
  });

  it("shares the decision lock with once/reject and preserves errors until a retry succeeds", async () => {
    let reject!: (error: Error) => void;
    const first = new Promise<void>((_, no) => { reject = no; });
    const approve = vi.fn().mockReturnValueOnce(first).mockResolvedValue(undefined);
    const refuse = vi.fn();
    render(<Transcript items={[item]} connectionStatus="connected" onApprove={approve} onReject={refuse} />);
    fireEvent.click(screen.getByRole("button", { name: "Always allow" }));
    fireEvent.click(screen.getByRole("button", { name: /Similar commands · this repository/ }));
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    fireEvent.click(screen.getByRole("button", { name: "Reject" }));
    expect(approve).toHaveBeenCalledExactlyOnceWith("approval", "Repository");
    expect(refuse).not.toHaveBeenCalled();
    expect((screen.getByRole("button", { name: "Always allow" }) as HTMLButtonElement).disabled).toBe(true);
    await act(async () => reject(new Error("ResolveApproval rejected: approval expired")));
    expect(screen.getByRole("alert").textContent).toContain("approval expired");
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Always allow" }));
    fireEvent.click(screen.getByRole("button", { name: "Always allow" }));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: /This action · this run/ })));
    expect(approve).toHaveBeenLastCalledWith("approval", "Run");
    expect(screen.getByRole("status").textContent).toContain("Decision sent");
    expect((screen.getByRole("button", { name: "Always allow" }) as HTMLButtonElement).disabled).toBe(true);
  });
});
