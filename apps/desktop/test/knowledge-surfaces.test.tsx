import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../src/App.js";
import { filterPalette, type PaletteEntry } from "../src/components/CommandPalette.js";
import { SkillsView } from "../src/components/SkillsView.js";
import { PluginsView, NO_PENDING_UPDATE } from "../src/components/PluginsView.js";
import { ContextView, latestContextUsage } from "../src/components/ContextView.js";
import {
  publishSlug,
  validPublishBranch,
  validPublishPath,
} from "../src/components/DocsView.js";
import type { Loaded, SkillCard, UiPluginLifecycleStatus } from "../src/components/knowledgeTransport.js";
import type { SessionEvent } from "@codypendent/protocol";

function loaded<T>(items: T[]): Loaded<T> {
  return { items, status: "loaded", detail: null };
}

describe("command palette ranking", () => {
  /**
   * The port of `palette_match_score` has to preserve the case the Rust
   * comment calls out: typing `mode` must put the mode row first even though
   * `model` also contains those four characters.
   */
  const entries: PaletteEntry[] = [
    {
      id: "model",
      title: "/model  Model picker",
      description: "choose the model pinned to your next and later runs",
      key: "—",
      group: "Models",
    },
    {
      id: "mode",
      title: "/mode  Mode picker",
      description: "choose the submission mode for the next run",
      key: "—",
      group: "Models",
    },
    {
      id: "memory",
      title: "/memory  Memory",
      description: "browse curated memories and their provenance",
      key: "M",
      group: "Workspace",
    },
  ];

  it("prefers an exact word match over a substring", () => {
    expect(filterPalette(entries, "mode").map((entry) => entry.id)).toEqual(["mode", "model"]);
  });

  it("matches on the key and on the description, and returns nothing for a miss", () => {
    expect(filterPalette(entries, "M").map((entry) => entry.id)).toEqual([
      "memory",
      "model",
      "mode",
    ]);
    expect(filterPalette(entries, "provenance").map((entry) => entry.id)).toEqual(["memory"]);
    expect(filterPalette(entries, "kubernetes")).toEqual([]);
  });

  it("preserves table order when the query is empty", () => {
    expect(filterPalette(entries, "  ").map((entry) => entry.id)).toEqual([
      "model",
      "mode",
      "memory",
    ]);
  });

  it("runs nothing when Enter is pressed against a zero-match filter", async () => {
    render(<App initialView="skills" />);
    fireEvent.keyDown(window, { key: "k", ctrlKey: true });

    const filter = screen.getByLabelText("Command palette filter");
    fireEvent.change(filter, { target: { value: "kubernetes" } });
    fireEvent.keyDown(filter, { key: "Enter" });

    // The palette is still open on its own no-match state; nothing navigated.
    expect(screen.getByText(/No command matches/)).toBeTruthy();
  });

  it("navigates to a mounted view and only to a mounted view", () => {
    render(<App initialView="sessions" />);
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    // Palette rows are listbox options now (aria-activedescendant follows
    // the keyboard highlight), so the accessible role is option, not button.
    fireEvent.click(screen.getByRole("option", { name: /Skills/ }));
    expect(screen.getByRole("heading", { name: "Skills" })).toBeTruthy();
  });
});

describe("a surface with no bridge command", () => {
  it("says which command is missing instead of drawing an empty list", () => {
    render(<App initialView="skills" />);
    const status = screen.getAllByRole("status").map((node) => node.textContent ?? "");
    expect(status.some((text) => text.includes("list_skills"))).toBe(true);
    // Never the empty state: that would assert there are no skills.
    expect(screen.queryByText("No skills are registered in this workspace.")).toBeNull();
  });

  it("offers no refresh affordance it could not honour", () => {
    render(<App initialView="plugins" />);
    expect(screen.queryByRole("button", { name: "Refresh" })).toBeNull();
    expect(
      screen.getAllByRole("status").some((node) => node.textContent?.includes("list_ui_plugins")),
    ).toBe(true);
  });
});

describe("Skill Studio", () => {
  const skill: SkillCard = {
    name: "cargo-test",
    kind: "tool",
    scope: "workspace codypendent",
    trust: "first-party",
    status: "active",
    risk: "medium",
    description: "runs the workspace test suite",
    permissions: ["filesystem_read: $REPOSITORY", "command: cargo"],
  };

  it("renders every requested permission verbatim", () => {
    render(<SkillsView skills={loaded([skill])} />);
    expect(screen.getByText("filesystem_read: $REPOSITORY")).toBeTruthy();
    expect(screen.getByText("command: cargo")).toBeTruthy();
  });

  it("distinguishes read-and-empty from never-read", () => {
    const { unmount } = render(
      <SkillsView skills={{ items: [], status: "loaded", detail: null }} />,
    );
    expect(screen.getByText("No skills are registered in this workspace.")).toBeTruthy();
    unmount();

    render(
      <SkillsView
        skills={{ items: [], status: "unavailable", detail: "the daemon refused: knowledge.locked" }}
      />,
    );
    expect(screen.getByText("the daemon refused: knowledge.locked")).toBeTruthy();
    expect(screen.queryByText("No skills are registered in this workspace.")).toBeNull();
  });
});

describe("UI plugin trust decisions", () => {
  const pending: UiPluginLifecycleStatus = {
    id: "acme-dashboard",
    version: "2.1.0",
    state: "installed",
    enabledScope: null,
    updateApprovalReceipt: "receipt-7f3a",
    updatePermissionDiff: "+ network: api.acme.example\n+ filesystem_read: $REPOSITORY",
  };
  const clean: UiPluginLifecycleStatus = {
    id: "quiet-plugin",
    version: "1.0.0",
    state: "enabled",
    enabledScope: "user",
    updateApprovalReceipt: null,
    updatePermissionDiff: null,
  };

  it("never one-click-approves: the confirm carries the exact receipt and diff", () => {
    const onApprove = vi.fn();
    render(<PluginsView plugins={loaded([pending])} onApprove={onApprove} />);

    fireEvent.click(screen.getByRole("button", { name: "Approve update…" }));
    expect(onApprove).not.toHaveBeenCalled();

    const dialog = screen.getByRole("dialog");
    expect(dialog.textContent).toContain("receipt-7f3a");
    expect(dialog.textContent).toContain("+ network: api.acme.example");

    fireEvent.click(screen.getByRole("button", { name: "Approve update" }));
    expect(onApprove).toHaveBeenCalledWith("acme-dashboard", "receipt-7f3a");
  });

  it("refuses to approve when there is no pending update rather than inventing a receipt", () => {
    const onApprove = vi.fn();
    render(<PluginsView plugins={loaded([clean])} onApprove={onApprove} />);

    fireEvent.click(screen.getByRole("button", { name: "Approve update…" }));
    expect(onApprove).not.toHaveBeenCalled();
    expect(screen.getByRole("alert").textContent).toContain(NO_PENDING_UPDATE);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("treats enabling as a trust transition and states what it grants", () => {
    const onEnable = vi.fn();
    render(<PluginsView plugins={loaded([clean])} onEnable={onEnable} />);

    fireEvent.click(screen.getByRole("button", { name: "Enable…" }));
    expect(onEnable).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog").textContent).toContain(
      "Enabling grants the permissions declared by the verified installed package.",
    );

    fireEvent.click(screen.getByRole("button", { name: "Enable" }));
    expect(onEnable).toHaveBeenCalledWith("quiet-plugin", "session");
  });

  it("confirms a revocation before tearing anything down", () => {
    const onRevoke = vi.fn();
    render(<PluginsView plugins={loaded([clean])} onRevoke={onRevoke} />);

    fireEvent.click(screen.getByRole("button", { name: "Revoke…" }));
    expect(onRevoke).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Revoke" }));
    expect(onRevoke).toHaveBeenCalledWith("quiet-plugin");
  });
});

describe("publish validators", () => {
  it("refuses the paths the TUI refuses", () => {
    expect(validPublishPath("docs/report.md")).toBe(true);
    expect(validPublishPath("docs/nested/report.MD")).toBe(true);
    expect(validPublishPath("")).toBe(false);
    expect(validPublishPath("/etc/passwd.md")).toBe(false);
    expect(validPublishPath("../secrets.md")).toBe(false);
    expect(validPublishPath("docs/../../secrets.md")).toBe(false);
    expect(validPublishPath("docs/report.txt")).toBe(false);
    expect(validPublishPath("docs/")).toBe(false);
    // A control character never reaches the daemon's publish engine.
    expect(validPublishPath("docs/report\u007f.md")).toBe(false);
  });

  it("refuses the branch names the TUI refuses", () => {
    expect(validPublishBranch("docs/payments-retry_guide.v2")).toBe(true);
    expect(validPublishBranch("")).toBe(false);
    expect(validPublishBranch("-delete-everything")).toBe(false);
    expect(validPublishBranch("/leading")).toBe(false);
    expect(validPublishBranch("trailing/")).toBe(false);
    expect(validPublishBranch("docs/../main")).toBe(false);
    expect(validPublishBranch("docs//main")).toBe(false);
    expect(validPublishBranch("docs/branch.lock")).toBe(false);
    expect(validPublishBranch("docs/branch;rm -rf /")).toBe(false);
    expect(validPublishBranch("docs/branch\u007f")).toBe(false);
  });

  it("slugs a title the way the reducer does", () => {
    expect(publishSlug("Payments Retry Guide")).toBe("payments-retry-guide");
    expect(publishSlug("docs/release-notes")).toBe("docs-release-notes");
    expect(publishSlug("!!!")).toBe("document");
  });
});

describe("context usage", () => {
  function usage(sequence: number, runId: string, used: number): SessionEvent {
    return {
      sequence,
      actor: { type: "System" },
      occurred_at: "2026-08-16T10:00:00Z",
      body: {
        type: "ContextUsage",
        run_id: runId,
        used_tokens: used,
        window_tokens: 200_000,
        system_tokens: 1_200,
        tool_tokens: 3_400,
        transcript_tokens: 5_600,
      },
    };
  }

  it("is strictly scoped to the run it reports on", () => {
    const events = [usage(1, "run-other", 90_000), usage(2, "run-1", 40_000)];
    expect(latestContextUsage(events, "run-1")?.used_tokens).toBe(40_000);
    // A report for a run this client has not materialised is not borrowed.
    expect(latestContextUsage([usage(1, "run-other", 90_000)], "run-1")).toBeNull();
    expect(latestContextUsage(events, null)).toBeNull();
  });

  it("leaves an unmeasured breakdown absent rather than rendering zeros", () => {
    render(<ContextView events={[]} activeRunId="run-1" />);
    expect(screen.getByText("Detailed breakdown not yet available from provider.")).toBeTruthy();
    expect(screen.queryByText(/0 tokens/)).toBeNull();
  });

  it("says there is no run rather than showing an empty breakdown", () => {
    render(<ContextView events={[]} activeRunId={null} />);
    expect(screen.getByText("No active run in current session.")).toBeTruthy();
  });

  it("renders the measured distribution when the provider reported one", () => {
    render(<ContextView events={[usage(1, "run-1", 40_000)]} activeRunId="run-1" />);
    expect(screen.getByText(/20% \(40,000\/200,000 tokens\)/)).toBeTruthy();
    expect(screen.getByText("1,200 tokens")).toBeTruthy();
  });
});
