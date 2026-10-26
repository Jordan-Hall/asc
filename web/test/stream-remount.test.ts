import { describe, expect, test } from "bun:test";
import { demoTransport } from "../src/api/fixtures";
import { createQueries } from "../src/api/queries";
import type { FileView } from "../src/api/types";
import { QueryCache } from "../src/lib/query";

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

describe("a streaming read that outlives its first subscriber", () => {
  test("a remounted page shows the first screenful before the stream ends", async () => {
    const t = demoTransport(5);
    const cache = new QueryCache();
    const q = createQueries({ transport: t, cache }).file("asc/demo", "main", "data/big.txt");

    // First mount subscribes, then goes away (a discarded render, a quick navigation): the
    // mutable query is aborted and dropped while the immutable read keeps streaming.
    cache.ensure(q.key, q.run, q.opts);
    const off = cache.subscribe(q.key, () => undefined);
    await wait(60);
    off();

    // Second mount starts a new run for the same key.
    cache.ensure(q.key, q.run, q.opts);
    const unsub = cache.subscribe(q.key, () => undefined);
    await wait(120);
    const entry = cache.peek<FileView>(q.key);
    unsub();
    expect(entry.data?.count ?? 0).toBeGreaterThan(0);
    expect(entry.data?.done).toBe(false);
    cache.clear();
  });
});
