import { describe, expect, test } from "bun:test";
import {
  ageBuckets,
  autolinkMd,
  type BlameApi,
  type BlameCommit,
  type BlameData,
  BlameService,
  debounce,
  escapeMd,
  hoverFor,
  hunkAt,
  inlineText,
  LRU_ENTRIES,
  lensesEnabled,
  lensTitle,
  Lru,
  relativeTime,
  skipReason,
} from "../src/blame/model";

const A = "a".repeat(40);
const B = "b".repeat(40);
const commit = (id: string, over: Partial<BlameCommit> = {}): BlameCommit => ({
  id,
  subject: "Fix #12",
  message: "Fix #12\n\nbody text",
  author: "Ada",
  time: 1000,
  changeId: "k".repeat(32),
  gen: 2,
  ...over,
});
const data = (): BlameData => ({
  hunks: [
    { origin: A, origPath: "f", origStart: 0, start: 0, len: 3 },
    { origin: B, origPath: "f", origStart: 0, start: 3, len: 2 },
    { origin: "", origPath: "f", origStart: 0, start: 5, len: 1 },
  ],
  commits: new Map([
    [A, commit(A, { time: 100 })],
    [B, commit(B, { time: 200_000, author: "Bo" })],
  ]),
  reviews: [{ forge: "forgejo", number: 7, url: "https://forge.example/pr/7", state: "open" }],
});

describe("blame: lookup and text", () => {
  test("hunkAt finds hunks and misses outside", () => {
    const d = data();
    expect(hunkAt(d.hunks, 0)?.origin).toBe(A);
    expect(hunkAt(d.hunks, 2)?.origin).toBe(A);
    expect(hunkAt(d.hunks, 3)?.origin).toBe(B);
    expect(hunkAt(d.hunks, 6)).toBeUndefined();
  });
  test("inline text and uncommitted lines", () => {
    const d = data();
    expect(inlineText(d, 3, 200_000 + 86_400 * 2)).toBe("Bo, 2 days ago • Fix #12");
    expect(inlineText(d, 5, 0)).toBe("You, not committed yet");
    expect(inlineText(d, 9, 0)).toBeNull();
  });
  test("relative time", () => {
    expect(relativeTime(100, 100)).toBe("just now");
    expect(relativeTime(0, 3600)).toBe("1 hour ago");
  });
  test("heatmap buckets by age quantile", () => {
    const d = data();
    const b = ageBuckets(d, 5);
    expect(b.get(d.hunks[0] as never)).toBe(0);
    expect(b.get(d.hunks[1] as never)).toBe(2);
    expect(b.get(d.hunks[2] as never)).toBe(4);
  });
  test("lens title uses the newest commit in range", () => {
    expect(lensTitle(data(), 0, 4, 200_000)).toBe("Bo +1, just now");
    expect(lensTitle(data(), 5, 5, 0)).toBeNull();
  });
  test("lenses are off above 5,000 lines", () => {
    expect(lensesEnabled(5000)).toBe(true);
    expect(lensesEnabled(5001)).toBe(false);
  });
});

describe("blame: hover", () => {
  test("shows commit, change, PR and actions", () => {
    const h = hoverFor(data(), 3, [{ prefix: "#", url: "https://i.example/<num>" }], 200_000, {
      root: "/r",
      path: "f",
    });
    expect(h?.markdown).toContain(String.raw`[\#12](https://i.example/12)`);
    expect(h?.markdown).toContain("PR #7");
    expect(h?.actions.map((a) => a.label)).toEqual([
      "Open changes",
      "Blame previous revision",
      "Open PR",
      "Copy change id",
    ]);
  });
  test("escapes markdown and refuses non-https autolinks and PR urls", () => {
    expect(escapeMd("[x](javascript:1) `c`")).not.toContain("](");
    expect(autolinkMd("see #3", [{ prefix: "#", url: "http://i.example/<num>" }])).toBe("see \\#3");
    const d = data();
    d.reviews = [{ forge: "f", number: 1, url: "javascript:alert(1)", state: "open" }];
    const h = hoverFor(d, 0, [], 0, { root: "/r", path: "f" });
    expect(h?.actions.some((a) => a.command === "asc.blame.openPr")).toBe(false);
  });
});

describe("blame: skip rules", () => {
  const ok = { scheme: "file", insideRoot: true, byteLength: 10 };
  test("skips asc, untitled, outside, big and binary", () => {
    expect(skipReason(ok)).toBeNull();
    expect(skipReason({ ...ok, scheme: "asc" })).not.toBeNull();
    expect(skipReason({ ...ok, scheme: "untitled" })).not.toBeNull();
    expect(skipReason({ ...ok, insideRoot: false })).not.toBeNull();
    expect(skipReason({ ...ok, byteLength: 2 * 1024 * 1024 + 1 })).not.toBeNull();
    expect(skipReason({ ...ok, head: new Uint8Array([65, 0]) })).not.toBeNull();
  });
});

describe("blame: cache", () => {
  const fake = () => {
    let n = 0;
    const api: BlameApi = {
      async blame() {
        n++;
        return data();
      },
      async autolinks() {
        return [];
      },
    };
    return { api, calls: () => n };
  };
  test("LRU evicts the oldest past 100", () => {
    const l = new Lru<number>(LRU_ENTRIES);
    for (let i = 0; i < LRU_ENTRIES + 1; i++) l.set(String(i), i);
    expect(l.size).toBe(LRU_ENTRIES);
    expect(l.get("0")).toBeUndefined();
    expect(l.get("1")).toBe(1);
  });
  test("one request serves repeats and concurrent loads; invalidate refetches", async () => {
    const f = fake();
    const s = new BlameService({ api: f.api });
    await Promise.all([s.load("/r", "f", 1, null), s.load("/r", "f", 1, null)]);
    await s.load("/r", "f", 1, null);
    expect(f.calls()).toBe(1);
    expect(s.peek("/r", "f", null)).toBeDefined();
    s.invalidate("/r");
    expect(s.peek("/r", "f", null)).toBeUndefined();
    await s.load("/r", "f", 1, null);
    expect(f.calls()).toBe(2);
  });
  test("document version keys dirty buffers", async () => {
    const f = fake();
    const s = new BlameService({ api: f.api });
    await s.load("/r", "f", 1, new Uint8Array([1]));
    await s.load("/r", "f", 2, new Uint8Array([2]));
    await s.load("/r", "f", 2, new Uint8Array([2]));
    expect(f.calls()).toBe(2);
  });
  test("a HEAD move during a request leaves it uncached", async () => {
    let release: () => void = () => {};
    const api: BlameApi = {
      blame: () => new Promise((res) => (release = () => res(data()))),
      autolinks: async () => [],
    };
    const s = new BlameService({ api });
    const p = s.load("/r", "f", 1, null);
    s.invalidate("/r");
    release();
    await p;
    expect(s.peek("/r", "f", null)).toBeUndefined();
  });
  test("provide-time peek never calls the api", () => {
    const f = fake();
    new BlameService({ api: f.api }).peek("/r", "f", null);
    expect(f.calls()).toBe(0);
  });
});

describe("blame: debounce", () => {
  test("collapses bursts into one trailing call", () => {
    const pending: (() => void)[] = [];
    const calls: number[] = [];
    const d = debounce(150, (n: number) => calls.push(n), {
      set: (f) => {
        pending.push(f);
        return pending.length - 1;
      },
      clear: (h) => {
        pending[h as number] = () => {};
      },
    });
    d(1);
    d(2);
    d(3);
    for (const f of pending) f();
    expect(calls).toEqual([3]);
  });
});
