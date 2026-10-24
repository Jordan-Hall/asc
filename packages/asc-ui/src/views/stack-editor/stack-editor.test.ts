import { describe, expect, test } from "bun:test";
import type { Stack } from "../../state/model";
import {
  actionBlockedReason,
  commitBlockedReason,
  initEditor,
  moveBlockedReason,
  moveItem,
  reorderArgv,
  setAction,
  setMessage,
  toTodo,
} from "./model";

const ch = (id: string, frozen = false) => ({
  id,
  subject: `subject ${id}`,
  branches: [],
  state: "draft",
  needsRestack: false,
  frozen,
  empty: false,
  current: false,
});
const stack: Stack = {
  trunk: "master",
  base: "aa",
  behind: 0,
  changes: [ch("aaaa"), ch("bbbb"), ch("cccc", true), ch("dddd")],
};

describe("stack editor model", () => {
  test("an untouched editor has nothing to commit", () => {
    expect(commitBlockedReason(initEditor(stack))).toBe("No edits");
  });

  test("reorder keeps frozen changes in place and yields a pick todo, bottom to top", () => {
    let s = initEditor(stack);
    expect(moveItem(s, 2, 0)).toBe(s);
    expect(moveBlockedReason(s, 1, 1)).toBe("Frozen");
    s = moveItem(s, 3, 0);
    expect(toTodo(s).items.map((i) => `${i.action}:${i.change}`)).toEqual([
      "pick:dddd",
      "pick:aaaa",
      "pick:bbbb",
      "pick:cccc",
    ]);
  });

  test("fold, drop and reword in one todo", () => {
    let s = initEditor(stack);
    s = setAction(s, "bbbb", "fold");
    s = setAction(s, "dddd", "drop");
    s = setAction(s, "aaaa", "reword");
    expect(commitBlockedReason(s)).toBeUndefined();
    s = setMessage(s, "aaaa", "new words");
    expect(toTodo(s).items).toEqual([
      { action: "reword", change: "aaaa", message: "new words" },
      { action: "fold", change: "bbbb" },
      { action: "pick", change: "cccc" },
      { action: "drop", change: "dddd" },
    ]);
  });

  test("fold carries a message only when edited", () => {
    let s = setAction(initEditor(stack), "bbbb", "fold");
    s = setMessage(s, "bbbb", "combined");
    expect(toTodo(s).items[1]).toEqual({ action: "fold", change: "bbbb", message: "combined" });
  });

  test("the first kept change cannot fold; a fold falls back to pick when moved to the bottom", () => {
    let s = initEditor(stack);
    expect(actionBlockedReason(s, "aaaa", "fold")).toBe("Nothing below to fold into");
    s = setAction(s, "bbbb", "fold");
    s = moveItem(s, 1, 0);
    expect(s.items[0]?.action).toBe("pick");
    s = setAction(initEditor(stack), "aaaa", "drop");
    expect(actionBlockedReason(s, "bbbb", "fold")).toBe("Nothing below to fold into");
  });

  test("frozen changes cannot be dropped, folded or reworded", () => {
    const s = initEditor(stack);
    for (const a of ["drop", "fold", "reword"] as const) {
      expect(actionBlockedReason(s, "cccc", a)).toBe("Frozen");
    }
  });

  test("blocked commits state a reason", () => {
    let s = initEditor(stack);
    s = setAction(s, "aaaa", "reword");
    s = setMessage(s, "aaaa", "  ");
    expect(commitBlockedReason(s)).toBe("Reword of aaaa needs a message");
    s = setMessage(s, "aaaa", "a\0b");
    expect(commitBlockedReason(s)).toBe("The message of aaaa has a NUL byte");
    const one: Stack = { ...stack, changes: [ch("aaaa")] };
    expect(commitBlockedReason(setAction(initEditor(one), "aaaa", "drop"))).toBe(
      "Dropping every change: use abandon",
    );
    expect(() => toTodo(initEditor(stack))).toThrow("No edits");
  });

  test("the todo goes by file, never argv", () => {
    expect(reorderArgv("/tmp/t.json")).toEqual(["reorder", "--end-of-options", "--todo", "/tmp/t.json"]);
  });
});
