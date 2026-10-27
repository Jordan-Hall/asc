import { describe, expect, test } from "bun:test";
import {
  CloudStatusModel,
  cloudView,
  MAX_COUNT,
  parseCloudStatus,
  type CloudApi,
} from "../src/stack/cloud";

const enc = (v: unknown) => new TextEncoder().encode(JSON.stringify(v));
const q = (o: Record<string, unknown>) =>
  enc({ state: "x", queue: { pending: 0, held: 0, failed: 0, too_large: 0, ...o } });

describe("cloud: status item", () => {
  test("synced, N pending and offline", () => {
    expect(cloudView(parseCloudStatus(q({})), false).text).toBe("synced");
    expect(cloudView(parseCloudStatus(q({ pending: 3 })), false).text).toBe("3 pending");
    expect(cloudView(parseCloudStatus(q({ pending: 3 })), true).text).toBe("offline");
  });
  test("held or failed rows read as needing attention", () => {
    expect(cloudView(parseCloudStatus(q({ pending: 2, held: 1, failed: 1 })), false).text).toBe(
      "2 need attention",
    );
  });
  test("cloud off hides the item", () => {
    expect(cloudView(parseCloudStatus(enc({ state: "off" })), false).visible).toBe(false);
    expect(cloudView(null, false).visible).toBe(false);
  });
  test("untrusted output: no text shown, counts clamped, junk rejected", () => {
    const hostile = enc({
      state: "synced",
      summary: "<script>",
      queue: { pending: 1e99, held: -4, failed: "x" },
    });
    const v = cloudView(parseCloudStatus(hostile), false);
    expect(v.text).toBe(`${MAX_COUNT} pending`);
    expect(v.text + v.tooltip).not.toContain("script");
    for (const bad of [
      null,
      new Uint8Array(),
      new TextEncoder().encode("{"),
      enc([]),
      enc({ state: "synced" }),
    ]) {
      expect(parseCloudStatus(bad)).toBeNull();
    }
  });
});

function setup() {
  const calls = { n: 0 };
  const resolvers: ((b: Uint8Array | null) => void)[] = [];
  const api: CloudApi = {
    status: () => {
      calls.n++;
      return new Promise((r) => resolvers.push(r));
    },
  };
  const m = new CloudStatusModel({ api, root: "/w" });
  return { m, calls, answer: (i: number, b: Uint8Array | null) => resolvers[i]?.(b) };
}

describe("cloud: model", () => {
  test("a cloud event refreshes; a stale answer is dropped", async () => {
    const s = setup();
    const a = s.m.onEvent("cloud");
    const b = s.m.onEvent("cloud");
    s.answer(1, q({ pending: 1 }));
    s.answer(0, q({ pending: 9 }));
    await Promise.all([a, b]);
    expect(s.m.view().text).toBe("1 pending");
  });
  test("connectivity loss shows offline without a status call; recovery refreshes", async () => {
    const s = setup();
    await s.m.onEvent("connectivity", false);
    expect(s.calls.n).toBe(0);
    expect(s.m.serverOffline).toBe(true);
    const p = s.m.onEvent("connectivity", true);
    s.answer(0, q({}));
    await p;
    expect(s.m.view().text).toBe("synced");
  });
  test("a failing source keeps the last view and reports", async () => {
    const errs: unknown[] = [];
    const m = new CloudStatusModel({
      api: { status: () => Promise.reject(new Error("x")) },
      root: "/w",
      onError: (e) => errs.push(e),
    });
    await m.refresh();
    expect(errs.length).toBe(1);
    expect(m.status).toBeNull();
  });
  test("a disposed model ignores events", async () => {
    const s = setup();
    s.m.dispose();
    await s.m.onEvent("cloud");
    expect(s.calls.n).toBe(0);
  });
});
