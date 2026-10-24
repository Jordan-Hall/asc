import { describe, expect, test } from "bun:test";
import { createForgejoClient, createGuard, ForgejoDeniedError, parseBaseUrl } from "../src/forgejo";
import allowlist from "../src/transport/allowlist.json" with { type: "json" };

const BASE = "https://forge.example";

interface Seen {
  url: string;
  method: string;
  auth: string | null;
  body: string;
}

function harness(opts: { repos?: string[]; token?: string } = {}) {
  const seen: Seen[] = [];
  const client = createForgejoClient({
    baseUrl: BASE,
    repos: opts.repos,
    getToken: () => opts.token,
    fetch: async (req) => {
      seen.push({
        url: req.url,
        method: req.method,
        auth: req.headers.get("authorization"),
        body: await req.text(),
      });
      return Response.json({ id: 1, login: "ann" });
    },
  });
  return { client, seen };
}

describe("forgejo client", () => {
  test("allowed GET goes through with the injected fetch and the token header", async () => {
    const { client, seen } = harness({ token: "s3cret" });
    const { data } = await client.api.GET("/user");
    expect(data).toBeDefined();
    expect(seen).toHaveLength(1);
    expect(seen[0]?.url).toBe(`${BASE}/api/v1/user`);
    expect(seen[0]?.auth).toBe("token s3cret");
  });

  test("path parameters are filled and encoded", async () => {
    const { client, seen } = harness();
    await client.api.GET("/repos/{o}/{r}/pulls/{n}", {
      params: { path: { o: "acme", r: "app", n: 7 } },
    });
    expect(seen[0]?.url).toBe(`${BASE}/api/v1/repos/acme/app/pulls/7`);
  });

  test("PATCH body is limited to the allowlisted fields", async () => {
    const { client, seen } = harness();
    await client.api.PATCH("/repos/{o}/{r}/pulls/{n}", {
      params: { path: { o: "acme", r: "app", n: 7 } },
      body: { title: "t", draft: false },
    });
    expect(JSON.parse(seen[0]?.body ?? "{}")).toEqual({ title: "t", draft: false });
    await expect(
      client.api.PATCH("/repos/{o}/{r}/pulls/{n}", {
        params: { path: { o: "acme", r: "app", n: 7 } },
        body: { title: "t", state: "closed" } as never,
      }),
    ).rejects.toBeInstanceOf(ForgejoDeniedError);
    expect(seen).toHaveLength(1);
  });

  test("repo scope rejects a repo that is not an opened asc root", async () => {
    const { client, seen } = harness({ repos: ["acme/app"] });
    await client.api.GET("/repos/{o}/{r}/assignees", { params: { path: { o: "acme", r: "app" } } });
    await expect(
      client.api.GET("/repos/{o}/{r}/assignees", { params: { path: { o: "evil", r: "app" } } }),
    ).rejects.toBeInstanceOf(ForgejoDeniedError);
    expect(seen).toHaveLength(1);
  });

  test("a review is created pending: event is rejected", async () => {
    const { client } = harness();
    await expect(
      client.api.POST("/repos/{o}/{r}/pulls/{n}/reviews", {
        params: { path: { o: "a", r: "b", n: 1 } },
        body: { body: "x", event: "APPROVED" } as never,
      }),
    ).rejects.toBeInstanceOf(ForgejoDeniedError);
  });

  test("the baseUrl must be https; loopback http is opt-in", () => {
    expect(() => parseBaseUrl("http://forge.example", false)).toThrow(ForgejoDeniedError);
    expect(() => parseBaseUrl("http://127.0.0.1:3000", false)).toThrow(ForgejoDeniedError);
    expect(parseBaseUrl("http://127.0.0.1:3000", true).hostname).toBe("127.0.0.1");
    expect(() => parseBaseUrl("http://forge.example", true)).toThrow(ForgejoDeniedError);
    expect(() => parseBaseUrl("https://u:p@forge.example", false)).toThrow(ForgejoDeniedError);
  });

  test("a caller-set authorization header is replaced, not forwarded", async () => {
    const { client, seen } = harness();
    await client.api.GET("/user", { headers: { authorization: "token attacker" } });
    expect(seen[0]?.auth).toBeNull();
  });
});

describe("guard", () => {
  const guard = createGuard({ pairs: allowlist.hostApi.providers.forgejo.pairs });
  const denied = (m: string, p: string, body?: unknown) => () => guard.check(m, p, body);

  test("DELETE and unlisted paths are denied", () => {
    expect(denied("DELETE", "/api/v1/repos/a/b/pulls/1")).toThrow(ForgejoDeniedError);
    expect(denied("GET", "/api/v1/admin/users")).toThrow(ForgejoDeniedError);
    expect(denied("GET", "/api/v1/repos/a/b/hooks")).toThrow(ForgejoDeniedError);
    expect(denied("PATCH", "/api/v1/repos/a/b")).toThrow(ForgejoDeniedError);
    expect(denied("GET", "/user")).toThrow(ForgejoDeniedError);
  });

  test("traversal and encoded slashes are denied", () => {
    expect(denied("GET", "/api/v1/repos/a/%2e%2e/pulls/1")).toThrow(ForgejoDeniedError);
    expect(denied("GET", "/api/v1/repos/a%2Fb/c/pulls/1")).toThrow(ForgejoDeniedError);
  });

  test("merge is flagged for modal confirmation", () => {
    const m = guard.check("POST", "/api/v1/repos/a/b/pulls/3/merge", { Do: "merge" });
    expect(m.pair.confirm).toBe("modal");
    expect(m.repo).toBe("a/b");
  });

  test("every allowlist pair matches itself", () => {
    for (const p of allowlist.hostApi.providers.forgejo.pairs) {
      const concrete = `/api/v1${p.p.replace(/\{[^}]*\}/g, "1")}`;
      expect(guard.check(p.m, concrete, undefined).pair.p).toBe(p.p);
    }
  });
});
