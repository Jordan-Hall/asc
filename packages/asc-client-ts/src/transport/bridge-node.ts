// `bridgeNode`: spawns one long-lived `asc api --stdio` child and speaks framed BridgeFrames with it
// (SPEC 24.2, IP-D2). Lazy start, restart with backoff (0.2 s, 1 s, 5 s), stream cancellation. The
// transport never puts a secret in argv: the command line is exactly what the caller passes, and the
// bridge finds credentials itself. Child stderr goes to `onStderr` only, never to a log of ours.
import { type ChildProcess, spawn } from "node:child_process";
import { create } from "@bufbuild/protobuf";
import { CallSchema } from "../gen/asc/v1/bridge_pb";
import { encodeFrame, FrameError, FrameReader, makeFrame } from "./frame";

/** gRPC status code numbers used by the bridge. */
export const Code = { Ok: 0, Cancelled: 1, Internal: 13, Unavailable: 14 } as const;

export class BridgeError extends Error {
  override readonly name = "BridgeError";
  constructor(
    readonly code: number,
    message: string,
    /** An encoded `asc.v1.ErrorDetail` when the server sent one. */
    readonly detail: Uint8Array = new Uint8Array(0),
    /** True when the bridge process went away, not the server. */
    readonly lost = false,
  ) {
    super(message);
  }
}

export type BridgeState = "idle" | "up" | "reconnecting" | "closed";

export interface CallOptions {
  /** `asc.v1.Workspace/Status` and so on. */
  method: string;
  request: Uint8Array;
  /** Absolute workspace root; empty selects the bridge's own directory. */
  root?: string;
  signal?: AbortSignal | undefined;
}

/** What a transport offers; `relay`/`host` (P10) implement the same shape. */
export interface BridgeTransport {
  readonly state: BridgeState;
  unary(call: CallOptions): Promise<Uint8Array>;
  stream(call: CallOptions): AsyncGenerator<Uint8Array>;
  close(): void;
}

export interface BridgeNodeOptions {
  /** The `asc` executable. */
  command: string;
  /** Defaults to `["api", "--stdio"]`. */
  args?: readonly string[];
  cwd?: string;
  env?: Record<string, string | undefined>;
  /** Delays before restart attempt 1, 2, 3 and later; default 200, 1000, 5000 ms. */
  backoffMs?: readonly number[];
  onState?: (state: BridgeState) => void;
  /** Called with each stderr line (cut to 1 KiB); the line may name paths, so do not forward it to telemetry. */
  onStderr?: (line: string) => void;
  /** Called after an unexpected exit, once the calls in flight have failed. */
  onExit?: (code: number | null) => void;
}

const DEFAULT_BACKOFF: readonly number[] = [200, 1000, 5000];
const MAX_STDERR_LINE = 1024;
const MAX_LIVE = 128;

interface Live {
  push(reply: Uint8Array): void;
  end(err: BridgeError | null): void;
}

function abortError(signal: AbortSignal): unknown {
  return signal.reason ?? new DOMException("aborted", "AbortError");
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(abortError(signal));
      return;
    }
    const onAbort = () => {
      clearTimeout(t);
      reject(abortError(signal as AbortSignal));
    };
    const t = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

export type BridgeNode = BridgeTransport & {
  /** The last stderr lines of the current or last child (at most 20). */
  stderrTail(): readonly string[];
  /** The pid of the running child, if any. */
  readonly pid: number | undefined;
};

export function bridgeNode(opts: BridgeNodeOptions): BridgeNode {
  const backoff = opts.backoffMs?.length ? opts.backoffMs : DEFAULT_BACKOFF;
  const live = new Map<number, Live>();
  let child: ChildProcess | null = null;
  let starting: Promise<void> | null = null;
  let failures = 0;
  let nextId = 0;
  let closed = false;
  let state: BridgeState = "idle";
  let tail: string[] = [];

  const setState = (s: BridgeState) => {
    if (state === s) return;
    state = s;
    opts.onState?.(s);
  };

  const failAll = (err: BridgeError) => {
    const all = [...live.values()];
    live.clear();
    for (const l of all) l.end(err);
  };

  const onGone = (gone: ChildProcess, code: number | null, why: string) => {
    if (child !== gone) return;
    child = null;
    if (closed) return;
    failures += 1;
    setState("reconnecting");
    failAll(new BridgeError(Code.Unavailable, why, undefined, true));
    opts.onExit?.(code);
  };

  const start = async (): Promise<void> => {
    const delay = failures === 0 ? 0 : (backoff[Math.min(failures - 1, backoff.length - 1)] ?? 0);
    if (delay > 0) await sleep(delay);
    if (closed) throw new BridgeError(Code.Cancelled, "bridge is closed");
    const reader = new FrameReader();
    const proc = spawn(opts.command, [...(opts.args ?? ["api", "--stdio"])], {
      ...(opts.cwd === undefined ? {} : { cwd: opts.cwd }),
      ...(opts.env === undefined ? {} : { env: opts.env as NodeJS.ProcessEnv }),
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
      shell: false,
    });
    child = proc;
    tail = [];
    proc.stdin?.on("error", () => {});
    proc.on("error", (e) =>
      onGone(proc, null, `cannot run the bridge: ${(e as NodeJS.ErrnoException).code ?? "error"}`),
    );
    proc.on("close", (code) => onGone(proc, code, "the bridge exited"));
    let partial = "";
    proc.stderr?.on("data", (d: Uint8Array) => {
      partial += Buffer.from(d).toString("utf8");
      let nl = partial.indexOf("\n");
      while (nl >= 0) {
        const line = partial.slice(0, nl).replace(/\r$/, "").slice(0, MAX_STDERR_LINE);
        partial = partial.slice(nl + 1);
        tail.push(line);
        if (tail.length > 20) tail.shift();
        opts.onStderr?.(line);
        nl = partial.indexOf("\n");
      }
      if (partial.length > MAX_STDERR_LINE * 4) partial = "";
    });
    proc.stdout?.on("data", (d: Uint8Array) => {
      let frames: ReturnType<FrameReader["push"]>;
      try {
        frames = reader.push(d);
      } catch (e) {
        // A framing error is fatal for this child: kill it and let the next call restart it.
        proc.kill();
        onGone(proc, null, e instanceof FrameError ? e.message : "bad frame");
        return;
      }
      if (frames.length > 0) failures = 0;
      for (const f of frames) {
        const l = live.get(f.id);
        if (!l) continue;
        switch (f.f.case) {
          case "reply":
            l.push(f.f.value);
            break;
          case "end":
            live.delete(f.id);
            l.end(
              f.f.value.code === Code.Ok
                ? null
                : new BridgeError(f.f.value.code, f.f.value.message, f.f.value.errorDetail),
            );
            break;
          default:
            break;
        }
      }
    });
    setState("up");
  };

  const ensure = async (): Promise<ChildProcess> => {
    if (closed) throw new BridgeError(Code.Cancelled, "bridge is closed");
    if (!child) {
      starting ??= start().finally(() => {
        starting = null;
      });
      await starting;
    }
    if (!child) {
      throw new BridgeError(Code.Unavailable, "the bridge is not running", undefined, true);
    }
    return child;
  };

  const send = (proc: ChildProcess, id: number, f: Parameters<typeof makeFrame>[1]): void => {
    let wire: Uint8Array;
    try {
      wire = encodeFrame(makeFrame(id, f));
    } catch (e) {
      throw new BridgeError(Code.Internal, e instanceof FrameError ? e.message : "cannot encode");
    }
    // A broken pipe is reported by the exit handler.
    proc.stdin?.write(wire);
  };

  async function* stream(call: CallOptions): AsyncGenerator<Uint8Array> {
    const { signal } = call;
    if (signal?.aborted) throw abortError(signal);
    const proc = await ensure();
    if (live.size >= MAX_LIVE) throw new BridgeError(Code.Unavailable, "too many calls in flight");
    let id = nextId;
    do {
      id = id >= 0xffffffff ? 1 : id + 1;
    } while (live.has(id));
    nextId = id;

    const queue: Uint8Array[] = [];
    let finished = false;
    let failure: unknown = null;
    let wake: (() => void) | null = null;
    const poke = () => {
      const w = wake;
      wake = null;
      w?.();
    };
    live.set(id, {
      push(r) {
        queue.push(r);
        poke();
      },
      end(err) {
        finished = true;
        failure = err;
        poke();
      },
    });
    const onAbort = () => {
      if (live.delete(id)) send(proc, id, { case: "cancel", value: true });
      finished = true;
      failure = signal ? abortError(signal) : null;
      poke();
    };
    signal?.addEventListener("abort", onAbort, { once: true });
    try {
      const request = create(CallSchema, {
        method: call.method,
        request: call.request,
        serverStream: true,
        root: call.root ?? "",
      });
      send(proc, id, { case: "call", value: request });
      for (;;) {
        const r = queue.shift();
        if (r) {
          yield r;
          continue;
        }
        if (finished) {
          if (failure) throw failure;
          return;
        }
        await new Promise<void>((resolve) => {
          wake = resolve;
        });
      }
    } finally {
      signal?.removeEventListener("abort", onAbort);
      // The consumer stopped early: drop the call (and the Run child) on the bridge.
      if (live.delete(id)) send(proc, id, { case: "cancel", value: true });
    }
  }

  return {
    get state() {
      return state;
    },
    stream,
    async unary(call) {
      let first: Uint8Array | undefined;
      for await (const r of stream(call)) {
        first ??= r;
      }
      if (!first) throw new BridgeError(Code.Internal, "the call returned no message");
      return first;
    },
    close() {
      if (closed) return;
      closed = true;
      setState("closed");
      failAll(new BridgeError(Code.Cancelled, "bridge is closed"));
      const proc = child;
      child = null;
      proc?.stdin?.end();
      proc?.kill();
    },
    get pid() {
      return child?.pid;
    },
    stderrTail: () => tail,
  };
}

export interface ResumableOptions {
  method: string;
  root?: string;
  signal?: AbortSignal;
  /** Builds the request; `token` is the last token seen, or undefined on the first subscribe. */
  request(token: string | undefined): Uint8Array;
  /** Reads the resume token (for `Workspace.Watch`, `WatchEvent.token`) out of a reply. */
  tokenOf(reply: Uint8Array): string | undefined;
  /** Delays between failed resubscribes; default 200, 1000, 5000 ms. */
  backoffMs?: readonly number[];
  onResume?(token: string | undefined): void;
}

/**
 * A stream that survives a bridge restart: on UNAVAILABLE it resubscribes from the last token. Other
 * errors, a clean end and an abort end it. The transport itself paces restarts of the child.
 */
export async function* resumable(
  t: BridgeTransport,
  o: ResumableOptions,
): AsyncGenerator<Uint8Array> {
  const delays = o.backoffMs?.length ? o.backoffMs : DEFAULT_BACKOFF;
  let token: string | undefined;
  let failed = 0;
  for (;;) {
    try {
      for await (const r of t.stream({
        method: o.method,
        request: o.request(token),
        ...(o.root === undefined ? {} : { root: o.root }),
        signal: o.signal,
      })) {
        failed = 0;
        token = o.tokenOf(r) ?? token;
        yield r;
      }
      return;
    } catch (e) {
      if (!(e instanceof BridgeError) || e.code !== Code.Unavailable || o.signal?.aborted) throw e;
      await sleep(delays[Math.min(failed, delays.length - 1)] ?? 0, o.signal);
      failed += 1;
      o.onResume?.(token);
    }
  }
}
