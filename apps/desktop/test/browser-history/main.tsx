import { useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import type { SessionEvent } from "@codypendent/protocol";
import { initialState, reduce } from "../../src/daemonState.js";
import { Transcript } from "../../src/components/Transcript.js";
import "../../src/theme.css";

// Production projection and components, with the same note count/format as the
// synthetic workspace fixture. No daemon, provider, native app or editor calls.
const events: SessionEvent[] = Array.from({ length: 25_000 }, (_, index) => ({
  sequence: index + 1, occurred_at: "2026-10-04T20:00:00Z",
  body: { type: "NoteAppended", text: `Synthetic history note ${index}: validating workspace navigation and durable history. ✓` },
} as SessionEvent));
const history = { type: "frame" as const, frame: { kind: "history" as const, session_id: "synthetic-history", through: 25_000, events } };
const startState = () => reduce(initialState, history);
const frame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
const round = (n: number) => Math.round(n * 100) / 100;

function HistoryCheck() {
  const [state, setState] = useState(startState);
  const [focus, setFocus] = useState<string | null>(null);
  const [feedback, setFeedback] = useState("No native actions are performed.");
  const [report, setReport] = useState<unknown>(null);
  const [running, setRunning] = useState(false);
  const host = useRef<HTMLDivElement>(null);
  const measure = async () => {
    setRunning(true);
    const samples: Array<{ replay: number; mountAndLayout: number; rows: number }> = [];
    for (let index = 0; index < 10; index++) {
      await frame();
      const started = performance.now();
      const next = startState();
      const replay = performance.now() - started;
      const mountStarted = performance.now();
      // Changing this benchmark-only key forces a fresh 200-row mount.
      flushSync(() => setState({ ...next, connectionEpoch: index + 1 }));
      host.current!.getBoundingClientRect();
      samples.push({ replay: round(replay), mountAndLayout: round(performance.now() - mountStarted),
        rows: host.current!.querySelector("[data-transcript-window]")!.children.length });
    }
    const measured = samples.slice(3);
    const summary = (key: "replay" | "mountAndLayout") => {
      const sorted = measured.map((sample) => sample[key]).sort((a, b) => a - b);
      return { min: sorted[0], median: sorted[3], max: sorted[6] };
    };
    setReport({ at: new Date().toISOString(), userAgent: navigator.userAgent,
      hardwareConcurrency: navigator.hardwareConcurrency, viewport: [innerWidth, innerHeight],
      events: events.length, transcriptRows: state.transcript.length, mountedRows: [...new Set(samples.map((sample) => sample.rows))],
      method: "Production reducer replay plus fresh React DOM commit and forced layout; excludes IPC, native WebKit, paint and GPU. First cycle + two warmups, then seven measurements.",
      summary: { replay: summary("replay"), mountAndLayout: summary("mountAndLayout") }, samples });
    setRunning(false);
  };
  const append = () => setState((current) => reduce(current, { type: "frame", frame: {
    kind: "event", session_id: "synthetic-history", event: { sequence: current.lastSequence + 1, occurred_at: "2026-10-04T20:00:01Z",
      body: { type: "NoteAppended", text: `Live note ${current.lastSequence + 1}` } } as SessionEvent } }));
  const archived = () => {
    setState((current) => ({ ...current, transcript: [
      { id: "archived-approval", type: "approval", timestamp: "", text: "Synthetic archived command", approvalId: "archived",
        approvalAction: { type: "ExecuteCommand", program: "python3", args: ["-m", "unittest"], cwd: null, environment: [] } },
      { id: "archived-patch", type: "system", timestamp: "", text: "Synthetic archived patch", patchFiles: ["src/example.ts"], patchRunId: "original-run",
        diffPreview: "--- /dev/null\n+++ b/src/example.ts\n@@ -0,0 +1,2 @@\n+const first = 1;\n+const second = 2;\n" },
      ...current.transcript.filter((item) => !item.id.startsWith("archived-")),
    ] }));
    setFocus("Approval:archived");
  };
  return <main style={{ height: "100vh", display: "flex", flexDirection: "column", background: "var(--cody-bg)", color: "var(--cody-text)" }}>
    <header style={{ padding: 12, borderBottom: "1px solid var(--cody-border)" }}>
      <strong>Synthetic 25,000-note history · production components</strong>
      <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginTop: 8 }}>
        <button onClick={() => void measure()} disabled={running}>{running ? "Measuring…" : "Run 7 measurements"}</button>
        <button onClick={append}>Append live note</button>
        <button onClick={archived}>Open archived approval</button>
        <button onClick={() => { setFocus(null); setState(startState()); }}>Reset history</button>
      </div>
      <p role="status" style={{ fontSize: 12, margin: "8px 0 0" }}>{feedback}</p>
      {report !== null && <details><summary>Benchmark results</summary><pre style={{ maxHeight: 250, overflow: "auto" }} id="history-report">{JSON.stringify(report, null, 2)}</pre></details>}
    </header>
    <div ref={host} style={{ display: "flex", flexDirection: "column", flex: 1, minHeight: 0 }}>
      <Transcript key={state.connectionEpoch} items={state.transcript} connectionStatus="connected" focusDecision={focus}
        onApprove={async () => setFeedback("Synthetic approval callback received.")}
        onOpenFile={async (path, line, run) => setFeedback(`Synthetic source callback: ${path}:${line} · ${run}`)} />
    </div>
    <textarea aria-label="Preserved composer draft" spellCheck={false} placeholder="Draft stays outside history paging" style={{ margin: 12, minHeight: 50 }} />
  </main>;
}

document.documentElement.dataset.theme = "dark";
document.body.style.margin = "0";
createRoot(document.getElementById("root")!).render(<HistoryCheck />);
