/**
 * What the wire says about an outcome reaches the screen.
 *
 * Two silent drops, held here: a failed tool call carried its reason in
 * `ToolOutcome::Failed { message }` and the client discarded it (a red
 * "error" with no cause anywhere in the app); and `PatchProposed` carried the
 * touched files, ± line counts, and a bounded diff preview, of which the
 * client printed only the artifact id.
 */
import { describe, expect, it } from "vitest";

import { initialState, reduce, type DaemonState } from "../src/daemonState.js";

function event(body: Record<string, unknown>, sequence: number) {
  return {
    type: "frame" as const,
    frame: {
      kind: "event" as const,
      session_id: "session-1",
      event: { sequence, occurred_at: "2026-01-01T00:00:00Z", body: body as never },
    } as never,
  };
}

function attached(): DaemonState {
  return { ...initialState, activeSessionId: "session-1", activeRunId: "run-1" };
}

describe("tool outcomes", () => {
  it("keeps ordinary context updates out of the chat and updates one near-limit warning", () => {
    let state = reduce(attached(), event({ type: "BudgetWarning", run_id: "run-1", dimension: { type: "Tokens" }, used: 1600, limit: 10000 }, 1));
    expect(state.transcript).toHaveLength(0);
    state = reduce(state, event({ type: "BudgetWarning", run_id: "run-1", dimension: { type: "Tokens" }, used: 8100, limit: 10000 }, 2));
    state = reduce(state, event({ type: "BudgetWarning", run_id: "run-1", dimension: { type: "Tokens" }, used: 9000, limit: 10000 }, 3));
    expect(state.transcript).toHaveLength(1);
    expect(state.transcript[0].text).toContain("9000/10000");
    expect(state.durableEvents).toHaveLength(3);
  });
  it("tool refinements preserve waiting, retrying and another active tool's activity", () => {
    let started = reduce(attached(), event({ type: "ToolStarted", run_id: "run-1", tool: "acp:first", args_digest: "", label: "Terminal · git status" }, 1));
    const activities: DaemonState["activity"][] = [
      { kind: "waiting", on: "approval" },
      { kind: "waiting", on: "question" },
      { kind: "retrying", attempt: 1, maxAttempts: 3, message: "busy", delayMs: 1000 },
      { kind: "tool", tool: "src/main.rs" },
    ];
    for (const activity of activities) {
      const next = reduce({ ...started, activity }, event({ type: "ToolUpdated", run_id: "run-1", tool: "acp:first", label: "Terminal · git status --short" }, 2));
      expect(next.activity).toEqual(activity);
      expect(next.transcript[0].toolLabel).toBe("git status --short");
    }
    const refined = reduce(started, event({ type: "ToolUpdated", run_id: "run-1", tool: "acp:first", label: "Terminal · git status --short" }, 2));
    expect(refined.activity).toEqual({ kind: "tool", tool: "git status --short" });
  });

  it("keeps concurrent ACP calls distinct and applies human action refinements to one card", () => {
    let state = attached();
    state = reduce(state, event({ type: "ToolStarted", run_id: "run-1", tool: "acp:first", args_digest: "", label: "Terminal" }, 1));
    state = reduce(state, event({ type: "ToolStarted", run_id: "run-1", tool: "acp:second", args_digest: "", label: "Terminal" }, 2));
    state = reduce(state, event({ type: "ToolUpdated", run_id: "run-1", tool: "acp:first", label: "Terminal · git status" }, 3));
    state = reduce(state, event({ type: "ToolUpdated", run_id: "run-1", tool: "acp:second", label: "Read File · src/main.rs" }, 4));
    expect(state.transcript).toHaveLength(2);
    expect(state.transcript[0]).toMatchObject({ toolKind: "Terminal", toolLabel: "git status", status: "running" });
    expect(state.transcript[1]).toMatchObject({ toolKind: "Read File", toolLabel: "src/main.rs", status: "running" });
    state = reduce(state, event({ type: "ToolCompleted", run_id: "run-1", tool: "acp:second", outcome: { type: "Failed", message: "missing file" } }, 5));
    expect(state.transcript[0].status).toBe("running");
    expect(state.transcript[1]).toMatchObject({ status: "error", toolResult: "missing file" });
    expect(state.activity).toEqual({ kind: "tool", tool: "git status" });
    state = reduce(state, event({ type: "ToolCompleted", run_id: "run-1", tool: "acp:first", outcome: { type: "Succeeded" } }, 6));
    expect(state.transcript[0].status).toBe("success");
    state = reduce(state, event({ type: "ToolUpdated", run_id: "run-1", tool: "acp:first", label: "Terminal · git status --short" }, 7));
    expect(state.transcript[0].status).toBe("success");
    expect(state.activity).toEqual({ kind: "thinking" });
  });

  it("does not complete the same named tool from another run", () => {
    let state = attached();
    state = reduce(state, event({ type: "ToolStarted", run_id: "run-1", tool: "shell.run", args_digest: "a" }, 1));
    state = reduce(state, event({ type: "ToolStarted", run_id: "run-2", tool: "shell.run", args_digest: "b" }, 2));
    state = reduce(state, event({ type: "ToolCompleted", run_id: "run-1", tool: "shell.run", outcome: { type: "Succeeded" } }, 3));
    expect(state.transcript.map((item) => item.status)).toEqual(["success", "running"]);
    state = reduce(state, event({ type: "RunCompleted", run_id: "run-1", disposition: { type: "Completed", summary: "done" } }, 4));
    expect(state.transcript.map((item) => item.status)).toEqual(["success", "running"]);
  });

  it("settles the actual older ACP journal shape neutrally when its run finishes", () => {
    let state = attached();
    for (let index = 0; index < 20; index++) {
      state = reduce(state, event({ type: "ToolStarted", run_id: "run-1", tool: index % 5 === 0 ? "Read File" : "Terminal", args_digest: "" }, index * 2 + 1));
      state = reduce(state, event({ type: "ToolCompleted", run_id: "run-1", tool: `toolu_${index}`, outcome: { type: "Succeeded" } }, index * 2 + 2));
    }
    const before = state;
    state = reduce(state, event({ type: "RunCompleted", run_id: "run-1", disposition: { type: "Completed", summary: "done" } }, 41));
    expect(state.transcript).toHaveLength(20);
    expect(state.transcript.every((item) => item.status === "finished")).toBe(true);
    expect(before.transcript.every((item) => item.status === "running")).toBe(true);
    expect(state.isRunning).toBe(false);
  });

  it("a terminal state settles only its own run without changing confirmed outcomes", () => {
    let state = attached();
    state = reduce(state, event({ type: "ToolStarted", run_id: "run-1", tool: "acp:first", args_digest: "", label: "Terminal" }, 1));
    state = reduce(state, event({ type: "ToolStarted", run_id: "run-2", tool: "acp:second", args_digest: "", label: "Terminal" }, 2));
    state = reduce(state, event({ type: "RunStateChanged", run_id: "run-2", state: { type: "Completed" } }, 3));
    expect(state.transcript.map((item) => item.status)).toEqual(["running", "finished"]);
    expect(state.activeRunId).toBe("run-1");
    state = reduce(state, event({ type: "ToolCompleted", run_id: "run-1", tool: "acp:first", outcome: { type: "Succeeded" } }, 4));
    state = reduce(state, event({ type: "RunCompleted", run_id: "run-1", disposition: { type: "Completed", summary: "done" } }, 5));
    expect(state.transcript.map((item) => item.status)).toEqual(["success", "finished"]);
  });

  it("keeps a failure's reason on the call it failed", () => {
    let state = reduce(
      attached(),
      event({ type: "ToolStarted", run_id: "run-1", tool: "shell.run", args_digest: "d", label: "cargo test" }, 1),
    );
    state = reduce(
      state,
      event(
        {
          type: "ToolCompleted",
          run_id: "run-1",
          tool: "shell.run",
          outcome: { type: "Failed", message: "exit status 101" },
        },
        2,
      ),
    );
    const call = state.transcript.find((item) => item.type === "tool_call");
    expect(call?.status).toBe("error");
    expect(call?.toolResult).toBe("exit status 101");
  });

  it("adds no reason to a success", () => {
    let state = reduce(
      attached(),
      event({ type: "ToolStarted", run_id: "run-1", tool: "shell.run", args_digest: "d" }, 1),
    );
    state = reduce(
      state,
      event({ type: "ToolCompleted", run_id: "run-1", tool: "shell.run", outcome: { type: "Succeeded" } }, 2),
    );
    const call = state.transcript.find((item) => item.type === "tool_call");
    expect(call?.status).toBe("success");
    expect(call?.toolResult).toBeUndefined();
  });
});

describe("proposed patches", () => {
  it("keeps the files, counts, and diff preview the wire carried", () => {
    const state = reduce(
      attached(),
      event(
        {
          type: "PatchProposed",
          run_id: "run-1",
          changeset_id: "cs-1",
          artifact: { id: "art-1", media_type: "text/x-diff", byte_length: 100, sha256: "x", sensitivity: { type: "Internal" } },
          files: ["src/lib.rs", "src/main.rs"],
          additions: 12,
          deletions: 3,
          // The field name the PROTOCOL uses (`EventBody::PatchProposed`).
          // This test previously sent `diff_preview`, which the client also
          // read — so both sides agreed on a name the daemon never sends, and
          // the card's diff was empty against a real daemon while the test
          // passed.
          preview: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@",
        },
        1,
      ),
    );
    const patch = state.transcript[state.transcript.length - 1];
    expect(patch.text).toBe("Patch proposed: 2 files (+12 −3)");
    expect(patch.patchFiles).toEqual(["src/lib.rs", "src/main.rs"]);
    expect(patch.patchRunId).toBe("run-1");
    expect(patch.diffPreview).toContain("+++ b/src/lib.rs");
    expect(patch.artifactId).toBe("art-1");
  });

  it("reads the preview off the protocol's field and no other", () => {
    // Pins the name: a body carrying only the old invented key leaves the
    // preview absent rather than quietly working.
    const state = reduce(
      attached(),
      event(
        {
          type: "PatchProposed",
          run_id: "run-1",
          changeset_id: "cs-2",
          artifact: {
            id: "art-3",
            media_type: "text/x-diff",
            byte_length: 10,
            sha256: "x",
            sensitivity: { type: "Internal" },
          },
          files: ["src/lib.rs"],
          diff_preview: "a preview under a name the daemon does not send",
        },
        1,
      ),
    );
    expect(state.transcript[state.transcript.length - 1].diffPreview).toBeUndefined();
  });

  it("falls back to the artifact id when the wire carried no file list", () => {
    const state = reduce(
      attached(),
      event(
        {
          type: "PatchProposed",
          run_id: "run-1",
          changeset_id: "cs-1",
          artifact: { id: "art-2", media_type: "text/x-diff", byte_length: 1, sha256: "x", sensitivity: { type: "Internal" } },
        },
        1,
      ),
    );
    const patch = state.transcript[state.transcript.length - 1];
    expect(patch.text).toBe("Patch proposed: artifact art-2");
  });
});
