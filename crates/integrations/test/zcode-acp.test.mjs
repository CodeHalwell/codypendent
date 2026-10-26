import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createCipheriv, createHash, randomBytes } from 'node:crypto';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { test } from 'node:test';

const bridge = new URL('../src/zcode_acp.mjs', import.meta.url);
const fixture = `
const readline = require('node:readline');
const send = value => process.stdout.write(JSON.stringify(value) + '\\n');
let seq = 0;
const event = (type, payload) => send({method:'session/event', params:{sessionId:'z-test',seq:++seq,type,payload}});
readline.createInterface({input:process.stdin}).on('line', line => {
 const value = JSON.parse(line);
 if (value.jsonrpc !== undefined) throw new Error('ZCode does not accept a jsonrpc field');
 if (value.method === 'session/create') {
  if (value.params.mode !== 'build' || value.params.mcpServers[0].name !== 'test-host') throw new Error('Lost policy mode or MCP server');
  send({id:'prefs',method:'session/requestRuntimePreferences',params:{}});
  send({id:value.id,result:{session:{sessionId:'z-test'}}});
 } else if (value.method === 'session/subscribe') {
  if(value.params.deliveryKind !== 'desktop-continuous') throw new Error('Wrong delivery contract');
  send({id:value.id,result:{}});
 } else if (value.method === 'session/send') {
  send({id:value.id,result:{accepted:true}});
  event('model.streaming',{kind:'reasoning_delta',delta:'private planning'});
  event('model.streaming',{kind:'text_delta',delta:'Visible answer'});
  event('part.delta',{field:'text',delta:'Visible answer'});
  event('tool.updated',{toolCallId:'tool-1',toolName:'desktop.test',kind:'scheduled',input:{value:3}});
  send({id:'ask-1',method:'interaction/requestPermission',params:{sessionId:'z-test',toolCallId:'tool-1',toolName:'desktop.test',input:{value:3},options:[{optionId:'persistent',kind:'allow_always',name:'Always allow',response:{decision:'allow',permissionUpdates:[{dangerous:true}]}}]}});
 } else if(value.method === 'session/stop') {
  send({id:value.id,result:{}});
 } else if(value.id === 'ask-1') {
  if (value.result.decision !== 'deny' || value.result.permissionUpdates !== undefined) throw new Error('Approval was widened');
  event('tool.updated',{toolCallId:'tool-1',toolName:'desktop.test',kind:'error',error:{message:'Denied'}});
  event('turn.completed',{response:'Visible answer'});
 }
});
`;

test('ZCode brokers only active advertised vendor auth and never exposes its key to ACP', { timeout: 10000 }, async () => {
  const directory = await mkdtemp(join(tmpdir(), 'cpd-zcode-auth-'));
  const secret = 'fixture-machine-secret';
  const sentinel = 'fixture-zcode-secret-never-in-acp';
  const providerId = 'account:zai-individual-coding-plan';
  const modelId = 'GLM-5.3-Flash';
  const iv = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', createHash('sha256').update(secret).digest(), iv);
  const encrypted = Buffer.concat([cipher.update(sentinel), cipher.final()]);
  await writeFile(join(directory, 'credentials.json'), JSON.stringify({
    [`account-provider:coding-plan:account:zai-individual-coding-plan:account:fixture:api-key`]:
      `enc:v1:${iv.toString('base64url')}.${cipher.getAuthTag().toString('base64url')}.${encrypted.toString('base64url')}`,
  }));
  await writeFile(join(directory, 'coding-plan-cache.json'), JSON.stringify({ entryStatus: { items: {
    'builtin:zai-coding-plan': { status: 'available' },
  } } }));
  await writeFile(join(directory, 'builtin.json'), JSON.stringify({ revision: 1, config: { providerConfigRules: { providerRules: [
    { providerId, config: { builtinModelIds: [modelId], access: { type: 'zhipu-account', mode: 'individual-coding-plan', accountType: 'zai' }, api: { baseUrl: 'https://api.z.ai/api/anthropic' } } },
  ] } } }));
  const path = join(directory, 'zcode.cjs');
  await writeFile(path, `
const readline = require('node:readline');
const send = value => process.stdout.write(JSON.stringify(value)+'\\n');
const ref = ${JSON.stringify({ providerId, modelId })}; let replies = 0, turns = 0, stopId, lateReplies = 0;
const authParams = {sessionId:'auth-chat',providerId:ref.providerId,modelSelection:ref};
readline.createInterface({input:process.stdin}).on('line', line => {
 const m = JSON.parse(line);
 if(m.method === 'provider/updateAccountConfig' || m.method === 'session/subscribe') send({id:m.id,result:{}});
 else if(m.method === 'session/create') send({id:m.id,result:{session:{sessionId:'auth-chat'},settings:{model:{current:ref,available:[{ref}]}}}});
 else if(m.method === 'session/send') {
  send({id:m.id,result:{accepted:true}});
  if(++turns > 1) { send({method:'session/event',params:{sessionId:'auth-chat',seq:2,type:'model.streaming',payload:{kind:'text_delta',delta:'Waiting'}}}); return; }
  const params = authParams;
  for(const [id,patch] of [['wrong-session',{sessionId:'other'}],['wrong-provider',{providerId:'account:other'}],['wrong-origin',{targetOrigin:'https://example.test'}],['valid',{}]])
   send({id,method:'interaction/requestProviderRuntimeHeaders',params:{...params,...patch}});
 } else if(m.id === 'valid' || String(m.id).startsWith('wrong-')) {
  if(m.id === 'valid') { if(m.result.requestAuth?.apiKey !== ${JSON.stringify(sentinel)}) throw new Error('Valid request did not receive its key'); }
  else if(m.result.headersApplied !== false || m.result.requestAuth) throw new Error('Unrelated request received auth');
  if(++replies === 4) send({method:'session/event',params:{sessionId:'auth-chat',seq:1,type:'turn.completed',payload:{response:'Ready.'}}});
 } else if(m.method === 'session/stop') {
  stopId = m.id;
  send({id:'late-auth',method:'interaction/requestProviderRuntimeHeaders',params:authParams});
  send({id:'late-permission',method:'interaction/requestPermission',params:{sessionId:'auth-chat',toolCallId:'late-tool',toolName:'write'}});
 } else if(m.id === 'late-auth' || m.id === 'late-permission') {
  if(m.id === 'late-auth' && (m.result.headersApplied !== false || m.result.requestAuth)) throw new Error('Cancelled turn received auth');
  if(m.id === 'late-permission' && m.result.decision !== 'deny') throw new Error('Cancelled turn received permission');
  if(++lateReplies === 2) send({id:stopId,result:{}});
 }
});
`);
  const child = spawn(process.execPath, [bridge.pathname], { env: { ...process.env,
    CODYPENDENT_ZCODE_PATH: path, ZCODE_BUILTIN_PROVIDER_CONFIG_FILE: join(directory, 'builtin.json'),
    ZCODE_PERSONAL_PROVIDER_CONFIG_FILE: join(directory, 'personal.json'), ZCODE_CREDENTIAL_SECRET: secret,
  }, stdio: ['pipe', 'pipe', 'pipe'] });
  const messages = [], waiting = [];
  let output = '', stderr = '';
  child.stderr.on('data', chunk => { stderr += chunk; });
  createInterface({ input: child.stdout }).on('line', line => {
    output += line;
    const value = JSON.parse(line);
    if (waiting.length) waiting.shift()(value); else messages.push(value);
  });
  const next = () => messages.length ? Promise.resolve(messages.shift()) : new Promise(resolve => waiting.push(resolve));
  const send = value => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', ...value })+'\n');
  try {
    send({ id: 1, method: 'session/new', params: { cwd: directory, mcpServers: [] } });
    assert.equal((await next()).result.sessionId, 'auth-chat');
    send({ id: 2, method: 'session/prompt', params: { sessionId: 'auth-chat', prompt: [{ type: 'text', text: 'Ready?' }] } });
    for (;;) { const message = await next(); if (message.id === 2) { assert.equal(message.result.stopReason, 'end_turn'); break; } }
    send({ id: 3, method: 'session/prompt', params: { sessionId: 'auth-chat', prompt: [{ type: 'text', text: 'Wait' }] } });
    for (;;) { const message = await next(); if (message.params?.update?.content?.text === 'Waiting') break; }
    send({ method: 'session/cancel', params: { sessionId: 'auth-chat' } });
    for (;;) {
      const message = await next();
      assert.notEqual(message.method, 'session/request_permission');
      if (message.id === 3) { assert.equal(message.result.stopReason, 'cancelled'); break; }
    }
    assert.equal(output.includes(sentinel), false);
    assert.equal(stderr, '');
  } finally {
    child.stdin.end();
    await new Promise(resolve => { child.once('exit', resolve); setTimeout(() => { child.kill('SIGKILL'); resolve(); }, 1000).unref(); });
    await rm(directory, { recursive: true, force: true });
  }
});

test('ZCode preserves MCP, explicit approval, thought flags and cancellation', { timeout: 10000 }, async () => {
  const directory = await mkdtemp(join(tmpdir(), 'cpd-zcode-test-'));
  const path = join(directory, 'zcode.cjs');
  await writeFile(path, fixture);
  const child = spawn(process.execPath, [bridge.pathname], {
    env: { ...process.env, CODYPENDENT_ZCODE_PATH: path }, stdio: ['pipe', 'pipe', 'pipe'],
  });
  const queued = [];
  const waiting = [];
  let stderr = '';
  child.stderr.on('data', chunk => { stderr += chunk; });
  createInterface({ input: child.stdout }).on('line', line => {
    const value = JSON.parse(line);
    if (waiting.length) waiting.shift()(value); else queued.push(value);
  });
  const next = () => queued.length ? Promise.resolve(queued.shift()) : new Promise(resolve => waiting.push(resolve));
  const send = value => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\n');
  try {
    send({ id: 1, method: 'initialize', params: { protocolVersion: 1 } });
    assert.equal((await next()).result.agentInfo.name, 'zcode');
    send({ id: 2, method: 'session/new', params: { cwd: directory,
      mcpServers: [{ name: 'test-host', command: '/test/host', args: [], env: [] }] } });
    assert.equal((await next()).result.sessionId, 'z-test');
    send({ id: 3, method: 'session/prompt', params: { sessionId: 'z-test', prompt: [{ type: 'text', text: 'test' }] } });
    const received = [];
    for (;;) {
      const value = await next(); received.push(value);
      if (value.method === 'session/request_permission') {
        assert.deepEqual(value.params.options.map(option => option.kind), ['allow_once', 'reject_once']);
        send({ id: value.id, result: { outcome: { outcome: 'selected', optionId: 'deny' } } });
      }
      if (value.id === 3) { assert.equal(value.result.stopReason, 'end_turn'); break; }
    }
    const chunks = received.filter(value => value.method === 'session/update').map(value => value.params.update);
    assert.equal(chunks.filter(value => value.sessionUpdate === 'agent_message_chunk').length, 1);
    assert.equal(chunks.find(value => value.sessionUpdate === 'agent_thought_chunk').content.text, 'private planning');
    assert.equal(chunks.find(value => value.sessionUpdate === 'tool_call').rawInput.value, 3);
    assert.equal(chunks.find(value => value.sessionUpdate === 'tool_call_update').status, 'failed');
    send({ id: 4, method: 'session/prompt', params: { sessionId: 'z-test', prompt: [{ type: 'text', text: 'cancel' }] } });
    for (;;) { if ((await next()).method === 'session/request_permission') break; }
    send({ method: 'session/cancel', params: { sessionId: 'z-test' } });
    let cancelled;
    for (;;) { const value = await next(); if (value.id === 4) { cancelled = value; break; } }
    assert.equal(cancelled.result.stopReason, 'cancelled');
    assert.equal(stderr, '');
  } finally {
    child.stdin.end();
    await new Promise(resolve => { child.once('exit', resolve); setTimeout(() => { child.kill('SIGKILL'); resolve(); }, 1000).unref(); });
    await rm(directory, { recursive: true, force: true });
  }
});
