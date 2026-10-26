import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type {
  ConnectionInfo,
  DaemonFrame,
  DesktopTransport,
  RunHandle,
} from "../src/transport.js";
import { useDaemon } from "../src/useDaemon.js";
import { runAtStake } from "../src/components/ConfirmCancel.js";

const INFO: ConnectionInfo = {
  socket_path: "/tmp/codypendent/daemon.sock",
  protocol_version: "1.4",
  daemon_version: "0.14.0",
  daemon_instance: "launch-guard",
  build_id: "launch-guard",
};

/**
 * A transport whose launch never settles until the test says so — the window
 * in which the daemon has not yet answered and `isRunning` is therefore still
 * false.
 */
class LaunchTransport {
  readonly objectives: string[] = [];
  private release: ((handle: RunHandle) => void) | null = null;
  private refuse: ((error: Error) => void) | null = null;
  private frames: ((frame: DaemonFrame) => void) | null = null;

  /** Set to make the NEXT connect attempt fail, as a dead daemon would. */
  failNextConnect = false;

  connect(handler: (frame: DaemonFrame) => void): Promise<ConnectionInfo> {
    this.frames = handler;
    if (this.failNextConnect) {
      this.failNextConnect = false;
      return Promise.reject(new Error("connection refused"));
    }
    return Promise.resolve(INFO);
  }

  disconnect(): Promise<void> {
    return Promise.resolve();
  }

  socketPath(): Promise<string> {
    return Promise.resolve(INFO.socket_path);
  }

  listSessions(): Promise<[]> {
    return Promise.resolve([]);
  }

  listInbox() {
    return Promise.resolve({ items: [], next_cursor: null });
  }

  startObjective(objective: string): Promise<RunHandle> {
    this.objectives.push(objective);
    return new Promise<RunHandle>((resolve, reject) => {
      this.release = resolve;
      this.refuse = reject;
    });
  }

  /** Let the daemon answer the launch that is waiting. */
  answer(handle: RunHandle = { session_id: "session-1", run_id: "run-1" }): void {
    this.release?.(handle);
  }

  reject(): void {
    this.refuse?.(new Error("StartRun refused"));
  }

  push(frame: DaemonFrame): void {
    this.frames?.(frame);
  }
}

function started(sessionId: string, runId: string, objective: string): DaemonFrame {
  return {
    kind: "event", session_id: sessionId,
    event: { sequence: 1, actor: { type: "System" }, occurred_at: "2026-10-04T18:30:34Z",
      body: { type: "RunStarted", run_id: runId, objective, mode: { type: "Build" } } },
  };
}

function renderDaemon(transport: LaunchTransport) {
  return renderHook(() => useDaemon(() => transport as unknown as DesktopTransport));
}

describe("a launch already in flight", () => {
  it("refuses a second concurrent submit instead of starting a second paid run", async () => {
    const transport = new LaunchTransport();
    const daemon = renderDaemon(transport);
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));

    // Two clicks inside the window before the daemon answers — a double-click
    // on Send, or on a failure card's Retry, which is enabled precisely while
    // `isRunning` is false.
    let first: Promise<boolean> = Promise.resolve(false);
    let second: Promise<boolean> = Promise.resolve(false);
    await act(async () => {
      first = daemon.result.current.submit("ship the thing");
      second = daemon.result.current.submit("ship the thing");
      transport.answer();
      await Promise.all([first, second]);
    });

    expect(await first).toBe(true);
    expect(await second).toBe(false);
    expect(transport.objectives).toEqual(["ship the thing"]);
  });

  it("accepts the next submit once the daemon has answered the last one", async () => {
    const transport = new LaunchTransport();
    const daemon = renderDaemon(transport);
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));

    await act(async () => {
      const launch = daemon.result.current.submit("first objective");
      transport.answer();
      await launch;
    });
    await act(async () => {
      const launch = daemon.result.current.submit("second objective");
      transport.answer();
      await launch;
    });

    expect(transport.objectives).toEqual(["first objective", "second objective"]);
  });

  it("retains early new-session metadata only after its launch is accepted", async () => {
    const transport = new LaunchTransport();
    const daemon = renderDaemon(transport);
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));
    await act(async () => transport.push(started("old-session", "old-run", "previous task")));
    let launch!: Promise<boolean>;
    await act(async () => {
      launch = daemon.result.current.submit("run the tests");
      transport.push(started("new-session", "new-run", "run the tests"));
      transport.push(started("unrelated-session", "other-run", "unrelated task"));
    });
    expect(daemon.result.current.state.activeSessionId).toBe("old-session");
    await act(async () => {
      transport.answer({ session_id: "new-session", run_id: "new-run" });
      await launch;
    });
    const state = daemon.result.current.state;
    expect(runAtStake(state.durableEvents, state.activeRunId)).toEqual({
      objective: "run the tests", startedAt: "2026-10-04T18:30:34Z",
    });
    expect(state.transcript.map((item) => item.text)).toEqual(["run the tests"]);
    expect(state.runState).toBe("Queued");
  });

  it("keeps an early terminal event authoritative when the launch reply arrives", async () => {
    const transport = new LaunchTransport();
    const daemon = renderDaemon(transport);
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));
    await act(async () => {
      const launch = daemon.result.current.submit("fast task");
      transport.push(started("session-1", "run-1", "fast task"));
      transport.push({ kind: "event", session_id: "session-1", event: {
        sequence: 2, actor: { type: "System" }, occurred_at: "2026-10-04T18:30:35Z",
        body: { type: "RunStateChanged", run_id: "run-1", state: { type: "Completed" } },
      } });
      transport.answer();
      await launch;
    });
    expect(daemon.result.current.state.activeRunId).toBeNull();
    expect(daemon.result.current.state.isRunning).toBe(false);
  });

  it.each(["refused", "disconnected"])("does not adopt early frames from a %s launch", async (failure) => {
    const transport = new LaunchTransport();
    const daemon = renderDaemon(transport);
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));
    await act(async () => transport.push(started("old-session", "old-run", "previous task")));
    let accepted = true;
    await act(async () => {
      const launch = daemon.result.current.submit("new task");
      transport.push(started("session-1", "run-1", "new task"));
      if (failure === "refused") transport.reject();
      else {
        transport.push({ kind: "disconnected", reason: "connection closed" });
        transport.answer();
      }
      accepted = await launch;
    });
    expect(accepted).toBe(false);
    expect(daemon.result.current.state.activeSessionId).toBe("old-session");
    expect(daemon.result.current.state.transcript.map((item) => item.text)).toEqual(["previous task"]);
  });
});

describe("reconnect reports what actually happened", () => {
  it("resolves only once the new connection is established", async () => {
    const transport = new LaunchTransport();
    const daemon = renderDaemon(transport);
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));

    let settled = false;
    await act(async () => {
      void daemon.result.current.reconnect().then(() => {
        settled = true;
      });
    });
    await waitFor(() => expect(settled).toBe(true));
    expect(daemon.result.current.state.status).toBe("connected");
  });

  it("does not leave an unhandled rejection when a caller ignores the outcome", async () => {
    // The disconnected banner's Retry discards the promise: the failure is
    // already on screen, since a failed attempt dispatches `connect-failed`,
    // which is what that banner renders. Discarding it must not surface as an
    // unhandled rejection, which would reach global error reporting.
    const transport = new LaunchTransport();
    const daemon = renderDaemon(transport);
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));

    const unhandled: unknown[] = [];
    const onUnhandled = (event: PromiseRejectionEvent) => {
      unhandled.push(event.reason);
      event.preventDefault();
    };
    window.addEventListener("unhandledrejection", onUnhandled);
    try {
      transport.failNextConnect = true;
      await act(async () => {
        // Exactly what `ConnectionBanner`'s Retry does.
        void daemon.result.current.reconnect().catch(() => undefined);
      });
      await waitFor(() => expect(daemon.result.current.state.status).not.toBe("connected"));
      await act(async () => {
        await Promise.resolve();
      });
      expect(unhandled).toEqual([]);
    } finally {
      window.removeEventListener("unhandledrejection", onUnhandled);
    }
  });

  it("rejects with the daemon's own detail when the attempt fails", async () => {
    // The bug this replaces: `reconnect()` bumped a counter and resolved at
    // once, so a caller announced success before the handshake had started —
    // and said the same thing when it failed.
    const transport = new LaunchTransport();
    const daemon = renderDaemon(transport);
    await waitFor(() => expect(daemon.result.current.state.status).toBe("connected"));

    transport.failNextConnect = true;
    let failure: Error | null = null;
    await act(async () => {
      void daemon.result.current.reconnect().catch((error: Error) => {
        failure = error;
      });
    });
    await waitFor(() => expect(failure).not.toBeNull());
    expect(String(failure)).toContain("connection refused");
  });
});
