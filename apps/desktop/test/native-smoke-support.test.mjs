import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { test } from 'node:test';
import path from 'node:path';
import { fixtureEnvironment, stopOwnedChildren } from './native-smoke-support.mjs';

test('native fixture pins every state home and drops inherited product/provider credentials', () => {
  const root = '/tmp/cody-native-isolation';
  const env = fixtureEnvironment(root, '/build/codypendent', {
    HOME: '/real-home', AGENT_RUNTIME_PRODUCT: 'son-of-anton',
    SON_OF_ANTON_DATA_DIR: '/real-son', SON_OF_ANTON_CONFIG_DIR: '/real-son-config', SON_OF_ANTON_SOCKET: '/real-son.sock',
    CODYPENDENT_DATA_DIR: '/real-cody', CODYPENDENT_CONFIG_DIR: '/real-cody-config', CODYPENDENT_SOCKET: '/real-cody.sock',
    OPENAI_API_KEY: 'fixture-secret-not-a-real-key', ANTHROPIC_API_KEY: 'fixture-other-secret',
    PATH: '/toolchain/bin', DISPLAY: ':42', XAUTHORITY: '/display/auth',
    HTTPS_PROXY: 'http://fixture-proxy:8080', NODE_EXTRA_CA_CERTS: '/trust/ca.pem',
  });
  assert.equal(env.AGENT_RUNTIME_PRODUCT, 'codypendent');
  for (const key of ['HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME',
    'XDG_RUNTIME_DIR', 'TMPDIR', 'CODYPENDENT_DATA_DIR', 'CODYPENDENT_CONFIG_DIR', 'CODYPENDENT_SOCKET']) {
    assert.ok(env[key].startsWith(root + path.sep), `${key} belongs to the fixture`);
  }
  assert.equal(env.CODYPENDENT_BINARY, '/build/codypendent');
  assert.equal(env.OPENAI_API_KEY, undefined);
  assert.equal(env.ANTHROPIC_API_KEY, undefined);
  assert.ok(Object.keys(env).every(key => !key.startsWith('SON_OF_ANTON_')));
  for (const [key, value] of Object.entries({ PATH: '/toolchain/bin', DISPLAY: ':42', XAUTHORITY: '/display/auth',
    HTTPS_PROXY: 'http://fixture-proxy:8080', NODE_EXTRA_CA_CERTS: '/trust/ca.pem' })) assert.equal(env[key], value);
});

async function readyChild(ignoreTerm = false) {
  const child = spawn(process.execPath, ['-e', `${ignoreTerm ? "process.on('SIGTERM', () => {});" : ''}
    process.stdout.write('ready'); setInterval(() => {}, 1000);`],
  { detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('exit', () => reject(new Error('fixture exited before ready')));
    child.stdout.once('data', resolve);
  });
  return child;
}

test('native cleanup waits for owned exits, escalates and leaves other processes alive',
  { skip: process.platform !== 'linux', timeout: 10_000 }, async () => {
    const children = [];
    try {
      const graceful = await readyChild(); children.push(graceful);
      const stubborn = await readyChild(true); children.push(stubborn);
      const unrelated = await readyChild(); children.push(unrelated);
      await stopOwnedChildren([graceful, stubborn], { graceMs: 100, killMs: 1_000 });
      assert.notEqual(graceful.signalCode, null, 'graceful child has exited before state can be deleted');
      assert.equal(stubborn.signalCode, 'SIGKILL', 'an ignored TERM is bounded');
      assert.equal(unrelated.exitCode, null);
      assert.equal(unrelated.signalCode, null, 'unlisted fixture process remains alive');
      // Already-exited children are safe to pass again, without rediscovery.
      await stopOwnedChildren([graceful, stubborn], { graceMs: 100, killMs: 1_000 });
    } finally {
      await stopOwnedChildren(children, { graceMs: 100, killMs: 1_000 });
    }
  });
