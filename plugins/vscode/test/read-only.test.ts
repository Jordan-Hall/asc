import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadAccess, parseAccess, safeRel, writable } from "../src/path-access";
import { relTo } from "../src/read-only";

const STORED =
  "asc path-access 1\nremote origin\nrepo write\n" +
  "dir write\t\ndir read\tdocs\ndir write\tdocs/drafts\ndir none\tsecret\n";

describe("path access", () => {
  test("the deepest rule directory decides, the repository level alone locks nothing", () => {
    const a = parseAccess(STORED);
    expect(a?.dirs.length).toBe(4);
    expect(writable(a, "src/main.rs")).toBe(true);
    expect(writable(a, "docs/a.md")).toBe(false);
    expect(writable(a, "docs/drafts/b.md")).toBe(true);
    expect(writable(a, "docsx/a.md")).toBe(true);
    expect(writable(a, "secret/x")).toBe(false);
    const reader = parseAccess("asc path-access 1\nremote origin\nrepo read\n");
    expect(writable(reader, "src/main.rs")).toBe(true);
    expect(writable(null, "docs/a.md")).toBe(true);
  });

  test("an unknown or unsafe file is ignored", () => {
    expect(parseAccess("asc path-access 2\nremote o\nrepo read\n")).toBeNull();
    expect(parseAccess(`${STORED}dir read\t../x\n`)).toBeNull();
    expect(parseAccess(`${STORED}dir sudo\tx\n`)).toBeNull();
    for (const p of ["a/../b", ".git/x", "a\\b", "c:x", "GIT~1", "a./b", "a /b", "a//b"]) {
      expect(safeRel(p)).toBe(false);
    }
    expect(safeRel("docs/drafts")).toBe(true);
  });

  test("the levels are read from the common git directory of a worktree", async () => {
    const d = mkdtempSync(join(tmpdir(), "asc-ro-"));
    try {
      const common = join(d, "repo.git");
      const wt = join(common, "worktrees", "w");
      mkdirSync(wt, { recursive: true });
      writeFileSync(join(wt, "commondir"), "../..\n");
      writeFileSync(join(common, "asc-path-access"), STORED);
      const a = await loadAccess(wt);
      expect(writable(a, "docs/a.md")).toBe(false);
      expect(await loadAccess(join(d, "none"))).toBeNull();
    } finally {
      rmSync(d, { recursive: true, force: true });
    }
  });

  test("paths are matched to their root", () => {
    expect(relTo(["C:\\w\\app"], "c:\\W\\App\\docs\\a.md", true)).toEqual({
      root: "C:\\w\\app",
      path: "docs/a.md",
    });
    expect(relTo(["/w/app"], "/w/apple/a.md", false)).toBeNull();
  });
});
