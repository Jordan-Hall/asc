// Host-side Forgejo executor for the relay (SPEC 24.4.5, IP-D4). The relay has already matched the
// request against the (method, path) pairs; this adds the token (read on the host per request, never
// logged and never returned to the webview), keeps the request on the forge origin over https, and
// refuses redirects. Responses are size-capped and returned as parsed JSON.
import type { ForgejoReply, ForgejoRequest } from "@asc/asc-client";
import { parseBaseUrl } from "@asc/asc-client/forgejo";

export const MAX_REPLY_BYTES = 8 * 1024 * 1024;
export const FORGEJO_TIMEOUT_MS = 30_000;

export interface ForgejoExecutorOptions {
  /** The forge origin; https only unless `allowLoopbackHttp`. */
  baseUrl: string;
  allowLoopbackHttp?: boolean;
  getToken: () => string | undefined | Promise<string | undefined>;
  fetch?: (input: Request) => Promise<Response>;
}

export function forgejoExecutor(
  o: ForgejoExecutorOptions,
): (req: ForgejoRequest, signal: AbortSignal) => Promise<ForgejoReply> {
  const origin = parseBaseUrl(o.baseUrl, o.allowLoopbackHttp ?? false);
  const prefix = origin.pathname.replace(/\/+$/, "");
  const doFetch = o.fetch ?? ((r: Request) => fetch(r));

  return async (req, signal) => {
    const url = new URL(`${origin.origin}${prefix}/api/v1${req.path}`);
    if (url.origin !== origin.origin) throw new Error("request leaves the forge origin");
    const headers = new Headers({ accept: "application/json" });
    const token = await o.getToken();
    if (token) headers.set("authorization", `token ${token}`);
    const hasBody = req.body !== undefined && req.method !== "GET" && req.method !== "HEAD";
    if (hasBody) headers.set("content-type", "application/json");
    const timeout = AbortSignal.timeout(FORGEJO_TIMEOUT_MS);
    const res = await doFetch(
      new Request(url.toString(), {
        method: req.method,
        headers,
        body: hasBody ? JSON.stringify(req.body) : null,
        redirect: "error",
        credentials: "omit",
        signal: AbortSignal.any([signal, timeout]),
      }),
    );
    const len = Number(res.headers.get("content-length") ?? "0");
    if (len > MAX_REPLY_BYTES) throw new Error("forge reply too large");
    const buf = new Uint8Array(await res.arrayBuffer());
    if (buf.byteLength > MAX_REPLY_BYTES) throw new Error("forge reply too large");
    const text = new TextDecoder().decode(buf);
    let body: unknown = null;
    if (text !== "") {
      try {
        body = JSON.parse(text);
      } catch {
        body = null;
      }
    }
    return { status: res.status, body };
  };
}
