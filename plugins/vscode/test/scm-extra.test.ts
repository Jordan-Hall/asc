import { describe, expect, test } from "bun:test";
import { opBytes } from "./helpers/op";
import type { CommandsUi, RunOutcome } from "../src/scm/commands";
import { planIgnore } from "../src/scm/ignore";
import { RemoteCommands, type RemoteApi } from "../src/scm/remote";
import { parseStashList, StashCommands } from "../src/scm/stash";
import {
  type StackSnap,
  StackEditCommands,
  type StatusApi,
  snapFromStacks,
  statusBarModel,
} from "../src/scm/statusbar";

function fake(over: Partial<StatusApi & RemoteApi> = {}) {
  const runs: { verb: string; args: string[] }[] = [];
  const outcomes: RunOutcome[] = [];
  const api: StatusApi & RemoteApi = {
    async run(_r, verb, args) {
      runs.push({ verb, args });
      return outcomes.shift() ?? { exit: 0, stderr: [], opResult: null };
    },
    async stage() {},
    async branchless() {
      return true;
    },
    async canMutate() {
      return true;
    },
    async readWorking() {
      return null;
    },
    async readFile() {
      return new Uint8Array();
    },
    async stack() {
      return null;
    },
    async remoteKind() {
      return "git";
    },
    ...over,
  };
  return { api, runs, outcomes };
}

function fakeUi(confirm = true) {
  const log = {
    out: [] as string[],
    info: [] as string[],
    warn: [] as string[],
    err: [] as string[],
    confirms: 0,
  };
  const ui: CommandsUi = {
    output: (l) => void log.out.push(l),
    info: (m) => void log.info.push(m),
    warn: (m) => void log.warn.push(m),
    error: (m, lines) => void log.err.push(m, ...lines),
    confirm: async () => {
      log.confirms++;
      return confirm;
    },
    review: async () => true,
  };
  return { ui, log };
}

const snap = (over: Partial<StackSnap> = {}): StackSnap => ({
  short: "kxyzq",
  position: 2,
  total: 5,
  behind: 3,
  trunk: "master",
  needsRestack: false,
  ...over,
});

describe("scm-extra: status bar", () => {
  test("shows change, position and behind", () => {
    const m = statusBarModel(snap());
    expect(m?.text).toBe("$(git-commit) kxyzq · 2/5 · ↓3");
    expect(m?.needsRestack).toBe(false);
  });
  test("hides the behind part at 0 and caps at 999", () => {
    expect(statusBarModel(snap({ behind: 0 }))?.text).toBe("$(git-commit) kxyzq · 2/5");
    expect(statusBarModel(snap({ behind: 5000 }))?.text).toContain("↓999");
    expect(statusBarModel(snap({ behind: -2 }))?.behind).toBe(0);
  });
  test("no stack gives no item; needs-restack is separate", () => {
    expect(statusBarModel(null)).toBeNull();
    expect(statusBarModel(snap({ needsRestack: true }))?.needsRestack).toBe(true);
  });
  test("snapFromStacks picks the stack with the current change", () => {
    const s = snapFromStacks([
      {
        trunk: "master",
        behind: 9,
        changes: [{ short: "a", current: false, needsRestack: false }],
      },
      {
        trunk: "master",
        behind: 4,
        changes: [
          { short: "b", current: false, needsRestack: true },
          { short: "c", current: true, needsRestack: false },
        ],
      },
    ]);
    expect(s).toEqual({
      short: "c",
      position: 2,
      total: 2,
      behind: 4,
      trunk: "master",
      needsRestack: true,
    });
    expect(snapFromStacks([])).toBeNull();
  });
});

describe("scm-extra: uncommit, unamend, abandon", () => {
  test("uncommit and unamend run their verbs", async () => {
    const f = fake();
    const c = new StackEditCommands(f.api, fakeUi().ui);
    expect(await c.uncommit("/w")).toEqual({ ok: true });
    expect(await c.unamend("/w")).toEqual({ ok: true });
    expect(f.runs).toEqual([
      { verb: "uncommit", args: [] },
      { verb: "unamend", args: [] },
    ]);
  });
  test("abandon asks first and a refusal runs nothing", async () => {
    const f = fake();
    const u = fakeUi(false);
    expect(await new StackEditCommands(f.api, u.ui).abandon("/w", ["abc"])).toEqual({
      ok: false,
      reason: "cancelled",
    });
    expect(u.log.confirms).toBe(1);
    expect(f.runs).toHaveLength(0);
  });
  test("abandon passes revisions and refuses flag-like ones", async () => {
    const f = fake();
    const c = new StackEditCommands(f.api, fakeUi().ui);
    expect(await c.abandon("/w", ["abc", "def"])).toEqual({ ok: true });
    expect(f.runs[0]).toEqual({ verb: "abandon", args: ["abc", "def"] });
    expect(await c.abandon("/w", ["--force"])).toEqual({ ok: false, reason: "invalid" });
    expect(
      await c.abandon(
        "/w",
        Array.from({ length: 65 }, (_, i) => `r${i}`),
      ),
    ).toEqual({
      ok: false,
      reason: "invalid",
    });
    expect(f.runs).toHaveLength(1);
  });
  test("an old asc is told to update and nothing runs", async () => {
    const f = fake({ canMutate: async () => false });
    const u = fakeUi();
    expect(await new StackEditCommands(f.api, u.ui).uncommit("/w")).toEqual({
      ok: false,
      reason: "disabled",
    });
    expect(u.log.warn[0]).toContain("Update asc");
    expect(f.runs).toHaveLength(0);
  });
  test("exit 1 reports conflicts and exit 3 shows stderr", async () => {
    const f = fake();
    const u = fakeUi();
    const c = new StackEditCommands(f.api, u.ui);
    f.outcomes.push({ exit: 1, stderr: [], opResult: null });
    expect(await c.restack("/w")).toEqual({ ok: false, reason: "conflict" });
    f.outcomes.push({ exit: 3, stderr: ["boom"], opResult: null });
    expect(await c.restack("/w")).toEqual({ ok: false, reason: "failed" });
    expect(u.log.err).toContain("boom");
  });
});

describe("scm-extra: stash", () => {
  test("push builds flags and message", async () => {
    const f = fake();
    const c = new StashCommands(f.api, fakeUi().ui);
    await c.push("/w", { message: "wip", untracked: true, paths: ["a.ts", "-b"] });
    expect(f.runs[0]).toEqual({ verb: "stash push", args: ["-u", "-m=wip", "a.ts", "./-b"] });
    await c.push("/w");
    expect(f.runs[1]).toEqual({ verb: "stash push", args: [] });
  });
  test("push refuses an oversized message", async () => {
    const f = fake();
    expect(
      await new StashCommands(f.api, fakeUi().ui).push("/w", { message: "x".repeat(1025) }),
    ).toEqual({
      ok: false,
      reason: "invalid",
    });
    expect(f.runs).toHaveLength(0);
  });
  test("pop, apply run; drop confirms", async () => {
    const f = fake();
    const u = fakeUi(false);
    const c = new StashCommands(f.api, u.ui);
    await c.pop("/w");
    await c.apply("/w");
    expect(await c.drop("/w")).toEqual({ ok: false, reason: "cancelled" });
    expect(f.runs.map((r) => r.verb)).toEqual(["stash pop", "stash apply"]);
  });
  test("list reads the typed stash result and tolerates junk", async () => {
    const enc = new TextEncoder();
    expect(
      parseStashList(
        opBytes({
          stash: [
            { index: 0, message: "a" },
            { index: 1, message: "b" },
          ],
        }),
      ),
    ).toEqual([
      { index: 0, message: "a" },
      { index: 1, message: "b" },
    ]);
    expect(parseStashList(opBytes())).toEqual([]);
    expect(parseStashList(enc.encode('{"stashes":[]}'))).toEqual([]);
    expect(parseStashList(enc.encode("not json"))).toEqual([]);
    expect(parseStashList(null)).toEqual([]);
    const f = fake();
    f.outcomes.push({ exit: 0, stderr: [], opResult: opBytes({ stash: [{ index: 2, message: "x" }] }) });
    expect(await new StashCommands(f.api, fakeUi().ui).list("/w")).toEqual([
      { index: 2, message: "x" },
    ]);
  });
});

describe("scm-extra: fetch, pull, push", () => {
  test("run in git-remote mode", async () => {
    const f = fake();
    const c = new RemoteCommands(f.api, fakeUi().ui);
    await c.fetch("/w", { prune: true });
    await c.pull("/w");
    await c.pull("/w", "rebase");
    await c.push("/w", { setUpstream: true });
    expect(f.runs).toEqual([
      { verb: "fetch", args: ["--prune"] },
      { verb: "pull", args: ["--ff-only"] },
      { verb: "pull", args: ["--rebase"] },
      { verb: "push", args: ["-u"] },
    ]);
  });
  test("refused in server mode and with no remote", async () => {
    for (const kind of ["asc", "none"]) {
      const f = fake({ remoteKind: async () => kind });
      const u = fakeUi();
      const c = new RemoteCommands(f.api, u.ui);
      expect(await c.fetch("/w")).toEqual({ ok: false, reason: "disabled" });
      expect(await c.pull("/w")).toEqual({ ok: false, reason: "disabled" });
      expect(await c.push("/w")).toEqual({ ok: false, reason: "disabled" });
      expect(f.runs).toHaveLength(0);
      expect(u.log.confirms).toBe(0);
    }
  });
  test("push asks first", async () => {
    const f = fake();
    expect(await new RemoteCommands(f.api, fakeUi(false).ui).push("/w")).toEqual({
      ok: false,
      reason: "cancelled",
    });
    expect(f.runs).toHaveLength(0);
  });
  test("no argument carries a URL or credential", async () => {
    const f = fake();
    const c = new RemoteCommands(f.api, fakeUi().ui);
    await c.fetch("/w");
    await c.pull("/w", "default");
    await c.push("/w");
    expect(
      f.runs.flatMap((r) => r.args).filter((a) => a.includes("://") || a.includes("@")),
    ).toEqual([]);
  });
});

describe("scm-extra: .ascignore", () => {
  const files = (m: Record<string, string>) => async (dir: string) => m[dir] ?? null;
  test("uses the nearest .ascignore", async () => {
    const p = await planIgnore("a/b/out.log", false, files({ a: "x\n", "": "y\n" }));
    expect(p).toMatchObject({ dir: "a", insert: "/b/out.log\n", exists: true, already: false });
  });
  test("creates the root file when none exists", async () => {
    const p = await planIgnore("a/out.log", false, files({}));
    expect(p).toMatchObject({ dir: "", insert: "/a/out.log\n", exists: false });
  });
  test("adds a newline when the file lacks one, and a slash for a directory", async () => {
    const p = await planIgnore("build", true, files({ "": "node_modules" }));
    expect(p?.insert).toBe("\n/build/\n");
  });
  test("skips an existing pattern", async () => {
    const p = await planIgnore("build", true, files({ "": "/build/\n" }));
    expect(p).toMatchObject({ already: true, insert: "" });
  });
  test("escapes glob specials and leading # or !", async () => {
    expect((await planIgnore("a[1].txt", false, files({})))?.pattern).toBe("/a\\[1\\].txt");
    expect((await planIgnore("#x", false, files({})))?.pattern).toBe("/\\#x");
    expect((await planIgnore("!x", false, files({})))?.pattern).toBe("/\\!x");
  });
  test("refuses traversal and control characters", async () => {
    for (const bad of ["../x", "a/../b", "/abs", "a\nb", "a\0b", "", "a//b"]) {
      expect(await planIgnore(bad, false, files({}))).toBeNull();
    }
  });
});
