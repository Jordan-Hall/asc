// Deny-by-default guard for Forgejo REST calls (SPEC §24.4.5.1). Only the (method, path template)
// pairs of the allowlist's `forgejo` section pass; a body may carry only the fields listed for the pair.
export interface ForgejoPair {
  readonly m: string;
  readonly p: string;
  readonly body?: readonly string[];
  readonly confirm?: string;
}

export class ForgejoDeniedError extends Error {
  override name = "ForgejoDeniedError";
}

interface Compiled {
  pair: ForgejoPair;
  segments: readonly string[];
  method: string;
}

const PARAM = /^\{[A-Za-z_]+\}$/;
const SEGMENT = /^[A-Za-z0-9._~-]{1,255}$/;

const compile = (pair: ForgejoPair): Compiled => ({
  pair,
  method: pair.m.toUpperCase(),
  segments: pair.p.split("/").filter(Boolean),
});

export interface Matched {
  readonly pair: ForgejoPair;
  /** `owner/repo` when the path is `/repos/{o}/{r}/...`. */
  readonly repo: string | undefined;
}

export interface Guard {
  /** Throws {@link ForgejoDeniedError} unless the request is allowed. */
  check(method: string, pathname: string, body: unknown): Matched;
}

export interface GuardOptions {
  readonly pairs: readonly ForgejoPair[];
  /** API path prefix on the forge, `/api/v1` by default. */
  readonly base?: string;
  /** When set, `/repos/{o}/{r}` must be one of these `owner/repo` names. */
  readonly repos?: ReadonlySet<string> | undefined;
}

export function createGuard(opts: GuardOptions): Guard {
  const compiled = opts.pairs.map(compile);
  const base = (opts.base ?? "/api/v1").replace(/\/+$/, "");
  return {
    check(method, pathname, body) {
      const m = method.toUpperCase();
      if (m === "DELETE") throw new ForgejoDeniedError("DELETE is never allowed");
      if (!pathname.startsWith(`${base}/`))
        throw new ForgejoDeniedError("path is outside the API base");
      let rel: string[];
      try {
        rel = pathname
          .slice(base.length)
          .split("/")
          .filter(Boolean)
          .map((s) => decodeURIComponent(s));
      } catch {
        throw new ForgejoDeniedError("malformed path");
      }
      if (rel.some((s) => s === "." || s === ".." || s.includes("/") || s.includes("\\") || !s)) {
        throw new ForgejoDeniedError("path traversal");
      }
      const hit = compiled.find(
        (c) =>
          c.method === m &&
          c.segments.length === rel.length &&
          c.segments.every((seg, i) =>
            PARAM.test(seg) ? SEGMENT.test(rel[i] ?? "") : seg === rel[i],
          ),
      );
      if (!hit) throw new ForgejoDeniedError(`${m} ${pathname} is not on the allowlist`);
      let repo: string | undefined;
      if (
        hit.segments[0] === "repos" &&
        PARAM.test(hit.segments[1] ?? "") &&
        PARAM.test(hit.segments[2] ?? "")
      ) {
        repo = `${rel[1]}/${rel[2]}`;
        if (opts.repos && !opts.repos.has(repo))
          throw new ForgejoDeniedError(`repo ${repo} is not an opened asc root`);
      }
      checkBody(hit.pair, body);
      return { pair: hit.pair, repo };
    },
  };
}

function checkBody(pair: ForgejoPair, body: unknown): void {
  if (body === undefined || body === null) return;
  if (typeof body !== "object" || Array.isArray(body))
    throw new ForgejoDeniedError("body must be a JSON object");
  const keys = Object.keys(body);
  if (keys.length === 0) return;
  const allowed = new Set(pair.body ?? []);
  for (const k of keys) {
    if (!allowed.has(k))
      throw new ForgejoDeniedError(`body field ${k} is not allowed for ${pair.m} ${pair.p}`);
  }
  // SPEC §24.4.5.1: creating a review sends a pending review only; `event` submits it.
  if (pair.m.toUpperCase() === "POST" && pair.p.endsWith("/reviews") && "event" in body) {
    throw new ForgejoDeniedError("a review must be created pending; submit it separately");
  }
}
