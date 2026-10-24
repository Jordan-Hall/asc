import { describe, expect, test } from "bun:test";
import { create, toBinary } from "@bufbuild/protobuf";
import { type BridgeTransport, type CallOptions } from "@asc/asc-client";
import { RunRequestSchema } from "@asc/asc-client/bridge";
import type { ConfirmRequest } from "@asc/asc-client/relay";
import { clean, confirmDetail, nativeConfirm } from "../src/webview/confirm";
import { forgejoExecutor } from "../src/webview/forgejo-relay";
import type { WebviewLike } from "../src/webview/host";
import { createRelayHandler } from "../src/webview/relay";

const calls: CallOptions[] = [];
const upstream: BridgeTransport = {
  state: "up",
  unary: async (c) => (calls.push(c), new Uint8Array([1])),
  stream: async function* (c) {
    calls.push(c);
    yield new Uint8Array([1]);
  },
  close() {},
};

function fixture(confirm: (t: string) => boolean = () => true) {
  const sent: unknown[] = [];
  const logs: string[] = [];
  const asked: string[] = [];
  calls.length = 0;
  const w = {
    postMessage: async (m: unknown) => (sent.push(m), true),
  } as unknown as WebviewLike;
  const handler = createRelayHandler({
    upstream: () => upstream,
    roots: () => [],
    forgeRepos: () => ["o/r"],
    confirm: async (c) => (asked.push(c.title), confirm(c.title)),
    log: (l) => logs.push(l),
  });
  const wait = () => new Promise((r) => setTimeout(r, 20));
  return { w, handler, sent, logs, asked, wait };
}

describe("relay", () => {
  test("forwards an allowlisted read RPC", async () => {
    const f = fixture();
    expect(f.handler(f.w, { id: 1, method: "asc.v1.Workspace/Status", bytes: new Uint8Array() })).toBe(true);
    await f.wait();
    expect(calls.length).toBe(1);
    expect(f.sent.length).toBeGreaterThan(0);
  });

  test("drops a never-listed RPC, garbage and a token-bearing field, logging no payload", async () => {
    const f = fixture();
    f.handler(f.w, { id: 2, method: "asc.v1.Bridge/Token", bytes: new Uint8Array() });
    f.handler(f.w, { id: 3, method: "asc.v1.Nope/X", bytes: new Uint8Array() });
    f.handler(f.w, "junk");
    await f.wait();
    expect(calls.length).toBe(0);
    expect(f.logs.length).toBeGreaterThanOrEqual(2);
    expect(f.logs.join("\n")).not.toContain("junk");
  });

  test("a rejected Run flag never reaches the bridge", async () => {
    const f = fixture();
    const bytes = toBinary(RunRequestSchema, create(RunRequestSchema, { verb: "sync", args: ["--exec", "x"] }));
    f.handler(f.w, { id: 4, method: "asc.v1.Bridge/Run", bytes });
    await f.wait();
    expect(calls.length).toBe(0);
  });

  test("forgejo is denied without an executor", async () => {
    const f = fixture();
    f.handler(f.w, { id: 5, forgejo: { method: "GET", path: "/repos/o/r/pulls" } });
    await f.wait();
    expect(f.sent).toEqual([{ id: 5, end: { code: 7, message: "request not allowed" } }]);
  });
});

describe("confirm", () => {
  test("sanitises control and bidi characters and caps the target list", () => {
    expect(clean("a\nb\u202ec")).toBe("a?b?c");
    const d = confirmDetail({ title: "t", targets: Array.from({ length: 12 }, (_, i) => `x${i}`), parsed: { kind: "rpc", method: "m" } });
    expect(d.split("\n").length).toBe(11);
    expect(d.endsWith("- and 2 more")).toBe(true);
  });
  test("only the action button confirms", async () => {
    const req: ConfirmRequest = { title: "Merge", targets: ["a"], parsed: { kind: "rpc", method: "m" } };
    expect(await nativeConfirm(async () => "Continue")(req)).toBe(true);
    expect(await nativeConfirm(async () => undefined)(req)).toBe(false);
    expect(await nativeConfirm(async () => { throw new Error("x"); })(req)).toBe(false);
  });
});

describe("forgejo executor", () => {
  test("rejects http, attaches the token on the host, refuses redirects", async () => {
    expect(() => forgejoExecutor({ baseUrl: "http://forge.example", getToken: () => "t" })).toThrow();
    let seen: Request | undefined;
    const ex = forgejoExecutor({
      baseUrl: "https://forge.example",
      getToken: () => "sekrit",
      fetch: async (r) => ((seen = r), new Response('{"ok":true}', { status: 200 })),
    });
    const r = await ex({ method: "GET", path: "/repos/o/r/pulls" }, new AbortController().signal);
    expect(r).toEqual({ status: 200, body: { ok: true } });
    expect(seen?.url).toBe("https://forge.example/api/v1/repos/o/r/pulls");
    expect(seen?.headers.get("authorization")).toBe("token sekrit");
    expect(seen?.redirect).toBe("error");
  });
  test("the reply never carries the token", async () => {
    const ex = forgejoExecutor({ baseUrl: "https://f.example", getToken: () => "sekrit", fetch: async () => new Response("not json") });
    expect(JSON.stringify(await ex({ method: "GET", path: "/user" }, new AbortController().signal))).not.toContain("sekrit");
  });
});

describe("run verbs", () => {
  test("a valid verb is forwarded and an invalid flag is not", async () => {
    const f = fixture();
    const ok = toBinary(RunRequestSchema, create(RunRequestSchema, { verb: "sync", args: [] }));
    f.handler(f.w, { id: 8, method: "asc.v1.Bridge/Run", bytes: ok });
    await f.wait();
    expect(calls.length).toBe(1);
  });
});
