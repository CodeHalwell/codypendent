// Browser-only production rendering fixture. No daemon, model, or editor is contacted.
import { useState } from "react";
import { createRoot } from "react-dom/client";
import { Transcript } from "../../src/components/Transcript.js";
import type { TranscriptItem } from "../../src/types.js";
import "../../src/theme.css";

const small = `diff --git a/src/words.ts b/src/words.ts
--- a/src/words.ts
+++ b/src/words.ts
@@ -5,2 +5,3 @@ function words()
 const words = text.split(" ");
-return words;
+const normalized = words.map(word => word.toLocaleLowerCase());
+return normalized;
@@ -20 +21 @@
-export default words;
+export { words };
diff --git a/old name.ts b/new name.ts
similarity index 100%
rename from old name.ts
rename to new name.ts
diff --git a/deleted.ts b/deleted.ts
deleted file mode 100644
--- a/deleted.ts
+++ /dev/null
@@ -1 +0,0 @@
-export const obsolete = true;
`;
const large = `diff --git a/generated.ts b/generated.ts
new file mode 100644
--- /dev/null
+++ b/generated.ts
@@ -0,0 +1,450 @@\n${Array.from({ length: 450 }, (_, index) => `+export const value${index + 1} = ${index + 1};`).join("\n")}\n`;
const broken = "--- a/../outside.ts\n+++ b/../outside.ts\n@@ -1 +1 @@\n-before\n+after\n";

function ReviewFixture() {
  const [scenario, setScenario] = useState("mixed");
  const [appearance, setAppearance] = useState("light");
  const [jump, setJump] = useState("No editor jump requested");
  const patch = scenario === "large" ? large : scenario === "unsafe" ? broken : small;
  const items: TranscriptItem[] = [{ id: scenario, type: "system", timestamp: "2026-10-04T18:00:00Z",
    text: `Review fixture: ${scenario === "large" ? "450-line addition" : scenario === "unsafe" ? "unsafe path" : "multiple files and changes"}`,
    patchRunId: "original-fixture-run", diffPreview: patch,
    patchFiles: scenario === "large" ? ["generated.ts"] : scenario === "unsafe" ? ["../outside.ts"] : ["src/words.ts", "new name.ts", "deleted.ts"],
  }];
  document.documentElement.dataset.theme = appearance;
  return <main style={{ height: "100dvh", display: "flex", flexDirection: "column", background: "var(--cody-canvas)", color: "var(--cody-text)" }}>
    <header style={{ padding: 16, borderBottom: "1px solid var(--cody-border)", display: "flex", gap: 16, flexWrap: "wrap", alignItems: "center", fontSize: 13 }}>
      <strong>Review component QA</strong>
      <label>Scenario <select aria-label="Scenario" value={scenario} onChange={(event) => { setScenario(event.target.value); setJump("No editor jump requested"); }}>
        <option value="mixed">Mixed files and hunks</option><option value="large">450-line patch</option><option value="unsafe">Unsafe path</option>
      </select></label>
      <label>Appearance <select aria-label="Appearance" value={appearance} onChange={(event) => setAppearance(event.target.value)}>
        <option value="light">Light</option><option value="dark">Dark</option>
      </select></label>
      <span>Production components · synthetic display data · no native actions</span>
    </header>
    <Transcript connectionStatus="connected" items={items} onOpenFile={async (path, line, run) => {
      if (path === "deleted.ts") throw new Error("The file no longer exists in this run's workspace");
      setJump(`Editor jump: ${path}:${line} · run ${run}`);
    }} />
    <output role="status" style={{ padding: 16, borderTop: "1px solid var(--cody-border)", fontSize: 13 }}>{jump}</output>
  </main>;
}
createRoot(document.getElementById("root")!).render(<ReviewFixture />);
