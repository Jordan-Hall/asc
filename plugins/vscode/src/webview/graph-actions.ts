// Graph context-menu wiring (SPEC 24.3.3, P47c): cherry-pick and revert from the asc-ui commit graph.
// The webview sends `Bridge.Run`; this layer sits in front of the P28b relay handler. It accepts only the
// exact shape the graph menu produces (one full hex commit id, no flags), composes the confirmation text
// itself from that id (nothing the webview sent is shown), and forwards only after an explicit Continue.
// Anything else for these two verbs is not taken here and falls to the relay, which applies the allowlist.
import { fromBinary } from "@bufbuild/protobuf";
import { RunRequestSchema } from "@asc/asc-client/bridge";
import type { ConfirmRequest } from "@asc/asc-client/relay";
import type { RequestHandler, WebviewLike } from "./host";

const RUN = "asc.v1.Bridge/Run";
const PERMISSION_DENIED = 7;
const FULL_ID = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;

export type GraphVerb = "cherry-pick" | "revert";
const TITLES: Record<GraphVerb, (short: string) => string> = {
  "cherry-pick": (s) => `Cherry-pick ${s} onto the current checkout?`,
  revert: (s) => `Revert ${s} on the current checkout?`,
};

export interface GraphRun {
  verb: GraphVerb;
  rev: string;
}

/** The graph-menu call in a webview message, or undefined when the message is anything else. */
export function parseGraphRun(message: unknown): { id: unknown; call: GraphRun } | undefined {
  if (typeof message !== "object" || message === null) return undefined;
  const m = message as Record<string, unknown>;
  if (m.method !== RUN || !(m.bytes instanceof Uint8Array)) return undefined;
  let req;
  try {
    req = fromBinary(RunRequestSchema, m.bytes);
  } catch {
    return undefined;
  }
  if (req.verb !== "cherry-pick" && req.verb !== "revert") return undefined;
  const rev = req.args[0];
  if (req.args.length !== 1 || rev === undefined || !FULL_ID.test(rev)) return undefined;
  return { id: m.id, call: { verb: req.verb, rev } };
}

/** Confirmation composed from the parsed call only. */
export function graphConfirm(call: GraphRun): ConfirmRequest {
  return {
    title: TITLES[call.verb](call.rev.slice(0, 8)),
    targets: [call.rev],
    parsed: { kind: "run", verb: call.verb, args: [call.rev] },
  };
}

export interface GraphActionsOptions {
  /** The window's relay handler (P28b). */
  inner: RequestHandler;
  /** The native modal (confirm.ts `nativeConfirm`). */
  confirm: (c: ConfirmRequest) => Promise<boolean>;
}

export function withGraphActions(o: GraphActionsOptions): RequestHandler {
  return async (webview: WebviewLike, message: unknown): Promise<boolean> => {
    const parsed = parseGraphRun(message);
    if (!parsed) return o.inner(webview, message);
    let ok = false;
    try {
      ok = (await o.confirm(graphConfirm(parsed.call))) === true;
    } catch {
      ok = false;
    }
    if (!ok) {
      await webview.postMessage({ id: parsed.id, end: { code: PERMISSION_DENIED, message: "not confirmed" } });
      return true;
    }
    return o.inner(webview, message);
  };
}
