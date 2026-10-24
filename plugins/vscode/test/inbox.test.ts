import { describe, expect, test } from "bun:test";
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
    expect(h.p.view().tooltip).toContain("forge offline");
    expect(h.p.total).toBe(3);
  });
});

const entry = (id: number, section: string, over: Partial<InboxEntry> = {}): InboxEntry => ({
  id,
  section,
  pull_request: { number: id, title: `pr ${id}` },
  ...over,
});

describe("inbox: tree", () => {
  test("groups by section, pinned first, hides snoozed, drops empty and unknown sections", () => {
    const now = Date.parse("2026-10-24T12:00:00Z");
    const g = group(
      [
        entry(3, "needs_review"),
        entry(1, "needs_review", { pinned: true }),
        entry(2, "needs_review", { snoozed_until: "2026-10-25T00:00:00Z" }),
        entry(4, "needs_review", { snoozed_until: "2026-10-01T00:00:00Z" }),
        entry(5, "drafts"),
        entry(6, "bogus"),
      ],
      now,
    );
    expect(g.map((x) => x.id)).toEqual(["needs_review", "drafts"]);
    expect(g[0]?.items.map((i) => i.id)).toEqual([1, 3, 4]);
  });

  function model(signedIn = true) {
    const ft = fakeTimers();
    const etags: (string | undefined)[] = [];
    const updates: unknown[] = [];
    let next: ListResult = { status: "ok", entries: [entry(1, "ready")], etag: '"e1"' };
    const api = {
      list: async (etag) => (etags.push(etag), next),
      update: async (id, body) => void updates.push([id, body]),
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

  test("fetches only while visible, then every 60 s with If-None-Match", async () => {
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
    expect(h.etags[1]).toBe('"e1"');
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
    expect(h.updates).toEqual([
      [1, { pinned: true }],
      [1, { snoozed_until: "1970-01-01T01:00:00.000Z" }],
    ]);
    expect(h.etags.slice(1)).toEqual([undefined, undefined]);
  });
});

