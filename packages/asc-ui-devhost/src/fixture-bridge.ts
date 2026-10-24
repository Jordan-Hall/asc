// A fixture BridgeTransport: deterministic canned replies, no process, no network. Unknown
// methods fail closed so a view cannot silently depend on an RPC the fixture lacks.
import type { BridgeTransport } from "@asc/asc-client";

export type FixtureHandler = (request: Uint8Array) => Uint8Array;

export function createFixtureBridge(
  handlers: Readonly<Record<string, FixtureHandler>> = {},
): BridgeTransport {
  const find = (method: string): FixtureHandler => {
    const h = handlers[method];
    if (!h) throw new Error(`fixture bridge: no handler for ${method}`);
    return h;
  };
  return {
    state: "up",
    async unary(call) {
      return find(call.method)(call.request);
    },
    async *stream(call) {
      yield find(call.method)(call.request);
    },
    close() {},
  };
}
