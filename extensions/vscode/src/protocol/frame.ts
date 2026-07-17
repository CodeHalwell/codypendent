/**
 * Length-prefixed JSON framing — the exact wire codec from
 * `crates/protocol/src/framing.rs`.
 *
 * ```text
 * +---------------------------+-----------------------------+
 * | u32 big-endian payload len | JSON bytes of one Envelope |
 * +---------------------------+-----------------------------+
 * ```
 *
 * To read: read the 4-byte big-endian length, then that many bytes, `JSON.parse`.
 * To write: `JSON.stringify` -> utf8 bytes -> 4-byte BE length prefix -> bytes.
 * Frames larger than {@link MAX_FRAME_BYTES} are a protocol violation.
 */
import type { Envelope } from "./types.js";

/** Frames larger than this are a protocol violation (16 MiB) — `framing.rs`. */
export const MAX_FRAME_BYTES = 16 * 1024 * 1024;

const LENGTH_PREFIX_BYTES = 4;

/** A framing-layer failure (oversize frame or malformed JSON). */
export class FrameError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "FrameError";
  }
}

/**
 * Encode one envelope as a length-prefixed frame.
 *
 * Mirrors `framing.rs::write_envelope`: serialize, reject if the payload exceeds
 * `MAX_FRAME_BYTES`, then prepend the big-endian length.
 */
export function encodeEnvelope(envelope: Envelope): Buffer {
  const json = Buffer.from(JSON.stringify(envelope), "utf8");
  if (json.length > MAX_FRAME_BYTES) {
    throw new FrameError(`frame of ${json.length} bytes exceeds MAX_FRAME_BYTES`);
  }
  const prefix = Buffer.allocUnsafe(LENGTH_PREFIX_BYTES);
  prefix.writeUInt32BE(json.length, 0);
  return Buffer.concat([prefix, json]);
}

/**
 * Buffers incoming bytes off a stream and yields complete {@link Envelope}s.
 *
 * A single TCP/Unix-socket chunk may contain a partial frame, exactly one frame,
 * or several frames; a frame may also span many chunks. {@link push} accumulates
 * bytes and returns every envelope that is now fully available, leaving any
 * trailing partial frame buffered for the next chunk — matching the read half of
 * `framing.rs` (length checked the moment the 4-byte prefix is available, before
 * waiting for the body).
 */
export class FrameDecoder {
  private buffer: Buffer = Buffer.alloc(0);

  /**
   * Append `chunk` and return every complete envelope now available.
   * @throws {FrameError} if a frame declares a length greater than
   *   `MAX_FRAME_BYTES` (checked as soon as the prefix is readable) or if a
   *   completed frame's body is not valid JSON.
   */
  push(chunk: Buffer): Envelope[] {
    this.buffer =
      this.buffer.length === 0 ? Buffer.from(chunk) : Buffer.concat([this.buffer, chunk]);

    const envelopes: Envelope[] = [];
    for (;;) {
      if (this.buffer.length < LENGTH_PREFIX_BYTES) {
        break;
      }
      const length = this.buffer.readUInt32BE(0);
      // Reject the moment the prefix is readable — do not buffer toward a frame
      // that can never be legal (framing.rs checks before allocating the body).
      if (length > MAX_FRAME_BYTES) {
        throw new FrameError(`frame of ${length} bytes exceeds MAX_FRAME_BYTES`);
      }
      const frameEnd = LENGTH_PREFIX_BYTES + length;
      if (this.buffer.length < frameEnd) {
        break;
      }
      const body = this.buffer.subarray(LENGTH_PREFIX_BYTES, frameEnd);
      let envelope: Envelope;
      try {
        envelope = JSON.parse(body.toString("utf8")) as Envelope;
      } catch (cause) {
        throw new FrameError(
          `frame body is not valid JSON: ${cause instanceof Error ? cause.message : String(cause)}`,
        );
      }
      envelopes.push(envelope);
      // Retain only the unconsumed tail (a copy, so the large concat buffer can
      // be released once fully drained).
      this.buffer = Buffer.from(this.buffer.subarray(frameEnd));
    }
    return envelopes;
  }

  /** Number of buffered bytes not yet forming a complete frame (for tests). */
  get pendingBytes(): number {
    return this.buffer.length;
  }
}
