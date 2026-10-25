/**
 * The two surfaces v0.12.0 shipped without: pause/resume, and the pending-prompt
 * queue.
 *
 * Both are guarded the same way — every assertion below is about the desktop
 * sending a REAL protocol command and rendering only what the daemon said:
 *
 *   - pause/resume are offered strictly from the run state the daemon reported
 *     (`RunStateChanged`), never from `isRunning`, and never at all when this
 *     client cannot tell (a compact catch-up carries `active_runs` but no run
 *     state);
 *   - the queue drawn is the daemon's `PendingPromptsChanged` snapshot, and a
 *     REFUSED mutation renders as a refusal rather than as an empty queue.
 */
import { act, fireEvent, render, renderHook, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { App } from "../src/App.js";
import { useDaemon } from "../src/useDaemon.js";
import type {
  ConnectionInfo,
  DaemonFrame,
  DesktopTransport,
  RunHandle,
  SessionRow,
} from "../src/transport.js";
import type {
  AnalyticsExportRequest,
  AnalyticsExportResult,
  AnalyticsPage,
  InboxEntry,
  InboxMutation,
  InboxPage,
  PromptDelivery,
} from "@codypendent/protocol";

/** What the UI actually sent, recorded per command. */
class QueueStub implements DesktopTransport {
  readonly paused: string[] = [];
  readonly resumed: string[] = [];
  readonly queued: Array<{ text: string; delivery: PromptDelivery }> = [];
  readonly promoted: string[] = [];
  readonly deleted: string[] = [];
  readonly updated: Array<{ promptId: string; text?: string | null }> = [];
  /** Set to make the next queue mutation fail the way a daemon refusal does. */
  queueFailure: string | null = null;
  queueGate: Promise<void> | null = null;
  private frames: ((frame: DaemonFrame) => void) | null = null;

  socketPath(): Promise<string> {
    return Promise.resolve("/tmp/codypendent/daemon.sock");
  }

  connect(onFrame: (frame: DaemonFrame) => void): Promise<ConnectionInfo> {
    this.frames = onFrame;
    return Promise.resolve({
      socket_path: "/tmp/codypendent/daemon.sock",
      protocol_version: "1.4",
      daemon_version: "0.12.1",
      daemon_instance: "instance-1",
      build_id: "build-1",
    });
  }

  disconnect(): Promise<void> {
    return Promise.resolve();
  }

  listSessions(): Promise<SessionRow[]> {
    return Promise.resolve([]);
  }

  startObjective(): Promise<RunHandle> {
    return Promise.resolve({ session_id: "session-1", run_id: "run-1" });
  }

  attachSession(): Promise<void> {
    return Promise.resolve();
  }

  cancelRun(): Promise<void> {
    return Promise.resolve();
  }

  resolveApproval(): Promise<void> {
    return Promise.resolve();
  }

  listInbox(): Promise<InboxPage> {
    return Promise.resolve({ items: [], next_cursor: null });
  }

  mutateInbox(mutation: InboxMutation): Promise<InboxEntry> {
    return Promise.reject(new Error(`no inbox in this stub: ${mutation.type}`));
  }

  queryAnalytics(): Promise<AnalyticsPage> {
    return Promise.resolve({ items: [], next_cursor: null });
  }

  exportAnalytics(_request: AnalyticsExportRequest): Promise<AnalyticsExportResult> {
    return Promise.reject(new Error("no analytics export in this stub"));
  }

  pauseRun(runId: string): Promise<void> {
    this.paused.push(runId);
    return Promise.resolve();
  }

  resumeRun(runId: string): Promise<void> {
    this.resumed.push(runId);
    return Promise.resolve();
  }

  queuePrompt(text: string, delivery: PromptDelivery): Promise<void> {
    if (this.queueFailure) {
      return Promise.reject(new Error(this.queueFailure));
    }
    this.queued.push({ text, delivery });
    return Promise.resolve();
  }

  updateQueuedPrompt(promptId: string, text?: string | null): Promise<void> {
    if (this.queueFailure) {
      return Promise.reject(new Error(this.queueFailure));
    }
    this.updated.push({ promptId, text });
    return this.queueGate ?? Promise.resolve();
  }

  promoteQueuedPrompt(promptId: string): Promise<void> {
    if (this.queueFailure) {
      return Promise.reject(new Error(this.queueFailure));
    }
    this.promoted.push(promptId);
    return Promise.resolve();
  }

  deleteQueuedPrompt(promptId: string): Promise<void> {
    if (this.queueFailure) {
      return Promise.reject(new Error(this.queueFailure));
    }
    this.deleted.push(promptId);
    return Promise.resolve();
  }

  async push(frame: DaemonFrame): Promise<void> {
    await act(async () => {
      this.frames?.(frame);
    });
  }
}

function event(sequence: number, body: Record<string, unknown>): DaemonFrame {
  return {
    kind: "event",
    session_id: "session-1",
    event: {
      sequence,
      actor: { type: "System" },
      occurred_at: "2026-08-17T10:00:00Z",
      body: body as unknown as import("@codypendent/protocol").EventBody,
    },
  };
}

async function renderWith(transport: QueueStub) {
  render(<App makeTransport={() => transport as DesktopTransport} />);
  await act(async () => undefined);
}

/** A session with one run the daemon has told us is `Running`. */
async function liveRun(transport: QueueStub) {
  await renderWith(transport);
  await transport.push(
    event(1, { type: "RunStarted", run_id: "run-1", objective: "port the queue", mode: { type: "Build" } }),
  );
  await transport.push(
    event(2, { type: "RunStateChanged", run_id: "run-1", state: { type: "Running" } }),
  );
}

describe("pausing and resuming a live run", () => {
  it("sends a real PauseRun for the run the daemon named", async () => {
    const transport = new QueueStub();
    await liveRun(transport);

    // Resume is NOT offered on a Running run: `validate_run_transition` admits
    // `ResumeRun` only from `Paused`, so a resume button here could only fail.
    expect(screen.queryByTestId("composer-resume")).toBeNull();

    await act(async () => {
      fireEvent.click(screen.getByTestId("composer-pause"));
    });
    expect(transport.paused).toEqual(["run-1"]);
    // Pausing is not cancelling: the run id is still held and no cancellation
    // was sent.
    expect(transport.resumed).toEqual([]);
  });

  it("offers resume only once the daemon reports the run Paused, and then sends ResumeRun", async () => {
    const transport = new QueueStub();
    await liveRun(transport);

    await transport.push(
      event(3, { type: "RunStateChanged", run_id: "run-1", state: { type: "Paused" } }),
    );

    // The two controls are mutually exclusive and driven by the daemon's state.
    expect(screen.queryByTestId("composer-pause")).toBeNull();
    await act(async () => {
      fireEvent.click(screen.getByTestId("composer-resume"));
    });
    expect(transport.resumed).toEqual(["run-1"]);
  });

  it("offers neither control while the run's state is unknown to this client", async () => {
    const transport = new QueueStub();
    await renderWith(transport);

    // A >500-event catch-up: `SessionProjection` carries `active_runs` but no
    // run state at all (`crates/protocol/src/catchup.rs`). The client therefore
    // cannot say whether the run is pausable, and must not guess.
    await transport.push({
      kind: "catchup",
      session_id: "session-1",
      snapshot: {
        type: "Snapshot",
        through: 900,
        projection: {
          session_id: "session-1",
          title: "caught up",
          last_sequence: 900,
          active_runs: ["run-1"],
          closed: false,
        },
      } as unknown as import("@codypendent/protocol").Catchup,
    });

    expect(screen.queryByTestId("composer-pause")).toBeNull();
    expect(screen.queryByTestId("composer-resume")).toBeNull();
    expect(transport.paused).toEqual([]);
  });

  it("withdraws both controls when the run reaches a terminal state", async () => {
    const transport = new QueueStub();
    await liveRun(transport);
    expect(screen.getByTestId("composer-pause")).toBeTruthy();

    await transport.push(
      event(3, { type: "RunStateChanged", run_id: "run-1", state: { type: "Completed" } }),
    );

    expect(screen.queryByTestId("composer-pause")).toBeNull();
    expect(screen.queryByTestId("composer-resume")).toBeNull();
  });
});

describe("the pending-prompt queue", () => {
  it("queues a follow-up from the composer while a run is live instead of refusing the text", async () => {
    const transport = new QueueStub();
    await liveRun(transport);

    const box = screen.getByPlaceholderText(
      "Queue a follow-up for this session (Enter to queue, Shift+Enter for newline)...",
    ) as HTMLTextAreaElement;
    expect(box.disabled).toBe(false);

    fireEvent.change(box, { target: { value: "then add the regression test" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Queue" }));
    });

    // A real `QueuePrompt` went out, with the delivery the TUI uses for a
    // submission during an active run.
    expect(transport.queued).toEqual([
      { text: "then add the regression test", delivery: { type: "Queue" } },
    ]);
    // ...and the queue is still empty, because the daemon has not reported the
    // entry yet. Nothing was drawn locally on the strength of a sent command.
    fireEvent.click(screen.getByTestId("composer-queue"));
    expect(screen.getByTestId("prompt-queue-empty")).toBeTruthy();
    expect(screen.queryAllByTestId("prompt-queue-row")).toEqual([]);
  });

  it("renders exactly the queue the daemon reported, and replaces it on the next snapshot", async () => {
    const transport = new QueueStub();
    await liveRun(transport);

    await transport.push(
      event(3, {
        type: "PendingPromptsChanged",
        prompts: [
          {
            id: "prompt-1",
            text: "then add the regression test",
            mode: { type: "Build" },
            delivery: { type: "Queue" },
          },
          {
            id: "prompt-2",
            text: "and update the changelog",
            mode: { type: "Build" },
            delivery: { type: "Steer" },
          },
        ],
      }),
    );

    expect(screen.getAllByTestId("prompt-queue-row")).toHaveLength(2);
    expect(screen.getByText("then add the regression test")).toBeTruthy();
    expect(screen.getByText("and update the changelog")).toBeTruthy();

    // Latest-wins: the event carries the WHOLE queue, so folding it REPLACES
    // the projection rather than appending to it.
    await transport.push(
      event(4, {
        type: "PendingPromptsChanged",
        prompts: [
          {
            id: "prompt-2",
            text: "and update the changelog",
            mode: { type: "Build" },
            delivery: { type: "Steer" },
          },
        ],
      }),
    );
    expect(screen.getAllByTestId("prompt-queue-row")).toHaveLength(1);
    expect(screen.queryByText("then add the regression test")).toBeNull();
  });

  it("sends real promote and delete commands for the row the operator picked", async () => {
    const transport = new QueueStub();
    await liveRun(transport);
    await transport.push(
      event(3, {
        type: "PendingPromptsChanged",
        prompts: [
          {
            id: "prompt-1",
            text: "then add the regression test",
            mode: { type: "Build" },
            delivery: { type: "Queue" },
          },
        ],
      }),
    );

    await act(async () => {
      fireEvent.click(screen.getByTestId("prompt-queue-promote"));
    });
    expect(transport.promoted).toEqual(["prompt-1"]);

    await act(async () => {
      fireEvent.click(screen.getByTestId("prompt-queue-delete"));
    });
    expect(transport.deleted).toEqual(["prompt-1"]);

    // The row is still on screen: the daemon has not reported it gone, and the
    // client does not remove it on its own say-so.
    expect(screen.getAllByTestId("prompt-queue-row")).toHaveLength(1);
  });

  it("renders a refused mutation as a refusal, not as an empty queue", async () => {
    const transport = new QueueStub();
    await liveRun(transport);
    transport.queueFailure = "QueuePrompt rejected: queued prompt text cannot be empty (prompt-queue.empty)";

    const box = screen.getByPlaceholderText(
      "Queue a follow-up for this session (Enter to queue, Shift+Enter for newline)...",
    );
    fireEvent.change(box, { target: { value: "something the daemon will refuse" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Queue" }));
    });

    const alert = screen.getByTestId("prompt-queue-error");
    expect(alert.getAttribute("role")).toBe("alert");
    expect(alert.textContent).toContain("The queue command failed.");
    expect(alert.textContent).toContain("prompt-queue.empty");
    expect((box as HTMLTextAreaElement).value).toBe("something the daemon will refuse");
    // The empty-queue wording is a different, non-alarming element. A refusal
    // must never be shown as "nothing is queued" alone.
    expect(screen.getByTestId("prompt-queue-empty")).toBeTruthy();
    expect(screen.getByTestId("prompt-queue-empty").getAttribute("role")).toBeNull();
    expect(transport.queued).toEqual([]);
  });

  it("edits a queued prompt in place through UpdateQueuedPrompt", async () => {
    const transport = new QueueStub();
    await liveRun(transport);
    await transport.push(
      event(3, {
        type: "PendingPromptsChanged",
        prompts: [
          {
            id: "prompt-1",
            text: "then add the regression test",
            mode: { type: "Build" },
            delivery: { type: "Queue" },
          },
        ],
      }),
    );

    fireEvent.click(screen.getByTestId("prompt-queue-start-edit"));
    fireEvent.change(screen.getByTestId("prompt-queue-edit"), {
      target: { value: "then add TWO regression tests" },
    });
    await act(async () => {
      fireEvent.click(screen.getByTestId("prompt-queue-save"));
    });

    expect(transport.updated).toEqual([
      { promptId: "prompt-1", text: "then add TWO regression tests" },
    ]);
  });
});

// These use the real controller so a retired request cannot reach the global
// queue/error banners even when its row's own feedback has already retired.
describe("queue transport generations", () => {
  it("ignores a late refusal after switching away and back to the same session", async () => {
    let refuse!: (error: Error) => void;
    const transport = new QueueStub();
    const { result } = renderHook(() => useDaemon(() => transport));
    await act(async () => undefined);
    await transport.push(event(1, { type: "PendingPromptsChanged", prompts: [] }));
    transport.queueGate = new Promise<void>((_, reject) => { refuse = reject; });
    let changing!: Promise<boolean>;
    act(() => { changing = result.current.updateQueuedPrompt("prompt-1", "old"); });
    await act(async () => { await result.current.selectSession("session-2"); });
    await act(async () => { await result.current.selectSession("session-1"); });
    await act(async () => { refuse(new Error("retired queue refusal")); expect(await changing).toBe(false); });
    expect(result.current.state.promptQueueError).toBeNull();
    expect(result.current.state.error).toBeNull();
  });

  it("does not let old acceptance clear a new refusal after reconnecting the same transport", async () => {
    let finish!: () => void;
    const transport = new QueueStub();
    const { result } = renderHook(() => useDaemon(() => transport));
    await act(async () => undefined);
    await transport.push(event(1, { type: "PendingPromptsChanged", prompts: [] }));
    transport.queueGate = new Promise<void>((resolve) => { finish = resolve; });
    let changing!: Promise<boolean>;
    act(() => { changing = result.current.updateQueuedPrompt("prompt-1", "old"); });
    await act(async () => { void result.current.reconnect(); });
    expect(result.current.state.status).toBe("connected");
    transport.queueFailure = "new connection refused removal";
    await act(async () => { expect(await result.current.deleteQueuedPrompt("prompt-2")).toBe(false); });
    await act(async () => { finish(); expect(await changing).toBe(false); });
    expect(result.current.state.promptQueueError).toContain("new connection refused removal");
    expect(result.current.state.error).toContain("new connection refused removal");
  });
});
