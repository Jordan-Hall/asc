// The install page shows commands; the installers must accept every flag and variable it names.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { INSTALL_PS1, INSTALL_PS1_PINNED, INSTALL_SH, INSTALL_SH_PINNED } from "../src/routes/info";

const sh = readFileSync(new URL("../../scripts/install.sh", import.meta.url), "utf8");
const ps1 = readFileSync(new URL("../../scripts/install.ps1", import.meta.url), "utf8");

describe("install page commands match the installers", () => {
  test("install.sh accepts the flags and variables shown", () => {
    for (const flag of ["--base-url", "--version", "--prefix", "--bin-dir", "--dry-run"]) {
      expect(sh).toContain(flag);
    }
    for (const shown of [INSTALL_SH, INSTALL_SH_PINNED]) {
      for (const flag of shown.match(/--[a-z-]+/g) ?? []) expect(sh).toContain(flag);
      for (const v of shown.match(/ASC_[A-Z_]+/g) ?? []) expect(sh).toContain(v);
    }
    expect(INSTALL_SH).toContain("sh install.sh");
  });

  test("install.ps1 accepts the parameters and variables shown", () => {
    for (const shown of [INSTALL_PS1, INSTALL_PS1_PINNED]) {
      for (const p of shown.match(/ -[A-Z][A-Za-z]+/g) ?? []) {
        expect(ps1).toContain(
          `[${p.trim() === "-DryRun" || p.trim() === "-NoModifyPath" ? "switch" : "string"}]$${p.trim().slice(1)}`,
        );
      }
      for (const v of shown.match(/ASC_[A-Z_]+/g) ?? []) expect(ps1).toContain(v);
    }
  });
});
