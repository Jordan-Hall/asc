import { describe, expect, test } from "bun:test";
import {
  hitDescription,
  hitLabel,
  MAX_HITS,
  runSearch,
  type SearchApi,
  type SearchHit,
  SearchSession,
} from "../src/search";

const hit = (i: number): SearchHit => ({ root: "/r", path: `f${i}.ts`, line: i + 1, col: 0, text: `  line ${i} ` });

function fake(over: Partial<SearchApi> = {}): SearchApi & { aborted: number; grepCalls: number } {
  const f = {
    aborted: 0,
    grepCalls: 0,
    hasSearchV1: async () => true,
    async *stream(_r: string, _p: string, signal: AbortSignal) {
      signal.addEventListener("abort", () => f.aborted++, { once: true });
      for (let i = 0; i < 1000; i += 50) {
        yield Array.from({ length: 50 }, (_, k) => hit(i + k));
        await Promise.resolve();
      }
    },
    async grep() {
      f.grepCalls++;
      return [hit(1), hit(2)];
    },
    ...over,
  };
  return f;
}

describe("search", () => {
  test("stops at 200 hits and aborts the stream", async () => {
    const api = fake();
    const r = await runSearch(api, ["/r"], "x", new AbortController().signal, { onHits() {} });
    expect(r.hits.length).toBe(MAX_HITS);
    expect(r.capped).toBe(true);
    expect(api.aborted).toBe(1);
  });

  test("streams batches to onHits", async () => {
    const seen: number[] = [];
    await runSearch(fake(), ["/r"], "x", new AbortController().signal, {
      onHits: (h) => seen.push(h.length),
    });
    expect(seen).toEqual([50, 100, 150, 200]);
  });

  test("empty query does nothing", async () => {
    const api = fake();
    const r = await runSearch(api, ["/r"], "  ", new AbortController().signal, { onHits() {} });
    expect(r.hits).toEqual([]);
    expect(api.aborted).toBe(0);
  });

  test("falls back to Repo.Grep without search.v1", async () => {
    const api = fake({ hasSearchV1: async () => false });
    const r = await runSearch(api, ["/r"], "x", new AbortController().signal, { onHits() {} });
    expect(r.fallback).toBe(true);
    expect(r.hits.length).toBe(2);
    expect(api.grepCalls).toBe(1);
  });

  test("a new keystroke cancels the old run and the stale run never reports", async () => {
    let release!: () => void;
    const gate = new Promise<void>((res) => {
      release = res;
    });
    const api = fake({
      async *stream(_r, p, signal) {
        await gate;
        if (signal.aborted) return;
        yield [hit(p === "old" ? 1 : 2)];
      },
    });
    const s = new SearchSession(api, ["/r"]);
    const got: string[] = [];
    const cb = (tag: string) => ({
      onHits: (h: readonly SearchHit[]) => got.push(`${tag}:${h.length}`),
      onDone: () => got.push(`${tag}:done`),
      onError: () => got.push(`${tag}:err`),
    });
    const a = s.start("old", cb("old"));
    const b = s.start("new", cb("new"));
    release();
    await Promise.all([a, b]);
    expect(got).toEqual(["new:1", "new:done"]);
  });

  test("a real error is reported", async () => {
    const api = fake({
      stream: () => ({
        [Symbol.asyncIterator]: () => ({ next: () => Promise.reject(new Error("boom")) }),
      }),
    });
    const s = new SearchSession(api, ["/r"]);
    const errs: unknown[] = [];
    await s.start("x", { onHits() {}, onDone() {}, onError: (e) => errs.push(e) });
    expect(errs.length).toBe(1);
  });

  test("labels use plain trimmed text", () => {
    expect(hitLabel(hit(3))).toBe("line 3");
    expect(hitDescription(hit(3))).toBe("f3.ts:4");
  });
});
