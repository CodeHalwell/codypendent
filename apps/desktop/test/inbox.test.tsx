import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { InboxView } from "../src/components/InboxView.js";
import type { InboxEntry } from "@codypendent/protocol";

describe("InboxView component", () => {
  const sampleEntries: InboxEntry[] = [
    {
      id: "inbox-1",
      repository_id: "repo-primary",
      kind: { type: "ApprovalRequest" },
      state: { type: "Unread" },
      title: "Write patch to fix auth leak",
      summary: "Agent requests permission to apply a diff to auth.rs",
      deep_link: { type: "Approval", approval_id: "app-123" },
      source: {
        dedup_key: "approval:app-123",
        identity: { type: "Approval", approval_id: "app-123" },
      },
      created_at: "2026-08-16T12:00:00Z",
    },
    {
      id: "inbox-2",
      repository_id: "repo-primary",
      kind: { type: "RunCompleted" },
      state: { type: "Acknowledged" },
      title: "Run completed successfully",
      summary: "All 14 unit tests passed",
      deep_link: { type: "Run", run_id: "run-456", session_id: "session-789" },
      source: {
        dedup_key: "run:run-456:terminal",
        identity: { type: "Run", run_id: "run-456" },
      },
      created_at: "2026-08-16T11:00:00Z",
    },
    {
      id: "inbox-3",
      repository_id: "repo-secondary",
      kind: { type: "BudgetWarning" },
      state: { type: "Dismissed" },
      title: "Monthly budget threshold 80% reached",
      summary: "Spend is $80.20 of $100.00",
      deep_link: { type: "Repository", repository_id: "repo-secondary" },
      source: {
        dedup_key: "budget:b-1:2026-08-01",
        identity: { type: "Budget", budget_id: "b-1" },
      },
      created_at: "2026-08-16T10:00:00Z",
    },
  ];

  it("renders unread entries by default and supports filtering", () => {
    const onAcknowledge = vi.fn();
    const onDismiss = vi.fn();
    const onNavigate = vi.fn();

    render(
      <InboxView
        entries={sampleEntries}
        onAcknowledge={onAcknowledge}
        onDismiss={onDismiss}
        onNavigate={onNavigate}
      />,
    );

    // Unread filter is selected by default, so only inbox-1 is shown
    expect(screen.getByText("Write patch to fix auth leak")).toBeDefined();
    expect(screen.queryByText("Run completed successfully")).toBeNull();

    // Switch filter to "All"
    fireEvent.click(screen.getByRole("button", { name: "All" }));
    expect(screen.getByText("Write patch to fix auth leak")).toBeDefined();
    expect(screen.getByText("Run completed successfully")).toBeDefined();
    expect(screen.getByText("Monthly budget threshold 80% reached")).toBeDefined();

    // Switch filter to "Acknowledged"
    fireEvent.click(screen.getByRole("button", { name: "Acknowledged" }));
    expect(screen.queryByText("Write patch to fix auth leak")).toBeNull();
    expect(screen.getByText("Run completed successfully")).toBeDefined();
  });

  it("handles acknowledge, dismiss, and navigate quick actions", async () => {
    const onAcknowledge = vi.fn();
    const onDismiss = vi.fn();
    const onNavigate = vi.fn();

    render(
      <InboxView
        entries={sampleEntries}
        onAcknowledge={onAcknowledge}
        onDismiss={onDismiss}
        onNavigate={onNavigate}
      />,
    );

    // Acknowledge button
    const ackBtn = screen.getByRole("button", { name: "Acknowledge Write patch to fix auth leak" });
    await act(async () => { fireEvent.click(ackBtn); });
    expect(onAcknowledge).toHaveBeenCalledWith("inbox-1");

    // Dismiss button
    const dismissBtn = screen.getByRole("button", { name: "Dismiss Write patch to fix auth leak" });
    await act(async () => { fireEvent.click(dismissBtn); });
    expect(onDismiss).toHaveBeenCalledWith("inbox-1");

    // Open/Navigate button
    const navBtn = screen.getByRole("button", { name: "Review Write patch to fix auth leak" });
    await act(async () => { fireEvent.click(navBtn); });
    expect(onNavigate).toHaveBeenCalledWith({ type: "Approval", approval_id: "app-123" }, sampleEntries[0]);
  });

  it("requires review of the full request instead of deciding from a summary", async () => {
    const onNavigate = vi.fn();
    render(<InboxView entries={sampleEntries} onAcknowledge={vi.fn()} onDismiss={vi.fn()} onNavigate={onNavigate} />);
    expect(screen.queryByRole("button", { name: /^Approve / })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Reject / })).toBeNull();
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Review Write patch to fix auth leak" })); });
    expect(onNavigate).toHaveBeenCalledWith(sampleEntries[0]!.deep_link, sampleEntries[0]);
    expect(screen.getByText(/Acknowledge and Dismiss only organize/)).toBeTruthy();
  });

  it("an unreadable inbox says so instead of claiming nothing is pending", () => {
    // An empty entry list is what "nothing pending" and "never answered" both
    // look like. Only the first of those licenses the empty state.
    render(
      <InboxView
        entries={[]}
        onAcknowledge={vi.fn()}
        onDismiss={vi.fn()}
        onNavigate={vi.fn()}
        unavailable="no daemon answered ListInbox"
      />,
    );

    expect(screen.queryByTestId("inbox-empty")).toBeNull();
    const panel = screen.getByTestId("inbox-unavailable");
    expect(panel.textContent).toContain("Inbox unavailable");
    expect(panel.textContent).toContain("no daemon answered ListInbox");
  });

  it("serializes mutations per entry while allowing different entries to proceed", async () => {
    let finish!: () => void;
    const pending = new Promise<void>((resolve) => { finish = resolve; });
    const acknowledge = vi.fn().mockReturnValue(pending);
    const dismiss = vi.fn().mockResolvedValue(undefined);
    render(<InboxView entries={[sampleEntries[0]!, { ...sampleEntries[0]!, id: "other", title: "Another request" }]}
      onAcknowledge={acknowledge} onDismiss={dismiss} onNavigate={vi.fn()} />);
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Acknowledge Write patch to fix auth leak" }));
      fireEvent.click(screen.getByRole("button", { name: "Acknowledge Write patch to fix auth leak" }));
      fireEvent.click(screen.getByRole("button", { name: "Dismiss Write patch to fix auth leak" }));
    });
    expect(acknowledge).toHaveBeenCalledTimes(1);
    expect(dismiss).not.toHaveBeenCalled();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Dismiss Another request" })));
    expect(dismiss).toHaveBeenCalledExactlyOnceWith("other");
    expect((screen.getByRole("button", { name: "Dismiss Write patch to fix auth leak" }) as HTMLButtonElement).disabled).toBe(true);
    await act(async () => finish());
    expect((screen.getByRole("button", { name: "Dismiss Write patch to fix auth leak" }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("keeps navigation and mutation refusals local and permits retry", async () => {
    const navigate = vi.fn().mockRejectedValueOnce(new Error("Session is unavailable")).mockResolvedValue(undefined);
    const acknowledge = vi.fn().mockImplementationOnce(() => { throw new Error("Cannot acknowledge"); }).mockResolvedValue(undefined);
    render(<InboxView entries={sampleEntries} onAcknowledge={acknowledge} onDismiss={vi.fn()} onNavigate={navigate} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Review Write patch to fix auth leak" })));
    expect(screen.getByRole("alert").textContent).toContain("Session is unavailable");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Review Write patch to fix auth leak" })));
    expect(screen.queryByRole("alert")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Acknowledge Write patch to fix auth leak" }));
    expect(screen.getByRole("alert").textContent).toContain("Cannot acknowledge");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Acknowledge Write patch to fix auth leak" })));
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("retires a disconnected mutation's error and disables unavailable controls", async () => {
    let reject!: (error: Error) => void;
    const pending = new Promise<void>((_, no) => { reject = no; });
    const props = { entries: sampleEntries, onAcknowledge: vi.fn().mockReturnValue(pending), onDismiss: vi.fn(), onNavigate: vi.fn() };
    const { rerender } = render(<InboxView {...props} connected />);
    fireEvent.click(screen.getByRole("button", { name: "Acknowledge Write patch to fix auth leak" }));
    rerender(<InboxView {...props} connected={false} />);
    await act(async () => reject(new Error("Old connection")));
    expect(screen.queryByRole("alert")).toBeNull();
    expect((screen.getByRole("button", { name: "Dismiss Write patch to fix auth leak" }) as HTMLButtonElement).disabled).toBe(true);
    rerender(<InboxView {...props} connected />);
    expect((screen.getByRole("button", { name: "Dismiss Write patch to fix auth leak" }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("an empty inbox that was actually read still shows the empty state", () => {
    render(
      <InboxView
        entries={[]}
        onAcknowledge={vi.fn()}
        onDismiss={vi.fn()}
        onNavigate={vi.fn()}
        unavailable={null}
      />,
    );

    expect(screen.queryByTestId("inbox-unavailable")).toBeNull();
    expect(screen.getByTestId("inbox-empty")).toBeDefined();
  });
});
