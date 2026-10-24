import { describe, expect, test } from "bun:test";
import { buildPath, compile, matchPattern } from "../src/lib/match";
import { blobHref, PATTERNS, treeHref } from "../src/lib/paths";

describe("matchPattern", () => {
  test("static, param and splat", () => {
    const p = compile(PATTERNS.blob);
    expect(matchPattern(p, "/asc/demo/blob/main/src/lib/a.ts")).toEqual({
      owner: "asc",
      repo: "demo",
      ref: "main",
      path: "src/lib/a.ts",
    });
    expect(matchPattern(p, "/asc/demo/tree/main/x")).toBeUndefined();
  });

  test("an empty splat matches", () => {
    expect(matchPattern(compile(PATTERNS.tree), "/o/r/tree/main")?.path).toBe("");
  });

  test("percent-encoded segments decode; bad encoding does not match", () => {
    const p = compile(PATTERNS.tree);
    expect(matchPattern(p, "/o/r/tree/feature%2Fx/a%20b")).toMatchObject({
      ref: "feature/x",
      path: "a b",
    });
    expect(matchPattern(p, "/o/r/tree/%E0%A4%A/x")).toBeUndefined();
  });

  test("length must match exactly", () => {
    expect(matchPattern(compile(PATTERNS.repo), "/o/r/extra")).toBeUndefined();
    expect(matchPattern(compile(PATTERNS.repo), "/o")).toBeUndefined();
  });

  test("a splat must be last", () => {
    expect(() => compile("/a/*x/b")).toThrow();
  });
});

describe("buildPath", () => {
  test("encodes and round-trips", () => {
    const href = treeHref("o", "r", "feature/x", "dir with space/f#1.ts");
    expect(href).toBe("/o/r/tree/feature%2Fx/dir%20with%20space/f%231.ts");
    expect(matchPattern(compile(PATTERNS.tree), href)).toEqual({
      owner: "o",
      repo: "r",
      ref: "feature/x",
      path: "dir with space/f#1.ts",
    });
    expect(blobHref("o", "r", "main", "a/b")).toBe("/o/r/blob/main/a/b");
    expect(buildPath("/", {})).toBe("/");
  });
});
