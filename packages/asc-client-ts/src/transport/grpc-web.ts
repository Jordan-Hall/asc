// `grpcWeb` transport: the browser transport for the web UI (`asc-ui` outside an IDE). It speaks
// gRPC-Web (`application/grpc-web+proto`) with `fetch` to an asc server; HTTPS only. Plain http is
// accepted only for a loopback host and only when the caller opts in. The bearer token is added by
// `headers()` per call and never goes into the URL, the query or a log.
import {
  BridgeError,
  type BridgeState,
  type BridgeTransport,
  type CallOptions,
} from "./bridge-node";
import { MAX_FRAME } from "./frame";

export interface GrpcWebOptions {
  /** `https://asc.example.org`. */
  baseUrl: string;
  /** Allow `http://localhost`, `127.0.0.1` and `[::1]` (development only). */
  allowLoopbackHttp?: boolean;
  /** Extra request headers, such as `authorization`; called for every request. */
  headers?: () => Record<string, string> | Promise<Record<string, string>>;
  fetch?: typeof fetch;
}

const LOOPBACK = new Set(["localhost", "127.0.0.1", "[::1]"]);

/** The base URL as a string without a trailing slash; throws for anything but https (or opted-in loopback). */
export function checkBaseUrl(raw: string, allowLoopbackHttp = false): string {
  const u = new URL(raw);
  const secure = u.protocol === "https:";
  if (!secure && !(allowLoopbackHttp && u.protocol === "http:" && LOOPBACK.has(u.hostname))) {
    throw new Error("grpcWeb needs an https URL");
  }
  if (u.username || u.password) throw new Error("grpcWeb URL must not carry credentials");
  return `${u.origin}${u.pathname.replace(/\/+$/, "")}`;
}

function frame(message: Uint8Array): Uint8Array {
  const out = new Uint8Array(5 + message.length);
  new DataView(out.buffer).setUint32(1, message.length, false);
  out.set(message, 5);
  return out;
}

/** gRPC status from a trailer block (`grpc-status: 0\r\ngrpc-message: ...`). */
function trailers(block: Uint8Array): { code: number; message: string } {
  const text = new TextDecoder().decode(block);
  let code = 2;
  let message = "";
  for (const line of text.split("\r\n")) {
    const i = line.indexOf(":");
    if (i < 0) continue;
    const k = line.slice(0, i).trim().toLowerCase();
    const v = line.slice(i + 1).trim();
    if (k === "grpc-status") code = Number(v);
    else if (k === "grpc-message") message = decodeURIComponent(v);
  }
  return { code: Number.isInteger(code) ? code : 2, message };
}

export function grpcWeb(options: GrpcWebOptions): BridgeTransport {
  const base = checkBaseUrl(options.baseUrl, options.allowLoopbackHttp);
  const doFetch = options.fetch ?? fetch;
  let state: BridgeState = "up";
  const all = new Set<AbortController>();

  async function* stream(call: CallOptions): AsyncGenerator<Uint8Array> {
    if (state === "closed") throw new BridgeError(14, "transport is closed", undefined, true);
    if (!/^[A-Za-z0-9_.]+\/[A-Za-z0-9_]+$/.test(call.method)) {
      throw new BridgeError(3, "bad method name");
    }
    const ac = new AbortController();
    all.add(ac);
    const onAbort = () => ac.abort();
    call.signal?.addEventListener("abort", onAbort, { once: true });
    try {
      const extra = (await options.headers?.()) ?? {};
      const res = await doFetch(`${base}/${call.method}`, {
        method: "POST",
        headers: {
          ...extra,
          "content-type": "application/grpc-web+proto",
          accept: "application/grpc-web+proto",
          "x-grpc-web": "1",
          ...(call.root ? { "x-asc-root": call.root } : {}),
        },
        body: frame(call.request).buffer as ArrayBuffer,
        signal: ac.signal,
        credentials: "omit",
        redirect: "error",
      });
      if (!res.ok || !res.body) {
        throw new BridgeError(14, `server answered HTTP ${res.status}`);
      }
      const reader = res.body.getReader();
      let buf = new Uint8Array(0);
      let sawTrailer = false;
      for (;;) {
        const { done, value } = await reader.read();
        if (value) {
          const next = new Uint8Array(buf.length + value.length);
          next.set(buf);
          next.set(value, buf.length);
          buf = next;
        }
        while (buf.length >= 5) {
          const len = new DataView(buf.buffer, buf.byteOffset + 1, 4).getUint32(0, false);
          if (len > MAX_FRAME) throw new BridgeError(8, "frame is larger than 16 MiB");
          if (buf.length < 5 + len) break;
          const flag = buf[0] ?? 0;
          const body = buf.slice(5, 5 + len);
          buf = buf.slice(5 + len);
          if (flag & 0x80) {
            sawTrailer = true;
            const t = trailers(body);
            if (t.code !== 0) throw new BridgeError(t.code, t.message);
          } else {
            yield body;
          }
        }
        if (done) break;
      }
      if (!sawTrailer)
        throw new BridgeError(14, "the stream ended without a status", undefined, true);
    } catch (e) {
      if (e instanceof BridgeError) throw e;
      if (call.signal?.aborted) throw new BridgeError(1, "cancelled");
      throw new BridgeError(14, "the server could not be reached", undefined, true);
    } finally {
      call.signal?.removeEventListener("abort", onAbort);
      ac.abort();
      all.delete(ac);
    }
  }

  return {
    get state() {
      return state;
    },
    async unary(call) {
      let first: Uint8Array | undefined;
      for await (const r of stream(call)) first ??= r;
      if (!first) throw new BridgeError(13, "the server sent no reply");
      return first;
    },
    stream,
    close() {
      state = "closed";
      for (const ac of all) ac.abort();
    },
  };
}
