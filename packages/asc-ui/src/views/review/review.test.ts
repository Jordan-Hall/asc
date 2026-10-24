import { describe, expect, test } from "bun:test";
import type { StackChange, Stacks } from "../../state/model";
import {
  approveState,
  buildNavigator,
  defaultPick,
  groupComments,
  mergeState,
  pickError,
  REASON_NO_LAST_REVIEW,
  REASON_UP_TO_DATE,
  sinceLastReview,
  type ChangeReviewData,
} from "./model";

const review = (n: number, extra: Record<string, unknown> = {}) => ({
  host: "f",
  number: n,
  url: "",
  state: "open",
  draft: false,
  approvals: 1,
  checks: "passing",
  ...extra,
});
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
      changes: [
        change("a", undefined, { review: review(1) }),
        change("b", "a", { review: review(2) }),
        change("c", "b", { review: review(3, { approvals: 0 }) }),
      ],
    },
  ],
};
const v = (n: number) => ({ v: n, at: n, actor: "u" });

describe("versions", () => {
  test("default pick is previous against latest", () => {
    expect(defaultPick([v(1), v(2), v(3)])).toEqual({ from: 2, to: 3 });
    expect(defaultPick([v(1)])).toEqual({ from: 1, to: 1 });
  });
  test("since my last review", () => {
    const d = (last?: number): ChangeReviewData => ({
      versions: [v(1), v(2), v(3)],
      ...(last === undefined ? {} : { lastReviewedV: last }),
    });
    expect(sinceLastReview(d())).toEqual({ reason: REASON_NO_LAST_REVIEW });
    expect(sinceLastReview(d(3))).toEqual({ reason: REASON_UP_TO_DATE });
    expect(sinceLastReview(d(1))).toEqual({ pick: { from: 1, to: 3 } });
  });
  test("pick validation", () => {
    expect(pickError([v(1), v(2)], { from: 2, to: 1 })).toBeDefined();
    expect(pickError([v(1), v(2)], { from: 1, to: 9 })).toBeDefined();
    expect(pickError([v(1), v(2)], { from: 1, to: 2 })).toBeUndefined();
  });
});

describe("actions", () => {
  test("merge is bottom-up and states its reason", () => {
    expect(mergeState(stacks, "a", {}).enabled).toBe(true);
    expect(mergeState(stacks, "b", {}).reason).toBe("Merge #1 first (stack order)");
    expect(mergeState(stacks, "b", { a: { versions: [], merged: true } }).enabled).toBe(true);
    expect(mergeState(stacks, "c", { a: { versions: [], merged: true }, b: { versions: [], merged: true } }).reason).toBe(
      "Needs an approval",
    );
  });
  test("conflicts and restack block merge", () => {
    expect(mergeState(stacks, "a", { a: { versions: [], mergeability: "conflicts" } }).reason).toBe(
      "Conflicts with the target",
    );
    expect(mergeState(stacks, "a", { a: { versions: [], mergeability: "needs_restack" } }).reason).toBe(
      "Needs restack first",
    );
  });
  test("approve refuses own, draft and closed", () => {
    const a = change("a", undefined, { review: review(1) });
    expect(approveState(a, { versions: [], own: true }).reason).toBe("You cannot approve your own change");
    expect(approveState(change("a", undefined, { review: review(1, { draft: true }) }), undefined).enabled).toBe(false);
    expect(approveState(change("a", undefined, { review: review(1, { state: "closed" }) }), undefined).enabled).toBe(false);
    expect(approveState(change("a"), undefined).reason).toBe("No pull request for this change");
    expect(approveState(a, undefined).enabled).toBe(true);
  });
});

describe("navigator and comments", () => {
  test("badges carry mergeability, overlap and open comments", () => {
    const rows = buildNavigator({
      stacks,
      selected: "b",
      changes: {
        b: { versions: [v(1)], mergeability: "conflicts", conflictPaths: ["x.ts"], overlaps: [{ change: "z", pr: 9, paths: ["y.ts"] }] },
      },
      comments: { b: [{ id: "1", path: "x.ts", author: "u", body: "hm", resolved: false }, { id: "2", path: "x.ts", author: "u", body: "ok", resolved: true }] },
    });
    expect(rows.map((r) => r.id)).toEqual(["a", "b", "c"]);
    const b = rows[1];
    expect(b?.selected).toBe(true);
    expect(b?.badges.map((x) => x.text)).toEqual(["#2 open", "conflicts", "overlaps 1", "1 open"]);
    expect(b?.badges[1]?.title).toBe("x.ts");
  });
  test("comments are grouped by path then line", () => {
    const g = groupComments([
      { id: "1", path: "b", line: 9, author: "u", body: "", resolved: false },
      { id: "2", path: "a", line: 5, author: "u", body: "", resolved: false },
      { id: "3", path: "b", line: 2, author: "u", body: "", resolved: false },
    ]);
    expect(g.map((x) => [x.path, x.comments.map((c) => c.id)])).toEqual([["a", ["2"]], ["b", ["3", "1"]]]);
  });
});
