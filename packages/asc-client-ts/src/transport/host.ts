// `host` transport: what `asc-ui` uses inside a webview (SPEC 24.4.5, IP-D4). It holds no token and no
// process access. Every call is a `postMessage` of `{id, method, bytes}` to the extension host, whose
// `relay` (relay.ts) forwards it only when it matches an entry of allowlist.json. Replies come back
// as `{id, reply}` and `{id, end}` messages.
import {
  BridgeError,
  type BridgeState,
  type BridgeTransport,
  type CallOptions,
} from "./bridge-node";

/** Webview to host. `hostApi` requests carry no token; the host adds it. */
export type HostRequest =
  | { id: number; method: string; bytes: Uint8Array; root?: string }
  | { id: number; cancel: true }
  | { id: number; hostApi: HostApiRequest };

/** Host to webview. */
export type HostReply =
  | { id: number; reply: Uint8Array }
  | { id: number; end: { code: number; message: string; detail?: Uint8Array } }
  | { id: number; hostApi: HostApiReply };

export interface HostApiRequest {
  /** Which host API adapter answers (a key of the allowlist's `hostApi.providers`). */
  provider: string;
  method: string;
  /** Path below the provider's API base, with the query string. */
  path: string;
  body?: unknown;
}

export interface HostApiReply {
  status: number;
  body: unknown;
}

/** The webview side of the message channel (`acquireVsCodeApi()`, `window.ascHost`). */
export interface HostPort {
  post(message: HostRequest): void;
  /** Subscribes to host messages; returns the unsubscribe function. */
  listen(handler: (message: HostReply) => void): () => void;
}

interface Pending {
  push(reply: Uint8Array): void;
  end(code: number, message: string, detail: Uint8Array): void;
}

export interface HostTransport extends BridgeTransport {
  /** A host API call through the host, which checks it against the allowlist and adds the token. */
  hostApi(request: HostApiRequest, signal?: AbortSignal): Promise<HostApiReply>;
}

export function host(port: HostPort): HostTransport {
  let next = 1;
  let state: BridgeState = "up";
  const calls = new Map<number, Pending>();
  const hostApiCalls = new Map<number, (r: HostApiReply | BridgeError) => void>();

  const stop = port.listen((m) => {
    const call = calls.get(m.id);
    if ("reply" in m) call?.push(m.reply);
    else if ("end" in m) {
      calls.delete(m.id);
      call?.end(m.end.code, m.end.message, m.end.detail ?? new Uint8Array(0));
    } else if ("hostApi" in m) {
      hostApiCalls.get(m.id)?.(m.hostApi);
      hostApiCalls.delete(m.id);
    }
  });

  async function* stream(call: CallOptions): AsyncGenerator<Uint8Array> {
    if (state === "closed") throw new BridgeError(14, "transport is closed", undefined, true);
    const id = next++;
    const queue: Uint8Array[] = [];
    let done: BridgeError | "ok" | undefined;
    let wake: (() => void) | undefined;
    const poke = () => {
      wake?.();
      wake = undefined;
    };
    calls.set(id, {
      push: (r) => {
        queue.push(r);
        poke();
      },
      end: (code, message, detail) => {
        done = code === 0 ? "ok" : new BridgeError(code, message, detail);
        poke();
      },
    });
    const onAbort = () => port.post({ id, cancel: true });
    call.signal?.addEventListener("abort", onAbort, { once: true });
    port.post({
      id,
      method: call.method,
      bytes: call.request,
      ...(call.root === undefined ? {} : { root: call.root }),
    });
    try {
      for (;;) {
        const item = queue.shift();
        if (item) {
          yield item;
        } else if (done === "ok") {
          return;
        } else if (done) {
          throw done;
        } else {
          await new Promise<void>((r) => {
            wake = r;
          });
        }
      }
    } finally {
      call.signal?.removeEventListener("abort", onAbort);
      if (calls.delete(id) && done === undefined) port.post({ id, cancel: true });
    }
  }

  return {
    get state() {
      return state;
    },
    async unary(call) {
      let first: Uint8Array | undefined;
      for await (const r of stream(call)) first ??= r;
      if (!first) throw new BridgeError(13, "the host sent no reply");
      return first;
    },
    stream,
    hostApi(request, signal) {
      const id = next++;
      return new Promise<HostApiReply>((resolve, reject) => {
        hostApiCalls.set(id, (r) => (r instanceof BridgeError ? reject(r) : resolve(r)));
        signal?.addEventListener(
          "abort",
          () => {
            hostApiCalls.delete(id);
            port.post({ id, cancel: true });
            reject(new BridgeError(1, "cancelled"));
          },
          { once: true },
        );
        port.post({ id, hostApi: request });
      });
    },
    close() {
      state = "closed";
      stop();
      for (const [id, c] of calls) {
        calls.delete(id);
        c.end(14, "transport is closed", new Uint8Array(0));
      }
    },
  };
}
