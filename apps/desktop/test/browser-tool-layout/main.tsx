// Real browser geometry regression: production transcript, synthetic events only.
import { useState } from "react";
import { createRoot } from "react-dom/client";
import { Transcript } from "../../src/components/Transcript.js";
import type { TranscriptItem } from "../../src/types.js";
import "../../src/theme.css";

const tool = (index: number): TranscriptItem => {
  const reading = index % 3 === 0;
  const label = reading ? `src/file-${index}.rs` : `rg --files src (step ${index})`;
  return { id: `tool-${index}`, type: "tool_call", timestamp: "", toolName: `acp:call-${index}`,
    text: label, toolLabel: label, toolKind: reading ? "Read File" : "Terminal", status: "running",
    toolArgs: reading ? { path: label } : { command: "rg --files src", cwd: "/synthetic/repository", limit: 100 } };
};
const fixture = (count: number, completed = false): TranscriptItem[] => [
  { id: "user", type: "user", timestamp: "", text: "Can you look through the project for me" },
  { id: "backstage", type: "backstage", timestamp: "", text: "", contextLines: 134, raw: ["Synthetic context manifest"] },
  ...Array.from({ length: count }, (_, index) => ({ ...tool(index), ...(completed ? { status: index === 5 ? "error" as const : "success" as const } : {}) })),
  { id: "approval", type: "approval", timestamp: "", text: "Synthetic command approval", approvalId: "layout",
    approvalPattern: "wc *", approvalAction: { type: "ExecuteCommand", program: "wc", args: ["-l", "src/example.rs"], cwd: null, environment: [] } },
  { id: "question", type: "question", timestamp: "", text: "Which source should be reviewed?", questionId: "layout-question",
    questionPrompts: [{ header: "Source", question: "Which source should be reviewed?", options: [{ label: "src" }, { label: "tests" }], multiple: false, custom: true }] },
  { id: "thought", type: "thought", timestamp: "", text: "Inspect the project layout, review the source and report the findings.\nKeep each step visible." },
];

function ToolLayoutCheck() {
  const [items, setItems] = useState(() => fixture(50, true));
  const [focus, setFocus] = useState<string | null>(null);
  const [report, setReport] = useState<unknown>(null);
  const measure = () => {
    const list = document.querySelector<HTMLDivElement>("[data-transcript-window]")!;
    const rows = Array.from(list.children, (row) => {
      const element = row as HTMLElement;
      const head = element.querySelector<HTMLElement>("[data-tool-header]");
      return { title: (head?.firstElementChild?.textContent ?? element.textContent ?? "").slice(0, 100),
        height: element.getBoundingClientRect().height, scrollHeight: element.scrollHeight,
        flexShrink: getComputedStyle(element).flexShrink,
        ...(head ? { headerHeight: head.getBoundingClientRect().height, headerFits: head.getBoundingClientRect().bottom <= element.getBoundingClientRect().bottom } : {}) };
    });
    const tools = rows.filter((row) => "headerFits" in row);
    setReport({ at: new Date().toISOString(), viewport: [innerWidth, innerHeight], transcriptWindow: list.dataset.transcriptWindow,
      scrollHeight: list.scrollHeight, clientHeight: list.clientHeight, scrollTop: list.scrollTop, tools: tools.length,
      allRowsKeepNaturalHeight: rows.every((row) => row.flexShrink === "0"),
      allToolHeadersFit: tools.every((row) => row.headerFits),
      allToolBodiesFit: tools.every((row) => row.height + 2 >= row.scrollHeight),
      scrolls: list.scrollHeight > list.clientHeight, rows });
  };
  return <main style={{ height: "100dvh", display: "flex", flexDirection: "column", background: "var(--cody-bg)", color: "var(--cody-text)" }}>
    <header style={{ padding: 12, flexShrink: 0, borderBottom: "1px solid var(--cody-border)" }}>
      <strong>Tool-heavy transcript · production component · synthetic data</strong>
      <div style={{ display: "flex", gap: 8, marginTop: 8, flexWrap: "wrap" }}>
        <button onClick={() => { setFocus(null); setItems(fixture(22)); }}>Show 22 tools</button>
        <button onClick={() => { setFocus(null); setItems(fixture(50, true)); }}>Show 50 completed tools</button>
        <button onClick={() => { setFocus(null); setItems(fixture(225)); }}>Show 225 tools</button>
        <button onClick={() => setItems((current) => [...current, tool(current.length)])}>Append live tool</button>
        <button onClick={() => setFocus("Approval:layout")}>Focus approval</button>
        <button onClick={measure}>Report layout</button>
      </div>
    </header>
    <Transcript items={items} connectionStatus="connected" focusDecision={focus} activity={{ kind: "tool", tool: "Terminal" }}
      onApprove={async (_id, scope = "Once") => { setReport({ acceptedScope: scope }); }} onResolveQuestion={async () => {}} />
    <footer style={{ padding: 12, flexShrink: 0, borderTop: "1px solid var(--cody-border)" }}>
      <textarea aria-label="Preserved composer draft" placeholder="Draft stays outside the scrolling transcript" style={{ width: "100%", minHeight: 50, boxSizing: "border-box" }} />
      {report !== null && <details><summary>Layout report</summary><pre id="tool-layout-report" style={{ maxHeight: 150, overflow: "auto" }}>{JSON.stringify(report, null, 2)}</pre></details>}
    </footer>
  </main>;
}
document.documentElement.dataset.theme = "dark";
document.body.style.margin = "0";
createRoot(document.getElementById("root")!).render(<ToolLayoutCheck />);
