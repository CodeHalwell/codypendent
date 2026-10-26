import { createRoot } from "react-dom/client";
import { useState } from "react";
import { App } from "../../src/App.js";
import type { DesktopTransport } from "../../src/transport.js";
import { ContinuityTransport } from "../fixtures/ContinuityTransport.js";
import "../../src/theme.css";

// Production app with an isolated deterministic responder; no native app or model calls.
const fixture = new ContinuityTransport();
const animationFrame = window.requestAnimationFrame.bind(window);
const showControls = new URLSearchParams(window.location.search).get("controls") !== "0";
function ContinuityCheck() {
  const [framesHeld, setFramesHeld] = useState(false);
  const [report, setReport] = useState("");
  return <>
    <App makeTransport={() => fixture as unknown as DesktopTransport} />
    {showControls && <aside aria-label="Synthetic acceptance controls" style={{ position: "fixed", left: 12, bottom: 72, zIndex: 100, width: 200, padding: 10, background: "var(--cody-panel)", border: "1px solid var(--cody-border)", borderRadius: 8, fontSize: 11, color: "var(--cody-text)" }}>
      <strong>Isolated conversation test</strong>
      <div style={{ display: "flex", flexWrap: "wrap", gap: 5, marginTop: 6 }}>
        <button onClick={() => {
          for (let index = 0; index < 650; index++) fixture.emit("session-1", { type: "NoteAppended", text: `Older message ${index}` });
        }}>Seed 650 rows</button>
        <button onClick={() => {
          window.requestAnimationFrame = framesHeld ? animationFrame : () => 1;
          setFramesHeld(!framesHeld);
        }}>{framesHeld ? "Resume frames" : "Hold frames"}</button>
        <button onClick={() => { fixture.failNext = true; }}>Refuse next send</button>
        <button onClick={() => fixture.emit("session-1", { type: "NoteAppended", text: "Background output" })}>Append background</button>
        <button onClick={() => {
          const list = document.querySelector<HTMLElement>("[data-transcript-window]")!;
          setReport(JSON.stringify({ window: list.dataset.transcriptWindow, scrollTop: list.scrollTop, scrollHeight: list.scrollHeight, clientHeight: list.clientHeight, distanceFromBottom: list.scrollHeight - list.scrollTop - list.clientHeight, framesHeld }));
        }}>Report scroll</button>
      </div>
      {report && <output id="scroll-report" style={{ display: "block", overflowWrap: "anywhere", marginTop: 6 }}>{report}</output>}
    </aside>}
  </>;
}
document.documentElement.dataset.theme = "dark";
document.body.style.margin = "0";
createRoot(document.getElementById("root")!).render(<ContinuityCheck />);
