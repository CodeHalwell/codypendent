// Translate ACP to the installed ZCode app-server protocol over local pipes.
import { spawn } from 'node:child_process';
import { createDecipheriv, createHash, randomUUID } from 'node:crypto';
import { readFileSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { homedir, platform, userInfo } from 'node:os';

const MAX_LINE = 4 * 1024 * 1024;
const backendPath = process.env.CODYPENDENT_ZCODE_PATH;
if (!backendPath) throw new Error('The installed ZCode agent was not found.');
const backend = spawn(/\.[cm]js$/i.test(backendPath) ? process.execPath : backendPath,
  [...(/\.[cm]js$/i.test(backendPath) ? [backendPath] : []), 'app-server'],
  { stdio: ['pipe', 'pipe', 'inherit'], env: process.env });
const pending = new Map();
const permissions = new Map();
const sessions = new Map();
let sequence = 0;
let closed = false;

function readJson(path, limit = 2 * 1024 * 1024) {
  if (!path || statSync(path).size > limit) throw new Error('ZCode metadata is unavailable or too large.');
  return JSON.parse(readFileSync(path, 'utf8'));
}
async function publishAccountMetadata() {
  const path = process.env.ZCODE_BUILTIN_PROVIDER_CONFIG_FILE;
  const personal = process.env.ZCODE_PERSONAL_PROVIDER_CONFIG_FILE;
  if (!path || !personal) return;
  let table, cache;
  try {
    table = readJson(path);
    cache = readJson(join(dirname(personal), 'coding-plan-cache.json'), 256 * 1024);
  } catch { return; }
  const verdicts = cache?.entryStatus?.items;
  if (!verdicts || !Object.keys(verdicts).length) return;
  const providers = {}, states = {};
  for (const rule of table?.config?.providerConfigRules?.providerRules ?? []) {
    const access = rule.config?.access;
    if (access?.type !== 'zhipu-account' || typeof rule.providerId !== 'string') continue;
    const plan = access.mode === 'individual-coding-plan' ? 'coding-plan' : access.mode;
    const entitled = verdicts[`builtin:${access.accountType}-${plan}`]?.status === 'available';
    providers[rule.providerId] = { builtinModelIds: rule.config.builtinModelIds, access: { type: 'zhipu-account', entitled } };
    states[rule.providerId] = { availability: entitled ? 'available' : 'unavailable', entitled, current: entitled };
  }
  if (!Object.keys(providers).length) return;
  try {
    await request('provider/updateAccountConfig', { revision: `account:codypendent:${Date.now()}`,
      basedOnZCodeBuiltinRevision: `zcode-builtin:${table.revision ?? 0}:${createHash('sha256').update(resolve(path)).digest('hex')}`,
      providers, states }, 30000);
  } catch (error) { if (error.code !== -32601) throw error; }
}
const modelValue = ref => `${ref.providerId}/${ref.modelId}`;
function modelOptions(session) {
  if (!session.models.size) return [];
  return [{ id: 'model', name: 'Model', category: 'model', type: 'select', currentValue: session.model,
    options: [...session.models].map(([value, entry]) => ({ value, name: entry.label ?? entry.ref.modelId })) }];
}
async function selectModel(sessionId, session, value) {
  const entry = session.models.get(value);
  if (!entry) throw new Error('That model is not advertised by the installed ZCode agent.');
  const level = entry.reasoning?.defaultLevel ?? entry.reasoning?.levels?.[0]?.value;
  await request('session/setModel', { sessionId, model: { ...entry.ref,
    ...(level ? { options: { reasoningLevel: level } } : {}) }, persistAsWorkspaceLastUsed: false });
  session.model = value;
}
function runtimeAuth(params) {
  try {
    const session = sessions.get(params.sessionId);
    const selected = session?.models.get(session.model)?.ref;
    if (!session || session.turn === null || session.cancelled || params.providerId !== selected?.providerId
      || params.modelSelection?.providerId !== selected.providerId
      || params.modelSelection?.modelId !== selected.modelId) return null;
    const table = readJson(process.env.ZCODE_BUILTIN_PROVIDER_CONFIG_FILE);
    const rule = table?.config?.providerConfigRules?.providerRules?.find(value => value.providerId === selected.providerId);
    if (rule?.config?.access?.type !== 'zhipu-account' || rule.config.access.mode !== 'individual-coding-plan') return null;
    const endpoint = new URL(rule.config.api.baseUrl);
    if (endpoint.protocol !== 'https:' || endpoint.username || endpoint.password || (endpoint.port && endpoint.port !== '443')
      || !['api.z.ai', 'open.bigmodel.cn'].includes(endpoint.hostname)) return null;
    if (params.targetOrigin !== undefined && params.targetOrigin !== endpoint.origin) return null;
    const directory = dirname(process.env.ZCODE_PERSONAL_PROVIDER_CONFIG_FILE);
    const cache = readJson(join(directory, 'coding-plan-cache.json'), 256 * 1024);
    if (cache?.entryStatus?.items?.[`builtin:${rule.config.access.accountType}-coding-plan`]?.status !== 'available') return null;
    const entries = readJson(join(directory, 'credentials.json'), 2 * 1024 * 1024);
    const prefix = `account-provider:coding-plan:account:${selected.providerId.slice('account:'.length)}:account:`;
    const matches = Object.entries(entries).filter(([key, value]) => key.startsWith(prefix) && key.endsWith(':api-key') && typeof value === 'string');
    if (matches.length !== 1) return null;
    let apiKey = matches[0][1];
    if (apiKey.startsWith('enc:v1:')) {
      const parts = apiKey.slice(7).split('.');
      if (parts.length !== 3) return null;
      const [iv, tag, data] = parts.map(value => Buffer.from(value, 'base64url'));
      if (iv.length !== 12 || tag.length !== 16 || data.length > 16384) return null;
      const secret = process.env.ZCODE_CREDENTIAL_SECRET?.trim()
        || `zcode-credential-fallback:${platform()}:${homedir()}:${userInfo().username}`;
      const decipher = createDecipheriv('aes-256-gcm', createHash('sha256').update(secret).digest(), iv);
      decipher.setAuthTag(tag);
      apiKey = Buffer.concat([decipher.update(data), decipher.final()]).toString('utf8');
    }
    if (!apiKey.trim() || apiKey.length > 16384 || /[\r\n\0]/.test(apiKey)) return null;
    return { apiKey };
  } catch { return null; }
}

function write(stream, value) {
  const text = JSON.stringify(value);
  if (Buffer.byteLength(text) > MAX_LINE) throw new Error('ZCode protocol message is too large.');
  if (!stream.write(text + '\n') && stream.writableLength > MAX_LINE * 2) {
    throw new Error('ZCode protocol output exceeded its buffer limit.');
  }
}
function response(id, result) { write(process.stdout, { jsonrpc: '2.0', id, result }); }
function failure(id, error) {
  write(process.stdout, { jsonrpc: '2.0', id,
    error: { code: -32000, message: String(error?.message ?? error).slice(0, 8192) } });
}
function update(sessionId, value) {
  write(process.stdout, { jsonrpc: '2.0', method: 'session/update',
    params: { sessionId, update: value } });
}
function request(method, params, timeout = 90000) {
  if (closed) return Promise.reject(new Error('The ZCode agent disconnected.'));
  if (pending.size >= 64) return Promise.reject(new Error('Too many pending ZCode requests.'));
  const id = `cody-${++sequence}`;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`ZCode ${method} timed out.`)); }, timeout);
    pending.set(id, { resolve, reject, timer });
    try { write(backend.stdin, { id, method, params }); }
    catch (error) { pending.delete(id); clearTimeout(timer); reject(error); }
  });
}
function finish(session, error, reason = 'end_turn') {
  if (!session || session.turn === null) return;
  const id = session.turn;
  session.turn = null;
  if (session.cancelled) { error = null; reason = 'cancelled'; }
  if (error) failure(id, error); else response(id, { stopReason: reason });
}
function close(error = new Error('The ZCode agent disconnected.')) {
  if (closed) return;
  closed = true;
  for (const entry of pending.values()) { clearTimeout(entry.timer); entry.reject(error); }
  pending.clear();
  permissions.clear();
  for (const session of sessions.values()) finish(session, error);
  backend.kill('SIGTERM');
  const timer = setTimeout(() => { backend.kill('SIGKILL'); process.exitCode = 1; }, 500);
  timer.unref();
}
function lines(stream, handler) {
  let buffer = Buffer.alloc(0);
  stream.on('data', chunk => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      const at = buffer.indexOf(10);
      if (at < 0) break;
      if (at > MAX_LINE) { close(new Error('ZCode protocol line is too large.')); return; }
      const line = buffer.subarray(0, at).toString('utf8'); buffer = buffer.subarray(at + 1);
      if (!line.trim()) continue;
      let value;
      try { value = JSON.parse(line); } catch { close(new Error('ZCode sent malformed protocol data.')); return; }
      Promise.resolve(handler(value)).catch(error => {
        if (value.id !== undefined) failure(value.id, error); else close(error);
      });
    }
    if (buffer.length > MAX_LINE) close(new Error('ZCode protocol line is too large.'));
  });
  stream.on('error', close);
}
function permission(message) {
  const params = message.params ?? {};
  const sessionId = params.sessionId;
  const session = sessions.get(sessionId);
  if (!session || session.turn === null || session.cancelled) {
    write(backend.stdin, { id: message.id, result: { decision: 'deny', reason: 'The chat is not active.' } });
    return;
  }
  if (permissions.size >= 64) throw new Error('Too many pending ZCode approvals.');
  const id = `permission-${++sequence}`;
  permissions.set(id, { backendId: message.id, sessionId });
  write(process.stdout, { jsonrpc: '2.0', id, method: 'session/request_permission', params: {
    sessionId,
    toolCall: { toolCallId: params.toolCallId ?? id, title: params.toolName ?? params.reason ?? 'ZCode action',
      kind: 'other', status: 'pending', rawInput: params.input ?? {} },
    options: [
      { optionId: 'allow', name: 'Allow once', kind: 'allow_once' },
      { optionId: 'deny', name: 'Decline', kind: 'reject_once' },
    ],
  } });
}
function backendEvent(message) {
  const entry = pending.get(message.id);
  if (entry && !message.method) {
    pending.delete(message.id); clearTimeout(entry.timer);
    if (message.error) entry.reject(Object.assign(new Error(message.error.message), { code: message.error.code })); else entry.resolve(message.result);
    return;
  }
  if (message.method === 'session/requestRuntimePreferences' && message.id !== undefined) {
    write(backend.stdin, { id: message.id, result: { nativeSearchEnhancementsEnabled: false,
      memoryEnabled: false, askUserQuestionAutoResolutionEnabled: false } });
    return;
  }
  if (message.method === 'interaction/requestPermission' && message.id !== undefined) return permission(message);
  if (message.method === 'interaction/requestProviderRuntimeHeaders' && message.id !== undefined) {
    const requestAuth = runtimeAuth(message.params ?? {});
    write(backend.stdin, { id: message.id, result: requestAuth ? { headersApplied: true, requestAuth }
      : { headersApplied: false, errorMessage: 'ZCode could not resolve the active model account for this chat.' } });
    return;
  }
  if (message.id !== undefined && message.method) {
    write(backend.stdin, { id: message.id, error: { code: -32601,
      message: `The installed ZCode agent requested an unsupported interaction: ${message.method}` } });
    return;
  }
  if (message.method !== 'session/event') return;
  const event = message.params ?? {};
  const session = sessions.get(event.sessionId);
  if (!session || session.turn === null || !Number.isSafeInteger(event.seq) || event.seq <= session.seq) return;
  session.seq = event.seq;
  const payload = event.payload ?? {};
  if (event.type === 'model.streaming' || event.type === 'part.delta') {
    const source = event.type;
    const field = payload.kind ?? payload.field ?? 'text';
    if (!['text_delta', 'reasoning_delta', 'text', 'reasoning'].includes(field)) return;
    if (session.source && session.source !== source) return;
    session.source = source;
    if (typeof payload.delta !== 'string' || !payload.delta) return;
    const thought = field.includes('reasoning');
    if (!thought) session.text = true;
    update(event.sessionId, { sessionUpdate: thought ? 'agent_thought_chunk' : 'agent_message_chunk',
      content: { type: 'text', text: payload.delta } });
  } else if (event.type === 'tool.updated') {
    if (!payload.toolCallId) return;
    const started = ['scheduled', 'started'].includes(payload.kind);
    const known = session.tools.has(payload.toolCallId);
    session.tools.add(payload.toolCallId);
    if (session.tools.size > 10000) throw new Error('ZCode tool event limit exceeded.');
    update(event.sessionId, { sessionUpdate: known ? 'tool_call_update' : 'tool_call',
      toolCallId: payload.toolCallId, title: payload.description ?? payload.toolName ?? 'ZCode action',
      kind: 'other', status: payload.kind === 'error' ? 'failed' : payload.kind === 'result' ? 'completed' : started ? 'in_progress' : 'in_progress',
      ...(payload.input !== undefined ? { rawInput: payload.input } : {}),
      ...(payload.result !== undefined ? { rawOutput: payload.result } : {}),
      ...(payload.error !== undefined ? { rawOutput: payload.error } : {}),
    });
  } else if (event.type === 'turn.completed') {
    if (!session.text && typeof payload.response === 'string' && payload.response) {
      update(event.sessionId, { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: payload.response } });
    }
    finish(session);
  } else if (event.type === 'turn.failed') {
    finish(session, new Error(payload.error?.message ?? (typeof payload.error === 'string' ? payload.error : 'The ZCode turn failed.')));
  }
}
async function clientEvent(message) {
  const params = message.params ?? {};
  if (!message.method) {
    const permission = permissions.get(message.id);
    if (!permission) return;
    permissions.delete(message.id);
    const selected = message.result?.outcome;
    const allowed = selected?.outcome === 'selected' && selected.optionId === 'allow';
    write(backend.stdin, { id: permission.backendId, result: { decision: allowed ? 'allow' : 'deny' } });
    return;
  }
  switch (message.method) {
    case 'initialize': response(message.id, { protocolVersion: 1, agentInfo: { name: 'zcode', title: 'ZCode', version: 'local' },
      agentCapabilities: { promptCapabilities: {}, mcpCapabilities: { http: true, sse: true } },
      authMethods: [{ id: 'agent', name: 'ZCode sign-in', description: 'Uses the installed ZCode agent and its existing sign-in.' }] }); return;
    case 'session/new': {
      if (sessions.size) throw new Error('This ZCode connection already has a chat.');
      if (typeof params.cwd !== 'string' || !params.cwd) throw new Error('ZCode requires an existing workspace.');
      await publishAccountMetadata();
      const result = await request('session/create', { workspace: { workspaceKey: params.cwd, workspacePath: params.cwd },
        mode: 'build', ...(params.mcpServers?.length ? { mcpServers: params.mcpServers } : {}) });
      const sessionId = result?.session?.sessionId ?? result?.sessionId;
      if (!sessionId) throw new Error('ZCode did not return a chat identifier.');
      const models = new Map((result.settings?.model?.available ?? []).filter(entry =>
        typeof entry.ref?.providerId === 'string' && typeof entry.ref?.modelId === 'string').slice(0, 1024)
        .map(entry => [modelValue(entry.ref), entry]));
      const current = result.settings?.model?.current;
      const session = { turn: null, seq: -1, source: null, text: false, cancelled: false, tools: new Set(),
        models, model: current?.providerId && current?.modelId ? modelValue(current) : '' };
      sessions.set(sessionId, session);
      if (!session.model && models.size) await selectModel(sessionId, session, models.keys().next().value);
      await request('session/subscribe', { sessionId, deliveryKind: 'desktop-continuous', includeSnapshot: false });
      response(message.id, { sessionId, configOptions: modelOptions(session) }); return;
    }
    case 'session/set_config_option': {
      const session = sessions.get(params.sessionId);
      if (!session || params.configId !== 'model') throw new Error('Unknown ZCode configuration option.');
      await selectModel(params.sessionId, session, params.value);
      response(message.id, { configOptions: modelOptions(session) }); return;
    }
    case 'session/prompt': {
      const session = sessions.get(params.sessionId);
      if (!session || session.turn !== null) throw new Error('The ZCode chat is unavailable or already running.');
      if (!Array.isArray(params.prompt) || params.prompt.some(block => block.type !== 'text')) throw new Error('ZCode currently accepts text prompts.');
      session.turn = message.id; session.source = null; session.text = false; session.cancelled = false; session.tools.clear();
      try { await request('session/send', { sessionId: params.sessionId,
        content: params.prompt.map(block => block.text).join('\n'), inputId: randomUUID() }); }
      catch (error) { finish(session, error); } return;
    }
    case 'session/cancel': {
      const session = sessions.get(params.sessionId);
      if (!session) return;
      session.cancelled = true;
      for (const [id, entry] of permissions) if (entry.sessionId === params.sessionId) {
        permissions.delete(id); write(backend.stdin, { id: entry.backendId, result: { decision: 'deny', reason: 'Cancelled.' } });
      }
      try { await request('session/stop', { sessionId: params.sessionId }, 5000); } finally { finish(session, null, 'cancelled'); }
      return;
    }
    default: if (message.id !== undefined) write(process.stdout, { jsonrpc: '2.0', id: message.id,
      error: { code: -32601, message: `Unsupported ACP method: ${message.method}` } });
  }
}
lines(backend.stdout, backendEvent);
lines(process.stdin, clientEvent);
backend.on('error', close);
backend.on('exit', () => { close(); process.stdin.destroy(); });
process.stdin.on('end', () => close());
process.on('SIGTERM', () => close());
process.on('SIGINT', () => close());
process.stdout.on('error', () => close());
