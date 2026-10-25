import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "../src/App.js";
import { NO_SHELL_DETAIL } from "../src/useDaemon.js";
import type { ConnectionInfo, DaemonFrame, DesktopTransport, RunHandle, SessionRow } from "../src/transport.js";

import type {
  AnalyticsExportRequest,
  AnalyticsExportResult,
  AnalyticsPage,
  AnalyticsQuery,
  InboxEntry,
  InboxListQuery,
  InboxMutation,
  InboxPage,
} from "@codypendent/protocol";

/**
 * A stand-in for the Tauri shell bridge. It records what the UI actually sent
 * and lets a test push the frames a daemon would emit, so "the transcript only
 * ever shows what the daemon said" is an assertion rather than a claim.
 */
class StubTransport implements DesktopTransport {
  readonly objectives: string[] = [];
  readonly cancelled: string[] = [];
  readonly attached: string[] = [];
  readonly approvals: Array<{ approvalId: string; decision: "approve" | "reject" }> = [];
  readonly inboxMutations: InboxMutation[] = [];
  private frames: ((frame: DaemonFrame) => void) | null = null;

  constructor(
    private readonly options: {
      connect?: () => Promise<ConnectionInfo>;
      sessions?: SessionRow[];
      run?: RunHandle;
      inbox?: InboxEntry[];
      analytics?: AnalyticsPage;
    } = {},
  ) {}

  socketPath(): Promise<string> {
    return Promise.resolve("/tmp/codypendent/daemon.sock");
  }

  connect(onFrame: (frame: DaemonFrame) => void): Promise<ConnectionInfo> {
    this.frames = onFrame;
    if (this.options.connect) {
      return this.options.connect();
    }
    return Promise.resolve({
      socket_path: "/tmp/codypendent/daemon.sock",
      protocol_version: "1.4",
      daemon_version: "0.9.0",
      daemon_instance: "instance-1",
      build_id: "build-1",
    });
  }

  disconnect(): Promise<void> {
    return Promise.resolve();
  }

  listSessions(): Promise<SessionRow[]> {
    return Promise.resolve(this.options.sessions ?? []);
  }

  startObjective(objective: string): Promise<RunHandle> {
    this.objectives.push(objective);
    return Promise.resolve(this.options.run ?? { session_id: "session-1", run_id: "run-1" });
  }

  attachSession(sessionId: string): Promise<void> {
    this.attached.push(sessionId);
    return Promise.resolve();
  }

  cancelRun(runId: string): Promise<void> {
    this.cancelled.push(runId);
    return Promise.resolve();
  }

  resolveApproval(approvalId: string, decision: "approve" | "reject"): Promise<void> {
    this.approvals.push({ approvalId, decision });
    return Promise.resolve();
  }

  listInbox(_query?: InboxListQuery): Promise<InboxPage> {
    return Promise.resolve({
      items: this.options.inbox ?? [],
      next_cursor: null,
    });
  }

  mutateInbox(mutation: InboxMutation): Promise<InboxEntry> {
    this.inboxMutations.push(mutation);
    const existing = this.options.inbox?.find((e) => "entry_id" in mutation && e.id === mutation.entry_id);
    const updated: InboxEntry = existing ?? {
      id: "entry_id" in mutation ? mutation.entry_id : "unknown",
      repository_id: "repo",
      kind: { type: "ApprovalRequest" },
      state: mutation.type === "Acknowledge" ? { type: "Acknowledged" } : { type: "Dismissed" },
      title: "Updated entry",
      deep_link: { type: "Session", session_id: "session-1" },
      source: { dedup_key: "k", identity: { type: "Unknown" } },
      created_at: "2026-08-16T10:00:00Z",
    };
    return Promise.resolve(updated);
  }

  queryAnalytics(_query?: AnalyticsQuery): Promise<AnalyticsPage> {
    return Promise.resolve(this.options.analytics ?? { items: [], next_cursor: null });
  }

  exportAnalytics(request: AnalyticsExportRequest): Promise<AnalyticsExportResult> {
    return Promise.resolve({
      artifact: {
        id: "art-1",
        byte_length: 100,
        media_type: "application/json",
        sensitivity: { type: "Public" },
        sha256: "hash",
      },
      format: request.format,
      generated_at: "2026-08-16T10:00:00Z",
      row_count: 1,
    });
  }

  /** Push a frame the way the shell's channel would. */
  async push(frame: DaemonFrame): Promise<void> {
    await act(async () => {
      this.emit(frame);
    });
  }

  emit(frame: DaemonFrame): void {
    this.frames?.(frame);
  }
}

function event(sequence: number, body: Record<string, unknown>): DaemonFrame {
  return {
    kind: "event",
    session_id: "session-1",
    event: {
      sequence,
      actor: { type: "System" },
      occurred_at: "2026-08-16T10:00:00Z",
      body: body as unknown as import("@codypendent/protocol").EventBody,
    },
  };
}

async function renderWith(transport: StubTransport) {
  const factory = () => transport as DesktopTransport;
  render(<App makeTransport={factory} />);
  // Let the connect promise (and the session listing behind it) settle.
  await act(async () => undefined);
}

describe("desktop client with no daemon transport", () => {
  afterEach(() => vi.useRealTimers());

  it("reports disconnected when the app is not running inside the shell", () => {
    // No stub: the real factory runs, finds no Tauri shell, and returns null.
    render(<App />);

    // The connection state is no longer a line in the sidebar footer: it is a
    // banner across the top of the main pane, where it cannot be overlooked.
    const banner = screen.getByTestId("connection-banner");
    expect(banner.getAttribute("role")).toBe("alert");
    expect(banner.textContent).toContain("Open Codypendent to connect");
    expect(banner.textContent).toContain(NO_SHELL_DETAIL);
    expect(screen.getAllByText(NO_SHELL_DETAIL).length).toBeGreaterThan(0);
    // ...and the sidebar dot still names the state for a screen reader.
    expect(screen.getByLabelText("codypendentd disconnected")).toBeTruthy();
    expect(screen.queryByLabelText("codypendentd connected")).toBeNull();
  });

  it("disables run controls and never fabricates a response over time", () => {
    vi.useFakeTimers();
    render(<App />);

    const objective = screen.getByRole("textbox") as HTMLTextAreaElement;
    const send = screen.getByRole("button", { name: "Send" }) as HTMLButtonElement;
    expect(objective.disabled).toBe(true);
    expect(send.disabled).toBe(true);
    expect(objective.placeholder).toBe("Connect your workspace to start a coding task.");

    fireEvent.change(objective, { target: { value: "do the thing" } });
    fireEvent.click(send);
    act(() => vi.advanceTimersByTime(60_000));

    // Nothing outside the composer's own textarea ever echoed the objective:
    // no transcript entry, no session, no run.
    const echoes = screen
      .queryAllByText(/do the thing/)
      .filter((element) => element.tagName !== "TEXTAREA");
    expect(echoes).toEqual([]);
    expect(screen.getByText("Connect to load your chats.")).toBeTruthy();
    expect(screen.getByText("Your workspace starts here")).toBeTruthy();
  });

  it("reports the daemon's own failure reason when the socket is unreachable", async () => {
    const transport = new StubTransport({
      connect: () => Promise.reject(new Error("No such file or directory (os error 2)")),
    });
    await renderWith(transport);

    expect(screen.getByTestId("connection-banner").textContent).toMatch(
      /No daemon on \/tmp\/codypendent\/daemon\.sock: No such file or directory/,
    );
    expect(
      screen.getAllByText(
        /No daemon on \/tmp\/codypendent\/daemon\.sock: No such file or directory/,
      ).length,
    ).toBeGreaterThan(0);
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
  });
});

describe("desktop client connected to a daemon", () => {
  it("reports connected only after a handshake, listing the daemon it reached", async () => {
    const transport = new StubTransport({
      sessions: [
        {
          session_id: "session-1",
          title: "Earlier session",
          state: "open",
          created_at: "2026-08-16T09:00:00Z",
          updated_at: "2026-08-16T09:30:00Z",
        },
      ],
    });
    await renderWith(transport);

    // A healthy connection is stated by the dot alone — no banner interrupts.
    expect(screen.queryByTestId("connection-banner")).toBeNull();
    const dot = screen.getByLabelText("codypendentd connected");
    expect(dot.getAttribute("title")).toBe(
      "codypendentd 0.9.0 on /tmp/codypendent/daemon.sock",
    );
    expect(screen.getByText("Earlier session")).toBeTruthy();
  });

  it("keeps what was typed WHILE a submission was in flight", async () => {
    // The textarea stays editable during the round trip — `sending` blocks a
    // second submit, not typing — so an unconditional clear on acceptance
    // deleted the next objective somebody had already started writing.
    const transport = new StubTransport();
    let release: (handle: RunHandle) => void = () => undefined;
    transport.startObjective = (objective: string) => {
      transport.objectives.push(objective);
      return new Promise<RunHandle>((resolve) => {
        release = resolve;
      });
    };
    await renderWith(transport);

    const objective = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(objective, { target: { value: "the first objective" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));

    // Still in flight: type the next one.
    fireEvent.change(objective, { target: { value: "the next objective" } });
    await act(async () => {
      release({ session_id: "session-1", run_id: "run-1" });
    });

    expect(transport.objectives).toEqual(["the first objective"]);
    expect(objective.value).toBe("the next objective");
  });

  it("clears the draft when the box still holds what was sent", async () => {
    const transport = new StubTransport();
    await renderWith(transport);

    const objective = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(objective, { target: { value: "ship it" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Send" }));
    });
    expect(objective.value).toBe("");
  });

  it("turns a submitted objective into a real outbound command and shows only daemon output", async () => {
    const transport = new StubTransport();
    await renderWith(transport);

    const objective = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(objective, { target: { value: "refactor the parser" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Send" }));
    });

    // The command really went out...
    expect(transport.objectives).toEqual(["refactor the parser"]);
    // ...and nothing appeared in the transcript that the daemon did not emit.
    expect(screen.queryByText("refactor the parser")).toBeNull();

    await transport.push(
      event(1, { type: "RunStarted", run_id: "run-1", objective: "refactor the parser", mode: { type: "Build" } }),
    );
    expect(screen.getByText("refactor the parser")).toBeTruthy();

    await transport.push(event(2, { type: "ModelStreamDelta", run_id: "run-1", text: "Reading " }));
    await transport.push(event(3, { type: "ModelStreamDelta", run_id: "run-1", text: "the parser." }));
    expect(screen.getByText("Reading the parser.")).toBeTruthy();
  });

  it("sends a real CancelRun for the run the daemon named", async () => {
    const transport = new StubTransport();
    await renderWith(transport);

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "build it" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Send" }));
    });
    await transport.push(event(1, { type: "RunStarted", run_id: "run-1", objective: "build it" }));

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Cancel Run" }));
    });
    // The click asks; it does not cancel. Nothing reaches the daemon until the
    // confirmation is answered, and the confirmation shows what is at stake.
    expect(transport.cancelled).toEqual([]);
    expect(screen.getByTestId("cancel-confirm-objective").textContent).toContain("build it");

    await act(async () => {
      fireEvent.click(screen.getByTestId("cancel-confirm-yes"));
    });
    expect(transport.cancelled).toEqual(["run-1"]);
  });

  it("shows cancellation metadata when RunStarted precedes a new session's launch reply", async () => {
    const transport = new StubTransport();
    await renderWith(transport);
    await transport.push({ ...event(1, { type: "RunStarted", run_id: "old-run", objective: "old task" }), session_id: "old-session" } as DaemonFrame);
    await transport.push({ ...event(2, { type: "RunStateChanged", run_id: "old-run", state: { type: "Completed" } }), session_id: "old-session" } as DaemonFrame);
    transport.startObjective = async () => {
      transport.emit(event(1, { type: "RunStarted", run_id: "run-1", objective: "run the acceptance tests" }));
      return { session_id: "session-1", run_id: "run-1" };
    };
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "run the acceptance tests" } });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Send" })));
    fireEvent.click(screen.getByRole("button", { name: "Cancel Run" }));
    expect(screen.getByTestId("cancel-confirm-objective").textContent).toBe("run the acceptance tests");
    expect(screen.getByTestId("cancel-confirm-elapsed").textContent).not.toContain("Unknown");
  });

  it("resolves approval cards through the daemon transport", async () => {
    const transport = new StubTransport();
    await renderWith(transport);

    await transport.push(
      event(1, {
        type: "ApprovalRequested",
        approval_id: "approval-1",
        action: { type: "ExecuteCommand", command: "cargo test" },
      }),
    );

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    });
    expect(transport.approvals).toEqual([{ approvalId: "approval-1", decision: "approve" }]);

    await transport.push(
      event(2, {
        type: "ApprovalRequested",
        approval_id: "approval-2",
        action: { type: "WriteFile", path: "src/lib.rs" },
      }),
    );
    const rejectButtons = screen.getAllByRole("button", { name: "Reject" });
    await act(async () => {
      fireEvent.click(rejectButtons[rejectButtons.length - 1]);
    });
    expect(transport.approvals.at(-1)).toEqual({ approvalId: "approval-2", decision: "reject" });

    await transport.push(
      event(3, {
        type: "ApprovalResolved",
        approval_id: "approval-1",
        decision: { type: "Approve" },
      }),
    );
    expect(screen.getAllByText("Approval Required")).toHaveLength(1);
  });

  it("renders compact snapshot state and restores authoritative paged history", async () => {
    const transport = new StubTransport();
    await renderWith(transport);

    await transport.push({
      kind: "catchup",
      session_id: "session-1",
      snapshot: {
        type: "Snapshot",
        through: 3,
        projection: {
          session_id: "session-1",
          title: "Long session",
          last_sequence: 3,
          active_runs: ["run-1"],
          pending_approvals: [
            {
              approval_id: "approval-snapshot",
              run_id: "run-1",
              action: { type: "ExecuteCommand", program: "npm", args: ["test"], environment: [["NODE_OPTIONS", "--require=/tmp/loader.js"]], cwd: "/tmp/recovered-repository" },
              risk: { level: { type: "Medium" }, reasons: [] },
            },
          ],
          pending_prompts: [],
          closed: false,
        },
      },
    });

    expect(screen.getByText("Run a command")).toBeTruthy();
    expect(screen.getByText("npm test")).toBeTruthy();
    expect(screen.getByLabelText("Proposed action details").textContent).toContain("--require=/tmp/loader.js");
    expect(screen.getByLabelText("Proposed action details").textContent).toContain("/tmp/recovered-repository");
    expect(screen.getByRole("button", { name: "Cancel Run" })).toBeTruthy();

    // A live event may outrun the auxiliary history read. The history frame
    // must merge by sequence and rebuild in order rather than duplicate it.
    await transport.push(event(3, { type: "ModelStreamDelta", run_id: "run-1", text: "the parser." }));
    await transport.push({
      kind: "history",
      session_id: "session-1",
      through: 3,
      events: [
        {
          sequence: 1,
          actor: { type: "System" },
          occurred_at: "2026-08-16T10:00:00Z",
          body: { type: "RunStarted", run_id: "run-1", objective: "refactor the parser", mode: { type: "Build" } },
        },
        {
          sequence: 2,
          actor: { type: "System" },
          occurred_at: "2026-08-16T10:00:01Z",
          body: { type: "ModelStreamDelta", run_id: "run-1", text: "Reading " },
        },
        {
          sequence: 3,
          actor: { type: "System" },
          occurred_at: "2026-08-16T10:00:02Z",
          body: { type: "ModelStreamDelta", run_id: "run-1", text: "the parser." },
        },
      ],
    });

    expect(screen.getByText("refactor the parser")).toBeTruthy();
    expect(screen.getByText("Reading the parser.")).toBeTruthy();
    expect(screen.getAllByText("Reading the parser.")).toHaveLength(1);
  });

  it("falls back to disconnected when the socket drops mid-run", async () => {
    const transport = new StubTransport();
    await renderWith(transport);
    expect(screen.queryByTestId("connection-banner")).toBeNull();

    await transport.push({ kind: "disconnected", reason: "the daemon closed the connection" });

    const banner = screen.getByTestId("connection-banner");
    expect(banner.getAttribute("role")).toBe("alert");
    expect(banner.textContent).toContain("the daemon closed the connection");
    expect(screen.getAllByText("the daemon closed the connection").length).toBeGreaterThan(0);
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("reports a rejected command without inventing agent output", async () => {
    const transport = new StubTransport();
    transport.startObjective = () => Promise.reject(new Error("StartRun rejected: policy denied"));
    await renderWith(transport);

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "rm -rf /" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Send" }));
    });

    expect(screen.getByRole("alert").textContent).toContain("StartRun rejected: policy denied");
    expect(screen.getByRole("heading", { name: "Your workspace. Your agents." })).toBeTruthy();
  });

  it("switches views between sessions, inbox, and analytics", async () => {
    const sampleInbox: InboxEntry[] = [
      {
        id: "inbox-1",
        repository_id: "repo-1",
        kind: { type: "ApprovalRequest" },
        state: { type: "Unread" },
        title: "Durable Inbox Test Entry",
        summary: "Need approval",
        deep_link: { type: "Approval", approval_id: "app-1" },
        source: { dedup_key: "k1", identity: { type: "Approval", approval_id: "app-1" } },
        created_at: "2026-08-16T10:00:00Z",
      },
    ];

    const transport = new StubTransport({ inbox: sampleInbox });
    await renderWith(transport);

    // Initial view is Sessions
    expect(screen.getByRole("textbox")).toBeDefined();

    // Switch to Inbox View
    const inboxTab = screen.getByRole("button", { name: "Inbox View" });
    await act(async () => {
      fireEvent.click(inboxTab);
    });
    expect(screen.getByRole("heading", { name: "Inbox" })).toBeDefined();
    expect(screen.getByText("Durable Inbox Test Entry")).toBeDefined();

    // Switch to Analytics View
    fireEvent.click(screen.getByRole("button", { name: "Run group" }));
    const analyticsTab = screen.getByRole("button", { name: "Usage View" });
    await act(async () => {
      fireEvent.click(analyticsTab);
    });
    expect(await screen.findByRole("heading", { name: "Usage" })).toBeDefined();

    // Switch back to Sessions View
    const sessionsTab = screen.getByRole("button", { name: "Sessions View" });
    await act(async () => {
      fireEvent.click(sessionsTab);
    });
    expect(screen.getByRole("textbox")).toBeDefined();
  });
});
