import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { mkdtemp, rm, readFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import { afterEach, describe, expect, it } from "vitest";
import { NodeDaemonClient, resolveSocketPath } from "../src/node.js";
import { encodeEnvelope, FrameDecoder } from "../src/framing.js";
import { PROTOCOL_V1 } from "../src/version.js";
import type { Envelope, Payload } from "../src/envelope.js";

const cleanup: Array<() => Promise<void> | void> = [];
afterEach(async () => { for (const close of cleanup.splice(0).reverse()) await close(); });

async function fixture(onRequest: (request: Envelope, reply: (payload: Payload, sessionId?: string) => void) => void) {
  const directory = await mkdtemp(path.join(os.tmpdir(), "cody-sdk-"));
  cleanup.push(() => rm(directory, { recursive: true, force: true }));
  const socketPath = path.join(directory, "daemon.sock");
  const sockets = new Set<net.Socket>();
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    const decoder = new FrameDecoder();
    socket.on("data", (chunk) => {
      for (const request of decoder.push(chunk)) {
        const reply = (payload: Payload, sessionId?: string) => socket.write(encodeEnvelope({
          protocol_version: PROTOCOL_V1, message_id: randomUUID(), client_id: request.client_id,
          correlation_id: request.message_id, payload,
          ...(sessionId ? { session_id: sessionId } : {}),
        }));
        if (request.payload.type === "ClientHello") {
          reply({ type: "ServerHello", selected_protocol: PROTOCOL_V1, daemon_version: "fixture",
            daemon_instance: randomUUID(), heartbeat_interval_ms: 15000, build_id: "fixture" });
        } else onRequest(request, reply);
      }
    });
  });
  await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(socketPath, resolve); });
  cleanup.push(() => new Promise<void>((resolve) => { for (const socket of sockets) socket.destroy(); server.close(() => resolve()); }));
  const client = new NodeDaemonClient({ socketPath, requestTimeoutMs: 30 });
  cleanup.push(() => client.stop());
  const ready = new Promise<void>((resolve) => client.once("serverHello", () => resolve()));
  client.start();
  await ready;
  return client;
}

describe("shared Node transport", () => {
  it("offers the current Rust protocol version", async () => {
    const rust = await readFile(new URL("../../../crates/protocol/src/version.rs", import.meta.url), "utf8");
    const current = rust.match(/pub const PROTOCOL_V1: ProtocolVersion = ProtocolVersion\s*\{\s*major:\s*(\d+),\s*minor:\s*(\d+),?\s*\}/);
    expect(PROTOCOL_V1).toEqual({ major: Number(current?.[1]), minor: Number(current?.[2]) });
  });
  it("uses the same discovery precedence as the daemon", () => {
    expect(resolveSocketPath({ HOME: "/users/dev", CODYPENDENT_SOCKET: "/tmp/explicit.sock",
      CODYPENDENT_DATA_DIR: "/tmp/data", XDG_RUNTIME_DIR: "/tmp/runtime" }, "linux"))
      .toBe("/tmp/explicit.sock");
    expect(resolveSocketPath({ HOME: "/users/dev", CODYPENDENT_DATA_DIR: "/tmp/data" }, "linux"))
      .toBe("/tmp/data/run/daemon.sock");
  });

  it("creates a session before attaching and retains the envelope's new session id", async () => {
    const sessionId = randomUUID();
    const client = await fixture((request, reply) => {
      if (request.payload.type !== "Command") return;
      expect(request.session_id).toBeUndefined();
      reply({ type: "CommandAccepted", command_id: request.payload.command_id }, sessionId);
    });
    const reply = await client.requestGlobal({ type: "CreateSession", title: "Anton", workspace: randomUUID() });
    expect(reply.session_id).toBe(sessionId);
    expect(reply.payload.type).toBe("CommandAccepted");
  });

  it("bounds unanswered requests", async () => {
    const client = await fixture(() => {});
    await expect(client.requestGlobal({ type: "ListSessions", limit: 10 })).rejects.toThrow("outcome may be unknown");
  });

  it("rejects a pending request on disconnect instead of replaying a mutation", async () => {
    const client = await fixture(() => {});
    const reply = client.requestGlobal({ type: "CreateSession", title: "Anton", workspace: randomUUID() });
    client.stop();
    await expect(reply).rejects.toThrow("connection closed");
  });
});
