// The deterministic scenarios of SPEC section 24.7.2. Each is built only with the asc CLI
// (`ASC_BIN`), through the fixed identity, the one-second-per-call clock and `asc.branchless=true`
// of `Asc`. Long linear histories go in through `asc am` (one process per series); everything else
// uses the verbs a person would type.
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { Asc } from "./asc";
import { buildMbox, type FileEdit, type PatchCommit } from "./mbox";

export const SCENARIOS = [
  "clean",
  "dirty",
  "stacks",
  "conflict",
  "worktrees",
  "history",
  "review",
  "big",
] as const;
export type ScenarioName = (typeof SCENARIOS)[number];
export type LocalScenario = Exclude<ScenarioName, "review">;

/** The sizes of SPEC section 24.7.2; tests pass smaller ones. */
export interface Sizes {
  readonly cleanCommits: number;
  readonly dirtyBigLines: number;
  readonly historyCommits: number;
  readonly bigFiles: number;
  readonly bigCommits: number;
}

export const DEFAULT_SIZES: Sizes = {
  cleanCommits: 200,
  dirtyBigLines: 10_000,
  historyCommits: 2_000,
  bigFiles: 100_000,
  bigCommits: 100_000,
};

export interface BuildOptions {
  /** The scenario is built in `<dir>/<name>`; an existing one is replaced. */
  readonly dir: string;
  readonly bin?: string | undefined;
  readonly sizes?: Partial<Sizes>;
  /** Commits per `asc am` call for the bulk scenarios. */
  readonly batch?: number;
}

export interface Fixture {
  readonly name: ScenarioName;
  /** The main workspace. */
  readonly root: string;
  readonly asc: Asc;
  /** Every workspace by name (`main` first). */
  readonly workspaces: Readonly<Record<string, string>>;
  /** Commits recorded while building, by scenario-specific name. */
  readonly marks: Readonly<Record<string, string>>;
  /** Cleanups, run in reverse by `dispose` (the review scenario adds the server). */
  readonly disposers: Array<() => Promise<void> | void>;
  dispose(): Promise<void>;
}

/** What a builder fills in. */
export interface Draft {
  workspaces: Record<string, string>;
  marks: Record<string, string>;
}

interface Env {
  readonly batch: number;
  readonly sizes: Sizes;
}

export function write(root: string, rel: string, content: string | Uint8Array): void {
  const p = join(root, rel);
  mkdirSync(dirname(p), { recursive: true });
  writeFileSync(p, content);
}

function init(asc: Asc, root: string, branchless = true): void {
  mkdirSync(root, { recursive: true });
  asc.ok(["init", "-q", "-b", "master", root]);
  // `asc init` writes an empty .ascignore; a scenario that does not track one starts without it, so
  // its work tree is clean.
  rmSync(join(root, ".ascignore"), { force: true });
  if (branchless) asc.ok(["config", "asc.branchless", "true"], { cwd: root });
}

/** Applies `commits` on the current branch through one `asc am` per `batch`. */
function applySeries(
  asc: Asc,
  root: string,
  tree: Map<string, string>,
  commits: readonly PatchCommit[],
  batch: number,
): void {
  const scratch = join(root, "..", "series");
  mkdirSync(scratch, { recursive: true });
  for (let i = 0; i < commits.length; i += batch) {
    const file = join(scratch, `series-${i}.mbox`);
    writeFileSync(file, buildMbox(commits.slice(i, i + batch), tree));
    asc.ok(["am", "-q", file], { cwd: root });
    rmSync(file);
  }
}

/** The 12-letter change id of a commit. */
export function changeId(asc: Asc, root: string, name: string): string {
  return asc.ok(["change-id", "--short", name], { cwd: root }).trim();
}

export function rev(asc: Asc, name: string, cwd?: string): string {
  return asc.ok(["rev-parse", name], cwd ? { cwd } : {}).trim();
}

// ---------------------------------------------------------------------------------------------

function seededRandom(seed: number): () => number {
  let s = seed >>> 0;
  return () => {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    return s / 0x1_0000_0000;
  };
}

function fileBody(path: string, version: number): string {
  let s = `// ${path}\n`;
  for (let i = 1; i <= Math.min(version, 12); i++) s += `revision ${i}\n`;
  return s;
}

/** A linear history that adds, edits and (with `renames`) renames files; fully set by `n`. */
export function linearHistory(n: number, opts: { files: number; renames: boolean }): PatchCommit[] {
  const rand = seededRandom(24_072);
  const names: string[] = [];
  const versions = new Map<string, number>();
  const out: PatchCommit[] = [];
  for (let i = 0; i < n; i++) {
    const roll = rand();
    let edit: FileEdit;
    if (names.length < opts.files || roll < 0.05) {
      const path = `src/m${String(names.length).padStart(4, "0")}.txt`;
      names.push(path);
      versions.set(path, 1);
      edit = { path, content: fileBody(path, 1) };
    } else if (opts.renames && roll < 0.12) {
      const k = Math.floor(rand() * names.length);
      const from = names[k] as string;
      const to = `src/r${String(i).padStart(5, "0")}.txt`;
      names[k] = to;
      versions.set(to, versions.get(from) ?? 1);
      versions.delete(from);
      edit = { path: to, renameFrom: from };
    } else {
      const path = names[Math.floor(rand() * names.length)] as string;
      const v = (versions.get(path) ?? 1) + 1;
      versions.set(path, v);
      edit = { path, content: fileBody(path, v) };
    }
    out.push({ subject: `change ${i + 1}`, at: i, edits: [edit] });
  }
  return out;
}

// ---------------------------------------------------------------------------------------------

function clean(asc: Asc, root: string, o: Env): void {
  init(asc, root, false);
  const tree = new Map<string, string>();
  applySeries(
    asc,
    root,
    tree,
    linearHistory(o.sizes.cleanCommits, { files: 40, renames: false }),
    o.batch,
  );
  asc.ok(["config", "asc.branchless", "true"], { cwd: root });
}

function history(asc: Asc, root: string, o: Env): void {
  init(asc, root, false);
  const tree = new Map<string, string>();
  applySeries(
    asc,
    root,
    tree,
    linearHistory(o.sizes.historyCommits, {
      files: Math.min(120, Math.max(5, Math.floor(o.sizes.historyCommits / 6))),
      renames: true,
    }),
    o.batch,
  );
  asc.ok(["config", "asc.branchless", "true"], { cwd: root });
}

function big(asc: Asc, root: string, o: Env): void {
  init(asc, root, false);
  const tree = new Map<string, string>();
  const commits = Math.max(o.sizes.bigCommits, 1);
  const perCommit = Math.max(Math.ceil(o.sizes.bigFiles / commits), 1);
  const series: PatchCommit[] = [];
  let made = 0;
  for (let i = 0; i < commits; i++) {
    const edits: FileEdit[] = [];
    for (let j = 0; j < perCommit && made < o.sizes.bigFiles; j++, made++) {
      const d = String(Math.floor(made / 1000)).padStart(3, "0");
      edits.push({
        path: `d${d}/f${String(made).padStart(6, "0")}.txt`,
        content: `file ${made}\n`,
      });
    }
    if (edits.length === 0) edits.push({ path: "d000/f000000.txt", content: `file 0 v${i}\n` });
    series.push({ subject: `big ${i + 1}`, at: i, edits });
  }
  applySeries(asc, root, tree, series, o.batch);
  asc.ok(["config", "asc.branchless", "true"], { cwd: root });
  // Virtual mode needs a host with a virtual file system provider (ProjFS or FUSE). A host
  // without one keeps the plain checkout, so the result is not an error here.
  asc.run(["config", "asc.mode", "virtual"], { cwd: root });
}

function binary(seed: number, len: number): Uint8Array {
  return Uint8Array.from({ length: len }, (_, i) => (i * 31 + seed) & 0xff);
}

function dirty(asc: Asc, root: string, o: Env, d: Draft): void {
  init(asc, root);
  const bigText = Array.from({ length: o.sizes.dirtyBigLines }, (_, i) => `line ${i + 1}\n`).join(
    "",
  );
  write(root, ".gitignore", "*.log\nbuild/\n");
  write(root, ".ascignore", "*.log\nbuild/\n*.tmp\n");
  write(root, "src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n");
  write(root, "docs/old_name.txt", "to be renamed\nsecond line\nthird line\n");
  write(root, "script.sh", "#!/bin/sh\necho hi\n");
  write(root, "image.bin", binary(1, 2048));
  write(root, "big.txt", bigText);
  asc.ok(["add", "."], { cwd: root });
  asc.ok(["commit", "-q", "-m", "base"], { cwd: root });
  d.marks.base = rev(asc, "HEAD", root);

  write(root, "src/lib.rs", "pub fn one() -> u32 {\n    2\n}\n"); // unstaged
  write(root, "image.bin", binary(2, 2048)); // unstaged binary
  write(root, "big.txt", bigText.replace("line 5\n", "line 5 edited\n")); // unstaged, in the big file
  write(root, "staged_new.txt", "staged\n");
  asc.ok(["add", "staged_new.txt"], { cwd: root });
  asc.ok(["mv", "docs/old_name.txt", "docs/new_name.txt"], { cwd: root }); // staged rename
  const blob = rev(asc, "HEAD:script.sh", root);
  asc.ok(["update-index", "--cacheinfo", `100755,${blob},script.sh`], { cwd: root }); // staged mode change
  write(root, "untracked.txt", "untracked\n");
  write(root, "new.bin", binary(3, 512)); // untracked binary
  write(root, "debug.log", "ignored\n");
  write(root, "build/out.o", "ignored\n");
  write(root, "scratch.tmp", "ignored by .ascignore\n");
}

/** Creates a change holding one file on the current HEAD and returns its commit. */
function change(asc: Asc, root: string, file: string, body: string, subject: string): string {
  write(root, file, body);
  asc.ok(["add", file], { cwd: root });
  asc.ok(["create", "-m", `feat: ${subject}`], { cwd: root });
  return rev(asc, "HEAD", root);
}

function trunk(asc: Asc, root: string, batch: number): void {
  init(asc, root, false);
  applySeries(
    asc,
    root,
    new Map(),
    [
      {
        subject: "base: f and g",
        at: 0,
        edits: [
          { path: "f", content: "a\nb\nc\n" },
          { path: "g", content: "g1\ng2\n" },
        ],
      },
      { subject: "base: readme", at: 1, edits: [{ path: "README.md", content: "# fixture\n" }] },
    ],
    batch,
  );
  asc.ok(["config", "asc.branchless", "true"], { cwd: root });
}

function stacks(asc: Asc, root: string, o: Env, d: Draft): void {
  trunk(asc, root, o.batch);

  // Stack A: three changes. Its first change is amended last, which leaves the others to restack.
  asc.ok(["switch", "-q", "-c", "stack-a"], { cwd: root });
  const a = [1, 2, 3].map((n) => change(asc, root, `a${n}.txt`, `a${n}\n`, `a${n}`));
  a.forEach((sha, i) => (d.marks[`a${i + 1}`] = sha));
  asc.ok(["branch", "label-a2", a[1] as string], { cwd: root }); // the branch label on a middle change

  // Stack B: five changes; b2 frozen, b4 divergent.
  asc.ok(["switch", "-q", "master"], { cwd: root });
  asc.ok(["switch", "-q", "-c", "stack-b"], { cwd: root });
  const b = [1, 2, 3, 4, 5].map((n) => change(asc, root, `b${n}.txt`, `b${n}\n`, `b${n}`));
  b.forEach((sha, i) => (d.marks[`b${i + 1}`] = sha));
  asc.ok(["stack", "freeze", changeId(asc, root, b[1] as string)], { cwd: root });

  // b4 gets two live successors: two amends of the same commit, each kept by a branch.
  const b4 = b[3] as string;
  asc.ok(["switch", "-q", "--detach", b4], { cwd: root });
  asc.ok(["commit", "-q", "--amend", "-m", "feat: b4 first rewrite"], { cwd: root });
  asc.ok(["branch", "b4-first"], { cwd: root });
  asc.ok(["switch", "-q", "--detach", b4], { cwd: root });
  asc.ok(["commit", "-q", "--amend", "-m", "feat: b4 second rewrite"], { cwd: root });
  asc.ok(["branch", "b4-second"], { cwd: root });

  asc.ok(["switch", "-q", "--detach", a[0] as string], { cwd: root });
  asc.ok(["commit", "-q", "--amend", "-m", "feat: a1 reworded"], { cwd: root });
  asc.ok(["branch", "a1-reworded"], { cwd: root }); // keeps the rewrite alive; a2 and a3 need a restack

  // Two hidden drafts.
  for (const n of [1, 2]) {
    asc.ok(["switch", "-q", "--detach", "master"], { cwd: root });
    const sha = change(asc, root, `scratch${n}.txt`, `scratch ${n}\n`, `scratch ${n}`);
    d.marks[`hidden${n}`] = sha;
    asc.ok(["hide", changeId(asc, root, sha)], { cwd: root });
  }
  asc.ok(["switch", "-q", "stack-a"], { cwd: root });
}

function conflict(asc: Asc, root: string, o: Env, d: Draft): void {
  trunk(asc, root, o.batch);
  asc.ok(["switch", "-q", "-c", "feature"], { cwd: root });
  const c1 = change(asc, root, "one.txt", "one\n", "one");
  write(root, "f", "a\nb-two\nc\n");
  asc.ok(["rm", "-q", "g"], { cwd: root });
  asc.ok(["add", "f"], { cwd: root });
  asc.ok(["create", "-m", "feat: two edits f and removes g"], { cwd: root });
  const c2 = rev(asc, "HEAD", root);
  change(asc, root, "three.txt", "three\n", "three");
  d.marks.c1 = c1;
  d.marks.c2 = c2;

  // Rewrite change one so it touches the same line of f and edits g: restacking change two onto
  // it conflicts in both files (content, and modify/delete).
  asc.ok(["switch", "-q", "--detach", c1], { cwd: root });
  write(root, "f", "a\nb-one\nc\n");
  write(root, "g", "g1\ng2 changed\n");
  asc.ok(["add", "f", "g"], { cwd: root });
  asc.ok(["commit", "-q", "--amend", "-m", "feat: one rewritten"], { cwd: root });
  asc.ok(["branch", "one-rewritten"], { cwd: root });
  const r = asc.run(["restack", "--upstack", changeId(asc, root, c2)], { cwd: root });
  if (r.code === 0) throw new Error("conflict fixture: the restack did not pause");
}

function worktrees(asc: Asc, root: string, o: Env, d: Draft): void {
  trunk(asc, root, o.batch);
  asc.ok(["switch", "-q", "-c", "main-work"], { cwd: root });
  change(asc, root, "m1.txt", "m1\n", "m1");
  change(asc, root, "m2.txt", "m2\n", "m2");
  const sibling = resolve(root, "..", "wt-feature");
  const ephemeral = resolve(root, "..", "wt-ephemeral");
  const nested = join(root, ".claude", "worktrees", "agent-one");
  asc.ok(["worktree", "add", "--mode", "plain", "-b", "feature", sibling, "master"], { cwd: root });
  asc.ok(
    [
      "worktree",
      "add",
      "--mode",
      "plain",
      "--ephemeral",
      "--ttl",
      "7d",
      "-b",
      "agent/tmp",
      ephemeral,
      "master",
    ],
    { cwd: root },
  );
  asc.ok(["worktree", "add", "--mode", "plain", "-b", "agent/nested", nested, "master"], {
    cwd: root,
  });
  d.workspaces.feature = sibling;
  d.workspaces.ephemeral = ephemeral;
  d.workspaces.nested = nested;
}

const BUILDERS: Record<LocalScenario, (asc: Asc, root: string, o: Env, d: Draft) => void> = {
  clean,
  dirty,
  stacks,
  conflict,
  worktrees,
  history,
  big,
};

/** The fixture shell around a built workspace. */
export function makeFixture(name: ScenarioName, root: string, asc: Asc, draft: Draft): Fixture {
  return {
    name,
    root,
    asc,
    workspaces: draft.workspaces,
    marks: draft.marks,
    disposers: [],
    async dispose() {
      for (const f of this.disposers.splice(0).toReversed()) await f();
    },
  };
}

/** Builds a scenario that needs no server. `review` goes through `buildFixture`. */
export function buildLocal(name: LocalScenario, opts: BuildOptions): Fixture {
  const base = resolve(opts.dir, name);
  rmSync(base, { recursive: true, force: true });
  const root = join(base, "repo");
  mkdirSync(root, { recursive: true });
  const asc = new Asc({
    ...(opts.bin ? { bin: opts.bin } : {}),
    home: join(base, "home"),
    cwd: root,
  });
  const draft: Draft = { workspaces: { main: root }, marks: {} };
  BUILDERS[name](
    asc,
    root,
    { batch: opts.batch ?? 250, sizes: { ...DEFAULT_SIZES, ...opts.sizes } },
    draft,
  );
  return makeFixture(name, root, asc, draft);
}

/** The stacks scenario, for the review scenario to extend. */
export function buildStacksInto(opts: BuildOptions, name: ScenarioName): Fixture {
  const base = resolve(opts.dir, name);
  rmSync(base, { recursive: true, force: true });
  const root = join(base, "repo");
  mkdirSync(root, { recursive: true });
  const asc = new Asc({
    ...(opts.bin ? { bin: opts.bin } : {}),
    home: join(base, "home"),
    cwd: root,
  });
  const draft: Draft = { workspaces: { main: root }, marks: {} };
  stacks(
    asc,
    root,
    { batch: opts.batch ?? 250, sizes: { ...DEFAULT_SIZES, ...opts.sizes } },
    draft,
  );
  return makeFixture(name, root, asc, draft);
}
