import { describe, expect, test } from "bun:test";
import {
  addPiece,
  assignFile,
  assignHunk,
  assignLine,
  buildPatch,
  commitBlockedReason,
  initSplit,
  parseDiff,
  pieceCounts,
  removePiece,
  safePath,
  setPieceMessage,
  splitRun,
  toSpec,
} from "./model";

/** Strict applier for the patches this model builds (context must match at the stated line). */
function apply(base: string[], patch: string): string[] {
  const out = [...base];
  let shift = 0;
  const lines = patch.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  for (let i = 0; i < lines.length; ) {
    const m = /^@@ -(\d+),(\d+) \+(\d+),(\d+) @@$/.exec(lines[i] ?? "");
    if (!m) {
      i++;
      continue;
    }
    const oldLen = Number(m[2]);
    let at = Number(m[1]) + shift - (oldLen === 0 ? 0 : 1);
    const start = at;
    i++;
    const removed: string[] = [];
    const added: string[] = [];
    let n = 0;
    while (i < lines.length && /^[ +-]/.test(lines[i] ?? "")) {
      const l = lines[i] ?? "";
      const t = l.slice(1);
      if (l[0] !== "+") {
        expect(out[at]).toBe(t);
        at++;
        n++;
        removed.push(t);
      }
      if (l[0] !== "-") added.push(t);
      i++;
    }
    expect(n).toBe(oldLen);
    out.splice(start, n, ...added);
    shift += added.length - removed.length;
  }
  return out;
}

const OLD = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"];
const NEW = ["a", "B", "c", "d", "e", "f", "g", "h", "i", "j", "K", "k2", "l"];
const diff = [
  "diff --git a/f.txt b/f.txt",
  "index 1111111..2222222 100644",
  "--- a/f.txt",
  "+++ b/f.txt",
  "@@ -1,4 +1,4 @@",
  " a",
  "-b",
  "+B",
  " c",
  " d",
  "@@ -9,4 +9,5 @@",
  " i",
  " j",
  "-k",
  "+K",
  "+k2",
  " l",
  "",
].join("\n");

describe("parseDiff", () => {
  test("a plain modified file is granular", () => {
    const [f] = parseDiff(diff);
    expect(f?.granular).toBe(true);
    expect(f?.path).toBe("f.txt");
    expect(f?.hunks).toHaveLength(2);
  });

  test("binary, rename, new and mode changes move whole", () => {
    const text = [
      "diff --git a/img.png b/img.png",
      "index 1..2 100644",
      "Binary files a/img.png and b/img.png differ",
      "diff --git a/o.txt b/n.txt",
      "similarity index 90%",
      "rename from o.txt",
      "rename to n.txt",
      "diff --git a/new.txt b/new.txt",
      "new file mode 100644",
      "index 0000000..1111111",
      "--- /dev/null",
      "+++ b/new.txt",
      "@@ -0,0 +1 @@",
      "+x",
      "",
    ].join("\n");
    const fs = parseDiff(text);
    expect(fs.map((f) => [f.path, f.granular, f.unsupported])).toEqual([
      ["img.png", false, undefined],
      ["n.txt", false, undefined],
      ["new.txt", false, undefined],
    ]);
  });

  test("unsafe and quoted paths are unsupported and block the split", () => {
    const quoted =
      'diff --git "a/we ird" "b/we ird"\n--- "a/we ird"\n+++ "b/we ird"\n@@ -1 +1 @@\n-a\n+b\n';
    const dots =
      "diff --git a/../x b/../x\nindex 1..2 100644\n--- a/../x\n+++ b/../x\n@@ -1 +1 @@\n-a\n+b\n";
    for (const t of [quoted, dots]) {
      const s = initSplit(t, false, "m");
      expect(s.files[0]?.unsupported).toBe("Unsupported path");
      expect(commitBlockedReason(s)).toContain("Unsupported path");
      expect(() => toSpec(s)).toThrow();
    }
  });

  test("a hunk whose counts disagree is unsupported", () => {
    const t = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,3 +1 @@\n-a\n+b\n";
    expect(parseDiff(t)[0]?.unsupported).toBe("Unparsable diff");
  });

  test("safePath refuses traversal, absolute, quote and control characters", () => {
    for (const p of ["../a", "a/../b", "/a", "a/", "a//b", 'a"b', "a\\b", "a\nb", ""])
      expect(safePath(p)).toBe(false);
    expect(safePath("src/a b.ts")).toBe(true);
  });
});

describe("split model", () => {
  test("a truncated diff blocks the split", () => {
    const s = initSplit(diff, true, "m");
    expect(commitBlockedReason(s)).toBe("The diff is truncated");
    expect(() => toSpec(s)).toThrow("truncated");
  });

  test("every piece needs changes and the lower ones a message", () => {
    let s = initSplit(diff, false, "orig");
    expect(commitBlockedReason(s)).toBe("Piece 1 has no changes");
    s = assignHunk(s, 0, 0, 0);
    expect(commitBlockedReason(s)).toBe("Piece 1 needs a message");
    s = setPieceMessage(s, 0, "first");
    expect(commitBlockedReason(s)).toBeUndefined();
    expect(pieceCounts(s)).toEqual([2, 3]);
    s = assignHunk(s, 0, 1, 0);
    expect(commitBlockedReason(s)).toBe("Piece 2 has no changes");
  });

  test("hunk-level pieces apply in order and the remainder is the rest", () => {
    let s = initSplit(diff, false, "orig");
    s = setPieceMessage(assignHunk(s, 0, 0, 0), 0, "first");
    const p0 = buildPatch(s, 0);
    expect(p0).toContain("@@ -1,4 +1,4 @@");
    expect(p0).not.toContain("k2");
    const after0 = apply(OLD, p0);
    expect(after0).toEqual(["a", "B", ...OLD.slice(2)]);
    const spec = toSpec(s);
    expect(spec.pieces).toHaveLength(2);
    expect(atob(spec.pieces[0]?.patch ?? "")).toBe(p0);
    expect(spec.pieces[1]).toEqual({ message: "orig" });
  });

  test("line-level pieces: a middle piece applies over the first, three pieces", () => {
    let s = initSplit(diff, false, "orig");
    s = addPiece(s);
    expect(s.pieces).toBe(3);
    // piece 0: the hunk-1 change; piece 1: only the "-k"/"+K" pair of hunk 2; remainder: "+k2".
    s = assignHunk(s, 0, 0, 0);
    s = assignLine(s, 0, 1, 2, 1);
    s = assignLine(s, 0, 1, 3, 1);
    s = setPieceMessage(setPieceMessage(s, 0, "one"), 1, "two");
    expect(commitBlockedReason(s)).toBeUndefined();
    const after0 = apply(OLD, buildPatch(s, 0));
    const after1 = apply(after0, buildPatch(s, 1));
    expect(after1).toEqual(["a", "B", "c", "d", "e", "f", "g", "h", "i", "j", "K", "l"]);
    // the remainder adds "k2" after "K": the final tree is NEW
    expect([...after1.slice(0, 11), "k2", ...after1.slice(11)]).toEqual(NEW);
  });

  test("a later hunk's patch is offset by what lower pieces added", () => {
    let s = initSplit(diff, false, "orig");
    s = addPiece(s);
    s = assignLine(s, 0, 1, 3, 0); // +K goes first
    s = assignLine(s, 0, 1, 4, 0); // +k2 goes first too
    s = assignHunk(s, 0, 0, 1); // hunk 1 second
    s = setPieceMessage(setPieceMessage(s, 0, "one"), 1, "two");
    const after0 = apply(OLD, buildPatch(s, 0));
    expect(after0).toEqual(["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "K", "k2", "l"]);
    const after1 = apply(after0, buildPatch(s, 1));
    expect(after1.slice(0, 3)).toEqual(["a", "B", "c"]);
  });

  test("whole files go in wholeFiles, granular files never do", () => {
    const text = `diff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\n${diff}`;
    let s = initSplit(text, false, "orig");
    s = assignFile(s, 0, 0);
    s = assignHunk(s, 1, 0, 0);
    s = setPieceMessage(s, 0, "assets");
    const spec = toSpec(s);
    expect(spec.pieces[0]?.wholeFiles).toEqual(["img.png"]);
    expect(spec.pieces[0]?.patch).toBeDefined();
    expect(spec.pieces[1]?.wholeFiles).toBeUndefined();
  });

  test("removing a piece returns its lines to the remainder and shifts the others", () => {
    let s = addPiece(initSplit(diff, false, "orig"));
    s = assignHunk(s, 0, 0, 0);
    s = assignHunk(s, 0, 1, 1);
    s = removePiece(s, 0);
    expect(s.pieces).toBe(2);
    expect(pieceCounts(s)).toEqual([3, 2]);
  });

  test("context lines cannot be assigned", () => {
    const s = initSplit(diff, false, "orig");
    expect(assignLine(s, 0, 0, 0, 0)).toBe(s);
  });

  test("splitRun never runs the interactive form and refuses option-like ids", () => {
    expect(splitRun("abcd1234", "/tmp/s.json")).toEqual({
      verb: "split",
      args: ["--hunks=/tmp/s.json", "abcd1234"],
    });
    expect(() => splitRun("--by-hunk", "/tmp/s.json")).toThrow();
  });
});
