// P10 acceptance: the relay forwards a webview message only when it matches an allowlist.json entry;
// gen-allowlist fails on any unclassified RPC, verb flag or Forgejo operation.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { create, toBinary } from "@bufbuild/protobuf";
import {
  ALLOWLIST_PATH,
  ARGV_SCHEMA_PATH,
  parseRustVerbs,
  protoRpcs,
  validateAllowlist,
} from "../scripts/gen-allowlist";
import { RunRequestSchema } from "../src/gen/asc/v1/bridge_pb";
import { RequestMergeRequestSchema } from "../src/gen/asc/v1/changes_pb";
import { WsStageRequestSchema, WsStatusRequestSchema } from "../src/gen/asc/v1/workspace_pb";
import {
  type BridgeTransport,
  type CallOptions,
  type HostApiRequest,
  type HostReply,
  type HostRequest,
  host,
} from "../src/index";
import { parseAllowlist, type RelayLog, relay } from "../src/transport/relay";

const here = dirname(fileURLToPath(import.meta.url));
const raw = JSON.parse(readFileSync(ALLOWLIST_PATH, "utf8")) as Record<string, unknown>;
const al = parseAllowlist(raw);
const rustVerbs = parseRustVerbs(readFileSync(ARGV_SCHEMA_PATH, "utf8"));
const ROOT = resolve(here, "fixture-root");
const TMP = resolve(here, "fixture-tmp");

describe("gen-allowlist", () => {
  const check = (a: unknown, swagger?: Parameters<typeof validateAllowlist>[0]["swagger"]) =>
    validateAllowlist({ allowlist: a, rpcs: protoRpcs(), rustVerbs, swagger });
  // oxlint-disable-next-line typescript/no-explicit-any -- a mutable JSON clone for the negative cases
  const clone = () => structuredClone(raw) as Record<string, any>;

  test("the checked-in allowlist is valid and the Rust table parses", () => {
    expect(rustVerbs.length).toBeGreaterThan(40);
    expect(check(raw)).toEqual([]);
  });

  test("an unclassified RPC fails", () => {
    const a = clone();
    a.rpc.never = a.rpc.never.filter((n: string) => n !== "asc.v1.Workspace/Ping");
    expect(check(a)).toContain("rpc asc.v1.Workspace/Ping is unclassified");
  });

  test("an unknown RPC and a double classification fail", () => {
    const a = clone();
    a.rpc.read.push("asc.v1.Workspace/Nope", "asc.v1.Workspace/Ping");
    const e = check(a);
    expect(e.some((x) => x.includes("Nope does not exist"))).toBe(true);
    expect(e.some((x) => x.includes("Ping is classified twice"))).toBe(true);
  });

  test("an unknown or differing verb flag fails", () => {
    const a = clone();
    a.run.verbs.absorb.flags["--exec"] = "bool";
    a.run.verbs.create.flags["--bogus"] = "bool";
    delete a.run.verbs.sync;
    const e = check(a).join("\n");
    expect(e).toContain("flag --exec is in run.reject");
    expect(e).toContain("flag --bogus is bool, Rust has none");
    expect(e).toContain('run verb "sync" is in argv_schema.rs');
  });

  test("a Forgejo pair outside the rules fails", () => {
    const a = clone();
    a.hostApi.providers.forgejo.pairs.push(
      { m: "DELETE", p: "/repos/{o}/{r}/pulls/{n}" },
      { m: "GET", p: "/admin/users" },
      { m: "PATCH", p: "/repos/{o}/{r}", body: ["name"] },
      { m: "POST", p: "/repos/{o}/{r}/issues" },
    );
    expect(check(a).length).toBeGreaterThanOrEqual(5);
  });

  test("pairs and body fields are checked against a swagger", () => {
    const swagger = {
      paths: {
        "/user": { get: {} },
        "/repos/{o}/{r}/pulls/{n}": {
          patch: { parameters: [{ in: "body", schema: { $ref: "#/definitions/Edit" } }] },
        },
      },
      definitions: { Edit: { properties: { title: {}, body: {} } } },
    };
    const e = check(raw, swagger).join("\n");
    expect(e).toContain("GET /user/asc/inbox is not in the swagger");
    expect(e).toContain("body field draft is not in the swagger");
    expect(e).not.toContain("GET /user is not");
  });
});

// ---- the relay ----

const call = (method: string, bytes: Uint8Array): CallOptions => ({ method, request: bytes });

function fakeUpstream() {
  const calls: CallOptions[] = [];
  const t: BridgeTransport = {
    state: "up",
    unary: async (c) => {
      calls.push(c);
      return new Uint8Array([1]);
    },
    async *stream(c) {
      calls.push(c);
      yield new Uint8Array([1]);
      yield new Uint8Array([2]);
    },
    close() {},
  };
  return { t, calls };
}

function rig(over: Partial<Parameters<typeof relay>[0]> = {}) {
  const up = fakeUpstream();
  const sent: HostReply[] = [];
  const logs: RelayLog[] = [];
  const hostApiCalls: HostApiRequest[] = [];
  const r = relay({
    upstream: up.t,
    allowlist: al,
    send: (m) => sent.push(m),
    roots: () => [ROOT],
    remoteRepos: () => ["acme/app"],
    tmpDir: TMP,
    confirm: async () => true,
    hostApi: async (req) => {
      hostApiCalls.push(req);
      return { status: 200, body: {} };
    },
    log: (e) => logs.push(e),
    ...over,
  });
  const tick = () => new Promise((res) => setTimeout(res, 5));
  return { r, up, sent, logs, hostApiCalls, tick };
}

const status = toBinary(WsStatusRequestSchema, create(WsStatusRequestSchema, {}));
const run = (verb: string, args: string[], root = ROOT) =>
  toBinary(RunRequestSchema, create(RunRequestSchema, { verb, args, root }));
const denied = (sent: HostReply[], id: number) =>
  sent.some((m) => m.id === id && "end" in m && m.end.code === 7);

describe("relay: rpc", () => {
  test("a listed read is forwarded and its replies end with OK", async () => {
    const t = rig();
    t.r.handle({ id: 1, method: "asc.v1.Workspace/Status", bytes: status });
    await t.tick();
    expect(t.up.calls.map((c) => c.method)).toEqual(["asc.v1.Workspace/Status"]);
    expect(t.sent.map((m) => ("reply" in m ? "reply" : "end" in m ? m.end.code : "?"))).toEqual([
      "reply",
      "reply",
      0,
    ]);
  });

  test("never, unknown, unclassified and malformed messages are dropped", async () => {
    const t = rig();
    const bad: unknown[] = [
      { id: 2, method: "asc.v1.Workspace/Shutdown", bytes: status },
      { id: 3, method: "asc.v1.Bridge/Token", bytes: status },
      { id: 4, method: "asc.v1.Repo/UpdateRefs", bytes: status },
      { id: 5, method: "asc.v1.Nope/Nope", bytes: status },
      { id: 6, method: "asc.v1.Workspace/Status", bytes: "not bytes" },
      { id: 0, method: "asc.v1.Workspace/Status", bytes: status },
      { method: "asc.v1.Workspace/Status", bytes: status },
      "text",
      null,
    ];
    for (const m of bad) t.r.handle(m);
    await t.tick();
    expect(t.up.calls).toEqual([]);
    for (const id of [2, 3, 4, 5, 6]) expect(denied(t.sent, id)).toBe(true);
    expect(t.logs.length).toBe(bad.length);
  });

  test("a root that was not opened is refused", async () => {
    const t = rig();
    t.r.handle({
      id: 1,
      method: "asc.v1.Workspace/Status",
      bytes: status,
      root: resolve(here, "other"),
    });
    await t.tick();
    expect(t.up.calls).toEqual([]);
    expect(denied(t.sent, 1)).toBe(true);
  });

  test("Stage is bounded by its limits", async () => {
    const t = rig();
    const stage = (add: number, patch: number) =>
      toBinary(
        WsStageRequestSchema,
        create(WsStageRequestSchema, {
          add: Array.from({ length: add }, () => new Uint8Array([97])),
          patch: new Uint8Array(patch),
        }),
      );
    t.r.handle({ id: 1, method: "asc.v1.Workspace/Stage", bytes: stage(2, 10) });
    t.r.handle({ id: 2, method: "asc.v1.Workspace/Stage", bytes: stage(10001, 0) });
    t.r.handle({ id: 3, method: "asc.v1.Workspace/Stage", bytes: stage(1, 1048577) });
    await t.tick();
    expect(t.up.calls.length).toBe(1);
    expect(denied(t.sent, 2) && denied(t.sent, 3)).toBe(true);
  });

  test("RequestMerge needs a native confirmation", async () => {
    const merge = toBinary(
      RequestMergeRequestSchema,
      create(RequestMergeRequestSchema, { change: new Uint8Array(16).fill(7) }),
    );
    const asked: string[] = [];
    const yes = rig({
      confirm: async (c) => {
        asked.push(`${c.title}|${c.targets.join(",")}`);
        return true;
      },
    });
    yes.r.handle({ id: 1, method: "asc.v1.Changes/RequestMerge", bytes: merge });
    await yes.tick();
    expect(yes.up.calls.length).toBe(1);
    expect(asked).toEqual([`Merge change|${"07".repeat(16)}`]);

    const no = rig({ confirm: async () => false });
    no.r.handle({ id: 1, method: "asc.v1.Changes/RequestMerge", bytes: merge });
    await no.tick();
    expect(no.up.calls).toEqual([]);
    expect(denied(no.sent, 1)).toBe(true);

    const none = rig({ confirm: undefined });
    none.r.handle({ id: 1, method: "asc.v1.Changes/RequestMerge", bytes: merge });
    await none.tick();
    expect(none.up.calls).toEqual([]);
  });

  test("a live id cannot be reused and a cancel stops the call", async () => {
    const sent: HostReply[] = [];
    let aborted = false;
    const hang: BridgeTransport = {
      state: "up",
      unary: async () => new Uint8Array(),
      async *stream(c) {
        await new Promise<void>((res) => c.signal?.addEventListener("abort", () => res()));
        aborted = true;
        yield new Uint8Array();
      },
      close() {},
    };
    const r = relay({
      upstream: hang,
      allowlist: al,
      send: (m) => sent.push(m),
      roots: () => [ROOT],
      remoteRepos: () => [],
    });
    r.handle({ id: 1, method: "asc.v1.Workspace/Watch", bytes: status });
    r.handle({ id: 1, method: "asc.v1.Workspace/Status", bytes: status });
    await new Promise((res) => setTimeout(res, 5));
    r.handle({ id: 1, cancel: true });
    await new Promise((res) => setTimeout(res, 5));
    expect(aborted).toBe(true);
    expect(sent).toEqual([]);
  });
});

describe("relay: Bridge.Run", () => {
  const ok = async (verb: string, args: string[]) => {
    const t = rig();
    t.r.handle({ id: 1, method: "asc.v1.Bridge/Run", bytes: run(verb, args), root: ROOT });
    await t.tick();
    return t;
  };

  test("a valid verb is forwarded", async () => {
    const t = await ok("modify", ["--into=abc123", "-m=hello", "src/a.ts"]);
    expect(t.up.calls.length).toBe(1);
    expect((await ok("up", ["3"])).up.calls.length).toBe(1);
    expect((await ok("stack get", ["--worktree=wt", "main"])).up.calls.length).toBe(1);
  });

  test("everything outside the schema is refused", async () => {
    const cases: [string, string[]][] = [
      ["modify", ["-c=core.pager=evil"]],
      ["modify", ["--exec=sh"]],
      ["push", ["--force"]],
      ["push", ["-u", "--force-with-lease"]],
      ["create", ["--bogus"]],
      ["not-a-verb", []],
      ["alias.x", []],
      ["reword", ["--", "x"]],
      ["reword", ["-m"]],
      ["reword", ["-m=a", "-m=b"]],
      ["absorb", ["-n=1"]],
      ["switch", ["--detach"]],
      ["switch", ["-x"]],
      ["move", ["--onto=--upload-pack=evil"]],
      ["reword", ["--upload-pack=x"]],
      ["up", ["101"]],
      ["up", ["1", "2"]],
      ["submit", ["--reviewer=bad name"]],
      ["restore", []],
      ["create", ["../outside"]],
      ["create", ["a\0b"]],
      ["reorder", ["--todo=relative.txt"]],
      ["reorder", [`--todo=${resolve(here, "elsewhere.txt")}`]],
      ["reword", [`-m=${"x".repeat(65537)}`]],
      ["create", Array.from({ length: 65 }, (_, i) => `f${i}`)],
    ];
    for (const [verb, args] of cases) {
      const t = await ok(verb, args);
      expect([verb, args.length, t.up.calls.length]).toEqual([verb, args.length, 0]);
      expect(denied(t.sent, 1)).toBe(true);
    }
  });

  test("a tmpfile inside the plugin directory is accepted", async () => {
    const t = await ok("reorder", [`--todo=${join(TMP, "todo.txt")}`]);
    expect(t.up.calls.length).toBe(1);
  });

  test("a confirming verb asks first, and the log holds no payload", async () => {
    const asked: string[] = [];
    const t = rig({
      confirm: async (c) => {
        asked.push(c.title);
        return false;
      },
    });
    t.r.handle({ id: 1, method: "asc.v1.Bridge/Run", bytes: run("abandon", ["abc"]), root: ROOT });
    t.r.handle({
      id: 2,
      method: "asc.v1.Bridge/Run",
      bytes: run("reword", ["-m=SECRET-TOKEN", "--exec=x"]),
      root: ROOT,
    });
    await t.tick();
    expect(asked).toEqual(['Run "asc abandon"']);
    expect(t.up.calls).toEqual([]);
    expect(JSON.stringify(t.logs)).not.toContain("SECRET-TOKEN");
  });

  test("the forwarded bytes are re-encoded from the parsed request", async () => {
    const t = await ok("sync", []);
    expect(t.up.calls[0]?.request).toEqual(run("sync", [], ROOT));
  });
});

describe("relay: host API", () => {
  const f = async (method: string, path: string, body?: unknown, over = {}) => {
    const t = rig(over);
    t.r.handle({
      id: 1,
      hostApi: { provider: "forgejo", method, path, ...(body === undefined ? {} : { body }) },
    });
    await t.tick();
    return t;
  };

  test("a listed pair for an opened repo is forwarded", async () => {
    const t = await f("GET", "/repos/acme/app/pulls/12/files?page=2");
    expect(t.hostApiCalls.length).toBe(1);
    expect(t.sent[0]).toEqual({ id: 1, hostApi: { status: 200, body: {} } });
    const patch = await f("PATCH", "/repos/acme/app/pulls/3", { title: "t", draft: true });
    expect(patch.hostApiCalls.length).toBe(1);
    expect((await f("GET", "/user")).hostApiCalls.length).toBe(1);
  });

  test("an unknown provider is dropped", async () => {
    const t = rig();
    t.r.handle({ id: 1, hostApi: { provider: "other", method: "GET", path: "/user" } });
    await t.tick();
    expect(t.hostApiCalls.length).toBe(0);
    expect(denied(t.sent, 1)).toBe(true);
  });

  test("everything else is dropped", async () => {
    const cases: [string, string, unknown?][] = [
      ["DELETE", "/repos/acme/app/pulls/3"],
      ["GET", "/admin/users"],
      ["GET", "/repos/acme/other/pulls/3"],
      ["GET", "/repos/acme/app/pulls/x"],
      ["GET", "/repos/acme/app/../../admin/users"],
      ["GET", "/repos/acme/app/pulls/3%2f..%2f"],
      ["GET", "/repos/acme/app/pulls/3", { x: 1 }],
      ["PATCH", "/repos/acme/app", { name: "x" }],
      ["PATCH", "/repos/acme/app/pulls/3", { state: "closed" }],
      ["POST", "/repos/acme/app/pulls/3/reviews", { body: "b", event: "APPROVE" }],
      ["POST", "/repos/acme/app/pulls/3/merge", { Do: "rm -rf" }],
      ["GET", "/repos/acme/app/hooks"],
      ["GET", "/repos/acme/app/pulls/3#frag"],
      ["GET", "repos/acme/app/pulls/3"],
    ];
    for (const [m, p, b] of cases) {
      const t = await f(m, p, b);
      expect([m, p, t.hostApiCalls.length]).toEqual([m, p, 0]);
      expect(denied(t.sent, 1)).toBe(true);
    }
  });

  test("merge and review submit ask for confirmation; no executor means no call", async () => {
    const asked: string[] = [];
    const t = await f(
      "POST",
      "/repos/acme/app/pulls/3/merge",
      { Do: "squash" },
      {
        confirm: async (c: { title: string }) => {
          asked.push(c.title);
          return true;
        },
      },
    );
    expect(asked).toEqual(["Merge pull request"]);
    expect(t.hostApiCalls.length).toBe(1);
    const s = await f(
      "POST",
      "/repos/acme/app/pulls/3/reviews/9",
      { body: "ok", event: "APPROVED" },
      {
        confirm: async (c: { title: string }) => {
          asked.push(c.title);
          return false;
        },
      },
    );
    expect(asked[1]).toBe("Submit review");
    expect(s.hostApiCalls.length).toBe(0);
    const none = await f("GET", "/user", undefined, { hostApi: undefined });
    expect(denied(none.sent, 1)).toBe(true);
  });
});

describe("host transport against the relay", () => {
  test("unary and stream round trip; a denied call is a BridgeError with code 7", async () => {
    const up = fakeUpstream();
    let toHost: (m: HostReply) => void = () => {};
    const rl = relay({
      upstream: up.t,
      allowlist: al,
      send: (m) => queueMicrotask(() => toHost(m)),
      roots: () => [ROOT],
      remoteRepos: () => [],
    });
    const t = host({
      post: (m: HostRequest) => queueMicrotask(() => rl.handle(m)),
      listen(h) {
        toHost = h;
        return () => {};
      },
    });
    expect(await t.unary(call("asc.v1.Workspace/Status", status))).toEqual(new Uint8Array([1]));
    const got: number[] = [];
    for await (const r of t.stream(call("asc.v1.Workspace/Watch", status))) got.push(r[0] ?? 0);
    expect(got).toEqual([1, 2]);
    await expect(t.unary(call("asc.v1.Workspace/Shutdown", status))).rejects.toMatchObject({
      code: 7,
    });
  });
});
