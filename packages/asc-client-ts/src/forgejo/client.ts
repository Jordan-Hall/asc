// Forgejo REST client: openapi-fetch over the generated, allowlist-only schema, with an injectable
// `fetch`. Every request passes the deny-by-default guard; the token is attached here and never logged.
import createClient from "openapi-fetch";
import allowlist from "../transport/allowlist.json" with { type: "json" };
import { createGuard, ForgejoDeniedError, type ForgejoPair, type Guard } from "./guard";
import type { paths } from "./schema";

export type FetchLike = (input: Request) => Promise<Response>;

export interface ForgejoClientOptions {
  /** The forge origin, `https://forge.example` (a sub-path install is allowed). */
  readonly baseUrl: string;
  /** Injectable transport; defaults to global `fetch`. */
  readonly fetch?: FetchLike;
  /** Supplies the token per request. It is sent as `Authorization: token ...` and never logged. */
  readonly getToken?: () => string | undefined | Promise<string | undefined>;
  /** Restrict `/repos/{o}/{r}` to these `owner/repo` names (the opened asc roots). */
  readonly repos?: Iterable<string> | undefined;
  /** Opt-in: allow `http://` to a loopback host (tests, local dev). Otherwise https only. */
  readonly allowLoopbackHttp?: boolean;
  /** Override the allowlist pairs (tests). */
  readonly pairs?: readonly ForgejoPair[];
}

const LOOPBACK = new Set(["localhost", "127.0.0.1", "[::1]"]);

export function parseBaseUrl(raw: string, allowLoopbackHttp: boolean): URL {
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    throw new ForgejoDeniedError("baseUrl is not a URL");
  }
  if (u.username || u.password) throw new ForgejoDeniedError("baseUrl must not carry credentials");
  const ok =
    u.protocol === "https:" ||
    (allowLoopbackHttp && u.protocol === "http:" && LOOPBACK.has(u.hostname));
  if (!ok) throw new ForgejoDeniedError("baseUrl must be https (loopback http only by opt-in)");
  return u;
}

export interface ForgejoClient {
  readonly api: ReturnType<typeof createClient<paths>>;
  readonly guard: Guard;
}

export function createForgejoClient(opts: ForgejoClientOptions): ForgejoClient {
  const origin = parseBaseUrl(opts.baseUrl, opts.allowLoopbackHttp ?? false);
  const prefix = origin.pathname.replace(/\/+$/, "");
  const pairs = opts.pairs ?? (allowlist.forgejo.pairs as ForgejoPair[]);
  const guard = createGuard({
    pairs,
    base: `${prefix}${allowlist.forgejo.base}`,
    repos: opts.repos ? new Set(opts.repos) : undefined,
  });
  const inner: FetchLike = opts.fetch ?? ((r) => fetch(r));

  const guarded: FetchLike = async (req) => {
    const url = new URL(req.url);
    if (url.origin !== origin.origin)
      throw new ForgejoDeniedError("request leaves the forge origin");
    let body: unknown;
    if (req.method !== "GET" && req.method !== "HEAD") {
      const text = await req.clone().text();
      if (text) {
        try {
          body = JSON.parse(text);
        } catch {
          throw new ForgejoDeniedError("body is not JSON");
        }
      }
    }
    guard.check(req.method, url.pathname, body);
    const headers = new Headers(req.headers);
    headers.delete("authorization");
    const token = await opts.getToken?.();
    if (token) headers.set("authorization", `token ${token}`);
    const hasBody = req.method !== "GET" && req.method !== "HEAD";
    return inner(
      new Request(req.url, {
        method: req.method,
        headers,
        body: hasBody ? await req.arrayBuffer() : null,
        redirect: "error",
        credentials: "omit",
        signal: req.signal,
      }),
    );
  };

  const api = createClient<paths>({
    baseUrl: origin.origin + prefix + allowlist.forgejo.base,
    fetch: guarded,
  });
  return { api, guard };
}
