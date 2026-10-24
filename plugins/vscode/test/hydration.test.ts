import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import {
  EXCLUDE_FOLDERS,
  findOffender,
  type HydrationApi,
  type HydrationStats,
  hydrationMessage,
  maybePromptExclusions,
  type Memento,
  missingExclusions,
  NOTIFY_INTERVAL_MS,
  pollHydration,
  sanitizeLabel,
  type SettingsApi,
  topFolders,
} from "../src/hydration";

function memento(): Memento & { m: Map<string, unknown> } {
  const m = new Map<string, unknown>();
  return {
    m,
    get: <T>(k: string) => m.get(k) as T | undefined,
    update: async (k, v) => {
      m.set(k, v);
    },
  };
}
function settings(init: Record<string, unknown> = {}): SettingsApi & { w: Map<string, unknown> } {
  const w = new Map<string, unknown>(Object.entries(init));
  return {
    w,
    getWorkspace: (k) => w.get(k) as never,
    updateWorkspace: async (k, v) => {
      w.set(k, v);
    },
  };
}
const stats = (over: Partial<HydrationStats> = {}): HydrationStats => ({
  byPath: [
    { key: "src/a/x.ts", count: 5 },
    { key: "src/a/y.ts", count: 4 },
    { key: "src\\b\\z.ts", count: 6 },
    { key: "lib/q.ts", count: 1 },
    { key: "top.ts", count: 99 },
  ],
  byProcess: [
    { key: "rg.exe", count: 5000 },
    { key: "Code Helper", count: 10 },
  ],
  ...over,
});
const answer = (a: string | undefined) => ({ notify: async () => a });

describe("exclusions prompt", () => {
  test("writes all three keys plus followSymlinks once, at workspace scope", async () => {
    const s = settings();
    const st = memento();
    let asked = 0;
    const prompt = {
      notify: async () => {
        asked++;
        return "Add exclusions";
      },
    };
    expect(await maybePromptExclusions(s, st, prompt)).toBe("written");
    for (const k of ["files.watcherExclude", "search.exclude", "files.exclude"]) {
      const v = s.w.get(k) as Record<string, boolean>;
      for (const f of EXCLUDE_FOLDERS) expect(v[f]).toBe(true);
    }
    expect(s.w.get("search.followSymlinks")).toBe(false);
    expect(await maybePromptExclusions(s, st, prompt)).toBe("skipped");
    expect(asked).toBe(1);
  });

  test("keeps user entries and writes nothing when declined or dismissed", async () => {
    const s = settings({ "search.exclude": { "**/node_modules": false, "**/keep": true } });
    const se = missingExclusions(s).get("search.exclude") as Record<string, boolean>;
    expect(se["**/node_modules"]).toBe(false);
    expect(se["**/keep"]).toBe(true);
    const st = memento();
    expect(await maybePromptExclusions(s, st, answer(undefined))).toBe("dismissed");
    expect(s.w.has("files.exclude")).toBe(false);
    expect(await maybePromptExclusions(s, st, answer("Not now"))).toBe("declined");
    expect(s.w.has("files.exclude")).toBe(false);
    expect(await maybePromptExclusions(s, st, answer("Add exclusions"))).toBe("skipped");
  });

  test("already complete settings do not prompt", async () => {
    const s = settings();
    for (const [k, v] of missingExclusions(s)) await s.updateWorkspace(k, v);
    let asked = 0;
    const r = await maybePromptExclusions(s, memento(), {
      notify: async () => {
        asked++;
        return undefined;
      },
    });
    expect(r).toBe("skipped");
    expect(asked).toBe(0);
  });
});

describe("per-process notification", () => {
  test("offender over budget, top three folders, sanitised", () => {
    expect(topFolders(stats().byPath)).toEqual(["src/a", "src/b", "lib"]);
    const o = findOffender(stats(), 1000);
    expect(o?.process).toBe("rg.exe");
    expect(findOffender(stats(), 5000)).toBeNull();
    expect(hydrationMessage(o as NonNullable<typeof o>)).toContain(
      "Use asc: Search code, or exclude folders",
    );
    expect(sanitizeLabel("a\u001b[31mb\n")).toBe("a [31mb");
  });

  test("once per 24 hours", async () => {
    const st = memento();
    const shown: string[] = [];
    let t = 1_000_000;
    const api: HydrationApi = { stats: async () => stats() };
    const run = () =>
      pollHydration({
        api,
        roots: ["/r"],
        budget: 1000,
        state: st,
        now: () => t,
        notify: async (m) => {
          shown.push(m);
          return undefined;
        },
        onAction() {},
      });
    expect(await run()).toBe(true);
    t += NOTIFY_INTERVAL_MS - 1;
    expect(await run()).toBe(false);
    t += 1;
    expect(await run()).toBe(true);
    expect(shown.length).toBe(2);
  });

  test("under budget and daemon errors are silent", async () => {
    const st = memento();
    const base = {
      roots: ["/r"],
      budget: 1000,
      state: st,
      now: () => 1,
      notify: async () => undefined,
      onAction() {},
    };
    const quiet = { stats: async () => stats({ byProcess: [] }) };
    expect(await pollHydration({ ...base, api: quiet })).toBe(false);
    const down = {
      stats: async (): Promise<HydrationStats> => {
        throw new Error("down");
      },
    };
    expect(await pollHydration({ ...base, api: down })).toBe(false);
    expect(st.m.size).toBe(0);
  });
});

describe("no hydration", () => {
  test("activation, reload and polling only ever call Stats", async () => {
    const calls: string[] = [];
    const api: HydrationApi = {
      stats: async () => {
        calls.push("Workspace.Stats");
        return stats({ byProcess: [] });
      },
    };
    const st = memento();
    for (let reload = 0; reload < 3; reload++) {
      await maybePromptExclusions(settings(), st, answer(undefined));
      await pollHydration({
        api,
        roots: ["/r"],
        budget: 1,
        state: memento(),
        now: () => 1,
        notify: async () => undefined,
        onAction() {},
      });
    }
    expect(calls.length).toBe(3);
    expect(calls.every((c) => c === "Workspace.Stats")).toBe(true);
  });

  test("the module imports no file system access and no ReadFile", () => {
    const src = readFileSync(new URL("../src/hydration.ts", import.meta.url), "utf8");
    expect(src).not.toMatch(/from "node:(fs|fs\/promises|child_process)"/);
    expect(src).not.toMatch(/readFile|readdir|workspace\.fs|findFiles/);
  });
});
