import { describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  type CommandsApi,
  type CommandsUi,
  CommitCommands,
  RESTORE_CHUNK,
  type RunOutcome,
  summariseOpResult,
} from "../src/scm/commands";
import {
  buildHunkPatch,
  buildPatch,
  changeBlocks,
  diffLines,
  quotePath,
  splitLines,
} from "../src/scm/stage-ranges";

const ROOT = "/w";
const dec = new TextDecoder();
const enc = new TextEncoder();

function fake(over: Partial<CommandsApi> = {}) {
  const runs: { verb: string; args: string[] }[] = [];
  const stages: { add?: string[]; remove?: string[]; patch?: string }[] = [];
  const outcomes: RunOutcome[] = [];
  const files = new Map<string, string>();
  const api: CommandsApi = {
    async run(_r, verb, args) {
      runs.push({ verb, args });
      return outcomes.shift() ?? { exit: 0, stderr: [], opResult: null };
    },
    async stage(_r, req) {
      stages.push(req);
    },
    async branchless() {
      return true;
    },
    async canMutate() {
      return true;
    },
    async readWorking(_r, p) {
      return files.get(`work:${p}`) ?? null;
    },
    async readFile(_r, p, at) {
      const k = at.kind === "stage" ? `index:${p}` : at.kind === "rev" ? `head:${p}` : "";
      const v = files.get(k);
      if (v === undefined) throw new Error("not found");
      return enc.encode(v);
    },
    ...over,
  };
  return { api, runs, stages, outcomes, files };
}

function fakeUi(confirm = true, review = true) {
  const log = { out: [] as string[], info: [] as string[], warn: [] as string[], err: [] as string[] };
  const ui: CommandsUi = {
    output: (l) => void log.out.push(l),
    info: (m) => void log.info.push(m),
    warn: (m) => void log.warn.push(m),
    error: (m, lines) => void log.err.push(m, ...lines),
    confirm: async () => confirm,
    review: async () => review,
  };
  return { ui, log };
}

describe("commit: create, amend, reword", () => {
  test("create uses --no-branch in branchless mode and plain create otherwise", async () => {
    const f = fake();
    const c = new CommitCommands(f.api, fakeUi().ui);
    expect(await c.create(ROOT, "feat: x")).toEqual({ ok: true });
    expect(f.runs[0]).toEqual({ verb: "create", args: ["--no-branch", "-m=feat: x"] });
    const g = fake({ branchless: async () => false });
    await new CommitCommands(g.api, fakeUi().ui).create(ROOT, "feat: y");
    expect(g.runs[0]).toEqual({ verb: "create", args: ["-m=feat: y"] });
  });
  test("an empty message never reaches the bridge", async () => {
    const f = fake();
    const u = fakeUi();
    expect(await new CommitCommands(f.api, u.ui).create(ROOT, "  ")).toEqual({
      ok: false,
      reason: "invalid",
    });
    expect(f.runs).toEqual([]);
    expect(u.log.warn.length).toBe(1);
  });
  test("a leading dash in the message stays a value", async () => {
    const f = fake();
    await new CommitCommands(f.api, fakeUi().ui).reword(ROOT, "-n fix");
    expect(f.runs[0]).toEqual({ verb: "reword", args: ["-m=-n fix"] });
  });
  test("amend with and without a message", async () => {
    const f = fake();
    const c = new CommitCommands(f.api, fakeUi().ui);
    await c.amend(ROOT);
    await c.amend(ROOT, "new");
    expect(f.runs.map((r) => r.args)).toEqual([[], ["-m=new"]]);
  });
  test("a too-old bridge disables mutations", async () => {
    const f = fake({ canMutate: async () => false });
    const u = fakeUi();
    const c = new CommitCommands(f.api, u.ui);
    expect(await c.create(ROOT, "m")).toEqual({ ok: false, reason: "disabled" });
    expect(await c.undo(ROOT)).toEqual({ ok: false, reason: "disabled" });
    expect(f.runs).toEqual([]);
  });
  test("hook output goes to the output channel and a failure shows the tail", async () => {
    const f = fake();
    f.outcomes.push({ exit: 7, stderr: ["husky: lint failed", "commitlint: bad"], opResult: null });
    const u = fakeUi();
    const r = await new CommitCommands(f.api, u.ui).create(ROOT, "bad");
    expect(r).toEqual({ ok: false, reason: "failed" });
    expect(u.log.out).toContain("husky: lint failed");
    expect(u.log.err).toContain("commitlint: bad");
    // the message is not logged
    expect(u.log.out.join("\n")).not.toContain("bad\n");
  });
  test("exit 1 is a conflict and exit 2 is ambiguous, with no error toast", async () => {
    const f = fake();
    f.outcomes.push({ exit: 1, stderr: [], opResult: null }, { exit: 2, stderr: [], opResult: null });
    const u = fakeUi();
    const c = new CommitCommands(f.api, u.ui);
    expect(await c.amend(ROOT)).toEqual({ ok: false, reason: "conflict" });
    expect(await c.amend(ROOT)).toEqual({ ok: false, reason: "ambiguous" });
    expect(u.log.err).toEqual([]);
    expect(u.log.warn.length).toBe(2);
  });
  test("mutations on one root run in call order", async () => {
    const order: string[] = [];
    let release: () => void = () => {};
    const gate = new Promise<void>((r) => (release = r));
    const f = fake({
      async run(_r, verb) {
        order.push(`start ${verb}`);
        if (verb === "modify") await gate;
        order.push(`end ${verb}`);
        return { exit: 0, stderr: [], opResult: null };
      },
    });
    const c = new CommitCommands(f.api, fakeUi().ui);
    const a = c.amend(ROOT);
    const b = c.undo(ROOT);
    await Promise.resolve();
    release();
    await Promise.all([a, b]);
    expect(order).toEqual(["start modify", "end modify", "start undo", "end undo"]);
  });
  test("undo and redo run their verbs", async () => {
    const f = fake();
    const c = new CommitCommands(f.api, fakeUi().ui);
    await c.undo(ROOT);
    await c.redo(ROOT);
    expect(f.runs.map((r) => r.verb)).toEqual(["undo", "redo"]);
  });
});

describe("commit: absorb", () => {
  test("previews with -n, then applies with -f after the review", async () => {
    const f = fake();
    f.outcomes.push({
      exit: 0,
      stderr: [],
      opResult: enc.encode(JSON.stringify({ status: "ok", rewritten: [{ change: "abc", subject: "fix" }] })),
    });
    const c = new CommitCommands(f.api, fakeUi(true, true).ui);
    expect(await c.absorb(ROOT)).toEqual({ ok: true });
    expect(f.runs.map((r) => r.args)).toEqual([["-n"], ["-f"]]);
  });
  test("a declined review never applies", async () => {
    const f = fake();
    const c = new CommitCommands(f.api, fakeUi(true, false).ui);
    expect(await c.absorb(ROOT)).toEqual({ ok: false, reason: "cancelled" });
    expect(f.runs.map((r) => r.args)).toEqual([["-n"]]);
  });
  test("nothing to absorb stops after the preview", async () => {
    const f = fake();
    f.outcomes.push({ exit: 0, stderr: [], opResult: enc.encode('{"status":"nothing"}') });
    const u = fakeUi();
    expect(await new CommitCommands(f.api, u.ui).absorb(ROOT)).toEqual({ ok: true, nothing: true });
    expect(f.runs.length).toBe(1);
    expect(u.log.info.length).toBe(1);
  });
  test("an op result that is not JSON reads as absent", () => {
    expect(summariseOpResult(new Uint8Array([0xff, 0x00]))).toBeNull();
    expect(summariseOpResult(null)).toBeNull();
  });
});

describe("commit: discard", () => {
  test("asks first and restores both stage and worktree", async () => {
    const f = fake();
    const c = new CommitCommands(f.api, fakeUi(true).ui);
    await c.discard(ROOT, [{ path: "a.ts", xy: ".M" }, { path: "-weird", xy: "M." }]);
    expect(f.runs).toEqual([
      { verb: "restore", args: ["--staged", "--worktree", "a.ts", "./-weird"] },
    ]);
  });
  test("a refusal runs nothing", async () => {
    const f = fake();
    const c = new CommitCommands(f.api, fakeUi(false).ui);
    expect(await c.discard(ROOT, [{ path: "a", xy: ".M" }])).toEqual({
      ok: false,
      reason: "cancelled",
    });
    expect(f.runs).toEqual([]);
  });
  test("untracked files are skipped and never restored", async () => {
    const f = fake();
    const c = new CommitCommands(f.api, fakeUi(true).ui);
    expect(await c.discard(ROOT, [{ path: "n", xy: "??" }])).toEqual({
      ok: false,
      reason: "invalid",
    });
    expect(f.runs).toEqual([]);
  });
  test("many paths run in chunks under the argument cap", async () => {
    const f = fake();
    const files = Array.from({ length: RESTORE_CHUNK * 2 + 1 }, (_, i) => ({ path: `f${i}`, xy: ".M" }));
    await new CommitCommands(f.api, fakeUi(true).ui).discard(ROOT, files);
    expect(f.runs.length).toBe(3);
    for (const r of f.runs) expect(r.args.length).toBeLessThanOrEqual(64);
  });
});

const BASE = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", ""].join("\n");

describe("commit: stage ranges", () => {
  test("diff finds blocks and survives a missing final newline", () => {
    const ops = diffLines(splitLines("a\nb\n"), splitLines("a\nb"));
    expect(changeBlocks(ops).length).toBe(1);
    expect(diffLines(splitLines(""), splitLines("")).length).toBe(0);
  });
  test("one of two hunks stages only that hunk", () => {
    const target = BASE.replace("b\n", "B\n").replace("i\n", "I\n");
    const p = buildPatch("f.txt", BASE, target, [{ start: 9, end: 9 }], "target");
    expect(p).toContain("+I\n");
    expect(p).not.toContain("+B\n");
    expect(p).toContain("-i\n");
    // unselected line stays as it is in the index
    expect(p).not.toContain("-b\n");
  });
  test("a partial selection inside a block leaves the other lines out", () => {
    const target = `${["a", "x", "y", "z", "b"].join("\n")}\n`;
    const base = "a\nb\n";
    const p = buildPatch("f", base, target, [{ start: 3, end: 3 }], "target") ?? "";
    expect(p).toContain("+y\n");
    expect(p).not.toContain("+x\n");
    expect(p).not.toContain("+z\n");
    expect(p).toContain("@@ -1,2 +1,3 @@");
  });
  test("a point selects a pure deletion", () => {
    const target = "a\nc\n";
    const p = buildPatch("f", "a\nb\nc\n", target, [{ start: 2, end: 1 }], "target");
    expect(p).toContain("-b\n");
    expect(buildPatch("f", "a\nb\nc\n", target, [{ start: 1, end: 1 }], "target")).toBeNull();
  });
  test("an empty selection or no change gives no patch", () => {
    expect(buildPatch("f", "a\n", "a\n", [{ start: 1, end: 5 }], "target")).toBeNull();
    expect(buildPatch("f", "a\n", "b\n", [], "target")).toBeNull();
  });
  test("a new final line without a newline is marked", () => {
    const p = buildPatch("f", "a\n", "a\nb", [{ start: 2, end: 2 }], "target") ?? "";
    expect(p).toContain("+b\n\\ No newline at end of file\n");
  });
  test("unstage numbers index lines", () => {
    // index has B and I changed; HEAD has b and i. Unstage only line 2 (B).
    const index = BASE.replace("b\n", "B\n").replace("i\n", "I\n");
    const p = buildPatch("f", index, BASE, [{ start: 2, end: 2 }], "base") ?? "";
    expect(p).toContain("-B\n");
    expect(p).toContain("+b\n");
    expect(p).not.toContain("-I\n");
  });
  test("hunk patches pick one block", () => {
    const target = BASE.replace("b\n", "B\n").replace("i\n", "I\n");
    expect(buildHunkPatch("f", BASE, target, 1)).toContain("+I\n");
    expect(buildHunkPatch("f", BASE, target, 1)).not.toContain("+B\n");
    expect(buildHunkPatch("f", BASE, target, 5)).toBeNull();
  });
  test("odd names are quoted like git does", () => {
    expect(quotePath("a b.ts")).toBe("a b.ts");
    expect(quotePath('q"x')).toBe('"q\\"x"');
    expect(quotePath("é")).toBe('"\\303\\251"');
  });

  const git = spawnSync("git", ["--version"]).status === 0;
  test.skipIf(!git)("git apply --cached accepts the patch and stages exactly the selection", () => {
    const dir = mkdtempSync(join(tmpdir(), "asc-stage-"));
    try {
      const run = (...a: string[]) => {
        const r = spawnSync("git", a, { cwd: dir, encoding: "utf8" });
        if (r.status !== 0) throw new Error(`git ${a.join(" ")}: ${r.stderr}`);
        return r.stdout;
      };
      run("init", "-q");
      run("config", "user.email", "t@example.com");
      run("config", "user.name", "t");
      writeFileSync(join(dir, "f.txt"), BASE);
      run("add", ".");
      run("commit", "-qm", "base");
      const target = BASE.replace("b\n", "B\n").replace("i\n", "I\n").replace("e\n", "");
      writeFileSync(join(dir, "f.txt"), target);
      const p = buildPatch("f.txt", BASE, target, [{ start: 8, end: 8 }], "target") ?? "";
      writeFileSync(join(dir, "p.patch"), p);
      run("apply", "--cached", "p.patch");
      const staged = run("diff", "--cached", "--unified=0");
      expect(staged).toContain("+I");
      expect(staged).not.toContain("+B");
      expect(staged).not.toContain("-e");
      // and the same selection unstages again
      const idx = run("show", ":f.txt");
      const q = buildPatch("f.txt", idx, BASE, [{ start: 9, end: 9 }], "base") ?? "";
      writeFileSync(join(dir, "q.patch"), q);
      run("apply", "--cached", "q.patch");
      expect(run("diff", "--cached", "--stat")).toBe("");
      expect(dec.decode(enc.encode(run("show", ":f.txt")))).toBe(BASE);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("commit: stage through the api", () => {
  test("a selection sends a patch; a file not yet in the index is added whole", async () => {
    const f = fake();
    f.files.set("index:a.txt", "a\nb\n");
    f.files.set("work:a.txt", "a\nB\n");
    const c = new CommitCommands(f.api, fakeUi().ui);
    expect(await c.stageRanges(ROOT, "a.txt", [{ start: 2, end: 2 }], "stage")).toEqual({ ok: true });
    expect(f.stages[0]?.patch).toContain("+B\n");
    f.files.set("work:new.txt", "x\n");
    await c.stageRanges(ROOT, "new.txt", [{ start: 1, end: 1 }], "stage");
    expect(f.stages[1]).toEqual({ add: ["new.txt"] });
  });
  test("unstage reads HEAD and removes a file that HEAD lacks", async () => {
    const f = fake();
    f.files.set("index:n.txt", "x\n");
    const c = new CommitCommands(f.api, fakeUi().ui);
    await c.stageRanges(ROOT, "n.txt", [{ start: 1, end: 1 }], "unstage");
    expect(f.stages[0]).toEqual({ remove: ["n.txt"] });
  });
  test("an oversized patch is refused before the bridge", async () => {
    const f = fake();
    const big = Array.from({ length: 60_000 }, (_, i) => `line number ${i}`).join("\n");
    f.files.set("index:b.txt", "");
    f.files.set("work:b.txt", `${big}\n`);
    const u = fakeUi();
    const r = await new CommitCommands(f.api, u.ui).stageRanges(
      ROOT,
      "b.txt",
      [{ start: 1, end: 60_000 }],
      "stage",
    );
    expect(r).toEqual({ ok: false, reason: "invalid" });
    expect(f.stages).toEqual([]);
  });
  test("a failing Stage reports and returns failed", async () => {
    const f = fake({
      stage: async () => {
        throw new Error("boom");
      },
    });
    f.files.set("index:a", "a\n");
    f.files.set("work:a", "b\n");
    const u = fakeUi();
    expect(await new CommitCommands(f.api, u.ui).stageHunk(ROOT, "a", 0, "stage")).toEqual({
      ok: false,
      reason: "failed",
    });
    expect(u.log.err[0]).toContain("boom");
  });
});
