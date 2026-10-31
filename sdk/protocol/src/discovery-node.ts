/**
 * Daemon socket discovery — mirrors `crates/protocol/src/discovery.rs`.
 *
 * Discovery is part of the protocol contract: a client MUST resolve the same
 * socket path as the daemon, with no coordination other than this logic.
 *
 * `AGENT_RUNTIME_PRODUCT` selects `codypendent` (the default) or `son-of-anton`.
 * An explicit product argument takes precedence. Each product reads only its
 * own `CODYPENDENT_*` or `SON_OF_ANTON_*` path overrides.
 *
 * Socket resolution order (from discovery.rs), for the selected product:
 *   1. `<PREFIX>_SOCKET`                           — explicit override.
 *   2. `<PREFIX>_DATA_DIR` + `/run/daemon.sock`     — isolated data override.
 *   3. `$XDG_RUNTIME_DIR/<product>/daemon.sock`     — short, user-private.
 *   4. `<platform data dir>/run/daemon.sock`        — fallback.
 *
 * The Rust daemon derives the platform data dir with
 * `directories::ProjectDirs::from("", "", product).data_dir()`. This
 * module reproduces that per-platform:
 *   - Linux:   `$XDG_DATA_HOME/<product>` (if absolute) else
 *              `$HOME/.local/share/<product>`
 *   - macOS:   `$HOME/Library/Application Support/<product>`
 *   - Windows: `%APPDATA%\<product>\data`
 */
import * as os from "node:os";
import * as path from "node:path";

/** Conservative bound below the platform SUN_LEN limits — `discovery.rs`. */
export const MAX_SOCKET_PATH_BYTES = 100;

export type RuntimeProduct = "codypendent" | "son-of-anton";

export interface RuntimePaths {
  dataDir: string;
  configDir: string;
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

/**
 * An environment override, or `undefined` when unset, empty, or whitespace-only.
 * Mirrors the Rust `non_empty_env`: an empty `CODYPENDENT_DATA_DIR` must not make
 * the data dir the empty path (which would yield relative socket paths).
 */
function nonEmpty(value: string | undefined): string | undefined {
  return value !== undefined && value.trim().length > 0 ? value : undefined;
}

function runtimeProduct(env: Env, product?: RuntimeProduct): RuntimeProduct {
  const selected = product ?? nonEmpty(env.AGENT_RUNTIME_PRODUCT)?.trim() ?? "codypendent";
  if (selected !== "codypendent" && selected !== "son-of-anton") {
    throw new DiscoveryError(`unknown runtime product \`${selected}\`; expected codypendent or son-of-anton`);
  }
  return selected;
}

function environmentPrefix(product: RuntimeProduct): "CODYPENDENT" | "SON_OF_ANTON" {
  return product === "son-of-anton" ? "SON_OF_ANTON" : "CODYPENDENT";
}

function homeDir(env: Env): string {
  const home = env.HOME ?? env.USERPROFILE ?? os.homedir();
  if (!home) {
    throw new DiscoveryError("cannot determine a home directory for the current user");
  }
  return home;
}

/**
 * Reproduce `ProjectDirs::from("", "", product).data_dir()`.
 */
export function platformDataDir(
  env: Env = process.env,
  platform: NodeJS.Platform = process.platform,
  product?: RuntimeProduct,
): string {
  return platformDirectory(env, platform, runtimeProduct(env, product), "data");
}

function platformDirectory(
  env: Env,
  platform: NodeJS.Platform,
  product: RuntimeProduct,
  kind: "data" | "config",
): string {
  if (platform === "darwin") {
    return path.join(homeDir(env), "Library", "Application Support", product);
  }
  if (platform === "win32") {
    const appData = env.APPDATA;
    const base = appData && appData.length > 0 ? appData : path.join(homeDir(env), "AppData", "Roaming");
    return path.join(base, product, kind);
  }
  // Linux / other unix: XDG Base Directory, ignoring relative XDG directories
  // exactly as the `directories` crate does.
  const xdg = kind === "data" ? env.XDG_DATA_HOME : env.XDG_CONFIG_HOME;
  const fallback = kind === "data" ? [".local", "share"] : [".config"];
  const base = xdg && path.isAbsolute(xdg) ? xdg : path.join(homeDir(env), ...fallback);
  return path.join(base, product);
}

/**
 * Resolve every runtime path from the environment. Defaults to the live process
 * environment and platform; both are injectable for tests.
 *
 * @throws {DiscoveryError} on an unknown product, a missing home directory or an
 *   over-long socket path (matching discovery.rs `resolve()`).
 */
export function resolveRuntimePaths(
  env: Env = process.env,
  platform: NodeJS.Platform = process.platform,
  product?: RuntimeProduct,
): RuntimePaths {
  const selected = runtimeProduct(env, product);
  const prefix = environmentPrefix(selected);
  const dataDirOverride = nonEmpty(env[`${prefix}_DATA_DIR`]);
  const configDirOverride = nonEmpty(env[`${prefix}_CONFIG_DIR`]);
  const socketOverride = nonEmpty(env[`${prefix}_SOCKET`]);
  const runtimeDir = nonEmpty(env.XDG_RUNTIME_DIR);
  const dataDir = dataDirOverride ?? platformDataDir(env, platform, selected);
  const configDir = configDirOverride ?? (dataDirOverride
    ? path.join(dataDir, "config")
    : platformDirectory(env, platform, selected, "config"));

  let socketPath: string;
  if (socketOverride) {
    socketPath = socketOverride;
  } else if (dataDirOverride) {
    socketPath = path.join(dataDir, "run", "daemon.sock");
  } else if (runtimeDir) {
    socketPath = path.join(runtimeDir, selected, "daemon.sock");
  } else {
    socketPath = path.join(dataDir, "run", "daemon.sock");
  }

  const runDir = path.dirname(socketPath);
  const paths: RuntimePaths = {
    dataDir,
    configDir,
    runDir,
    socketPath,
    pidPath: path.join(runDir, "daemon.pid"),
    logDir: path.join(dataDir, "logs"),
  };
  validateSocketPath(paths.socketPath, selected);
  return paths;
}

/** Convenience: just the socket path (what a client needs to connect). */
export function resolveSocketPath(
  env: Env = process.env,
  platform: NodeJS.Platform = process.platform,
  product?: RuntimeProduct,
): string {
  return resolveRuntimePaths(env, platform, product).socketPath;
}

/**
 * Fail early with an actionable error rather than letting `connect` fail with an
 * opaque SUN_LEN error — `discovery.rs::validate_socket_path`.
 */
export function validateSocketPath(socketPath: string, product: RuntimeProduct = "codypendent"): void {
  const length = Buffer.byteLength(socketPath, "utf8");
  if (length > MAX_SOCKET_PATH_BYTES) {
    const prefix = environmentPrefix(product);
    throw new DiscoveryError(
      `socket path \`${socketPath}\` is ${length} bytes; Unix domain socket paths are limited to ` +
        `roughly 104-108 bytes. Set ${prefix}_SOCKET to a shorter path (for example under /tmp) ` +
        `or use a shorter ${prefix}_DATA_DIR.`,
    );
  }
}
