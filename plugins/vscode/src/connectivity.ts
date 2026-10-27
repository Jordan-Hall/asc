// `Bridge.Events`: the connectivity of the daemon, the asc server and the remote per root (SPEC 24.6). The
// stream is the only source of "offline" in the UI, and it never drives a poll. A dropped stream
// reconnects with a growing delay, like the `Watch` pump. Editor-free.
import { bridge, callStream } from "@asc/asc-client";
import type { BridgeManager } from "./bridge";

export type ConnState = "up" | "down" | "not-configured" | "unknown";

export interface ConnectivityEvent {
  root: string;
  daemon: ConnState;
  server: ConnState;
  remote: ConnState;
}

export interface ConnectivityApi {
  events(signal: AbortSignal): AsyncIterable<ConnectivityEvent>;
}

const NAME: Record<number, ConnState> = {
  [bridge.Connectivity.UP]: "up",
  [bridge.Connectivity.DOWN]: "down",
  [bridge.Connectivity.NOT_CONFIGURED]: "not-configured",
};

export const connState = (c: number): ConnState => NAME[c] ?? "unknown";

export function bridgeConnectivity(mgr: BridgeManager): ConnectivityApi {
  return {
    async *events(signal) {
      for await (const ev of callStream(mgr.node(), bridge.Bridge.method.events, {}, { signal })) {
        if (ev.event.case !== "connectivity") continue;
        const c = ev.event.value;
        yield {
          root: c.root,
          daemon: connState(c.daemon),
          server: connState(c.server),
          remote: connState(c.remote),
        };
      }
    },
  };
}

/** Calls `fn` for each event until aborted; reconnects after an error with a growing delay. */
export async function followConnectivity(
  api: ConnectivityApi,
  signal: AbortSignal,
  fn: (e: ConnectivityEvent) => void,
  onError?: (e: unknown) => void,
  delays: readonly number[] = [500, 2000, 10000],
): Promise<void> {
  let attempt = 0;
  while (!signal.aborted) {
    try {
      for await (const e of api.events(signal)) {
        attempt = 0;
        fn(e);
      }
    } catch (e) {
      if (signal.aborted) return;
      onError?.(e);
    }
    if (signal.aborted) return;
    const d = delays[Math.min(attempt++, delays.length - 1)] ?? 10000;
    await new Promise<void>((resolve) => {
      const t = setTimeout(resolve, d);
      signal.addEventListener(
        "abort",
        () => {
          clearTimeout(t);
          resolve();
        },
        { once: true },
      );
    });
  }
}
