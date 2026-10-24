// `u32 little-endian length || BridgeFrame`, at most 16 MiB per frame (SPEC 24.2, bridge.proto).
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { type BridgeFrame, BridgeFrameSchema } from "../gen/asc/v1/bridge_pb";

export const MAX_FRAME = 16 * 1024 * 1024;

export class FrameError extends Error {
  override readonly name = "FrameError";
}

export function encodeFrame(frame: BridgeFrame): Uint8Array {
  const body = toBinary(BridgeFrameSchema, frame);
  if (body.length > MAX_FRAME) throw new FrameError("frame is larger than 16 MiB");
  const out = new Uint8Array(4 + body.length);
  new DataView(out.buffer).setUint32(0, body.length, true);
  out.set(body, 4);
  return out;
}

export function makeFrame(id: number, f: BridgeFrame["f"]): BridgeFrame {
  return create(BridgeFrameSchema, { id, f });
}

/** Incremental decoder: push byte chunks, get whole frames. A bad length or body throws. */
export class FrameReader {
  #buf: Uint8Array<ArrayBufferLike> = new Uint8Array(0);

  push(chunk: Uint8Array): BridgeFrame[] {
    if (this.#buf.length === 0) {
      this.#buf = chunk;
    } else {
      const next = new Uint8Array(this.#buf.length + chunk.length);
      next.set(this.#buf);
      next.set(chunk, this.#buf.length);
      this.#buf = next;
    }
    const frames: BridgeFrame[] = [];
    let at = 0;
    while (this.#buf.length - at >= 4) {
      const len = new DataView(this.#buf.buffer, this.#buf.byteOffset + at, 4).getUint32(0, true);
      if (len > MAX_FRAME) throw new FrameError("frame is larger than 16 MiB");
      if (this.#buf.length - at - 4 < len) break;
      const body = this.#buf.subarray(at + 4, at + 4 + len);
      try {
        frames.push(fromBinary(BridgeFrameSchema, body));
      } catch {
        throw new FrameError("frame does not decode");
      }
      at += 4 + len;
    }
    this.#buf = at === 0 ? this.#buf : this.#buf.slice(at);
    return frames;
  }
}
