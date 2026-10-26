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

assert.equal(process.platform, 'linux', 'This smoke test uses WebKitGTK WebDriver on Linux.');
const root = fileURLToPath(new URL('../../../', import.meta.url));
const cli = process.env.CODYPENDENT_TEST_BINARY || path.join(root, 'target/debug/codypendent');
const app = process.env.CODYPENDENT_DESKTOP_BINARY || path.join(root, 'apps/desktop/src-tauri/target/debug/codypendent-desktop');
await Promise.all([stat(cli), stat(app)]);
const directory = await mkdtemp(path.join(tmpdir(), 'cody-desktop-native-'));
const data = path.join(directory, 'data'), repository = path.join(directory, 'repo'), socketPath = path.join(directory, 'daemon.sock');
await Promise.all([mkdir(data), mkdir(repository)]);
execFileSync('git', ['init', '-q'], { cwd: repository });
const workspace = randomUUID();
await writeFile(path.join(data, 'desktop.json'), JSON.stringify({ repository, workspace }));
let releaseModel;
const gate = new Promise(resolve => { releaseModel = resolve; });
let requested = false;
const model = createServer(async (request, response) => {
  if (request.url?.endsWith('/models')) { response.end(JSON.stringify({ data: [{ id: 'fixture' }] })); return; }
  for await (const _chunk of request) { /* consume fixture request */ }
  requested = true; await gate;
  response.writeHead(200, { 'Content-Type': 'text/event-stream' });
  response.end('data: ' + JSON.stringify({ id: 'fixture', object: 'chat.completion.chunk', model: 'fixture',
    choices: [{ index: 0, delta: { role: 'assistant', content: 'Native desktop handoff verified.' }, finish_reason: null }] }) + '\n\n'
    + 'data: ' + JSON.stringify({ id: 'fixture', object: 'chat.completion.chunk', model: 'fixture',
      choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] }) + '\n\ndata: [DONE]\n\n');
});
await new Promise(resolve => model.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(data, 'models.toml'), `[[model]]\nid = "hosted-default"\nprovider = "openai-compatible"\nbase_url = "http://127.0.0.1:${model.address().port}/v1"\nmodel = "fixture"\napi_key_env = ""\n`);
const env = { ...process.env, CODYPENDENT_DATA_DIR: data, CODYPENDENT_SOCKET: socketPath, CODYPENDENT_BINARY: cli,
  XDG_CONFIG_HOME: path.join(directory, 'config'), XDG_DATA_HOME: path.join(directory, 'local-data'),
  TAURI_WEBVIEW_AUTOMATION: 'true', RUST_LOG: 'warn' };
const children = [];
let logs = '';
const start = (binary, args, extraEnv = {}) => {
  const child = spawn(binary, args, { cwd: repository, env: { ...env, ...extraEnv }, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
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
try {
  if (process.env.CODYPENDENT_XVFB) {
    env.DISPLAY = ':173'; start(process.env.CODYPENDENT_XVFB, [env.DISPLAY, '-screen', '0', '1280x840x24', '-nolisten', 'tcp', '-ac']);
    await until(() => stat('/tmp/.X11-unix/X173').then(() => true, () => false), 'Virtual display');
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
  await execute(`const group = [...document.querySelectorAll('button')].find(item => item.getAttribute('aria-label') === 'Workspace group'); if (group) group.click();`);
  await until(() => execute(`const button = [...document.querySelectorAll('button')].find(item => item.getAttribute('aria-label') === 'Memory View'); if (button) button.click(); return !!button;`), 'Native memory navigation');
  await until(async () => { lastText = await execute('return document.body.innerText'); return lastText.includes('I prefer explicit validation evidence.'); }, 'Authenticated knowledge in native window');
  await execute(`const button = [...document.querySelectorAll('button')].find(item => item.getAttribute('aria-label') === 'Session Native desktop proof'); button.click();`);
  if (process.env.CODYPENDENT_SMOKE_OUTPUT) {
    await mkdir(process.env.CODYPENDENT_SMOKE_OUTPUT, { recursive: true });
    const screenshot = await request('GET', `/session/${session}/screenshot`);
    await writeFile(path.join(process.env.CODYPENDENT_SMOKE_OUTPUT, 'desktop.png'), Buffer.from(screenshot, 'base64'));
    await writeFile(path.join(process.env.CODYPENDENT_SMOKE_OUTPUT, 'report.json'), JSON.stringify({ success: true, elapsedMs: Date.now() - started, sessionId: id, nativeWindow: true, authenticatedKnowledge: true, model: 'loopback fixture', handoff: 'SDK → Tauri → disconnect → daemon completes → Tauri replay' }, null, 2));
  }
  console.log('Native Tauri/daemon handoff and restart replay passed.');
} catch (error) {
  console.error(lastText, logs); throw error;
} finally {
  releaseModel(); await detachWindow().catch(() => {}); issuer.disconnect(); observer.disconnect();
  for (const child of children.reverse()) {
    if (child.pid && child.exitCode === null) { try { process.kill(-child.pid, 'SIGTERM'); } catch {} }
  }
  model.closeAllConnections(); await new Promise(resolve => model.close(resolve));
  await rm(directory, { recursive: true, force: true });
}
