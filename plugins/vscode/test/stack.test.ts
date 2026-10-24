import { describe, expect, test } from "bun:test";
import type { CommandsApi, RunOutcome } from "../src/scm/commands";
import { StackActions } from "../src/stack/actions";
import { type Candidate, parseCandidates, StackNav, validChange, type NavUi } from "../src/stack/nav";
import { shareCurrent, shareLink } from "../src/stack/share";

function fake(canMutate = true) {
  const runs: { verb: string; args: string[] }[] = [];
  const outcomes: RunOutcome[] = [];
  const api = {
    async run(_r: string, verb: string, args: string[]) {
      runs.push({ verb, args });
      return outcomes.shift() ?? { exit: 0, stderr: [], opResult: null };
    },
    async canMutate() {
      return canMutate;
    },
  } as unknown as CommandsApi;
  return { api, runs, outcomes };
}

function ui(opts: { confirm?: boolean; pick?: string } = {}) {
  const log = { warn: [] as string[], confirms: 0, picks: 0 };
  const u: NavUi = {
    output() {},
    info() {},
    warn: (m) => void log.warn.push(m),
    error() {},
    async confirm() {
      log.confirms++;
      return opts.confirm ?? true;
    },
    async review() {
      return true;
    },
    async pickCandidate(_c: readonly Candidate[]) {
      log.picks++;
      return opts.pick;
    },
  };
  return { u, log };
}

const fork = (cands: unknown[]): RunOutcome => ({
  exit: 2,
  stderr: [],
  opResult: new TextEncoder().encode(JSON.stringify({ status: "ambiguous", candidates: cands })),
});

describe("stack: navigation", () => {
  test("up, down, top and bottom run their verbs", async () => {
    const f = fake();
    const n = new StackNav(f.api, ui().u);
    for (const v of ["up", "down", "top", "bottom"] as const) expect(await n.go("/w", v)).toEqual({ ok: true });
    expect(f.runs.map((r) => r.verb)).toEqual(["up", "down", "top", "bottom"]);
    expect(f.runs.every((r) => r.args.length === 0)).toBe(true);
  });
  test("a step count is validated and passed", async () => {
    const f = fake();
    const n = new StackNav(f.api, ui().u);
    expect(await n.go("/w", "up", 3)).toEqual({ ok: true });
    expect(f.runs[0]).toEqual({ verb: "up", args: ["3"] });
    for (const bad of [0, 101, 1.5, -1, Number.NaN]) {
      expect(await n.go("/w", "down", bad)).toEqual({ ok: false, reason: "invalid" });
    }
    expect(await n.go("/w", "top", 2)).toEqual({ ok: false, reason: "invalid" });
    expect(f.runs).toHaveLength(1);
  });
  test("a fork shows the quick pick and runs again with the chosen change", async () => {
    const f = fake();
    f.outcomes.push(fork([{ change: "aaaa", subject: "one" }, { change: "bbbb", subject: "two" }]));
    const x = ui({ pick: "bbbb" });
    expect(await new StackNav(f.api, x.u).go("/w", "up")).toEqual({ ok: true });
    expect(x.log.picks).toBe(1);
    expect(f.runs).toEqual([
      { verb: "up", args: [] },
      { verb: "up", args: ["--to=bbbb"] },
    ]);
  });
  test("a dismissed pick, or a change the verb did not offer, runs nothing more", async () => {
    for (const pick of [undefined, "--evil", "cccc"]) {
      const f = fake();
      f.outcomes.push(fork([{ change: "aaaa", subject: "one" }]));
      const r = await new StackNav(f.api, ui(pick === undefined ? {} : { pick }).u).go("/w", "down");
      expect(r).toEqual({ ok: false, reason: "cancelled" });
      expect(f.runs).toHaveLength(1);
    }
  });
  test("an exit 2 without candidates is ambiguous and asks nothing", async () => {
    const f = fake();
    f.outcomes.push({ exit: 2, stderr: [], opResult: null });
    const x = ui();
    expect(await new StackNav(f.api, x.u).go("/w", "up")).toEqual({ ok: false, reason: "ambiguous" });
    expect(x.log.picks).toBe(0);
  });
  test("candidates with flag-like or blank changes are dropped", () => {
    const bytes = new TextEncoder().encode(
      JSON.stringify({ candidates: [{ change: "--to=x" }, { change: "" }, { change: "a b" }, { change: "ok1", subject: 5 }, 7] }),
    );
    expect(parseCandidates(bytes)).toEqual([{ change: "ok1", subject: "" }]);
    expect(parseCandidates(new TextEncoder().encode("{"))).toEqual([]);
    expect(parseCandidates(null)).toEqual([]);
    expect(validChange("-x")).toBe(false);
  });
  test("an asc that is too old runs nothing", async () => {
    const f = fake(false);
    const x = ui();
    expect(await new StackNav(f.api, x.u).go("/w", "up")).toEqual({ ok: false, reason: "disabled" });
    expect(f.runs).toHaveLength(0);
  });
});

describe("stack: actions", () => {
  test("restack and sync run their verbs", async () => {
    const f = fake();
    const a = new StackActions(f.api, ui().u);
    await a.restack("/w");
    await a.sync("/w");
    expect(f.runs.map((r) => r.verb)).toEqual(["restack", "sync"]);
  });
  test("submit asks first; a refusal runs nothing", async () => {
    const f = fake();
    const x = ui({ confirm: false });
    expect(await new StackActions(f.api, x.u).submit("/w")).toEqual({ ok: false, reason: "cancelled" });
    expect(x.log.confirms).toBe(1);
    expect(f.runs).toHaveLength(0);
  });
  test("a confirmed submit runs submit -s", async () => {
    const f = fake();
    expect(await new StackActions(f.api, ui().u).submit("/w")).toEqual({ ok: true });
    expect(f.runs).toEqual([{ verb: "submit", args: ["-s"] }]);
  });
  test("a conflict is reported", async () => {
    const f = fake();
    f.outcomes.push({ exit: 1, stderr: [], opResult: null });
    expect(await new StackActions(f.api, ui().u).restack("/w")).toEqual({ ok: false, reason: "conflict" });
  });
});

describe("stack: share", () => {
  test("only a hex change id makes an asc: link", () => {
    expect(shareLink("abcd1234")).toBe("asc:abcd1234");
    for (const bad of ["", "abc", "ABCD", "ab cd", "../x", "asc:abcd", "xyz12"]) expect(shareLink(bad)).toBeNull();
  });
  test("share copies the link, or warns when there is no change", async () => {
    const copied: string[] = [];
    const warns: string[] = [];
    const s = { copy: async (t: string) => void copied.push(t), info() {}, warn: (m: string) => void warns.push(m) };
    const snap = { short: "abcd12", position: 1, total: 1, behind: 0, trunk: "master", needsRestack: false };
    expect(await shareCurrent(snap, s)).toBe("asc:abcd12");
    expect(copied).toEqual(["asc:abcd12"]);
    expect(await shareCurrent(null, s)).toBeNull();
    expect(warns).toHaveLength(1);
    expect(copied).toHaveLength(1);
  });
});
