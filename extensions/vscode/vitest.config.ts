import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // The pure protocol/client modules run in plain Node with no VS Code
    // runtime. Tests import them directly; `src/extension.ts` (the only module
    // that imports `vscode`) is never touched by the suite.
    environment: "node",
    include: ["test/**/*.test.ts"],
  },
});
