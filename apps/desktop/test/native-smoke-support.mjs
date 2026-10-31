import path from 'node:path';

// Preserve only platform/display/toolchain and proxy trust settings. Provider
// credentials, product selectors and application state never cross this seam.
const PLATFORM_KEYS = [
  'PATH', 'LD_LIBRARY_PATH', 'ORT_LIB_LOCATION', 'LANG', 'LC_ALL', 'TZ',
  'DISPLAY', 'WAYLAND_DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS', 'GDK_BACKEND',
  'LIBGL_ALWAYS_SOFTWARE', 'WEBKIT_DISABLE_COMPOSITING_MODE', 'WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS',
  'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'NO_PROXY', 'http_proxy', 'https_proxy', 'all_proxy', 'no_proxy',
  'SSL_CERT_FILE', 'SSL_CERT_DIR', 'NODE_EXTRA_CA_CERTS', 'REQUESTS_CA_BUNDLE', 'CURL_CA_BUNDLE',
];

export function fixtureEnvironment(directory, cli, inherited = process.env) {
  const env = Object.fromEntries(PLATFORM_KEYS.filter(key => inherited[key] !== undefined).map(key => [key, inherited[key]]));
  return {
    ...env,
    HOME: path.join(directory, 'home'),
    XDG_CONFIG_HOME: path.join(directory, 'config'),
    XDG_DATA_HOME: path.join(directory, 'local-data'),
    XDG_CACHE_HOME: path.join(directory, 'cache'),
    XDG_STATE_HOME: path.join(directory, 'state'),
    XDG_RUNTIME_DIR: path.join(directory, 'run'),
    TMPDIR: path.join(directory, 'tmp'),
    GIT_CONFIG_NOSYSTEM: '1',
    GIT_CONFIG_GLOBAL: path.join(directory, 'home', '.gitconfig'),
    GIT_TERMINAL_PROMPT: '0',
    AGENT_RUNTIME_PRODUCT: 'codypendent',
    CODYPENDENT_DATA_DIR: path.join(directory, 'data'),
    CODYPENDENT_CONFIG_DIR: path.join(directory, 'config', 'codypendent'),
    CODYPENDENT_SOCKET: path.join(directory, 'run', 'daemon.sock'),
    CODYPENDENT_BINARY: cli,
    TAURI_WEBVIEW_AUTOMATION: 'true',
    RUST_LOG: 'warn',
  };
}

const exited = child => !child.pid || child.exitCode !== null || child.signalCode !== null;

function waitForExit(child, timeoutMs) {
  if (exited(child)) return Promise.resolve(true);
  return new Promise(resolve => {
    let timer;
    const finish = result => {
      clearTimeout(timer);
      child.off('exit', onExit);
      resolve(result);
    };
    const onExit = () => finish(true);
    child.once('exit', onExit);
    timer = setTimeout(() => finish(exited(child)), timeoutMs);
    if (exited(child)) finish(true);
  });
}

/** Only detached children created by this fixture are eligible for signaling. */
export async function stopOwnedChildren(children, { graceMs = 2_000, killMs = 2_000 } = {}) {
  for (const child of [...children].reverse()) {
    if (exited(child)) continue;
    const signal = name => {
      if (exited(child)) return;
      try { process.kill(-child.pid, name); }
      catch (error) { if (error.code !== 'ESRCH') throw error; }
    };
    signal('SIGTERM');
    if (await waitForExit(child, graceMs)) continue;
    signal('SIGKILL');
    if (!await waitForExit(child, killMs)) {
      throw new Error(`Fixture child ${child.pid} did not exit; temporary state must be retained.`);
    }
  }
}
