import { describe, expect, test } from "bun:test";
import type { StackChange, Stacks } from "../../state/model";
import {
  buildRows,
  changeActions,
  dropBlockedReason,
  predictionText,
  REASON_DIRTY,
  REASON_FROZEN,
  runCall,
  type ChangeRow,
  type SmartlogInput,
} from "./model";

const change = (id: string, parent?: string, extra: Partial<StackChange> = {}): StackChange => ({
  id,
  ...(parent === undefined ? {} : { parent }),
  subject: `s ${id}`,
  branches: [],
  state: "draft",
  needsRestack: false,
  frozen: false,
  empty: false,
  current: false,
  ...extra,
});

const stacks: Stacks = {
  version: 1,
  trunks: [{ name: "master", tip: "aa", remote: "origin" }],
  stacks: [
    {
      trunk: "master",
      base: "aa",
      behind: 0,
      changes: [change("a"), change("b", "a", { current: true }), change("c", "b", { frozen: true })],
    },
    { trunk: "master", base: "aa", behind: 0, changes: [change("x")] },
  ],
};

const input = (over: Partial<SmartlogInput> = {}): SmartlogInput => ({
  stacks,
  extras: {},
  uncommitted: [],
  hidden: new Set(),
  cloudDrafts: [],
  showCloud: false,
  showHidden: false,
  checked: new Set(),
  ...over,
});

const action = (row: ChangeRow, id: string) => row.actions.find((a) => a.id === id);

describe("rows", () => {
  test("tips first, trunk drop zones are separate", () => {
    const ids = buildRows(input()).map((r) => r.id);
    expect(ids).toEqual(["c", "b", "a", "x"]);
  });

  test("badges: PR, checks, restack, divergent, frozen, worktree", () => {
    const s: Stacks = {
      ...stacks,
      stacks: [
        {
          trunk: "master",
          base: "aa",
          behind: 0,
          changes: [
            change("a", undefined, {
              needsRestack: true,
              frozen: true,
              review: { host: "f", number: 7, url: "", state: "open", draft: true, approvals: 0, checks: "passing" },
            }),
          ],
        },
      ],
    };
    const row = buildRows(input({ stacks: s, extras: { a: { divergent: true, worktree: "wt1", version: 3 } } }))[0] as ChangeRow;
    expect(row.badges.map((b) => b.kind)).toEqual(["pr", "checks", "restack", "divergent", "frozen", "worktree", "version"]);
    expect(row.badges[0]?.text).toBe("#7 draft open");
  });

  test("uncommitted node appears only when dirty and gates create/modify on selection", () => {
    expect(buildRows(input())[0]?.kind).toBe("change");
    const rows = buildRows(input({ uncommitted: [{ path: "f.ts", status: "M" }] }));
    const u = rows[0];
    expect(u?.kind).toBe("uncommitted");
    if (u?.kind !== "uncommitted") throw new Error("x");
    expect(u.actions.every((a) => !a.enabled && a.reason === "Select at least one file")).toBe(true);
    const checked = buildRows(input({ uncommitted: [{ path: "f.ts", status: "M" }], checked: new Set(["f.ts"]) }))[0];
    if (checked?.kind !== "uncommitted") throw new Error("x");
    expect(checked.actions.every((a) => a.enabled)).toBe(true);
  });

  test("hidden and cloud rows follow the toggles", () => {
    const hidden = new Set(["a"]);
    expect(buildRows(input({ hidden })).map((r) => r.id)).not.toContain("a");
    expect(buildRows(input({ hidden, showHidden: true })).map((r) => r.id)).toContain("a");
    const cloud = [{ id: "z", subject: "z", machine: "laptop" }];
    expect(buildRows(input({ cloudDrafts: cloud })).some((r) => r.kind === "cloud")).toBe(false);
    expect(buildRows(input({ cloudDrafts: cloud, showCloud: true })).at(-1)?.kind).toBe("cloud");
  });

  test("a stack forking off another is indented", () => {
    const s: Stacks = { ...stacks, stacks: [{ trunk: "master", base: "aa", behind: 0, changes: [change("k", "a")] }] };
    expect((buildRows(input({ stacks: s }))[0] as ChangeRow).depth).toBe(1);
  });
});

describe("disabled reasons", () => {
  test("every disabled action states a reason, every enabled one none", () => {
    for (const c of [change("a"), change("b", undefined, { frozen: true }), change("c", undefined, { state: "landed", empty: true })]) {
      for (const dirty of [false, true]) {
        for (const a of changeActions(c, undefined, false, dirty)) {
          expect(a.enabled ? a.reason === undefined : (a.reason?.length ?? 0) > 0).toBe(true);
        }
      }
    }
  });

  test("dirty tree blocks checkout with the spec text; frozen blocks mutation", () => {
    const acts = changeActions(change("a"), undefined, false, true);
    expect(acts.find((a) => a.id === "checkout")?.reason).toBe(REASON_DIRTY);
    const f = changeActions(change("a", undefined, { frozen: true, current: true }), { version: 2 }, false, false);
    for (const id of ["restack", "uncommit", "unamend", "abandon"]) {
      expect(f.find((a) => a.id === id)?.reason).toBe(REASON_FROZEN);
    }
  });

  test("uncommit/unamend need the current change; unamend needs an earlier version", () => {
    const row = buildRows(input({ extras: { b: { version: 2 } } })).find((r) => r.id === "b") as ChangeRow;
    expect(action(row, "uncommit")?.enabled).toBe(true);
    expect(action(row, "unamend")?.enabled).toBe(true);
    const a = buildRows(input()).find((r) => r.id === "a") as ChangeRow;
    expect(action(a, "uncommit")?.reason).toBe("Check out this change first");
    const b = buildRows(input()).find((r) => r.id === "b") as ChangeRow;
    expect(action(b, "unamend")?.reason).toBe("No earlier version to restore");
  });

  test("hidden changes offer unhide instead of hide", () => {
    const acts = changeActions(change("a"), undefined, true, false);
    expect(acts.some((a) => a.id === "unhide")).toBe(true);
    expect(acts.some((a) => a.id === "hide")).toBe(false);
  });
});

describe("drag and runCall", () => {
  test("blocks onto self, descendants and frozen sources", () => {
    expect(dropBlockedReason(stacks, "a", { kind: "change", id: "b" })).toBeDefined();
    expect(dropBlockedReason(stacks, "c", { kind: "trunk", name: "master" })).toBe("A frozen change cannot be moved");
    expect(dropBlockedReason(stacks, "x", { kind: "change", id: "b" })).toBeUndefined();
    expect(dropBlockedReason(stacks, "x", { kind: "trunk", name: "master" })).toBeUndefined();
  });

  test("move dry run is `move --source --onto -n`; revs stay positional values, no shell", () => {
    expect(runCall({ verb: "move", change: "b", onto: { kind: "change", id: "x" }, dryRun: true })).toEqual({
      verb: "move",
      args: ["--source", "b", "--onto", "x", "-n"],
    });
    expect(runCall({ verb: "move", change: "b", onto: { kind: "trunk", name: "master" }, dryRun: false }).args).toEqual([
      "--source", "b", "--onto", "master",
    ]);
    expect(runCall({ verb: "modify", into: "b", files: ["p"] })).toEqual({ verb: "modify", args: ["--into", "b", "p"] });
    expect(runCall({ verb: "create", files: ["p", "q"] }).args).toEqual(["p", "q"]);
    expect(runCall({ verb: "hide", change: "a" })).toEqual({ verb: "hide", args: ["a"] });
  });

  test("prediction text", () => {
    expect(predictionText({ ok: true, conflicts: 0 })).toBe("no conflicts");
    expect(predictionText({ ok: true, conflicts: 2 })).toBe("2 predicted conflicts");
    expect(predictionText("pending")).toBe("checking conflicts");
  });
});
