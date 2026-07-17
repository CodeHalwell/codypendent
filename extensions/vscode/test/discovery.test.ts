import * as path from "node:path";
import { describe, expect, it } from "vitest";

import {
  DiscoveryError,
  MAX_SOCKET_PATH_BYTES,
  platformDataDir,
  resolveRuntimePaths,
  validateSocketPath,
  type Env,
} from "../src/protocol/discovery.js";

// Resolution order mirrors crates/protocol/src/discovery.rs exactly.

describe("resolveRuntimePaths socket resolution order", () => {
  it("1. CODYPENDENT_SOCKET overrides everything", () => {
    const env: Env = {
      CODYPENDENT_SOCKET: "/tmp/custom.sock",
      CODYPENDENT_DATA_DIR: "/data",
      XDG_RUNTIME_DIR: "/run/user/1000",
      HOME: "/home/dana",
    };
    expect(resolveRuntimePaths(env, "linux").socketPath).toBe("/tmp/custom.sock");
  });

  it("2. CODYPENDENT_DATA_DIR keeps the socket under <data>/run", () => {
    const env: Env = {
      CODYPENDENT_DATA_DIR: "/data/cody",
      XDG_RUNTIME_DIR: "/run/user/1000",
      HOME: "/home/dana",
    };
    const paths = resolveRuntimePaths(env, "linux");
    expect(paths.socketPath).toBe(path.join("/data/cody", "run", "daemon.sock"));
    expect(paths.pidPath).toBe(path.join("/data/cody", "run", "daemon.pid"));
    expect(paths.logDir).toBe(path.join("/data/cody", "logs"));
  });

  it("3. XDG_RUNTIME_DIR is used when no data-dir override is set", () => {
    const env: Env = {
      XDG_RUNTIME_DIR: "/run/user/1000",
      HOME: "/home/dana",
    };
    expect(resolveRuntimePaths(env, "linux").socketPath).toBe(
      path.join("/run/user/1000", "codypendent", "daemon.sock"),
    );
  });

  it("4. falls back to <platform data dir>/run/daemon.sock", () => {
    const env: Env = { HOME: "/home/dana" };
    expect(resolveRuntimePaths(env, "linux").socketPath).toBe(
      path.join("/home/dana", ".local", "share", "codypendent", "run", "daemon.sock"),
    );
  });
});

describe("platformDataDir mirrors the directories crate", () => {
  it("linux honours an absolute XDG_DATA_HOME", () => {
    expect(platformDataDir({ XDG_DATA_HOME: "/xdg/data", HOME: "/home/dana" }, "linux")).toBe(
      path.join("/xdg/data", "codypendent"),
    );
  });

  it("linux ignores a non-absolute XDG_DATA_HOME", () => {
    expect(platformDataDir({ XDG_DATA_HOME: "relative/data", HOME: "/home/dana" }, "linux")).toBe(
      path.join("/home/dana", ".local", "share", "codypendent"),
    );
  });

  it("macOS uses Application Support", () => {
    expect(platformDataDir({ HOME: "/Users/dana" }, "darwin")).toBe(
      path.join("/Users/dana", "Library", "Application Support", "codypendent"),
    );
  });

  it("windows uses %APPDATA%\\codypendent\\data", () => {
    expect(platformDataDir({ APPDATA: "C:\\Users\\dana\\AppData\\Roaming" }, "win32")).toBe(
      path.join("C:\\Users\\dana\\AppData\\Roaming", "codypendent", "data"),
    );
  });

  it("falls back to os.homedir() when HOME is unset (matches the dirs passwd fallback)", () => {
    // With no HOME/USERPROFILE in the injected env, resolution defers to
    // os.homedir() — the faithful equivalent of the Rust `dirs` crate consulting
    // the passwd database — and still yields an absolute codypendent data dir.
    const resolved = platformDataDir({}, "linux");
    expect(path.isAbsolute(resolved)).toBe(true);
    expect(resolved.endsWith(path.join(".local", "share", "codypendent"))).toBe(true);
  });
});

describe("validateSocketPath", () => {
  it("accepts a short path", () => {
    expect(() => validateSocketPath("/run/user/1000/codypendent/daemon.sock")).not.toThrow();
  });

  it("rejects a path longer than the SUN_LEN bound", () => {
    const tooLong = "/" + "a".repeat(MAX_SOCKET_PATH_BYTES + 1);
    expect(() => validateSocketPath(tooLong)).toThrowError(DiscoveryError);
  });
});
