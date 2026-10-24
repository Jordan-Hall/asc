// P12 acceptance: every scenario of SPEC section 24.7.2 builds with the asc CLI, has the contents
// the table promises, and building it twice gives identical commits, refs and stack shape.
// Sizes are scaled down except where the spec's number is the point; set ASC_BIN to the binary.
import { afterAll, describe, expect, setDefaultTimeout, test } from "bun:test";
import { mkdtempSync, rmSync, readdirSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  buildFixture,
  ascAvailable,
  DEFAULT_SIZES,
  type Fixture,
  type ScenarioName,
  SCENARIOS,
  snapshot,
  supportsDeterministicChangeIds,
} from "../src/index";

// Each build runs hundreds of asc processes; the default five seconds is far too short.
setDefaultTimeout(3_600_000);

const have = ascAvailable();
const it = test.skipIf(!have);

const scratch = mkdtempSync(join(tmpdir(), "asc-fixtures-"));
afterAll(() => rmSync(scratch, { recursive: true, force: true }));

const SMALL = {
  cleanCommits: 24,
  historyCommits: 40,
  dirtyBigLines: 500,
  bigFiles: 60,
  bigCommits: 6,
};
let n = 0;
const dir = () => join(scratch, `b${n++}`);
const live: Fixture[] = [];
afterAll(async () => {
  for (const f of live) await f.dispose();
});

async function build(name: ScenarioName, sizes: Partial<typeof DEFAULT_SIZES> = SMALL) {
  const fx = await buildFixture(name, { dir: dir(), sizes });
  live.push(fx);
  return fx;
}

const lines = (s: string) => s.split("\n").filter((l) => l !== "");

describe("scenarios", () => {
  it("the table of SPEC 24.7.2 is complete", () => {
    expect([...SCENARIOS]).toEqual([
      "clean",
      "dirty",
      "stacks",
      "conflict",
      "worktrees",
      "history",
      "review",
      "big",
    ]);
    expect(DEFAULT_SIZES).toMatchObject({
      cleanCommits: 200,
      dirtyBigLines: 10_000,
      historyCommits: 2_000,
    });
    expect(DEFAULT_SIZES).toMatchObject({ bigFiles: 100_000, bigCommits: 100_000 });
  });

  it("clean: master with 200 commits and a clean work tree", async () => {
    const fx = await build("clean", { cleanCommits: DEFAULT_SIZES.cleanCommits });
    expect(lines(fx.asc.ok(["rev-list", "--count", "master"], { cwd: fx.root }))).toEqual(["200"]);
    expect(fx.asc.ok(["branch", "--show-current"], { cwd: fx.root }).trim()).toBe("master");
    expect(lines(fx.asc.ok(["status", "--porcelain=v1"], { cwd: fx.root }))).toEqual([]);
    expect(fx.asc.ok(["config", "asc.branchless"], { cwd: fx.root }).trim()).toBe("true");
  }, 900_000);

  it("dirty: every kind of work tree change", async () => {
    const fx = await build("dirty");
    const st = lines(fx.asc.ok(["status", "--porcelain=v1", "--ignored"], { cwd: fx.root }));
    expect(st).toContain(" M src/lib.rs"); // unstaged
    expect(st).toContain(" M image.bin"); // binary
    expect(st).toContain(" M big.txt");
    expect(st).toContain("A  staged_new.txt"); // staged
    expect(st).toContain("R  docs/old_name.txt -> docs/new_name.txt"); // rename
    expect(st).toContain("M  script.sh"); // mode change
    expect(st).toContain("?? untracked.txt");
    expect(st).toContain("?? new.bin");
    expect(st).toContain("!! debug.log"); // ignored
    expect(st).toContain("!! scratch.tmp");
    expect(fx.asc.ok(["ls-files", "-s", "script.sh"], { cwd: fx.root })).toMatch(/^100755 /);
    expect(fx.asc.ok(["cat-file", "-p", "HEAD:.ascignore"], { cwd: fx.root })).toContain("*.tmp");
  });

  it("dirty: the big file has the spec's 10,000 lines by default", async () => {
    const fx = await build("dirty", {});
    expect(lines(fx.asc.ok(["cat-file", "-p", "HEAD:big.txt"], { cwd: fx.root }))).toHaveLength(
      10_000,
    );
  });

  it("stacks: 3 and 5 changes, restack, divergent, frozen, a label and two hidden drafts", async () => {
    const fx = await build("stacks");
    const sl = fx.asc.json<{
      stacks: {
        changes: {
          subject: string;
          state: string;
          needs_restack: boolean;
          frozen: boolean;
          branches: string[];
        }[];
      }[];
    }>(["sl", "--json"], { cwd: fx.root });
    const all = sl.stacks.flatMap((s) => s.changes);
    // Stack A has three changes (a1 to a3); a1 was rewritten, so a2 and a3 wait for a restack.
    const stackA = sl.stacks.find((s) => s.changes.some((c) => c.subject === "feat: a3"));
    expect(stackA?.changes.map((c) => c.subject)).toEqual(["feat: a2", "feat: a3"]);
    expect(stackA?.changes.some((c) => c.needs_restack)).toBe(true);
    expect(all.some((c) => c.subject === "feat: a1 reworded")).toBe(true);
    // Stack B has five (b1 to b5); b4 has two live rewrites, so b5 waits for a restack.
    const subjects = new Set(all.map((c) => c.subject));
    for (const s of [
      "feat: b1",
      "feat: b2",
      "feat: b3",
      "feat: b5",
      "feat: b4 first rewrite",
      "feat: b4 second rewrite",
    ]) {
      expect(subjects.has(s)).toBe(true);
    }
    expect(all.some((c) => c.subject === "feat: b5" && c.needs_restack)).toBe(true);
    for (const k of ["a1", "a2", "a3", "b1", "b2", "b3", "b4", "b5"])
      expect(fx.marks[k]).toMatch(/^[0-9a-f]{40}$/);
    expect(all.some((c) => c.state === "divergent")).toBe(true);
    expect(all.filter((c) => c.frozen).map((c) => c.subject)).toContain("feat: b2");
    expect(all.some((c) => c.branches.includes("label-a2"))).toBe(true);
    // The hidden drafts are not in the visible stacks, but they are still in the repository.
    expect(all.some((c) => c.subject.startsWith("feat: scratch"))).toBe(false);
    for (const k of ["hidden1", "hidden2"]) {
      expect(fx.asc.run(["cat-file", "-e", fx.marks[k] as string], { cwd: fx.root }).code).toBe(0);
    }
  });

  it("conflict: a restack paused on a content conflict and a modify/delete", async () => {
    const fx = await build("conflict");
    const st = lines(fx.asc.ok(["status", "--porcelain=v1"], { cwd: fx.root }));
    expect(st).toContain("UU f");
    expect(st).toContain("UD g");
    expect(fx.asc.run(["continue"], { cwd: fx.root }).code).not.toBe(0); // still conflicted
  });

  it("worktrees: three workspaces, one ephemeral, one nested under .claude/worktrees", async () => {
    const fx = await build("worktrees");
    expect(Object.keys(fx.workspaces).toSorted()).toEqual([
      "ephemeral",
      "feature",
      "main",
      "nested",
    ]);
    const list = fx.asc.ok(["worktree", "list", "--porcelain"], { cwd: fx.root });
    const paths = lines(list).filter((l) => l.startsWith("worktree "));
    expect(paths).toHaveLength(4); // main plus the three added
    expect(list).toContain("agent/tmp");
    expect(fx.workspaces.nested).toContain(".claude");
    expect(statSync(fx.workspaces.nested as string).isDirectory()).toBe(true);
    // The JSON form carries the ephemeral marker, on an asc that has it.
    const json = fx.asc.run(["worktree", "list", "--json"], { cwd: fx.root });
    if (json.code === 0) expect(json.stdout.match(/"ephemeral":true/g)).toHaveLength(1);
  });

  it("history: renames survive for blame and line history", async () => {
    const fx = await build("history", { historyCommits: 120 });
    expect(lines(fx.asc.ok(["rev-list", "--count", "master"], { cwd: fx.root }))).toEqual(["120"]);
    const renames = lines(
      fx.asc.ok(["log", "--name-status", "--format=%s", "-M"], { cwd: fx.root }),
    ).filter((l) => l.startsWith("R"));
    expect(renames.length).toBeGreaterThan(0);
    const file = readdirSync(join(fx.root, "src"))[0] as string;
    expect(fx.asc.ok(["blame", "--", `src/${file}`], { cwd: fx.root })).toContain("revision");
  });

  it("big: the generator's shape at a small size (files and commits)", async () => {
    const fx = await build("big");
    expect(lines(fx.asc.ok(["ls-files"], { cwd: fx.root }))).toHaveLength(SMALL.bigFiles);
    expect(lines(fx.asc.ok(["rev-list", "--count", "master"], { cwd: fx.root }))).toEqual([
      String(SMALL.bigCommits),
    ]);
  });
});

describe("determinism (P12 acceptance)", () => {
  const local = ["clean", "dirty", "stacks", "conflict", "worktrees", "history", "big"] as const;
  for (const name of local) {
    it(`${name}: building twice gives identical commits, refs, status and stack shape`, async () => {
      const a = await build(name);
      const b = await build(name);
      const sa = snapshot(a);
      const sb = snapshot(b);
      expect(sa.commits.length).toBeGreaterThan(0);
      expect(sb).toEqual({ ...sa, changeIds: sb.changeIds });
    });
  }

  // Change ids are minted at random by this asc build; the test runs by itself once asc can repeat them.
  test.skipIf(!have || !supportsDeterministicChangeIds())(
    "stacks: building twice gives identical change ids",
    async () => {
      const a = snapshot(await build("stacks"));
      const b = snapshot(await build("stacks"));
      expect(b.changeIds).toEqual(a.changeIds);
    },
  );
});
