// VS Code relay (SPEC 24.4.5, 24.4.5.1, IP-D4): connects the webview message channel to the
// allowlist-enforcing relay of @asc/asc-client. One relay per webview; replies go back through that
// webview. Deny by default: the relay drops and logs anything not in allowlist.json, and the host
// composes every confirmation (confirm.ts). This module holds no token.
import { type BridgeTransport } from "@asc/asc-client";
import allowlistJson from "@asc/asc-client/allowlist.json" with { type: "json" };
import {
  type Allowlist,
  type ConfirmRequest,
  parseAllowlist,
  type Relay,
  type RelayLog,
  relay,
} from "@asc/asc-client/relay";
import type { ForgejoReply, ForgejoRequest } from "@asc/asc-client";
import type { RequestHandler, WebviewLike } from "./host";

export interface RelayHandlerOptions {
  /** The window's bridge transport; resolved on the first message, so nothing spawns earlier. */
  upstream: () => BridgeTransport;
  /** Absolute asc workspace roots. */
  roots: () => readonly string[];
  /** `owner/repo` of the forge repos of the roots. */
  forgeRepos: () => readonly string[];
  confirm: (c: ConfirmRequest) => Promise<boolean>;
  /** Absent: Forgejo requests are denied. */
  forgejo?: ((req: ForgejoRequest, signal: AbortSignal) => Promise<ForgejoReply>) | undefined;
  tmpDir?: string | undefined;
  /** Log lines never carry request payloads. */
  log: (line: string) => void;
  allowlist?: Allowlist;
}

export function createRelayHandler(o: RelayHandlerOptions): RequestHandler & { dispose(): void } {
  const allowlist = o.allowlist ?? parseAllowlist(allowlistJson);
  const relays = new Map<WebviewLike, Relay>();
  const logEvent = (e: RelayLog) =>
    o.log(`relay ${e.event}: ${e.what ?? "-"}: ${e.reason}`);

  const forWebview = (w: WebviewLike): Relay => {
    let r = relays.get(w);
    if (r) return r;
    const created: Relay = relay({
      upstream: o.upstream(),
      allowlist,
      send: (reply) => {
        // A disposed webview rejects or returns false: stop its calls.
        Promise.resolve(w.postMessage(reply)).then(
          (ok) => {
            if (ok === false) drop(w);
          },
          () => drop(w),
        );
      },
      roots: o.roots,
      forgeRepos: o.forgeRepos,
      tmpDir: o.tmpDir,
      confirm: o.confirm,
      forgejo: o.forgejo,
      log: logEvent,
    });
    r = created;
    relays.set(w, r);
    return r;
  };
  const drop = (w: WebviewLike) => {
    relays.get(w)?.close();
    relays.delete(w);
  };

  const handler = ((w: WebviewLike, message: unknown): boolean => {
    forWebview(w).handle(message);
    return true;
  }) as RequestHandler & { dispose(): void };
  handler.dispose = () => {
    for (const r of relays.values()) r.close();
    relays.clear();
  };
  return handler;
}
