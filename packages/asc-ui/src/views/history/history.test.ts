import { expect, test } from "bun:test";
import { actionState, type WorktreeRow } from "../worktrees/model";
import { layoutHistory, MAX_SIZE, MIN_SIZE, sizeFor } from "./model";

const e = (
  id: string,
  author: string,
  stats?: { path: string; added: number; removed: number }[],
) => ({
  id,
  subject: id,
  author,
  time: 0,
  ...(stats ? { stats: stats.map((s) => ({ ...s, binary: false })) } : {}),
});

test("size scales by square root of churn within bounds", () => {
  expect(sizeFor(0, 100)).toBe(MIN_SIZE);
  expect(sizeFor(100, 100)).toBe(MAX_SIZE);
  expect(sizeFor(25, 100)).toBe(Math.round(MIN_SIZE + 0.5 * (MAX_SIZE - MIN_SIZE)));
});

test("lanes are per author in first-seen order; renames are detected", () => {
  const l = layoutHistory(
    [
      e("3", "bob", [{ path: "new.ts", added: 4, removed: 0 }]),
      e("2", "amy", [{ path: "new.ts", added: 1, removed: 1 }]),
      e("1", "bob", [{ path: "old.ts", added: 9, removed: 0 }]),
    ],
    "new.ts",
  );
  expect(l.lanes).toEqual(["bob", "amy"]);
  expect(l.nodes.map((n) => n.lane)).toEqual([0, 1, 0]);
  expect(l.nodes[1]?.renamedFrom).toBe("old.ts");
  expect(l.nodes[2]?.path).toBe("old.ts");
  expect(l.nodes[2]?.size).toBe(MAX_SIZE);
});

test("without stats sizes are uniform", () => {
  const l = layoutHistory([e("1", "a"), e("2", "a")], "f");
  expect(l.hasStats).toBe(false);
  expect(new Set(l.nodes.map((n) => n.size)).size).toBe(1);
});

const wt = (o: Partial<WorktreeRow>): WorktreeRow => ({
  wsId: "w",
  path: "/p",
  mode: "",
  headRef: "main",
  dirty: 0,
  staged: 0,
  ahead: 0,
  behind: 0,
  locked: "",
  inProgress: "",
  current: false,
  main: false,
  ...o,
});

test("remove states its reason", () => {
  expect(actionState(wt({ main: true }), "remove").reason).toBe(
    "The main worktree cannot be removed",
  );
  expect(actionState(wt({ dirty: 2 }), "remove").reason).toBe("Uncommitted changes");
  expect(actionState(wt({ locked: "ci" }), "remove").reason).toBe("Locked: ci");
  expect(actionState(wt({}), "remove").enabled).toBe(true);
});
