import * as path from "node:path";
import { describe, expect, it } from "vitest";
import {
  DiscoveryError, platformDataDir, resolveRuntimePaths, resolveSocketPath,
  type Env, type RuntimeProduct,
} from "../src/node.js";

const home = "/users/dev";

describe("runtime product discovery", () => {
  it.each([undefined, "", " \t", "codypendent"])("preserves Codypendent defaults for selector %j", (selector) => {
    const paths = resolveRuntimePaths({ HOME: home, AGENT_RUNTIME_PRODUCT: selector }, "linux");
    expect(paths).toEqual({
      dataDir: path.join(home, ".local", "share", "codypendent"),
      configDir: path.join(home, ".config", "codypendent"),
      runDir: path.join(home, ".local", "share", "codypendent", "run"),
      socketPath: path.join(home, ".local", "share", "codypendent", "run", "daemon.sock"),
      pidPath: path.join(home, ".local", "share", "codypendent", "run", "daemon.pid"),
      logDir: path.join(home, ".local", "share", "codypendent", "logs"),
    });
  });

  it("ignores inherited Codypendent state when selecting Son Of Anton", () => {
    const env: Env = {
      HOME: home, AGENT_RUNTIME_PRODUCT: " son-of-anton ", XDG_RUNTIME_DIR: "/run/user/9",
      CODYPENDENT_DATA_DIR: "/cody/data", CODYPENDENT_CONFIG_DIR: "/cody/config",
      CODYPENDENT_SOCKET: "/cody/daemon.sock",
    };
    expect(resolveRuntimePaths(env, "linux")).toEqual({
      dataDir: path.join(home, ".local", "share", "son-of-anton"),
      configDir: path.join(home, ".config", "son-of-anton"),
      runDir: path.join("/run/user/9", "son-of-anton"),
      socketPath: path.join("/run/user/9", "son-of-anton", "daemon.sock"),
      pidPath: path.join("/run/user/9", "son-of-anton", "daemon.pid"),
      logDir: path.join(home, ".local", "share", "son-of-anton", "logs"),
    });
  });

  it("ignores Son Of Anton overrides for the default Codypendent profile", () => {
    const env = {
      HOME: home, SON_OF_ANTON_DATA_DIR: "/anton/data", SON_OF_ANTON_CONFIG_DIR: "/anton/config",
      SON_OF_ANTON_SOCKET: "/anton/daemon.sock",
    };
    expect(resolveRuntimePaths(env, "linux")).toEqual(resolveRuntimePaths({ HOME: home }, "linux"));
  });

  it.each<RuntimeProduct>(["codypendent", "son-of-anton"])("reads only %s overrides", (product) => {
    const env = {
      HOME: home, XDG_RUNTIME_DIR: "/run/user/9",
      CODYPENDENT_DATA_DIR: "/cody/data", CODYPENDENT_CONFIG_DIR: "/cody/config",
      CODYPENDENT_SOCKET: "/cody/daemon.sock",
      SON_OF_ANTON_DATA_DIR: "/anton/data", SON_OF_ANTON_CONFIG_DIR: "/anton/config",
      SON_OF_ANTON_SOCKET: "/anton/daemon.sock",
    };
    const base = product === "codypendent" ? "/cody" : "/anton";
    expect(resolveRuntimePaths(env, "linux", product)).toEqual({
      dataDir: `${base}/data`, configDir: `${base}/config`, runDir: base,
      socketPath: `${base}/daemon.sock`, pidPath: path.join(base, "daemon.pid"),
      logDir: path.join(base, "data", "logs"),
    });
    expect(resolveSocketPath(env, "linux", product)).toBe(`${base}/daemon.sock`);
  });

  it.each<RuntimeProduct>(["codypendent", "son-of-anton"])("keeps %s config independent from socket and data overrides", (product) => {
    const prefix = product === "codypendent" ? "CODYPENDENT" : "SON_OF_ANTON";
    const env = { HOME: home, XDG_RUNTIME_DIR: "/run/user/9", [`${prefix}_DATA_DIR`]: "/private/data" };
    const paths = resolveRuntimePaths(env, "linux", product);
    expect(paths.configDir).toBe(path.join("/private/data", "config"));
    expect(paths.socketPath).toBe(path.join("/private/data", "run", "daemon.sock"));
    const configured = resolveRuntimePaths({ ...env, [`${prefix}_CONFIG_DIR`]: "/private/config" }, "linux", product);
    expect(configured.configDir).toBe("/private/config");
    expect(configured.dataDir).toBe(paths.dataDir);
    expect(configured.socketPath).toBe(paths.socketPath);
    expect(resolveRuntimePaths({ HOME: home, [`${prefix}_SOCKET`]: "/tmp/agent.sock" }, "linux", product).configDir)
      .toBe(path.join(home, ".config", product));
  });

  it.each(["", " \t"])("ignores empty product path overrides %j", (empty) => {
    const env = {
      HOME: home, XDG_RUNTIME_DIR: "/run/user/9", AGENT_RUNTIME_PRODUCT: "son-of-anton",
      SON_OF_ANTON_DATA_DIR: empty, SON_OF_ANTON_CONFIG_DIR: empty, SON_OF_ANTON_SOCKET: empty,
      CODYPENDENT_DATA_DIR: "/cody/data", CODYPENDENT_CONFIG_DIR: "/cody/config",
      CODYPENDENT_SOCKET: "/cody/daemon.sock",
    };
    const paths = resolveRuntimePaths(env, "linux");
    expect(paths.dataDir).toBe(path.join(home, ".local", "share", "son-of-anton"));
    expect(paths.configDir).toBe(path.join(home, ".config", "son-of-anton"));
    expect(paths.socketPath).toBe(path.join("/run/user/9", "son-of-anton", "daemon.sock"));
  });

  it.each(["unknown", "Codypendent"])("rejects an unknown selector %j instead of opening default state", (selector) => {
    const env = { HOME: home, AGENT_RUNTIME_PRODUCT: selector };
    expect(() => resolveRuntimePaths(env, "linux")).toThrow(DiscoveryError);
    expect(() => resolveSocketPath(env, "linux")).toThrow(DiscoveryError);
    expect(() => platformDataDir(env, "linux")).toThrow(DiscoveryError);
  });

  it.each<RuntimeProduct>(["codypendent", "son-of-anton"])("lets explicit %s override even an invalid environment selector", (product) => {
    const env = { HOME: home, AGENT_RUNTIME_PRODUCT: "unknown" };
    expect(resolveRuntimePaths(env, "linux", product).dataDir)
      .toBe(path.join(home, ".local", "share", product));
    expect(resolveSocketPath(env, "linux", product))
      .toBe(path.join(home, ".local", "share", product, "run", "daemon.sock"));
    expect(platformDataDir(env, "linux", product)).toBe(path.join(home, ".local", "share", product));
  });

  it("uses the selected product in socket-length remediation", () => {
    expect(() => resolveSocketPath({ HOME: home, AGENT_RUNTIME_PRODUCT: "son-of-anton",
      SON_OF_ANTON_SOCKET: `/tmp/${"x".repeat(100)}.sock` }, "linux"))
      .toThrow("SON_OF_ANTON_SOCKET");
  });
});

describe("platform directories", () => {
  it.each<RuntimeProduct>(["codypendent", "son-of-anton"])("matches platform directory rules for %s", (product) => {
    const cases: Array<{ platform: NodeJS.Platform; env: Env; data: string; config: string }> = [
      { platform: "linux", env: { XDG_DATA_HOME: "/xdg/data", XDG_CONFIG_HOME: "/xdg/config" },
        data: path.join("/xdg/data", product), config: path.join("/xdg/config", product) },
      { platform: "linux", env: { XDG_DATA_HOME: "relative/data", XDG_CONFIG_HOME: "relative/config" },
        data: path.join(home, ".local", "share", product), config: path.join(home, ".config", product) },
      { platform: "darwin", env: { XDG_DATA_HOME: "/ignored/data", XDG_CONFIG_HOME: "/ignored/config" },
        data: path.join(home, "Library", "Application Support", product),
        config: path.join(home, "Library", "Application Support", product) },
      { platform: "win32", env: { APPDATA: "/appdata" },
        data: path.join("/appdata", product, "data"), config: path.join("/appdata", product, "config") },
      { platform: "win32", env: {}, data: path.join(home, "AppData", "Roaming", product, "data"),
        config: path.join(home, "AppData", "Roaming", product, "config") },
    ];
    for (const { platform, env, data, config } of cases) {
      const input = { HOME: home, AGENT_RUNTIME_PRODUCT: product, ...env };
      expect(platformDataDir(input, platform)).toBe(data);
      const resolved = resolveRuntimePaths(input, platform);
      expect({ data: resolved.dataDir, config: resolved.configDir }).toEqual({ data, config });
    }
  });
});
