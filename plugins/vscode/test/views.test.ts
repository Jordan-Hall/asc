import { describe, expect, test } from "bun:test";
import { navigate, stepRevision } from "../src/revnav";
import {
  FileHistoryModel,
  type HistoryEntry,
  type HistoryPage,
  type HistoryQuery,
} from "../src/views/file-history";
import { MAX_ENTRIES, OpLogModel, PAGE, parseOpLog } from "../src/views/oplog";
import {
  describeWorktree,
  pump,
  removeConfirmText,
  type RunResult,
  type ViewApi,
  type WatchKindName,
  type WorktreeInfo,
  WorktreesModel,
} from "../src/views/worktrees";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const ok = (entries: unknown): RunResult => ({
  exit: 0,
  opResult: new TextEncoder().encode(JSON.stringify({ entries })),
  stderr: [],
});
const ops = (n: number) =>
  Array.from({ length: n }, (_, i) => ({ id: `op${i}`, description: "d" }));

const wt = (o: Partial<WorktreeInfo> = {}): WorktreeInfo => ({
  wsId: "w",
  path: "C:/work/feat-a",
  mode: "full",
  headRef: "feat-a",
  dirty: 0,
  staged: 0,
  ahead: 0,
  behind: 0,
  ephemeral: false,
  locked: "",
  inProgress: "",
  ...o,
});

describe("views: worktrees", () => {
  test("fetches only while visible and refreshes stale on show", async () => {
    let calls = 0;
    const api = {
      worktrees: async () => (calls++, [wt()]),
    } as unknown as ViewApi;
    const m = new WorktreesModel({ root: "r", api });
    m.onWatch();
    await sleep(5);
    expect(calls).toBe(0);
    expect(m.isStale).toBe(true);
    m.setVisible(true);
    await sleep(5);
    expect(calls).toBe(1);
    expect(m.items).toHaveLength(1);
    expect(m.isStale).toBe(false);
    m.onWatch();
    await sleep(5);
    expect(calls).toBe(2);
  });
  test("a late reply from an older refresh is dropped", async () => {
    const waits = [30, 1];
    let i = 0;
    const api = {
      worktrees: async () => {
        const n = i++;
        await sleep(waits[n] ?? 1);
        return [wt({ path: `/p${n}` })];
      },
    } as unknown as ViewApi;
    const m = new WorktreesModel({ root: "r", api });
    m.setVisible(true);
    m.onWatch();
    await sleep(60);
    expect(m.items[0]?.path).toBe("/p1");
  });
  test("add and remove send fixed argv with end-of-options", async () => {
    const seen: [string, string[]][] = [];
    const api = {
      worktrees: async () => [],
      run: async (_r: string, verb: string, args: string[]) => (seen.push([verb, args]), ok([])),
    } as unknown as ViewApi;
    const m = new WorktreesModel({ root: "r", api });
    await m.add("/x/--evil");
    await m.remove(wt({ path: "/y" }));
    expect(seen).toEqual([
      ["worktree add", ["--ephemeral", "--end-of-options", "/x/--evil"]],
      ["worktree remove", ["--end-of-options", "/y"]],
    ]);
  });
  test("labels and the removal prompt come from parsed fields", () => {
    const d = describeWorktree(wt({ dirty: 2, ahead: 1, locked: "busy", ephemeral: true }));
    expect(d.label).toBe("feat-a");
    expect(d.description).toContain("2 changed");
    expect(d.description).toContain("locked");
    expect(removeConfirmText(wt({ dirty: 1 }))).toContain("uncommitted");
  });
  test("pump reconnects after an error and stops on abort", async () => {
    let n = 0;
    const api = {
      async *watch(): AsyncGenerator<WatchKindName> {
        n++;
        if (n === 1) throw new Error("boom");
        yield "paths";
        await sleep(20);
      },
    };
    const ac = new AbortController();
    const got: string[] = [];
    const errs: unknown[] = [];
    const p = pump(
      api,
      "r",
      ac.signal,
      (k) => got.push(k),
      (e) => errs.push(e),
      [1],
    );
    await sleep(15);
    ac.abort();
    await p;
    expect(errs).toHaveLength(1);
    expect(got[0]).toBe("paths");
  });
});

describe("views: op log", () => {
  test("parses entries and ignores junk", () => {
    expect(parseOpLog(null)).toEqual([]);
    expect(parseOpLog(new TextEncoder().encode("not json"))).toEqual([]);
    const e = parseOpLog(
      new TextEncoder().encode(JSON.stringify({ entries: [{ id: "a" }, {}, 3] })),
    );
    expect(e).toEqual([{ id: "a", description: "", time: 0, user: "" }]);
  });
  test("debounces a burst of op-finished into one run, and only when visible", async () => {
    const runs: string[][] = [];
    const api = {
      run: async (_r: string, _v: string, a: string[]) => (runs.push(a), ok(ops(3))),
    };
    const m = new OpLogModel({ root: "r", api, debounceMs: 20, minIntervalMs: 0 });
    m.onWatch("op-finished");
    await sleep(40);
    expect(runs).toHaveLength(0);
    m.setVisible(true);
    await sleep(10);
    expect(runs).toEqual([["-n", "50"]]);
    for (let i = 0; i < 5; i++) {
      m.onWatch("op-finished");
      await sleep(5);
    }
    m.onWatch("paths");
    await sleep(60);
    expect(runs).toHaveLength(2);
    expect(m.entries).toHaveLength(3);
  });
  test("one child at a time and one per interval", async () => {
    let live = 0;
    let max = 0;
    const starts: number[] = [];
    const api = {
      run: async () => {
        starts.push(Date.now());
        max = Math.max(max, ++live);
        await sleep(15);
        live--;
        return ok(ops(1));
      },
    };
    const m = new OpLogModel({ root: "r", api, debounceMs: 1, minIntervalMs: 60 });
    m.setVisible(true);
    await sleep(5);
    m.onWatch("op-finished");
    await sleep(5);
    m.onWatch("op-finished");
    await sleep(150);
    expect(max).toBe(1);
    expect(starts.length).toBe(2);
    expect((starts[1] ?? 0) - (starts[0] ?? 0)).toBeGreaterThanOrEqual(55);
  });
  test("load more grows by 50 up to 500 then points at the terminal", async () => {
    const asked: number[] = [];
    const api = {
      run: async (_r: string, _v: string, a: string[]) => {
        const n = Number(a[1]);
        asked.push(n);
        return ok(ops(n));
      },
    };
    const m = new OpLogModel({ root: "r", api, debounceMs: 0, minIntervalMs: 0 });
    m.setVisible(true);
    await sleep(15);
    expect(m.canLoadMore).toBe(true);
    for (let i = 0; i < 12; i++) {
      m.loadMore();
      await sleep(15);
    }
    expect(Math.max(...asked)).toBe(MAX_ENTRIES);
    expect(asked[0]).toBe(PAGE);
    expect(m.canLoadMore).toBe(false);
    expect(m.atCap).toBe(true);
  });
  test("a failed run keeps the old entries and reports", async () => {
    let n = 0;
    const errs: unknown[] = [];
    const api = {
      run: async () => (n++ === 0 ? ok(ops(2)) : { exit: 1, opResult: null, stderr: ["x"] }),
    };
    const m = new OpLogModel({
      root: "r",
      api,
      debounceMs: 1,
      minIntervalMs: 0,
      onError: (e) => errs.push(e),
    });
    m.setVisible(true);
    await sleep(10);
    m.onWatch("op-finished");
    await sleep(20);
    expect(m.entries).toHaveLength(2);
    expect(errs.length).toBeGreaterThan(0);
  });
});

const entry = (id: string, parent: string | null = null): HistoryEntry => ({
  id,
  parent,
  subject: id,
  author: "a",
  time: 1,
  added: 0,
  removed: 0,
});

const fake = (pages: HistoryPage[], seen: HistoryQuery[] = []) => ({
  history: async (_r: string, q: HistoryQuery) => {
    seen.push(q);
    return pages[seen.length - 1] ?? { entries: [], next: "" };
  },
});
describe("views: file history", () => {
  test("loads for the active file only when visible and pages with a token", async () => {
    const seen: HistoryQuery[] = [];
    const m = new FileHistoryModel({
      api: fake(
        [
          { entries: [entry("a"), entry("b")], next: "t1" },
          { entries: [entry("c")], next: "" },
        ],
        seen,
      ),
    });
    m.setTarget({ root: "r", path: "src/a.ts" });
    await sleep(5);
    expect(seen).toHaveLength(0);
    m.setVisible(true);
    await sleep(5);
    expect(m.entries.map((e) => e.id)).toEqual(["a", "b"]);
    expect(m.hasMore).toBe(true);
    m.loadMore();
    await sleep(5);
    expect(m.entries.map((e) => e.id)).toEqual(["a", "b", "c"]);
    expect(seen[1]?.pageToken).toBe("t1");
    expect(seen[0]?.limit).toBe(50);
    expect(m.hasMore).toBe(false);
  });
  test("line mode and head events reload; a null target clears", async () => {
    const seen: HistoryQuery[] = [];
    const m = new FileHistoryModel({ api: fake([{ entries: [entry("a")], next: "" }], seen) });
    m.setVisible(true);
    m.setTarget({ root: "r", path: "p" });
    await sleep(5);
    m.setLines({ start: 3, end: 9 });
    await sleep(5);
    expect(m.mode).toBe("line");
    expect(seen[1]?.lines).toEqual({ start: 3, end: 9 });
    m.onWatch("paths");
    await sleep(5);
    expect(seen).toHaveLength(2);
    m.onWatch("head");
    await sleep(5);
    expect(seen).toHaveLength(3);
    m.setTarget(null);
    await sleep(5);
    expect(m.entries).toEqual([]);
    expect(m.mode).toBe("file");
  });
});

describe("views: revision navigation", () => {
  const ids = ["c3", "c2", "c1"];
  test("previous goes older, next goes newer and returns to the working file", () => {
    expect(stepRevision(ids, null, "previous")).toEqual({ kind: "rev", rev: "c3" });
    expect(stepRevision(ids, null, "next")).toBeNull();
    expect(stepRevision(ids, "c3", "previous")).toEqual({ kind: "rev", rev: "c2" });
    expect(stepRevision(ids, "c2", "next")).toEqual({ kind: "rev", rev: "c3" });
    expect(stepRevision(ids, "c3", "next")).toEqual({ kind: "working" });
    expect(stepRevision(ids, "c1", "previous")).toBeNull();
    expect(stepRevision(ids, "zz", "previous")).toBeNull();
    expect(stepRevision([], null, "previous")).toBeNull();
  });
  test("navigate reads the follow history of the path", async () => {
    const seen: HistoryQuery[] = [];
    const api = {
      history: async (_r: string, q: HistoryQuery) => (
        seen.push(q), { entries: ids.map((i) => entry(i)), next: "" }
      ),
    };
    const t = await navigate(api, { root: "r", path: "a.ts", rev: "c2" }, "previous");
    expect(t).toEqual({ kind: "rev", rev: "c1" });
    expect(seen[0]?.path).toBe("a.ts");
  });
});
