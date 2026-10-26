import { describe, expect, test } from "bun:test";
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { type BridgeNode, changeIdToString } from "@asc/asc-client";
import {
  AddCommentRequestSchema,
  AddCommentResponseSchema,
  DiffSide,
  ListThreadsResponseSchema,
  ResolveThreadRequestSchema,
  ResolveThreadResponseSchema,
  ThreadStatus,
} from "@asc/asc-client/review";
import type { BridgeManager } from "../src/bridge";
import { checkoutArgs, StackCheckout, targetArg } from "../src/review/checkout";
import {
  bridgeReviewApi,
  type ChangeRef,
  type ControllerPort,
  mapUri,
  MAX_COMMENT_BYTES,
  MAX_CONTROLLERS,
  MAX_THREADS_PER_CHANGE,
  moreThreadsNotice,
  type NewComment,
  type ReviewApi,
  ReviewControllers,
  THREAD_PAGE,
  type ThreadRow,
  validateCommentText,
} from "../src/review/comments";

const BASE = "a".repeat(40);
const HEAD = "b".repeat(40);
const ref = (n = 1): ChangeRef => ({
  root: "/w",
  repo: "o/r",
  change: String.fromCharCode(122 - (n % 16)).repeat(31) + String.fromCharCode(97 + n),
  mergeBase: BASE,
  head: HEAD,
});
const q = (rev: string) => `root=%2Fw&rev=${rev}`;

const thread = (id: number, over: Partial<ThreadRow> = {}): ThreadRow => ({
  id,
  path: "src/a.ts",
  version: 1,
  side: "new",
  line: id,
  resolved: false,
  comments: [{ id, author: "ada", body: `c${id}`, createdAt: "2026-01-01T00:00:00Z" }],
  ...over,
});

function fakes(total: number, over: (i: number) => Partial<ThreadRow> = () => ({})) {
  const pages: string[] = [];
  const added: NewComment[] = [];
  const resolved: [number, boolean][] = [];
  const api: ReviewApi = {
    async listThreads(_c, token, limit) {
      pages.push(token);
      const from = token === "" ? 0 : Number(token);
      const items: ThreadRow[] = [];
      for (let i = from; i < Math.min(total, from + limit); i++) items.push(thread(i + 1, over(i)));
      const end = from + items.length;
      return { items, next: end < total ? String(end) : "" };
    },
    async addComment(_c, n) {
      added.push(n);
    },
    async resolveThread(_c, id, r) {
      resolved.push([id, r]);
    },
    async requestReview() {},
  };
  const ports: { notices: string[]; live: number; disposed: boolean; metas: number[] }[] = [];
  const make = (): ControllerPort => {
    const st = { notices: [] as string[], live: 0, disposed: false, metas: [] as number[] };
    ports.push(st);
    return {
      createThread(_f, _q, _l, _c, _r, meta) {
        st.live++;
        st.metas.push(meta.id);
        return { dispose: () => void st.live-- };
      },
      notice: (m) => void st.notices.push(m),
      dispose: () => void (st.disposed = true),
    };
  };
  return { api, pages, added, resolved, ports, make };
}

describe("review: uri mapping", () => {
  test("head is new, merge base is old, others unmapped", () => {
    expect(mapUri(ref(), "/src/a.ts", q(HEAD))).toEqual({ path: "src/a.ts", side: "new" });
    expect(mapUri(ref(), "/src/a.ts", q(BASE))).toEqual({ path: "src/a.ts", side: "old" });
    expect(mapUri(ref(), "/src/a.ts", q("c".repeat(40)))).toBeNull();
    expect(mapUri(ref(), "/src/a.ts", "root=%2Fother&rev=" + HEAD)).toBeNull();
    expect(mapUri(ref(), "/src/a.ts", "stage=0&root=%2Fw")).toBeNull();
    expect(mapUri(ref(), "/../x", q(HEAD))).toBeNull();
  });
});

describe("review: thread caps", () => {
  test("loads pages of 50 and stops at 200 with a notice", async () => {
    const f = fakes(260);
    const rc = new ReviewControllers(f.api, f.make);
    const r = rc.open(ref());
    await r.load();
    expect(f.pages).toEqual(["", "50", "100", "150"]);
    expect(MAX_THREADS_PER_CHANGE).toBe(200);
    expect(THREAD_PAGE).toBe(50);
    expect(f.ports[0]?.notices).toEqual([moreThreadsNotice(200)]);
  });

  test("a short result makes no notice and one request", async () => {
    const f = fakes(3);
    const r = new ReviewControllers(f.api, f.make).open(ref());
    await r.load();
    expect(f.pages).toEqual([""]);
    expect(f.ports[0]?.notices).toEqual([]);
  });

  test("threads show per document and dispose on close", async () => {
    const f = fakes(5, (i) => (i < 3 ? { line: 3 } : i === 3 ? { side: "old" } : {}));
    const r = new ReviewControllers(f.api, f.make).open(ref());
    await r.show("/src/a.ts", q(HEAD));
    expect(f.ports[0]?.live).toBe(4); // the old-side thread is not here
    await r.show("/src/a.ts", q(HEAD)); // idempotent
    expect(f.ports[0]?.live).toBe(4);
    await r.show("/src/a.ts", q(BASE)); // old side: one thread
    expect(f.ports[0]?.live).toBe(5);
    r.close("/src/a.ts", q(HEAD));
    expect(f.ports[0]?.live).toBe(1);
  });

  test("a thread on the whole change has no line and is not drawn", async () => {
    const f = fakes(2, (i) => (i === 0 ? { line: 0 } : {}));
    const r = new ReviewControllers(f.api, f.make).open(ref());
    await r.show("/src/a.ts", q(HEAD));
    expect(f.ports[0]?.metas).toEqual([2]);
  });

  test("at most 20 controllers, least recently used goes first", () => {
    const f = fakes(0);
    const rc = new ReviewControllers(f.api, f.make);
    for (let i = 1; i <= MAX_CONTROLLERS; i++) rc.open(ref(i));
    rc.open(ref(1)); // touch 1
    rc.open(ref(99));
    expect(rc.size()).toBe(MAX_CONTROLLERS);
    expect(f.ports[1]?.disposed).toBe(true); // change 2 was the oldest
    expect(f.ports[0]?.disposed).toBe(false);
  });
});

describe("review: comments", () => {
  test("validates size and target, posts at once, then redraws from the server", async () => {
    const f = fakes(1);
    const r = new ReviewControllers(f.api, f.make).open(ref());
    await r.show("/f", q(HEAD));
    expect(validateCommentText("  ")).toMatch(/empty/);
    expect(validateCommentText("x".repeat(MAX_COMMENT_BYTES + 1))).toMatch(/64 KiB/);
    expect(validateCommentText("x".repeat(MAX_COMMENT_BYTES))).toBeNull();
    expect(await r.add("/f", q("c".repeat(40)), 0, "hi")).not.toBeNull();
    expect(await r.add("/f", q(HEAD), 0, " ")).toMatch(/empty/);
    expect(f.added).toEqual([]);
    expect(await r.add("/f", q(HEAD), 4, "hi")).toBeNull();
    expect(await r.add("/f", q(BASE), 0, "old", 7)).toBeNull();
    expect(f.added).toEqual([
      { path: "f", side: "new", line: 5, body: "hi" },
      { threadId: 7, path: "f", side: "old", line: 1, body: "old" },
    ]);
    expect(f.pages.length).toBe(3); // the first load and one re-read per posted comment
  });

  test("resolving re-reads the threads", async () => {
    const f = fakes(2);
    const r = new ReviewControllers(f.api, f.make).open(ref());
    await r.show("/src/a.ts", q(HEAD));
    await r.resolve(2, true);
    expect(f.resolved).toEqual([[2, true]]);
    expect(f.pages.length).toBe(2);
    expect(f.ports[0]?.live).toBe(2);
  });
});

describe("review: bridge adapter", () => {
  const change = new Uint8Array(16).fill(3);
  const c: ChangeRef = { ...ref(), change: changeIdToString(change) };

  function fake() {
    const seen: string[] = [];
    const node: BridgeNode = {
      state: "up",
      pid: undefined,
      stderrTail: () => [],
      close() {},
      stream: async function* () {},
      unary: async (o) => {
        if (o.method === "asc.v1.Review/ListThreads") {
          return toBinary(
            ListThreadsResponseSchema,
            create(ListThreadsResponseSchema, {
              nextPageToken: "n",
              threads: [
                {
                  id: 4n,
                  status: ThreadStatus.RESOLVED,
                  anchor: {
                    path: new TextEncoder().encode("src/a.ts"),
                    version: 2,
                    side: DiffSide.OLD,
                    startLine: 7,
                  },
                  comments: [{ id: 1n, author: "ada", body: "hi", created: 86400n }],
                },
              ],
            }),
          );
        }
        if (o.method === "asc.v1.Review/AddComment") {
          const r = fromBinary(AddCommentRequestSchema, o.request);
          seen.push(
            `add:${r.repo?.name}:${r.threadId}:${r.anchor?.side}:${r.anchor?.startLine}:${r.body}`,
          );
          return toBinary(AddCommentResponseSchema, create(AddCommentResponseSchema, {}));
        }
        const r = fromBinary(ResolveThreadRequestSchema, o.request);
        seen.push(`resolve:${r.threadId}:${r.resolved}`);
        return toBinary(ResolveThreadResponseSchema, create(ResolveThreadResponseSchema, {}));
      },
    };
    return { seen, mgr: { node: () => node } as unknown as BridgeManager };
  }

  test("threads map status, side, line and the comment time", async () => {
    const api = bridgeReviewApi(fake().mgr);
    expect(await api.listThreads(c, "", 50)).toEqual({
      next: "n",
      items: [
        {
          id: 4,
          path: "src/a.ts",
          version: 2,
          side: "old",
          line: 7,
          resolved: true,
          comments: [{ id: 1, author: "ada", body: "hi", createdAt: "1970-01-02T00:00:00.000Z" }],
        },
      ],
    });
  });

  test("a comment and a resolve carry the repository, thread and side", async () => {
    const f = fake();
    const api = bridgeReviewApi(f.mgr);
    await api.addComment(c, { path: "a", side: "old", line: 3, body: "b" });
    await api.addComment(c, { threadId: 9, path: "a", side: "new", line: 3, body: "c" });
    await api.resolveThread(c, 9, false);
    expect(f.seen).toEqual([
      `add:o/r:0:${DiffSide.OLD}:3:b`,
      `add:o/r:9:${DiffSide.NEW}:3:c`,
      "resolve:9:false",
    ]);
  });
});

describe("review: stack checkout", () => {
  const mk = (exit: number, stderr: string[] = [], mutate = true) => {
    const runs: { verb: string; args: string[] }[] = [];
    const out: string[] = [];
    const sc = new StackCheckout(
      {
        async run(_r, verb, args) {
          runs.push({ verb, args });
          return { exit, stderr, opResult: null };
        },
        canMutate: async () => mutate,
      },
      {
        info: (m) => void out.push(`i:${m}`),
        error: (m, l) => void out.push(`e:${m}|${l.join(",")}`),
      },
    );
    return { sc, runs, out };
  };

  test("argument validation blocks flag injection", () => {
    expect(targetArg({ kind: "pr", number: 7 })).toBe("7");
    expect(targetArg({ kind: "pr", number: 0 })).toBeNull();
    expect(targetArg({ kind: "branch", name: "--worktree" })).toBeNull();
    expect(targetArg({ kind: "branch", name: "feat/x" })).toBe("feat/x");
    expect(targetArg({ kind: "branch", name: "a\nb" })).toBeNull();
    expect(targetArg({ kind: "change", id: "kkkkkkkk" })).toBe("kkkkkkkk");
    expect(targetArg({ kind: "change", id: "-x" })).toBeNull();
    expect(checkoutArgs({ kind: "pr", number: 3 }, { worktree: "/tmp/w" })).toEqual([
      "--worktree",
      "/tmp/w",
      "3",
    ]);
    expect(checkoutArgs({ kind: "pr", number: 3 }, { worktree: "-rf" })).toBeNull();
  });

  test("runs stack get and reports success", async () => {
    const { sc, runs, out } = mk(0);
    expect(await sc.run("/w", { kind: "pr", number: 3 })).toBe(true);
    expect(runs).toEqual([{ verb: "stack get", args: ["3"] }]);
    expect(out).toEqual(["i:Stack checked out"]);
  });

  test("exit 128 on a plain remote is explained; invalid input never runs", async () => {
    const a = mk(128, ["asc: stack get needs an asc server (not available on a plain git remote)"]);
    expect(await a.sc.run("/w", { kind: "pr", number: 3 })).toBe(false);
    expect(a.out[0]).toContain("needs an asc server");
    const b = mk(0);
    expect(await b.sc.run("/w", { kind: "branch", name: "-x" })).toBe(false);
    expect(b.runs).toEqual([]);
    const c = mk(0, [], false);
    expect(await c.sc.run("/w", { kind: "pr", number: 1 })).toBe(false);
    expect(c.runs).toEqual([]);
  });
});
