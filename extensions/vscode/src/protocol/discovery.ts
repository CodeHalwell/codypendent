/**
 * Daemon socket discovery — mirrors `crates/protocol/src/discovery.rs`.
 *
 * Discovery is part of the protocol contract: a client MUST resolve the same
 * socket path as the daemon, with no coordination other than this logic.
 *
 * Socket resolution order (from discovery.rs):
 *   1. `CODYPENDENT_SOCKET`                         — explicit override.
 *   2. `<CODYPENDENT_DATA_DIR>/run/daemon.sock`     — when the data dir is
 *                                                     overridden, everything
 *                                                     stays under it.
 *   3. `$XDG_RUNTIME_DIR/codypendent/daemon.sock`   — short, user-private.
 *   4. `<platform data dir>/run/daemon.sock`        — fallback.
 *
 * The Rust daemon derives the platform data dir with
 * `directories::ProjectDirs::from("", "", "codypendent").data_dir()`. This
 * module reproduces that per-platform:
 *   - Linux:   `$XDG_DATA_HOME/codypendent` (if absolute) else
 *              `$HOME/.local/share/codypendent`
 *   - macOS:   `$HOME/Library/Application Support/codypendent`
 *   - Windows: `%APPDATA%\codypendent\data`
 */
import * as os from "node:os";
import * as path from "node:path";

/** Conservative bound below the platform SUN_LEN limits — `discovery.rs`. */
export const MAX_SOCKET_PATH_BYTES = 100;

export interface RuntimePaths {
  dataDir: string;
  runDir: string;
  socketPath: string;
  pidPath: string;
  logDir: string;
}

export class DiscoveryError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "DiscoveryError";
  }
}

/** Env source; injectable so the resolution order is unit-testable. */
export type Env = Record<string, string | undefined>;

function homeDir(env: Env): string {
  const home = env.HOME ?? env.USERPROFILE ?? os.homedir();
  if (!home) {
    throw new DiscoveryError("cannot determine a home directory for the current user");
  }
  return home;
}

/**
 * Reproduce `ProjectDirs::from("", "", "codypendent").data_dir()`.
 */
export function platformDataDir(env: Env = process.env, platform: NodeJS.Platform = process.platform): string {
  if (platform === "darwin") {
    return path.join(homeDir(env), "Library", "Application Support", "codypendent");
  }
  if (platform === "win32") {
    const appData = env.APPDATA;
    const base = appData && appData.length > 0 ? appData : path.join(homeDir(env), "AppData", "Roaming");
    return path.join(base, "codypendent", "data");
  }
  // Linux / other unix: XDG Base Directory, ignoring a non-absolute XDG_DATA_HOME
  // exactly as the `directories` crate does.
  const xdg = env.XDG_DATA_HOME;
  const base = xdg && path.isAbsolute(xdg) ? xdg : path.join(homeDir(env), ".local", "share");
  return path.join(base, "codypendent");
}

/**
 * Resolve every runtime path from the environment. Defaults to the live process
 * environment and platform; both are injectable for tests.
 *
 * @throws {DiscoveryError} on a missing home directory or an over-long socket
 *   path (matching discovery.rs `resolve()`, which validates before returning).
 */
export function resolveRuntimePaths(
  env: Env = process.env,
  platform: NodeJS.Platform = process.platform,
): RuntimePaths {
  const dataDirOverride = env.CODYPENDENT_DATA_DIR;
  const dataDir = dataDirOverride ?? platformDataDir(env, platform);

  let socketPath: string;
  if (env.CODYPENDENT_SOCKET) {
    socketPath = env.CODYPENDENT_SOCKET;
  } else if (dataDirOverride) {
    socketPath = path.join(dataDir, "run", "daemon.sock");
  } else if (env.XDG_RUNTIME_DIR) {
    socketPath = path.join(env.XDG_RUNTIME_DIR, "codypendent", "daemon.sock");
  } else {
    socketPath = path.join(dataDir, "run", "daemon.sock");
  }

  const runDir = path.dirname(socketPath);
  const paths: RuntimePaths = {
    dataDir,
    runDir,
    socketPath,
    pidPath: path.join(runDir, "daemon.pid"),
    logDir: path.join(dataDir, "logs"),
  };
  validateSocketPath(paths.socketPath);
  return paths;
}

/** Convenience: just the socket path (what a client needs to connect). */
export function resolveSocketPath(
  env: Env = process.env,
  platform: NodeJS.Platform = process.platform,
): string {
  return resolveRuntimePaths(env, platform).socketPath;
}

/**
 * Fail early with an actionable error rather than letting `connect` fail with an
 * opaque SUN_LEN error — `discovery.rs::validate_socket_path`.
 */
export function validateSocketPath(socketPath: string): void {
  const length = Buffer.byteLength(socketPath, "utf8");
  if (length > MAX_SOCKET_PATH_BYTES) {
    throw new DiscoveryError(
      `socket path \`${socketPath}\` is ${length} bytes; Unix domain socket paths are limited to ` +
        "roughly 104-108 bytes. Set CODYPENDENT_SOCKET to a shorter path (for example under /tmp) " +
        "or use a shorter CODYPENDENT_DATA_DIR.",
    );
  }
}
