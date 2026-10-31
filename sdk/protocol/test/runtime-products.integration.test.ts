import { execFile, spawn, type ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { access, mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";
import { NodeSession, resolveRuntimePaths, type RuntimePaths, type RuntimeProduct } from "../src/node.js";

// Opt in with a matching, locally built CLI. No daemon or fixture is created by
// the normal SDK unit suite, and this test never submits a model prompt.
const configuredBinary = process.env.CODYPENDENT_TEST_BINARY?.trim();
const testBinary = configuredBinary ? path.resolve(configuredBinary) : undefined;
const execute = promisify(execFile);

interface RuntimeFixture {
  product: RuntimeProduct;
  env: NodeJS.ProcessEnv;
  paths: RuntimePaths;
  modelIds: [string, string];
  child?: ChildProcess;
  session?: NodeSession;
  closed: boolean;
  spawnError?: Error;
  log: string;
}

function fixtureEnvironment(root: string, product: RuntimeProduct): NodeJS.ProcessEnv {
  // Credentials and actual user config never enter a test child. Keep only the
  // executable/library search paths and platform proxy/CA plumbing it may need.
  const env: NodeJS.ProcessEnv = {};
  for (const key of [
    "PATH", "SystemRoot", "SYSTEMROOT", "WINDIR", "PATHEXT",
    "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH",
    "ORT_LIB_LOCATION", "ORT_PREFER_DYNAMIC_LINK", "SSL_CERT_FILE", "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE",
    "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
    "http_proxy", "https_proxy", "all_proxy", "no_proxy",
  ]) {
    if (process.env[key] !== undefined) env[key] = process.env[key];
  }
  const home = path.join(root, product === "codypendent" ? "c" : "a", "home");
  return {
    ...env, HOME: home, USERPROFILE: home, TMPDIR: path.join(root, "tmp"),
    XDG_DATA_HOME: path.join(home, "data"), XDG_CONFIG_HOME: path.join(home, "config"),
    XDG_CACHE_HOME: path.join(home, "cache"), XDG_STATE_HOME: path.join(home, "state"),
    XDG_RUNTIME_DIR: path.join(root, "run"), RUST_LOG: "warn",
    GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"),
    AGENT_RUNTIME_PRODUCT: product,
    // Both prefixes are deliberately present: the foreign one points at the
    // other test profile, so accidentally honoring it breaks the assertions.
    CODYPENDENT_DATA_DIR: path.join(root, "c", "data"),
    CODYPENDENT_CONFIG_DIR: path.join(root, "c", "config"),
    CODYPENDENT_SOCKET: path.join(root, "c", "daemon.sock"),
    SON_OF_ANTON_DATA_DIR: path.join(root, "a", "data"),
    SON_OF_ANTON_CONFIG_DIR: path.join(root, "a", "config"),
    SON_OF_ANTON_SOCKET: path.join(root, "a", "daemon.sock"),
  };
}

async function waitForExit(fixture: RuntimeFixture, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (!fixture.closed && Date.now() < deadline) await delay(25);
  return fixture.closed;
}

async function startDaemon(binary: string, fixture: RuntimeFixture, repository: string): Promise<void> {
  const child = spawn(binary, ["__daemon"], { cwd: repository, env: fixture.env, stdio: ["ignore", "pipe", "pipe"] });
  fixture.child = child;
  child.once("error", error => { fixture.spawnError = error; });
  child.once("close", () => { fixture.closed = true; });
  const append = (chunk: Buffer): void => { fixture.log = (fixture.log + chunk.toString("utf8")).slice(-16_000); };
  child.stdout?.on("data", append);
  child.stderr?.on("data", append);
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    if (fixture.spawnError) throw fixture.spawnError;
    if (fixture.closed) throw new Error(`${fixture.product} daemon exited before listening: ${fixture.log}`);
    try {
      if ((await stat(fixture.paths.socketPath)).isSocket()) return;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
    await delay(50);
  }
  throw new Error(`${fixture.product} daemon did not listen within 20 seconds: ${fixture.log}`);
}

async function stopOwnedDaemon(fixture: RuntimeFixture): Promise<void> {
  if (!fixture.child || fixture.closed) return;
  // Cleanup must not depend on the discovery code under test. Signal only the
  // tracked child; the explicit scoped CLI shutdown is asserted in the test.
  fixture.child.kill("SIGTERM");
  if (await waitForExit(fixture, 3_000)) return;
  fixture.child.kill("SIGKILL");
  if (!await waitForExit(fixture, 3_000)) throw new Error(`could not stop test-owned ${fixture.product} daemon`);
}

describe.skipIf(!testBinary)("real runtime product isolation", () => {
  it("keeps models, user workflows, and SDK sessions private and stops only the selected daemon", async () => {
    if (!testBinary) throw new Error("CODYPENDENT_TEST_BINARY must name a locally built CLI");
    await access(testBinary);
    const root = await mkdtemp(path.join(tmpdir(), "rt-"));
    const repository = path.join(root, "repo");
    const fixtures: RuntimeFixture[] = [];
    let providerRequests = 0;
    const provider = createServer((_request, response) => {
      providerRequests += 1;
      response.writeHead(503).end("This fixture must not receive model requests.");
    });
    let providerListening = false;
    let failure: unknown;
    const cleanupErrors: unknown[] = [];
    try {
      await mkdir(repository, { recursive: true, mode: 0o700 });
      await mkdir(path.join(root, "tmp"), { mode: 0o700 });
      await mkdir(path.join(root, "run"), { mode: 0o700 });
      await writeFile(path.join(root, "gitconfig"), "");
      for (const product of ["codypendent", "son-of-anton"] as const) {
        const env = fixtureEnvironment(root, product);
        const paths = resolveRuntimePaths(env, process.platform, product);
        await Promise.all([paths.dataDir, paths.configDir, env.HOME!].map(directory => mkdir(directory, { recursive: true, mode: 0o700 })));
        fixtures.push({ product, env, paths, modelIds: [`${product}-first`, `${product}-second`], closed: false, log: "" });
      }
      const [cody, anton] = fixtures;
      expect(cody.paths.runDir).not.toBe(anton.paths.runDir);
      expect(cody.paths.pidPath).not.toBe(anton.paths.pidPath);
      const gitOptions = { cwd: repository, env: cody.env, timeout: 5_000 };
      await execute("git", ["init", "--quiet"], gitOptions);
      await writeFile(path.join(repository, "fixture.ts"), "export const runtimeFixture = true;\n");
      await execute("git", ["add", "fixture.ts"], gitOptions);
      await execute("git", ["-c", "user.name=Runtime Test", "-c", "user.email=runtime@example.invalid", "commit", "--quiet", "-m", "fixture"], gitOptions);

      await new Promise<void>((resolve, reject) => {
        provider.once("error", reject);
        provider.listen(0, "127.0.0.1", () => { providerListening = true; resolve(); });
      });
      const address = provider.address();
      if (!address || typeof address === "string") throw new Error("fixture provider did not bind a TCP port");
      const endpoint = `http://127.0.0.1:${address.port}/v1`;
      for (const fixture of fixtures) {
        // Production builds the user source from this product's data_dir/workflows.
        // Only discovery is exercised: this valid tool-only workflow is never run.
        const workflowDirectory = path.join(fixture.paths.dataDir, "workflows");
        await mkdir(workflowDirectory, { recursive: true, mode: 0o700 });
        await writeFile(path.join(workflowDirectory, "private-fixture.yaml"), [
          "schema_version: 1", `id: ${fixture.product}-private-workflow`, "version: 1",
          "steps:", "  - id: inspect", "    tool: repository.test", "",
        ].join("\n"));
        await writeFile(path.join(fixture.paths.dataDir, "providers.toml"), [
          "[[provider]]", 'id = "local-fixture"', 'name = "Test-owned local fixture"',
          'protocol = "openai-chat"', `base_url = "${endpoint}"`, "local = true",
          "[[provider.auth]]", 'kind = "none"', "",
        ].join("\n"));
        await writeFile(path.join(fixture.paths.dataDir, "models.toml"), fixture.modelIds.map((id, index) => [
          "[[model]]", `id = "${id}"`, 'provider = "openai-compatible"',
          `base_url = "${endpoint}"`, `model = "fixture-${index}"`, 'api_key_env = ""',
          'provider_id = "local-fixture"', `context_tokens = ${4_096 * (index + 1)}`, "",
        ].join("\n")).join("\n"));
        const options = { cwd: repository, env: fixture.env, timeout: 10_000, maxBuffer: 1024 * 1024 };
        await execute(testBinary, ["models", "add", "local-fixture", "fixture-0", "--id", fixture.modelIds[0]], options);
        const listed = await execute(testBinary, ["models", "list", "--json"], options);
        expect(JSON.parse(listed.stdout)).toEqual(fixture.modelIds.map((id, index) => ({
          id, provider: "local-fixture", endpoint, context_tokens: 4_096 * (index + 1), key: "none",
        })));
        const models = await readFile(path.join(fixture.paths.dataDir, "models.toml"), "utf8");
        const foreign = fixture === cody ? anton : cody;
        expect(models).not.toContain(foreign.modelIds[0]);
      }
      expect(providerRequests).toBe(0);

      for (const fixture of fixtures) {
        await startDaemon(testBinary, fixture, repository);
        fixture.session = new NodeSession({
          socketPath: fixture.paths.socketPath, timeoutMs: 5_000, canWrite: () => true,
          onChange: () => {}, onError: message => { fixture.log = (fixture.log + `\nSDK: ${message}`).slice(-16_000); },
          clientName: "runtime-product-integration-test",
        });
        await fixture.session.connect();
      }
      for (const fixture of fixtures) {
        const foreign = fixture === cody ? anton : cody;
        const catalog = await fixture.session!.listWorkflows(repository);
        expect(catalog.items.find(entry => entry.id === `${fixture.product}-private-workflow`)?.source.type).toBe("User");
        expect(catalog.items.some(entry => entry.id === `${foreign.product}-private-workflow`)).toBe(false);
      }
      const codyId = await cody.session!.createSession("Codypendent private session", randomUUID(), repository);
      const antonId = await anton.session!.createSession("Son Of Anton private session", randomUUID(), repository);
      expect(codyId).not.toBe(antonId);
      expect((await cody.session!.listSessions()).map(session => session.session_id)).toEqual([codyId]);
      expect((await anton.session!.listSessions()).map(session => session.session_id)).toEqual([antonId]);
      expect(cody.session!.snapshot.sessionId).toBe(codyId);
      expect(anton.session!.snapshot.sessionId).toBe(antonId);

      anton.session!.disconnect();
      const stopped = await execute(testBinary, ["daemon", "stop"], { cwd: repository, env: anton.env, timeout: 10_000 });
      expect(stopped.stdout).toContain("daemon stopped");
      expect(await waitForExit(anton, 5_000)).toBe(true);
      expect(cody.closed).toBe(false);
      expect((await cody.session!.listSessions()).map(session => session.session_id)).toEqual([codyId]);
      expect(providerRequests).toBe(0);
    } catch (error) {
      failure = error;
    } finally {
      for (const fixture of fixtures) fixture.session?.disconnect();
      for (const fixture of [...fixtures].reverse()) {
        try { await stopOwnedDaemon(fixture); } catch (error) { cleanupErrors.push(error); }
      }
      if (providerListening) {
        provider.closeAllConnections();
        await new Promise<void>(resolve => provider.close(() => resolve()));
      }
      // Keep evidence if an owned child cannot be reaped; never remove a live
      // daemon's files or fall back to stopping anything outside this fixture.
      if (!cleanupErrors.length) await rm(root, { recursive: true, force: true });
    }
    if (failure !== undefined || cleanupErrors.length) {
      throw new AggregateError([...(failure === undefined ? [] : [failure]), ...cleanupErrors],
        `Runtime product integration failed.\n${fixtures.map(fixture => `${fixture.product}: ${fixture.log}`).join("\n")}`);
    }
  }, 120_000);
});
