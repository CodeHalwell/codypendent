import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ArtifactRef } from "@codypendent/protocol";
import { ArtifactSaveButton, PatchContent } from "../src/components/ArtifactActions.js";
import { AnalyticsDashboard } from "../src/components/AnalyticsDashboard.js";
import { Transcript } from "../src/components/Transcript.js";
import { initialState, reduce } from "../src/daemonState.js";

const artifact: ArtifactRef = {
  id: "artifact-1", media_type: "text/x-diff", byte_length: 12,
  sha256: "a".repeat(64), sensitivity: { type: "Internal" },
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

describe("artifact access", () => {
  it("discards a loaded patch when the same id acquires a different reference", async () => {
    const read = vi.fn().mockResolvedValue(new TextEncoder().encode("old full patch"));
    const { rerender } = render(<PatchContent artifact={artifact} onRead={read} preview="old preview" />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Load full patch" })));
    rerender(<PatchContent artifact={{ ...artifact }} onRead={read} preview="old preview" />);
    expect(screen.getByLabelText("Loaded patch").textContent).toBe("old full patch");
    rerender(<PatchContent artifact={{ ...artifact, sha256: "b".repeat(64) }} onRead={read} preview="new preview" />);
    expect(screen.queryByLabelText("Loaded patch")).toBeNull();
    expect(screen.getByLabelText("Patch preview").textContent).toBe("new preview");
  });

  it("ignores old reads when artifacts change and then return to the original reference", async () => {
    const old = deferred<Uint8Array>();
    const fresh = deferred<Uint8Array>();
    const read = vi.fn().mockReturnValueOnce(old.promise).mockReturnValueOnce(fresh.promise);
    const { rerender } = render(<PatchContent artifact={artifact} onRead={read} />);
    fireEvent.click(screen.getByRole("button", { name: "Load full patch" }));
    rerender(<PatchContent artifact={{ ...artifact, id: "other" }} onRead={read} />);
    rerender(<PatchContent artifact={artifact} onRead={read} />);
    fireEvent.click(screen.getByRole("button", { name: "Load full patch" }));
    await act(async () => fresh.resolve(new TextEncoder().encode("new bytes")));
    await act(async () => old.resolve(new TextEncoder().encode("obsolete bytes")));
    expect(screen.getByLabelText("Loaded patch").textContent).toBe("new bytes");
    expect(read).toHaveBeenCalledTimes(2);
  });

  it("retires old source failures after reconnect without clearing the new result", async () => {
    const old = deferred<Uint8Array>();
    const read = vi.fn().mockReturnValue(old.promise);
    const fresh = vi.fn().mockResolvedValue(new TextEncoder().encode("current connection"));
    const { rerender } = render(<PatchContent artifact={artifact} onRead={read} />);
    fireEvent.click(screen.getByRole("button", { name: "Load full patch" }));
    rerender(<PatchContent artifact={artifact} onRead={undefined} />);
    rerender(<PatchContent artifact={artifact} onRead={fresh} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Load full patch" })));
    await act(async () => old.reject(new Error("Retired socket error")));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByLabelText("Loaded patch").textContent).toBe("current connection");
  });

  it("does not attribute a late save result to a different artifact", async () => {
    const old = deferred<string | null>();
    const save = vi.fn().mockReturnValueOnce(old.promise).mockResolvedValueOnce("/tmp/new.patch");
    const { rerender } = render(<ArtifactSaveButton artifact={artifact} onSave={save} />);
    const button = screen.getByRole("button", { name: "Save file…" });
    act(() => {
      fireEvent.click(button);
      fireEvent.click(button);
    });
    expect(save).toHaveBeenCalledTimes(1);
    const newer = { ...artifact, byte_length: 80 };
    rerender(<ArtifactSaveButton artifact={newer} onSave={save} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Save file…" })));
    await act(async () => old.resolve("/tmp/old.patch"));
    expect(screen.getByRole("status").textContent).toBe("Saved to /tmp/new.patch");
    expect(save).toHaveBeenLastCalledWith(newer);
  });

  it("clears saved-file notices and ignores a pending failure when the save source changes", async () => {
    const old = deferred<string | null>();
    const save = vi.fn().mockResolvedValueOnce("/tmp/first.patch").mockReturnValueOnce(old.promise);
    const fresh = vi.fn().mockResolvedValue(null);
    const { rerender } = render(<ArtifactSaveButton artifact={artifact} onSave={save} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Save file…" })));
    expect(screen.getByRole("status").textContent).toContain("first.patch");
    fireEvent.click(screen.getByRole("button", { name: "Save file…" }));
    rerender(<ArtifactSaveButton artifact={artifact} onSave={fresh} />);
    await act(async () => old.reject(new Error("Old save failure")));
    expect(screen.queryByRole("status")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Save file…" })));
    expect(screen.getByRole("status").textContent).toBe("Save cancelled.");
  });

  it("keeps load focus in the patch during a request and restores it after refusal", async () => {
    const pending = deferred<Uint8Array>();
    render(<PatchContent artifact={artifact} onRead={() => pending.promise} />);
    const button = screen.getByRole("button", { name: "Load full patch" });
    button.focus();
    fireEvent.click(button);
    expect(document.activeElement).toBe(screen.getByRole("group", { name: "Patch content" }));
    await act(async () => pending.reject(new Error("Try again")));
    expect(document.activeElement).toBe(button);
  });
  it("loads the patch from the complete reference preserved by the event store", async () => {
    const read = vi.fn().mockResolvedValue(new TextEncoder().encode("--- old\n+++ new\n+complete patch"));
    const state = reduce(initialState, { type: "frame", frame: {
      kind: "event", session_id: "s1", event: {
        sequence: 1, occurred_at: "2026-09-08T00:00:00Z", actor: { type: "System" },
        body: { type: "PatchProposed", run_id: "run-1", changeset_id: "change-1", artifact, files: ["file.rs"], preview: "+preview", additions: 1, deletions: 0 },
      },
    } });
    render(<Transcript connectionStatus="connected" items={state.transcript} onReadArtifact={read} />);
    fireEvent.click(screen.getByText(/Patch proposed:/));
    expect(read).not.toHaveBeenCalled();
    const load = await screen.findByRole("button", { name: "Load full patch" });
    await act(async () => { fireEvent.click(load); });
    expect(read).toHaveBeenCalledWith(artifact);
    expect(screen.getByLabelText("Loaded patch").textContent).toContain("+complete patch");
    expect(screen.queryByLabelText("Patch preview")).toBeNull();
  });

  it("keeps a failed preview retryable", async () => {
    const read = vi.fn().mockRejectedValueOnce(new Error("artifact checksum mismatch"))
      .mockResolvedValueOnce(new TextEncoder().encode("verified patch"));
    render(<PatchContent artifact={artifact} preview="preview" onRead={read} />);
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Load full patch" })); });
    expect(screen.getByRole("alert").textContent).toContain("checksum mismatch");
    expect(screen.getByLabelText("Patch preview").textContent).toBe("preview");
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Load full patch" })); });
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByLabelText("Loaded patch").textContent).toBe("verified patch");
  });

  it("bounds large previews without claiming to display the full file", async () => {
    const read = vi.fn().mockResolvedValue(new TextEncoder().encode("a".repeat(999_999) + "世界"));
    render(<PatchContent artifact={artifact} onRead={read} />);
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Load full patch" })); });
    expect(screen.getByText(/showing the first 1 MB/)).toBeTruthy();
    expect(screen.getByLabelText("Loaded patch").textContent).toHaveLength(8_000);
    expect(screen.getByText(/Raw preview limited to 32,000/)).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("distinguishes cancellation, write failure, and a saved file", async () => {
    const save = vi.fn().mockResolvedValueOnce(null).mockRejectedValueOnce(new Error("disk full"))
      .mockResolvedValueOnce("/tmp/report.patch");
    render(<ArtifactSaveButton artifact={artifact} onSave={save} />);
    const button = screen.getByRole("button", { name: "Save file…" });
    await act(async () => { fireEvent.click(button); });
    expect(screen.getByRole("status").textContent).toBe("Save cancelled.");
    await act(async () => { fireEvent.click(button); });
    expect(screen.getByRole("alert").textContent).toContain("disk full");
    expect(screen.queryByRole("status")).toBeNull();
    await act(async () => { fireEvent.click(button); });
    expect(save).toHaveBeenLastCalledWith(artifact);
    expect(screen.getByRole("status").textContent).toBe("Saved to /tmp/report.patch");
  });

  it("saves the analytics export the daemon actually returned", async () => {
    const save = vi.fn().mockResolvedValue("/tmp/report.csv");
    const exportAnalytics = vi.fn().mockResolvedValue({ artifact, format: { type: "csv" }, generated_at: "now", row_count: 3, truncated: true });
    render(<AnalyticsDashboard initialPage={{ items: [] }} onExportAnalytics={exportAnalytics} onSaveArtifact={save} />);
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Export CSV" })); });
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Save file…" })); });
    expect(save).toHaveBeenCalledWith(artifact);
    expect(screen.getByText(/Export ready/).textContent).toContain("truncated");
  });
});
