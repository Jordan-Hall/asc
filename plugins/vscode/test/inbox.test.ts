import { describe, expect, test } from "bun:test";
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { type BridgeNode, changeIdToString } from "@asc/asc-client";
import {
  InboxCountRequestSchema,
  InboxCountResponseSchema,
  InboxItemSchema,
  InboxListResponseSchema,
  InboxSection,
  InboxSetStateRequestSchema,
  InboxSetStateResponseSchema,
} from "@asc/asc-client/inbox";
import type { BridgeManager } from "../src/bridge";
import {
  type CountApi,
  type CountResult,
  InboxCountPoller,
  POLL_MS,
  STALE_MS,
  type Timers,
} from "../src/inbox-count";
import {
  type InboxApi,
  type InboxEntry,
  InboxModel,
  type ListResult,
  bridgeInboxApi,
  group,
  REFRESH_MS,
} from "../src/views/inbox";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function fakeTimers() {
  const live = new Map<number, { fn: () => void; ms: number }>();
  let n = 0;
  const t: Timers = {
    setInterval: (fn, ms) => (live.set(++n, { fn, ms }), n),
    clearInterval: (h) => void live.delete(h as number),
  };
  return { t, live, tick: () => [...live.values()].forEach((x) => x.fn()) };
}

describe("inbox: count poll", () => {
  function make(over: Partial<{ signedIn: boolean; enabled: boolean }> = {}) {
    const calls: (string | undefined)[] = [];
    let result: CountResult | Error = { status: "ok", total: 3, etag: '"a"' };
    const api: CountApi = {
      needsReviewCount: async (etag) => {
        calls.push(etag);
        if (result instanceof Error) throw result;
        return result;
      },
    };
    const clock = { now: 1_000_000 };
    const ft = fakeTimers();
    const state = { signedIn: over.signedIn ?? true, enabled: over.enabled ?? true };
    const p = new InboxCountPoller({
      api,
      signedIn: () => state.signedIn,
      enabled: () => state.enabled,
      now: () => clock.now,
      timers: ft.t,
    });
    return { p, calls, ft, clock, state, set: (r: CountResult | Error) => (result = r) };
  }

  test("polls once on focus, then on a 5 minute timer, and stops on blur", async () => {
    const h = make();
    h.p.setFocused(true);
    await sleep(2);
    expect(h.calls).toHaveLength(1);
    expect([...h.ft.live.values()][0]?.ms).toBe(POLL_MS);
    h.ft.tick();
    await sleep(2);
    expect(h.calls).toHaveLength(2);
    expect(h.calls[1]).toBe('"a"');
    h.p.setFocused(false);
    expect(h.p.running).toBe(false);
    expect(h.ft.live.size).toBe(0);
  });

  test("a 304 keeps the count and refreshes the age", async () => {
    const h = make();
    h.p.setFocused(true);
    await sleep(2);
    h.clock.now += 6 * 60_000;
    h.set({ status: "not-modified" });
    await h.p.poll();
    expect(h.p.total).toBe(3);
    expect(h.p.view().dimmed).toBe(false);
    expect(h.p.view().tooltip).toContain("updated 0 min ago");
  });

  test("does not poll when signed out or when the setting is off", async () => {
    const out = make({ signedIn: false });
    out.p.setFocused(true);
    await sleep(2);
    expect(out.calls).toHaveLength(0);
    expect(out.p.running).toBe(false);
    const off = make({ enabled: false });
    off.p.setFocused(true);
    off.p.afterReviewAction();
    await sleep(2);
    expect(off.calls).toHaveLength(0);
  });

  test("a setting change stops the timer", async () => {
    const h = make();
    h.p.setFocused(true);
    await sleep(2);
    h.state.enabled = false;
    h.p.reevaluate();
    expect(h.p.running).toBe(false);
  });

  test("a review action polls at once only while focused", async () => {
    const h = make();
    h.p.afterReviewAction();
    await sleep(2);
    expect(h.calls).toHaveLength(0);
    h.p.setFocused(true);
    await sleep(2);
    h.p.afterReviewAction();
    await sleep(2);
    expect(h.calls).toHaveLength(2);
  });

  test("dims when older than 15 minutes or offline", async () => {
    const h = make();
    h.p.setFocused(true);
    await sleep(2);
    expect(h.p.view().dimmed).toBe(false);
    expect(h.p.view().text).toContain("3");
    h.clock.now += STALE_MS + 60_000;
    const v = h.p.view();
    expect(v.dimmed).toBe(true);
    expect(v.tooltip).toContain("updated 16 min ago");
    h.clock.now = 1_000_000;
    h.set(new Error("offline"));
    await h.p.poll();
    expect(h.p.view().dimmed).toBe(true);
    expect(h.p.view().tooltip).toContain("server offline");
    expect(h.p.total).toBe(3);
  });
});

const entry = (id: string, section: string, over: Partial<InboxEntry> = {}): InboxEntry => ({
  root: "/w",
  id,
  section,
  author: "ada",
  updated: 100,
  ...over,
});
const cid = (n: number) => String.fromCharCode(122 - n).repeat(32);

describe("inbox: tree", () => {
  test("groups by section, pinned first then newest, hides snoozed, drops empty and unknown sections", () => {
    const now = Date.parse("2026-10-24T12:00:00Z");
    const sec = (iso: string) => Date.parse(iso) / 1000;
    const g = group(
      [
        entry(cid(3), "needs_review", { updated: 30 }),
        entry(cid(1), "needs_review", { pinned: true, updated: 10 }),
        entry(cid(2), "needs_review", { snoozedUntil: sec("2026-10-25T00:00:00Z") }),
        entry(cid(4), "needs_review", { snoozedUntil: sec("2026-10-01T00:00:00Z"), updated: 20 }),
        entry(cid(5), "checks_failed"),
        entry(cid(6), "bogus"),
      ],
      now,
    );
    expect(g.map((x) => x.id)).toEqual(["needs_review", "checks_failed"]);
    expect(g[0]?.items.map((i) => i.id)).toEqual([cid(1), cid(3), cid(4)]);
    expect(g[0]?.items[0]?.label).toBe(`${cid(1).slice(0, 8)} by ada`);
  });

  test("the row names the requester, open threads and version", () => {
    const g = group(
      [
        entry(cid(1), "needs_review", { requestedBy: "bob", latestVersion: 2 }),
        entry(cid(2), "returned", { openThreads: 1 }),
      ],
      0,
    );
    expect(g[0]?.items[0]?.description).toBe("from bob  v2");
    expect(g[1]?.items[0]?.description).toBe("1 open thread");
  });

  function model(signedIn = true) {
    const ft = fakeTimers();
    const etags: (string | undefined)[] = [];
    const updates: unknown[] = [];
    let next: ListResult = {
      status: "ok",
      entries: [entry(cid(1), "needs_review")],
      etag: "e1",
    };
    const api = {
      list: async (etag) => (etags.push(etag), next),
      update: async (ref, body) => void updates.push([ref, body]),
      needsReviewCount: async () => ({ status: "not-modified" }) as CountResult,
    } satisfies InboxApi;
    const m = new InboxModel({
      api,
      signedIn: () => signedIn,
      setInterval: ft.t.setInterval,
      clearInterval: ft.t.clearInterval,
      now: () => 0,
    });
    return { m, ft, etags, updates, set: (r: ListResult) => (next = r) };
  }

  test("fetches only while visible, then every 60 s with the last etag", async () => {
    const h = model();
    h.m.setFocused(true);
    await sleep(2);
    expect(h.etags).toHaveLength(0);
    h.m.setVisible(true);
    await sleep(2);
    expect(h.etags).toEqual([undefined]);
    expect([...h.ft.live.values()][0]?.ms).toBe(REFRESH_MS);
    h.set({ status: "not-modified" });
    h.ft.tick();
    await sleep(2);
    expect(h.etags[1]).toBe("e1");
    expect(h.m.groups[0]?.items).toHaveLength(1);
    h.m.setVisible(false);
    expect(h.ft.live.size).toBe(0);
    expect(h.m.isStale).toBe(true);
  });

  test("stops on blur and does nothing when signed out", async () => {
    const h = model();
    h.m.setVisible(true);
    await sleep(2);
    h.m.setFocused(false);
    expect(h.ft.live.size).toBe(0);
    const out = model(false);
    out.m.setVisible(true);
    await sleep(2);
    expect(out.etags).toHaveLength(0);
    expect(out.ft.live.size).toBe(0);
  });

  test("pin and snooze go through the update call and refetch without the etag", async () => {
    const h = model();
    h.m.setVisible(true);
    await sleep(2);
    const item = h.m.groups[0]?.items[0];
    if (!item) throw new Error("no item");
    await h.m.pin(item, true);
    await h.m.snooze(item, 3_600_000);
    const ref = { root: "/w", id: cid(1) };
    expect(h.updates).toEqual([
      [ref, { pinned: true }],
      [ref, { snoozedUntil: 3600 }],
    ]);
    expect(h.etags.slice(1)).toEqual([undefined, undefined]);
  });
});

describe("inbox: bridge adapter", () => {
  const change = (n: number) => new Uint8Array(16).fill(n);

  function fake() {
    const calls: string[] = [];
    let etag = "t1";
    let total = 2;
    const items = [
      create(InboxItemSchema, {
        change: change(1),
        section: InboxSection.NEEDS_REVIEW,
        author: "ada",
        updated: 50n,
        requestedBy: "bob",
        pinned: true,
      }),
      create(InboxItemSchema, { change: change(2), section: InboxSection.WAITING, updated: 40n }),
    ];
    const node: BridgeNode = {
      state: "up",
      pid: undefined,
      stderrTail: () => [],
      close() {},
      stream: async function* () {},
      unary: async (o) => {
        if (o.method === "asc.v1.Inbox/Count") {
          const req = fromBinary(InboxCountRequestSchema, o.request);
          calls.push(`count:${req.repo?.name}:${req.sections.length}:${req.ifNoneMatch}`);
          return toBinary(
            InboxCountResponseSchema,
            create(
              InboxCountResponseSchema,
              req.ifNoneMatch === etag ? { notModified: true } : { etag, total },
            ),
          );
        }
        if (o.method === "asc.v1.Inbox/List") {
          calls.push("list");
          return toBinary(InboxListResponseSchema, create(InboxListResponseSchema, { items }));
        }
        const req = fromBinary(InboxSetStateRequestSchema, o.request);
        calls.push(`set:${req.pinned}:${req.snoozedUntil}`);
        return toBinary(InboxSetStateResponseSchema, create(InboxSetStateResponseSchema, {}));
      },
    };
    const mgr = { node: () => node } as unknown as BridgeManager;
    return {
      calls,
      mgr,
      bump: () => {
        etag = "t2";
        total = 3;
      },
    };
  }
  const targets = [{ root: "/w", repo: "o/r" }];
  const signal = new AbortController().signal;

  test("a list maps sections and change ids, then a matching etag answers not-modified without paging", async () => {
    const f = fake();
    const api = bridgeInboxApi(f.mgr, () => targets);
    const r = await api.list(undefined, signal);
    if (r.status !== "ok") throw new Error("expected ok");
    expect(r.entries.map((e) => [e.section, e.id, e.pinned, e.requestedBy])).toEqual([
      ["needs_review", changeIdToString(change(1)), true, "bob"],
      ["waiting", changeIdToString(change(2)), false, ""],
    ]);
    expect(f.calls).toEqual(["count:o/r:0:", "list"]);
    expect((await api.list(r.etag, signal)).status).toBe("not-modified");
    expect(f.calls.slice(2)).toEqual(["count:o/r:0:t1"]);
    f.bump();
    expect((await api.list(r.etag, signal)).status).toBe("ok");
    expect(f.calls.slice(3)).toEqual(["count:o/r:0:t1", "list"]);
  });

  test("the count asks for the needs-review section only and sums the roots", async () => {
    const f = fake();
    const two = [...targets, { root: "/x", repo: "o/s" }];
    const api = bridgeInboxApi(f.mgr, () => two);
    const r = await api.needsReviewCount(undefined, signal);
    expect(r).toMatchObject({ status: "ok", total: 4 });
    expect(f.calls).toEqual(["count:o/r:1:", "count:o/s:1:"]);
    expect((await api.needsReviewCount("x", signal)).status).toBe("not-modified");
  });

  test("an update sends only the fields given and refuses a root without a server", async () => {
    const f = fake();
    const api = bridgeInboxApi(f.mgr, () => targets);
    await api.update({ root: "/w", id: changeIdToString(change(1)) }, { snoozedUntil: 90 });
    expect(f.calls).toEqual(["set:undefined:90"]);
    await expect(
      api.update({ root: "/none", id: changeIdToString(change(1)) }, { pinned: true }),
    ).rejects.toThrow("no asc server");
  });
});
