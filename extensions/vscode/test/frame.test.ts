import { describe, expect, it } from "vitest";

import { encodeEnvelope, FrameDecoder, FrameError, MAX_FRAME_BYTES } from "../src/protocol/frame.js";
import { PROTOCOL_V1, type Envelope, type Payload } from "../src/protocol/types.js";

function envelope(payload: Payload, overrides: Partial<Envelope> = {}): Envelope {
  return {
    protocol_version: PROTOCOL_V1,
    message_id: "11111111-1111-1111-1111-111111111111",
    client_id: "22222222-2222-2222-2222-222222222222",
    payload,
    ...overrides,
  };
}

function ping(): Envelope {
  return envelope({ type: "Ping" });
}

describe("frame codec", () => {
  it("round-trips an envelope through encode -> decode", () => {
    const original = envelope({ type: "Pong" }, { sequence: 7 });
    const decoder = new FrameDecoder();
    const out = decoder.push(encodeEnvelope(original));
    expect(out).toHaveLength(1);
    expect(out[0]).toEqual(original);
    expect(decoder.pendingBytes).toBe(0);
  });

  it("writes a 4-byte big-endian length prefix", () => {
    const frame = encodeEnvelope(ping());
    const declaredLen = frame.readUInt32BE(0);
    expect(declaredLen).toBe(frame.length - 4);
    // The body is exactly the JSON bytes.
    expect(frame.subarray(4).toString("utf8")).toBe(JSON.stringify(ping()));
  });

  it("reassembles a frame delivered one byte at a time across chunk boundaries", () => {
    const frame = encodeEnvelope(envelope({ type: "Ping" }, { sequence: 42 }));
    const decoder = new FrameDecoder();
    const collected: Envelope[] = [];
    for (let i = 0; i < frame.length; i += 1) {
      const produced = decoder.push(frame.subarray(i, i + 1));
      collected.push(...produced);
      // Nothing emitted until the very last byte completes the frame.
      if (i < frame.length - 1) {
        expect(produced).toHaveLength(0);
      }
    }
    expect(collected).toHaveLength(1);
    expect(collected[0].sequence).toBe(42);
    expect(decoder.pendingBytes).toBe(0);
  });

  it("splits at an arbitrary boundary in the middle of the length prefix", () => {
    const frame = encodeEnvelope(ping());
    const decoder = new FrameDecoder();
    expect(decoder.push(frame.subarray(0, 2))).toHaveLength(0); // partial prefix
    expect(decoder.push(frame.subarray(2, 5))).toHaveLength(0); // rest of prefix + 1 body byte
    const rest = decoder.push(frame.subarray(5));
    expect(rest).toHaveLength(1);
  });

  it("yields several frames packed into a single chunk, keeping a trailing partial", () => {
    const a = encodeEnvelope(envelope({ type: "Ping" }, { sequence: 1 }));
    const b = encodeEnvelope(envelope({ type: "Pong" }, { sequence: 2 }));
    const c = encodeEnvelope(envelope({ type: "Ping" }, { sequence: 3 }));
    const decoder = new FrameDecoder();

    // a, b, and the first half of c arrive together.
    const half = Math.floor(c.length / 2);
    const first = decoder.push(Buffer.concat([a, b, c.subarray(0, half)]));
    expect(first.map((e) => e.sequence)).toEqual([1, 2]);
    expect(decoder.pendingBytes).toBe(half);

    // The tail of c arrives later.
    const second = decoder.push(c.subarray(half));
    expect(second.map((e) => e.sequence)).toEqual([3]);
    expect(decoder.pendingBytes).toBe(0);
  });

  it("rejects an oversize frame the moment the length prefix is readable", () => {
    const decoder = new FrameDecoder();
    const prefix = Buffer.allocUnsafe(4);
    prefix.writeUInt32BE(MAX_FRAME_BYTES + 1, 0);
    // No body bytes supplied at all — rejection must not wait for them.
    expect(() => decoder.push(prefix)).toThrowError(FrameError);
  });

  it("accepts a frame declaring exactly MAX_FRAME_BYTES as legal (boundary)", () => {
    const decoder = new FrameDecoder();
    const prefix = Buffer.allocUnsafe(4);
    prefix.writeUInt32BE(MAX_FRAME_BYTES, 0);
    // Legal length; body simply isn't here yet, so nothing is emitted / thrown.
    expect(decoder.push(prefix)).toHaveLength(0);
  });

  it("rejects encoding an envelope whose payload exceeds MAX_FRAME_BYTES", () => {
    const huge = "x".repeat(MAX_FRAME_BYTES);
    const oversized = envelope({ type: "Note", text: huge } as unknown as Payload);
    expect(() => encodeEnvelope(oversized)).toThrowError(FrameError);
  });

  it("raises FrameError when a completed frame body is not valid JSON", () => {
    const decoder = new FrameDecoder();
    const body = Buffer.from("{not json", "utf8");
    const prefix = Buffer.allocUnsafe(4);
    prefix.writeUInt32BE(body.length, 0);
    expect(() => decoder.push(Buffer.concat([prefix, body]))).toThrowError(FrameError);
  });
});
