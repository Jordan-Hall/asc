import { expect, test } from "bun:test";
import { compareRequest, comparePairs, summary } from "../compare/model";
import { diffPairs, safeUrl, splitMessage, statusOf, totals } from "./model";

test("message splits into subject and trimmed body", () => {
  expect(splitMessage("a\r\n\r\nb\nc\n\n")).toEqual({ subject: "a", body: "b\nc" });
  expect(splitMessage("only")).toEqual({ subject: "only", body: "" });
});

test("status letters map, rename scores keep their letter", () => {
  expect(statusOf("R100")).toBe("renamed");
  expect(statusOf("M")).toBe("modified");
  expect(statusOf("X")).toBe("unknown");
});

test("totals omit counts when none are known", () => {
  expect(totals([{ path: "a", status: "added" }]).known).toBe(false);
  expect(totals([{ path: "a", status: "added", added: 2, removed: 1 }])).toEqual({
    files: 1,
    added: 2,
    removed: 1,
    known: true,
  });
});

test("diff pairs skip binaries, use the old path of renames and an empty left for a root", () => {
  const pairs = diffPairs({
    id: "c",
    parents: [],
    files: [
      { path: "b", oldPath: "a", status: "renamed" },
      { path: "img", status: "added", binary: true },
    ],
  });
  expect(pairs).toEqual([{ path: "b", oldPath: "a", leftRev: "", rightRev: "c" }]);
});

test("only https URLs are links", () => {
  expect(safeUrl("https://x.example/p")).toBe("https://x.example/p");
  expect(safeUrl("http://x.example/p")).toBeUndefined();
  expect(safeUrl("javascript:alert(1)")).toBeUndefined();
});

test("compare request resolves heads and picks the dot form", () => {
  const rev = { kind: "revision", value: "main" } as const;
  const stack = { kind: "stack", value: "top1" } as const;
  expect(compareRequest(rev, { kind: "revision", value: "x" }, {})).toEqual({ a: "main", b: "x", threeDot: false });
  expect(compareRequest(rev, stack, { top1: "deadbeef" })).toEqual({ a: "main", b: "deadbeef", threeDot: true });
  expect(compareRequest(rev, rev, {}, true).threeDot).toBe(true);
});

test("compare summary and pairs", () => {
  expect(summary({ ahead: 0, behind: 0 })).toBe("No differences in history");
  expect(summary({ ahead: 2, behind: 1 })).toBe("2 ahead, 1 behind");
  const req = { a: "m", b: "t", threeDot: true };
  expect(comparePairs({ mergeBase: "mb", files: [{ path: "f", status: "modified" }] }, req)[0]?.leftRev).toBe("mb");
  expect(comparePairs({ mergeBase: "mb", files: [{ path: "f", status: "modified" }] }, { ...req, threeDot: false })[0]?.leftRev).toBe("m");
});
