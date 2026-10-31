import { resolveRuntimePaths, type Env } from "@codypendent/protocol/node";

/** Wire discovery stays shared; this branded client pins its own default product. */
export {
  DiscoveryError,
  MAX_SOCKET_PATH_BYTES,
  platformDataDir,
  resolveRuntimePaths,
  resolveSocketPath,
  validateSocketPath,
  type Env,
  type RuntimePaths,
} from "@codypendent/protocol/node";

/** Explicit connections win; an inherited embedding-host selector is not one. */
export function resolveClientSocketPath(
  selection: { handoff?: string; attached?: string; configured?: string } = {},
  env: Env = process.env,
  platform: NodeJS.Platform = process.platform,
): string {
  return selection.handoff ?? selection.attached ?? (selection.configured?.trim() || undefined)
    ?? resolveRuntimePaths(env, platform, "codypendent").socketPath;
}
