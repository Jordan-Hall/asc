import { describe, expect, test } from "bun:test";
import { engineMin, exeNames, parseArgs, satisfiesEngine, verdict } from "./lib";

describe("smoke helpers", () => {
  test("parseArgs", () => {
    const a = parseArgs(["--editor", "vscode", "--strict-probe"], { ASC_BIN: "asc" });
    expect(a.editor).toBe("vscode");
    expect(a.strictProbe).toBe(true);
    expect(a.asc).toBe("asc");
    expect(() => parseArgs(["--editor", "emacs"], {})).toThrow();
    expect(() => parseArgs(["--bogus"], {})).toThrow();
    expect(() => parseArgs(["--vsix"], {})).toThrow();
  });
  test("engine range", () => {
    expect(engineMin("^1.101.0")).toEqual([1, 101]);
    expect(satisfiesEngine("1.101.2", "^1.101.0")).toBe(true);
    expect(satisfiesEngine("1.100.9", "^1.101.0")).toBe(false);
    expect(satisfiesEngine("2.0.0", "^1.101.0")).toBe(true);
  });
  test("exe names", () => {
    expect(exeNames("vscodium", "win32")).toEqual(["VSCodium.exe"]);
  });
  test("verdict", () => {
    const ok = { mode: "asc", editorVersion: "1.101.0", failures: [], notes: [] };
    const plain = { ...ok, mode: "plain", probeMissing: true };
    expect(verdict([ok, plain], false)).toEqual([]);
    expect(verdict([ok, plain], true)).toHaveLength(1);
    expect(verdict([{ ...ok, failures: ["x"] }, plain], false)).toEqual(["[asc] x"]);
    expect(verdict([ok], false)).toHaveLength(1);
  });
});
