// What "the same build" means: every commit id, every ref, the work tree status and the shape of
// the stacks. Change ids are random in this asc build (`mint_change_id`), so the snapshot holds
// them twice: raw (`changeIds`, compared only where `supportsDeterministicChangeIds` says asc can
// repeat them) and as ordinals in first-seen order (`stacks`).
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Asc } from "./asc";
import type { Fixture } from "./scenarios";

interface SlChange {
  change: string;
  commit: string;
  subject: string;
  state: string;
  needs_restack: boolean;
  frozen: boolean;
  parent_change: string | null;
  branches: string[];
}

export interface Snapshot {
  /** `refname objectname` for every ref, sorted. */
  readonly refs: readonly string[];
  /** Every reachable commit, oldest first. */
  readonly commits: readonly string[];
  readonly status: readonly string[];
  /** Stacks with change ids replaced by `c1`, `c2`, ... in order of first appearance. */
  readonly stacks: readonly (readonly {
    id: string;
    parent: string | null;
    subject: string;
    state: string;
    needsRestack: boolean;
    frozen: boolean;
    branches: readonly string[];
    commit: string;
  }[])[];
  /** Raw change ids in the same order as the ordinals. */
  readonly changeIds: readonly string[];
}

const lines = (out: string) => out.split("\n").filter((l) => l !== "");

export function snapshot(fx: Fixture): Snapshot {
  const { asc, root } = fx;
  const refs = lines(
    asc.ok(["for-each-ref", "--format=%(refname) %(objectname)"], { cwd: root }),
  ).toSorted();
  const commits = lines(asc.ok(["rev-list", "--all", "--reverse"], { cwd: root }));
  const status = lines(asc.ok(["status", "--porcelain=v1", "--ignored"], { cwd: root }));
  const sl = asc.json<{ stacks: { changes: SlChange[] }[] }>(["sl", "--json"], { cwd: root });
  const ordinal = new Map<string, string>();
  const name = (id: string | null): string | null => {
    if (id === null) return null;
    let o = ordinal.get(id);
    if (o === undefined) {
      o = `c${ordinal.size + 1}`;
      ordinal.set(id, o);
    }
    return o;
  };
  const stacks = sl.stacks.map((s) =>
    s.changes.map((c) => ({
      id: name(c.change) as string,
      parent: name(c.parent_change),
      subject: c.subject,
      state: c.state,
      needsRestack: c.needs_restack,
      frozen: c.frozen,
      branches: c.branches.toSorted(),
      commit: c.commit,
    })),
  );
  return { refs, commits, status, stacks, changeIds: [...ordinal.keys()] };
}

/**
 * Whether this asc build mints the same change ids for the same history. Today it does not: ids
 * come from `mint_change_id` (random v4). Known gap (C09): a seeded mint (for example an env var that
 * `Recorder::new` reads) makes this true, and the strict determinism test then runs by itself.
 */
export function supportsDeterministicChangeIds(bin?: string): boolean {
  const base = mkdtempSync(join(tmpdir(), "asc-fixtures-probe-"));
  try {
    const ids: string[] = [];
    for (const n of [1, 2]) {
      const root = join(base, `r${n}`);
      mkdirSync(root, { recursive: true });
      const asc = new Asc({ ...(bin ? { bin } : {}), home: join(base, `h${n}`), cwd: root });
      asc.ok(["init", "-q", "-b", "master", root]);
      asc.ok(["config", "asc.branchless", "true"], { cwd: root });
      writeFileSync(join(root, "a"), "a\n");
      asc.ok(["add", "a"], { cwd: root });
      asc.ok(["commit", "-q", "-m", "base"], { cwd: root });
      writeFileSync(join(root, "b"), "b\n");
      asc.ok(["add", "b"], { cwd: root });
      asc.ok(["create", "-m", "feat: probe"], { cwd: root });
      ids.push(asc.ok(["change-id", "HEAD"], { cwd: root }).trim());
    }
    return ids[0] === ids[1] && ids[0] !== "";
  } catch {
    return false;
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
}
