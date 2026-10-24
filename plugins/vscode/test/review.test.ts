import { describe, expect, test } from "bun:test";
import { checkoutArgs, StackCheckout, targetArg } from "../src/review/checkout";
import {
  type CommentPage,
  type ControllerPort,
  type ForgeComment,
  type ForgeReviewApi,
  mapUri,
  MAX_COMMENT_BYTES,
  MAX_PR_CONTROLLERS,
  MAX_THREADS_PER_PR,
  moreThreadsNotice,
  type PrRef,
  ReviewControllers,
  THREAD_PAGE,
  validateCommentText,
} from "../src/review/comments";

const BASE = "a".repeat(40);
const HEAD = "b".repeat(40);
const pr = (n = 1): PrRef => ({
  root: "/w",
  owner: "o",
  repo: "r",
  number: n,
  mergeBase: BASE,
  head: HEAD,
});
const q = (rev: string) => `root=%2Fw&rev=${rev}`;

const comment = (id: number, over: Partial<ForgeComment> = {}): ForgeComment => ({
  id,
  path: "src/a.ts",
  line: id,
  side: "new",
  body: `c${id}`,
  author: "ada",
  createdAt: "2026-01-01T00:00:00Z",
  resolved: false,
  ...over,
});

function fakes(total: number, over: (i: number) => Partial<ForgeComment> = () => ({})) {
  const pages: number[] = [];
  const saved: unknown[] = [];
  const api: ForgeReviewApi = {
    async listComments(_p, page, limit): Promise<CommentPage> {
      pages.push(page);
      const from = (page - 1) * limit;
      const items = [];
      for (let i = from; i < Math.min(total, from + limit); i++) items.push(comment(i + 1, over(i)));
      return { items, total };
    },
    async savePending(_p, c) {
      saved.push(c);
    },
  };
  const ports: { notices: string[]; live: number; disposed: boolean }[] = [];
  const make = (): ControllerPort => {
    const st = { notices: [] as string[], live: 0, disposed: false };
    ports.push(st);
    return {
      createThread() {
        st.live++;
        return { dispose: () => void st.live-- };
      },
      notice: (m) => void st.notices.push(m),
      dispose: () => void (st.disposed = true),
    };
  };
  return { api, pages, saved, ports, make };
}

describe("review: uri mapping", () => {
  test("head is new, merge base is old, others unmapped", () => {
    expect(mapUri(pr(), "/src/a.ts", q(HEAD))).toEqual({ path: "src/a.ts", side: "new" });
    expect(mapUri(pr(), "/src/a.ts", q(BASE))).toEqual({ path: "src/a.ts", side: "old" });
    expect(mapUri(pr(), "/src/a.ts", q("c".repeat(40)))).toBeNull();
    expect(mapUri(pr(), "/src/a.ts", "root=%2Fother&rev=" + HEAD)).toBeNull();
    expect(mapUri(pr(), "/src/a.ts", "stage=0&root=%2Fw")).toBeNull();
    expect(mapUri(pr(), "/../x", q(HEAD))).toBeNull();
  });
});

describe("review: thread caps", () => {
  test("loads pages of 50 and stops at 200 with a notice", async () => {
    const f = fakes(260);
    const rc = new ReviewControllers(f.api, f.make);
    const r = rc.open(pr());
    await r.load();
    expect(f.pages).toEqual([1, 2, 3, 4]);
    expect(MAX_THREADS_PER_PR).toBe(200);
    expect(THREAD_PAGE).toBe(50);
    expect(f.ports[0]?.notices).toEqual([moreThreadsNotice(200, 260)]);
    expect(moreThreadsNotice(199, 200)).toBe("1 more thread, open in the review panel");
  });

  test("a short result makes no notice and one request", async () => {
    const f = fakes(3);
    const r = new ReviewControllers(f.api, f.make).open(pr());
    await r.load();
    expect(f.pages).toEqual([1]);
    expect(f.ports[0]?.notices).toEqual([]);
  });

  test("threads show per document and dispose on close", async () => {
    const f = fakes(5, (i) => (i < 3 ? { line: 3 } : {})); // two comments share a line
    const r = new ReviewControllers(f.api, f.make).open(pr());
    await r.show("/src/a.ts", q(HEAD));
    expect(f.ports[0]?.live).toBe(3);
    await r.show("/src/a.ts", q(HEAD)); // idempotent
    expect(f.ports[0]?.live).toBe(3);
    await r.show("/src/a.ts", q(BASE)); // old side: nothing there
    expect(f.ports[0]?.live).toBe(3);
    r.close("/src/a.ts", q(HEAD));
    expect(f.ports[0]?.live).toBe(0);
  });

  test("at most 20 controllers, least recently used goes first", () => {
    const f = fakes(0);
    const rc = new ReviewControllers(f.api, f.make);
    for (let i = 1; i <= MAX_PR_CONTROLLERS; i++) rc.open(pr(i));
    rc.open(pr(1)); // touch 1
    rc.open(pr(99));
    expect(rc.size()).toBe(MAX_PR_CONTROLLERS);
    expect(f.ports[1]?.disposed).toBe(true); // PR 2 was the oldest
    expect(f.ports[0]?.disposed).toBe(false);
  });
});

describe("review: new comments", () => {
  test("validates size and target, then saves one pending review", async () => {
    const f = fakes(0);
    const r = new ReviewControllers(f.api, f.make).open(pr());
    expect(validateCommentText("  ")).toMatch(/empty/);
    expect(validateCommentText("x".repeat(MAX_COMMENT_BYTES + 1))).toMatch(/64 KiB/);
    expect(validateCommentText("x".repeat(MAX_COMMENT_BYTES))).toBeNull();
    expect(r.add("/f", q("c".repeat(40)), 0, "hi")).not.toBeNull();
    expect(r.add("/f", q(HEAD), 4, "hi")).toBeNull();
    expect(r.add("/f", q(BASE), 0, "old")).toBeNull();
    expect(r.pendingCount()).toBe(2);
    expect(await r.savePending()).toBe(2);
    expect(r.pendingCount()).toBe(0);
    expect(f.saved).toEqual([
      [
        { path: "f", side: "new", line: 5, body: "hi" },
        { path: "f", side: "old", line: 1, body: "old" },
      ],
    ]);
    expect(await r.savePending()).toBe(0);
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
      { info: (m) => void out.push(`i:${m}`), error: (m, l) => void out.push(`e:${m}|${l.join(",")}`) },
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
