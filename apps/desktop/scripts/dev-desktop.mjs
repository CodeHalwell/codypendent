import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const desktop = fileURLToPath(new URL("..", import.meta.url));
const repository = path.resolve(desktop, "../..");
const npm = process.platform === "win32" ? "npm.cmd" : "npm";
const cargo = process.env.CARGO ?? (existsSync(path.join(process.env.HOME ?? "", ".cargo/bin/cargo")) ? path.join(process.env.HOME, ".cargo/bin/cargo") : "cargo");

function run(program, args, cwd) {
  return new Promise((resolve, reject) => {
    const child = spawn(program, args, { cwd, stdio: "inherit", env: process.env, shell: process.platform === "win32" && program.endsWith(".cmd") });
    child.on("error", reject);
    child.on("exit", (code, signal) => code === 0 ? resolve() : reject(new Error(`${path.basename(program)} stopped (${signal ?? code}).`)));
    const stop = () => child.kill("SIGTERM");
    process.once("SIGINT", stop);
    process.once("SIGTERM", stop);
    child.once("exit", () => { process.off("SIGINT", stop); process.off("SIGTERM", stop); });
  });
}

try {
  for (const name of ["protocol", "ui"]) {
    const directory = path.join(repository, "sdk", name);
    if (!existsSync(path.join(directory, "node_modules"))) await run(npm, ["ci"], directory);
    await run(npm, ["run", "build"], directory);
  }
  console.log("Building the matching workspace service…");
  await run(cargo, ["build", "-p", "codypendent-cli", "-p", "codypendent-ui-worker-launcher"], repository);
  console.log("Opening Codypendent with hot reload…");
  await run(path.join(desktop, "node_modules", ".bin", process.platform === "win32" ? "tauri.cmd" : "tauri"), ["dev", "-f", "app"], desktop);
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
