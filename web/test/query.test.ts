import { describe, expect, test } from "bun:test";
import { keyOf, QueryCache } from "../src/lib/query";

const tick = () => new Promise((r) => setTimeout(r, 0));

describe("QueryCache", () => {
  test("dedupes concurrent reads of one key", async () => {
    const cache = new QueryCache();
    let calls = 0;
    const run = async () => {
      calls += 1;
      return 7;
    };
    const [a, b] = await Promise.all([cache.fetch("k", run), cache.fetch("k", run)]);
    expect([a, b, calls]).toEqual([7, 7, 1]);
  });

  test("fresh data is not refetched, stale data is, and keeps showing meanwhile", async () => {
    let now = 0;
    const cache = new QueryCache({ now: () => now });
    let n = 0;
    const run = async () => ++n;
    await cache.fetch("k", run, { staleMs: 100 });
    cache.ensure("k", run, { staleMs: 100 });
    await tick();
    expect(n).toBe(1);
    now = 500;
    const during = cache.ensure("k", run, { staleMs: 100 });
    expect(during.data).toBe(1);
    expect(during.fetching).toBe(true);
    await tick();
    expect(cache.peek<number>("k").data).toBe(2);
  });

  test("an immutable entry never goes stale", async () => {
    let now = 0;
    const cache = new QueryCache({ now: () => now });
    let n = 0;
    const run = async () => ++n;
    await cache.fetch("k", run, { staleMs: Number.POSITIVE_INFINITY });
    now = 1e12;
    cache.ensure("k", run, { staleMs: Number.POSITIVE_INFINITY });
    await tick();
    expect(n).toBe(1);
  });

  test("a stream publishes partial data, then the final value", async () => {
    const cache = new QueryCache();
    const seen: number[][] = [];
    await cache.fetch<number[]>("s", async ({ emit }) => {
      const acc: number[] = [];
      for (let i = 0; i < 3; i++) {
        acc.push(i);
        emit([...acc]);
        await tick();
      }
      return acc;
    });
    cache.subscribe("s", () => undefined);
    const d = cache.peek<number[]>("s");
    if (d.data) seen.push(d.data);
    expect(seen.at(-1)).toEqual([0, 1, 2]);
    expect(d.done).toBe(true);
  });

  test("errors are kept and rejected by fetch", async () => {
    const cache = new QueryCache();
    const run = async (): Promise<number> => {
      throw new Error("boom");
    };
    await expect(cache.fetch("e", run)).rejects.toThrow("boom");
    expect(cache.peek("e").status).toBe("error");
  });

  test("abortOnRelease aborts the run and drops an empty entry", async () => {
    const cache = new QueryCache();
    let aborted = false;
    cache.ensure(
      "a",
      ({ signal }) =>
        new Promise<number>(() => {
          signal.addEventListener("abort", () => {
            aborted = true;
          });
        }),
      { abortOnRelease: true },
    );
    const unsub = cache.subscribe("a", () => undefined);
    await tick();
    unsub();
    await tick();
    expect(aborted).toBe(true);
    expect(cache.size).toBe(0);
  });

  test("evicts the least recently used unobserved entry", async () => {
    const cache = new QueryCache({ max: 2 });
    for (const k of ["a", "b", "c"]) await cache.fetch(k, async () => k);
    expect(cache.size).toBeLessThanOrEqual(2);
    expect(cache.peek("a").status).toBe("pending");
  });

  test("invalidate refetches mounted keys", async () => {
    const cache = new QueryCache();
    let n = 0;
    const run = async () => ++n;
    await cache.fetch("p:1", run);
    cache.subscribe("p:1", () => undefined);
    cache.invalidate("p:");
    await tick();
    expect(n).toBe(2);
  });

  test("keyOf is stable", () => {
    expect(keyOf("a", { b: 1 }, ["c"])).toBe('["a",{"b":1},["c"]]');
  });
});
