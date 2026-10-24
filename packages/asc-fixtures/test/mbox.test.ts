import { expect, test } from "bun:test";
import { buildMbox, linearHistory } from "../src/index";

test("a series is the same bytes every time", () => {
  const a = buildMbox(linearHistory(30, { files: 5, renames: true }), new Map());
  const b = buildMbox(linearHistory(30, { files: 5, renames: true }), new Map());
  expect(a).toBe(b);
  expect(a.match(/^From: /gm)?.length).toBe(30);
});

test("history covers adds, edits and renames", () => {
  const h = linearHistory(300, { files: 10, renames: true });
  const kinds = new Set(
    h.flatMap((c) => c.edits.map((e) => ("renameFrom" in e ? "rename" : "content"))),
  );
  expect(kinds).toEqual(new Set(["rename", "content"]));
  expect(
    linearHistory(50, { files: 10, renames: false }).some((c) =>
      c.edits.some((e) => "renameFrom" in e),
    ),
  ).toBe(false);
});

test("an edit of a file the tree lacks is refused", () => {
  expect(() =>
    buildMbox([{ subject: "x", at: 0, edits: [{ path: "a", remove: true }] }], new Map()),
  ).toThrow();
  expect(() =>
    buildMbox([{ subject: "x", at: 0, edits: [{ path: "b", renameFrom: "a" }] }], new Map()),
  ).toThrow();
});

test("patches carry a fixed date and the fixture identity", () => {
  const m = buildMbox([{ subject: "s", at: 5, edits: [{ path: "a", content: "x\n" }] }], new Map());
  expect(m).toContain("Date: Mon, 01 Jan 2024 00:00:05 +0000");
  expect(m).toContain("From: Fixture Author <fixture@example.invalid>");
});
