import { useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { Transcript } from "../../src/components/Transcript.js";
import { parsePatchReview, REVIEW_PAGE_LINES } from "../../src/patchReview.js";
import type { TranscriptItem } from "../../src/types.js";
import "../../src/theme.css";

// A bounded synthetic display workload. No provider, daemon, or editor is contacted.
const FILES = 10, HUNKS_PER_FILE = 2, LINES_PER_HUNK = 500;
const source = Array.from({ length: FILES }, (_, file) => [
  `diff --git a/src/file-${file}.ts b/src/file-${file}.ts`,
  `--- a/src/file-${file}.ts`, `+++ b/src/file-${file}.ts`,
  ...Array.from({ length: HUNKS_PER_FILE }, (_, hunk) => [
    `@@ -${hunk * 20 + 1},0 +${hunk * (LINES_PER_HUNK + 20) + 1},${LINES_PER_HUNK} @@`,
    ...Array.from({ length: LINES_PER_HUNK }, (_, line) => `+export const file_${file}_hunk_${hunk}_line_${line} = ${line};`),
  ].join("\n")),
].join("\n")).join("\n") + "\n";
const items: TranscriptItem[] = [{ id: "large-review-benchmark", type: "system", timestamp: "",
  text: "Synthetic 10,000-line patch", diffPreview: source,
  patchFiles: Array.from({ length: FILES }, (_, index) => `src/file-${index}.ts`),
}];
const noEditorAction = async () => {};
const METRICS = ["parse", "closedMount", "firstExpansion", "nextPage", "nextHunk", "lastFile"] as const;
type Metric = typeof METRICS[number];
type Sample = Record<Metric, number>;
const rounded = (value: number) => Math.round(value * 100) / 100;
const nextFrame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

function Benchmark() {
  const host = useRef<HTMLDivElement>(null);
  const runningRef = useRef(false);
  const [running, setRunning] = useState(false);
  const [report, setReport] = useState<unknown>(null);
  const [summary, setSummary] = useState<Array<{ metric: string; median: number; max: number; commit?: number; layout?: number }>>([]);
  const run = async () => {
    if (runningRef.current || !host.current) return;
    runningRef.current = true; setRunning(true); setReport(null); setSummary([]);
    const container = host.current;
    const fixture = createRoot(container);
    const samples: Sample[] = [];
    const stages: Array<Array<{ commit: number; layout: number }>> = [];
    let currentStages: Array<{ commit: number; layout: number }> = [];
    const rowCounts = new Set<number>();
    const firstLines: Record<string, string> = {};
    let firstCycle: Sample | undefined;
    let closedReviewSubtrees = -1;
    let maximumTables = 0;
    try {
      const measure = (operation: () => void) => {
        const start = performance.now();
        flushSync(operation);
        const committed = performance.now();
        // Include synchronous style/layout through the rendered table. This
        // measures DOM commit + layout, not paint, GPU work, IPC, or native startup.
        (container.querySelector("table") ?? container).getBoundingClientRect();
        const finished = performance.now();
        currentStages.push({ commit: committed - start, layout: finished - committed });
        return finished - start;
      };
      const checkPage = (name: string) => {
        const rows = container.querySelectorAll(".patch-review-table tbody tr");
        rowCounts.add(rows.length);
        maximumTables = Math.max(maximumTables, container.querySelectorAll("table").length);
        firstLines[name] = rows[0]?.querySelector("code")?.textContent ?? "";
        if (rows.length !== REVIEW_PAGE_LINES || container.querySelectorAll("table").length !== 1) {
          throw new Error(`Expected one selected table with ${REVIEW_PAGE_LINES} rows at ${name}`);
        }
      };
      const click = (selector: string) => {
        const button = container.querySelector<HTMLButtonElement>(selector);
        if (!button || button.disabled) throw new Error(`Missing active control: ${selector}`);
        button.click();
      };
      // One initial cycle, two warm-up cycles, then exactly seven measured cycles.
      for (let iteration = 0; iteration < 10; iteration++) {
        await nextFrame();
        currentStages = [];
        const started = performance.now();
        const parsed = parsePatchReview(source);
        const parse = performance.now() - started;
        if (parsed.notice || parsed.files.length !== FILES || parsed.files.flatMap((file) => file.hunks).length !== FILES * HUNKS_PER_FILE) {
          throw new Error("The complete synthetic patch did not parse");
        }
        const closedMount = measure(() => fixture.render(<Transcript items={items} connectionStatus="connected" onOpenFile={noEditorAction} />));
        closedReviewSubtrees = container.querySelectorAll(".patch-review").length;
        if (closedReviewSubtrees !== 0 || container.querySelector("table")) throw new Error("Closed patch mounted its review subtree");
        const firstExpansion = measure(() => {
          const fold = container.querySelector<HTMLDetailsElement>("details");
          if (!fold) throw new Error("Patch fold missing");
          fold.open = true;
          fold.dispatchEvent(new Event("toggle"));
        });
        checkPage("firstExpansion");
        const nextPage = measure(() => {
          const button = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find((item) => item.textContent === "Next lines");
          if (!button) throw new Error("Next lines control missing");
          button.click();
        });
        checkPage("nextPage");
        const nextHunk = measure(() => click('[aria-label="Next change"]'));
        checkPage("nextHunk");
        const lastFile = measure(() => click('.patch-review-files button:last-child'));
        checkPage("lastFile");
        const sample = { parse, closedMount, firstExpansion, nextPage, nextHunk, lastFile };
        if (iteration === 0) firstCycle = sample;
        if (iteration >= 3) { samples.push(sample); stages.push(currentStages); }
        flushSync(() => fixture.render(null));
      }
      const timings = Object.fromEntries(METRICS.map((metric, index) => {
        const values = samples.map((sample) => sample[metric]).sort((a, b) => a - b);
        const commit = metric === "parse" ? undefined : rounded(stages.map((stage) => stage[index - 1]!.commit).sort((a, b) => a - b)[3]!);
        const layout = metric === "parse" ? undefined : rounded(stages.map((stage) => stage[index - 1]!.layout).sort((a, b) => a - b)[3]!);
        return [metric, { firstCycleMs: rounded(firstCycle![metric]), medianMs: rounded(values[3]!), minMs: rounded(values[0]!), maxMs: rounded(values[values.length - 1]!), commitMedianMs: commit, layoutMedianMs: layout }];
      }));
      setSummary(Object.entries(timings).map(([metric, values]) => ({ metric, median: values.medianMs, max: values.maxMs, commit: values.commitMedianMs, layout: values.layoutMedianMs })));
      setReport({
        kind: "Synthetic production frontend benchmark; not native latency",
        measuredAt: new Date().toISOString(),
        environment: { userAgent: navigator.userAgent, platform: navigator.platform, hardwareConcurrency: navigator.hardwareConcurrency, viewport: `${innerWidth}x${innerHeight}`, devicePixelRatio },
        workload: { files: FILES, hunks: FILES * HUNKS_PER_FILE, lines: FILES * HUNKS_PER_FILE * LINES_PER_HUNK, characters: source.length, utf8Bytes: new TextEncoder().encode(source).byteLength, initialCycles: 1, warmupCycles: 2, measuredCycles: samples.length },
        timings, bounds: { closedReviewSubtrees, renderedRowCounts: [...rowCounts], maximumTables, firstLines },
        samples: samples.map((sample) => Object.fromEntries(METRICS.map((metric) => [metric, rounded(sample[metric])]))),
        stageSamples: stages.map((stage) => stage.map((values) => ({ commit: rounded(values.commit), layout: rounded(values.layout) }))),
      });
    } catch (error) {
      setReport({ error: error instanceof Error ? error.message : String(error) });
    } finally {
      fixture.unmount(); runningRef.current = false; setRunning(false);
    }
  };
  return <main style={{ padding: 24, fontFamily: "system-ui", color: "var(--cody-text)", background: "var(--cody-canvas)", minHeight: "100vh" }}>
    <h1 style={{ fontSize: 22 }}>Bounded patch review benchmark</h1>
    <p>Production components · 10,000 lines · 10 files · 20 hunks · synthetic display data</p>
    <p>One initial cycle, two warm-up cycles, seven measured cycles. Render timings include synchronous DOM commit and layout, not paint or native latency.</p>
    <button disabled={running} onClick={() => void run()}>{running ? "Measuring…" : "Run bounded benchmark"}</button>
    {summary.length > 0 && <table style={{ marginTop: 24, borderSpacing: "24px 8px", textAlign: "right" }}>
      <caption style={{ textAlign: "left" }}>Production component timings (milliseconds)</caption>
      <thead><tr><th>Operation</th><th>Median</th><th>Maximum</th><th>Commit median</th><th>Layout median</th></tr></thead>
      <tbody>{summary.map((row) => <tr key={row.metric}><th style={{ textAlign: "left" }}>{row.metric}</th><td>{row.median}</td><td>{row.max}</td><td>{row.commit ?? "—"}</td><td>{row.layout ?? "—"}</td></tr>)}</tbody>
    </table>}
    <pre data-testid="benchmark-results" aria-live="polite" style={{ whiteSpace: "pre-wrap", fontSize: 12 }}>{report ? JSON.stringify(report, null, 2) : "No measurements yet."}</pre>
    <div ref={host} style={{ height: 600, overflow: "hidden" }} aria-label="Measured review fixture" />
  </main>;
}
createRoot(document.getElementById("root")!).render(<Benchmark />);
