/** Real Tauri window + Rust daemon + SDK handoff. The model is a loopback SSE fixture. */
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, rm, stat } from 'node:fs/promises';
import { spawn, execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { NodeSession } from '../../../sdk/protocol/dist/node.js';
import { fixtureEnvironment, stopOwnedChildren } from './native-smoke-support.mjs';

assert.equal(process.platform, 'linux', 'This smoke test uses WebKitGTK WebDriver on Linux.');
const root = fileURLToPath(new URL('../../../', import.meta.url));
const cli = path.resolve(process.env.CODYPENDENT_TEST_BINARY || path.join(root, 'target/debug/codypendent'));
const app = path.resolve(process.env.CODYPENDENT_DESKTOP_BINARY || path.join(root, 'apps/desktop/src-tauri/target/debug/codypendent-desktop'));
await Promise.all([stat(cli), stat(app)]);
const directory = await mkdtemp(path.join(tmpdir(), 'cody-desktop-native-'));
const env = fixtureEnvironment(directory, cli);
const data = env.CODYPENDENT_DATA_DIR, repository = path.join(directory, 'repo'), socketPath = env.CODYPENDENT_SOCKET;
await Promise.all([repository, data, env.HOME, env.XDG_CONFIG_HOME, env.XDG_DATA_HOME, env.XDG_CACHE_HOME,
  env.XDG_STATE_HOME, env.XDG_RUNTIME_DIR, env.TMPDIR, env.CODYPENDENT_CONFIG_DIR]
  .map(directory => mkdir(directory, { recursive: true, mode: 0o700 })));
execFileSync('git', ['init', '-q'], { cwd: repository, env });
const workflowDirectory = path.join(repository, '.codypendent', 'workflows');
await mkdir(workflowDirectory, { recursive: true });
await writeFile(path.join(workflowDirectory, 'native-window-proof.yaml'),
  'schema_version: 1\nid: native-window-proof\nversion: 1\ndescription: Native repository catalog proof\n'
  + 'inputs:\n  request:\n    type: string\n    required: true\n'
  + 'steps:\n  - id: verify\n    tool: repository.test\n');
const workspace = randomUUID();
await writeFile(path.join(env.CODYPENDENT_CONFIG_DIR, 'desktop.json'), JSON.stringify({ repository, workspace }));
let releaseModel;
const gate = new Promise(resolve => { releaseModel = resolve; });
let requested = false;
let completionRequests = 0;
const completionKinds = { ask: 0, memoryExtraction: 0, unexpected: 0 };
const model = createServer(async (request, response) => {
  if (request.url?.endsWith('/models')) { response.end(JSON.stringify({ data: [{ id: 'fixture' }] })); return; }
  let body = '';
  for await (const chunk of request) body += chunk;
  const messages = JSON.parse(body).messages ?? [];
  const serializedMessages = JSON.stringify(messages);
  const memoryExtraction = serializedMessages.includes('Extract at most 10 discrete, standalone facts');
  const ask = !memoryExtraction && serializedMessages.includes('Keep this run alive while the native desktop closes.');
  completionKinds[memoryExtraction ? 'memoryExtraction' : ask ? 'ask' : 'unexpected'] += 1;
  requested ||= ask; completionRequests += 1;
  if (ask) await gate;
  response.writeHead(200, { 'Content-Type': 'text/event-stream' });
  response.end('data: ' + JSON.stringify({ id: 'fixture', object: 'chat.completion.chunk', model: 'fixture',
    choices: [{ index: 0, delta: { role: 'assistant', content: memoryExtraction ? '[]' : 'Native desktop handoff verified.' }, finish_reason: null }] }) + '\n\n'
    + 'data: ' + JSON.stringify({ id: 'fixture', object: 'chat.completion.chunk', model: 'fixture',
      choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] }) + '\n\ndata: [DONE]\n\n');
});
await new Promise(resolve => model.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(data, 'models.toml'), `[[model]]\nid = "hosted-default"\nprovider = "openai-compatible"\nbase_url = "http://127.0.0.1:${model.address().port}/v1"\nmodel = "fixture"\napi_key_env = ""\n`);
const children = [];
let logs = '';
const start = (binary, args, extraEnv = {}, extraStdio = []) => {
  const child = spawn(binary, args, { cwd: repository, env: { ...env, ...extraEnv }, detached: true, stdio: ['ignore', 'pipe', 'pipe', ...extraStdio] });
  child.on('error', error => { logs = (logs + error.message).slice(-32_000); });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', bytes => { logs = (logs + bytes).slice(-32_000); });
  children.push(child); return child;
};
const until = async (condition, label, timeout = 30_000) => {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) { if (await condition()) return; await new Promise(resolve => setTimeout(resolve, 100)); }
  throw new Error(`${label} timed out. ${logs}`);
};
const issuer = new NodeSession({ socketPath, canWrite: () => true, onChange: () => {}, onError: () => {}, timeoutMs: 10_000 });
const observer = new NodeSession({ socketPath, canWrite: () => false, onChange: () => {}, onError: () => {}, timeoutMs: 10_000 });
let session;
let driverUrl;
let lastText = '';
const request = async (method, route, body) => {
  const response = await fetch(driverUrl + route, { method, headers: { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(60_000) });
  const result = await response.json();
  assert.ok(response.ok && !result.value?.error, JSON.stringify(result)); return result.value;
};
const execute = (script, args = []) => request('POST', `/session/${session}/execute/sync`, { script, args });
const saveScreenshot = async name => {
  if (!process.env.CODYPENDENT_SMOKE_OUTPUT) return;
  await mkdir(process.env.CODYPENDENT_SMOKE_OUTPUT, { recursive: true });
  const screenshot = await request('GET', `/session/${session}/screenshot`);
  await writeFile(path.join(process.env.CODYPENDENT_SMOKE_OUTPUT, name), Buffer.from(screenshot, 'base64'));
};
const navigate = (groupName, viewLabel) => until(() => execute(`
  const tools = document.querySelector('nav[aria-label="Workspace tools"] > button');
  if (tools?.getAttribute('aria-expanded') === 'false') tools.click();
  const group = [...document.querySelectorAll('button')].find(item => item.getAttribute('aria-label') === arguments[0] + ' group');
  if (group?.getAttribute('aria-expanded') === 'false') group.click();
  const button = [...document.querySelectorAll('button')].find(item => item.getAttribute('aria-label') === arguments[1]);
  if (button?.getAttribute('aria-current') === 'page') return true;
  if (button) button.click(); return false;
`, [groupName, viewLabel]), `Native ${viewLabel} navigation`);
const attachWindow = async () => {
  const result = await request('POST', '/session', { capabilities: { alwaysMatch: {
    'webkitgtk:browserOptions': { binary: app, args: [] } } } });
  session = result.sessionId;
  await until(async () => {
    lastText = await execute('return document.body.innerText');
    return execute(`const button = [...document.querySelectorAll('button')].find(item => item.getAttribute('aria-label') === 'Session Native desktop proof'); if (button) button.click(); return !!button;`);
  }, 'Native session navigation');
};
const detachWindow = async () => { if (session) { await request('DELETE', `/session/${session}`); session = undefined; } };
const started = Date.now();
let completedReport;
try {
  if (process.env.CODYPENDENT_XVFB) {
    const display = start(process.env.CODYPENDENT_XVFB,
      ['-displayfd', '3', '-screen', '0', '1280x840x24', '-nolisten', 'tcp'], {}, ['pipe']);
    let displayNumber = '';
    display.stdio[3].on('data', bytes => { displayNumber += bytes; });
    await until(() => /^\d+\s*$/.test(displayNumber), 'Private virtual display allocation');
    env.DISPLAY = `:${Number(displayNumber.trim())}`;
  }
  start(cli, ['__daemon']);
  await until(() => stat(socketPath).then(() => true, () => false), 'Native daemon');
  await issuer.connect();
  const id = await issuer.createSession('Native desktop proof', workspace, repository);
  await issuer.rememberMemory('I prefer explicit validation evidence.');
  const record = (await issuer.listSessions()).find(item => item.session_id === id);
  assert.ok(record); await observer.connect(record);
  await issuer.submit('Keep this run alive while the native desktop closes.', 'Ask');
  issuer.disconnect();
  await until(() => requested, 'Loopback model request');
  const portProbe = createServer();
  await new Promise(resolve => portProbe.listen(0, '127.0.0.1', resolve));
  const port = portProbe.address().port; await new Promise(resolve => portProbe.close(resolve));
  driverUrl = `http://127.0.0.1:${port}`;
  start(process.env.CODYPENDENT_WEBDRIVER || 'WebKitWebDriver', [`--port=${port}`, '--host=127.0.0.1']);
  await until(() => fetch(driverUrl + '/status').then(response => response.ok, () => false), 'Native WebDriver');
  await attachWindow();
  await until(async () => { lastText = await execute('return document.body.innerText'); return lastText.includes('Keep this run alive'); }, 'Native pending transcript');
  await detachWindow();
  releaseModel();
  await until(() => observer.snapshot.projection.runs?.some(run => run.state === 'Completed'), 'Daemon completion after desktop closed');
  await attachWindow();
  await until(async () => { lastText = await execute('return document.body.innerText'); return lastText.includes('Native desktop handoff verified.'); }, 'Native replay after reopen');
  await until(() => completionKinds.memoryExtraction === 1, 'Automatic post-run memory extraction');
  const beforeCatalogRequests = completionRequests;
  await navigate('Workflow', 'Workflows View');
  await until(() => execute(`
    const button = [...document.querySelectorAll('ul[aria-label="Available Workflows"] button')]
      .find(item => item.querySelector('strong')?.textContent === 'native-window-proof');
    if (!button || !button.textContent.includes('Repository') || !button.textContent.includes('1 steps')) return false;
    button.click(); return true;
  `), 'Repository-owned workflow catalog in native window');
  await until(() => execute(`return !!document.querySelector('form[aria-label="Start native-window-proof"] textarea[aria-required="true"]');`), 'Typed workflow input preview without execution');
  await saveScreenshot('workflow-catalog.png');
  await navigate('Workspace', 'Memory View');
  await until(async () => { lastText = await execute('return document.body.innerText'); return lastText.includes('I prefer explicit validation evidence.'); }, 'Authenticated knowledge in native window');
  await execute(`const button = [...document.querySelectorAll('button')].find(item => item.getAttribute('aria-label') === 'Session Native desktop proof'); button.click();`);
  assert.equal(completionRequests, beforeCatalogRequests, 'catalog browsing and input preview must not start another model request');
  assert.deepEqual(completionKinds, { ask: 1, memoryExtraction: 1, unexpected: 0 }, 'only the Ask run and its automatic memory extraction may call the loopback model');
  if (process.env.CODYPENDENT_SMOKE_OUTPUT) {
    await mkdir(process.env.CODYPENDENT_SMOKE_OUTPUT, { recursive: true });
    await saveScreenshot('desktop.png');
    completedReport = { success: true, sessionId: id, nativeWindow: true, authenticatedKnowledge: true, workflowCatalog: true, workflowExecution: 'not exercised', model: 'loopback fixture', completionRequests, completionKinds, catalogModelRequests: completionRequests - beforeCatalogRequests, webkitRendererSandboxOverride: env.WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS === '1', daemonAndWorkerExecutionPolicy: 'unchanged', distributionAssets: 'not assessed by this window test', handoff: 'SDK → Tauri → disconnect → daemon completes → Tauri replay' };
  }
} catch (error) {
  if (session) {
    lastText = await execute('return document.body.innerText').catch(() => lastText);
    await saveScreenshot('failure.png').catch(() => {});
  }
  console.error(lastText, logs); throw error;
} finally {
  releaseModel(); await detachWindow().catch(() => {}); issuer.disconnect(); observer.disconnect();
  try {
    await stopOwnedChildren(children);
  } catch (error) {
    console.error(`Fixture state retained at ${directory}: owned child cleanup did not complete.`);
    throw error;
  } finally {
    model.closeAllConnections(); await new Promise(resolve => model.close(resolve));
  }
  await rm(directory, { recursive: true, force: true });
}

if (completedReport) {
  await writeFile(path.join(process.env.CODYPENDENT_SMOKE_OUTPUT, 'report.json'), JSON.stringify({ ...completedReport, elapsedMs: Date.now() - started, cleanupCompleted: true }, null, 2));
}
console.log('Native Tauri/daemon handoff, catalog preview and restart replay passed; owned cleanup completed.');
