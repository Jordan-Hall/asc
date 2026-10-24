import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Asc, EPOCH } from "../src/index";

const dir = mkdtempSync(join(tmpdir(), "asc-fixtures-env-"));
const saved = { ...process.env };
beforeAll(() => {
  process.env.ASC_TOKEN = "must-not-leak";
  process.env.GIT_AUTHOR_DATE = "1 +0000";
  process.env.GIT_DIR = "/elsewhere";
});
afterAll(() => {
  for (const k of ["ASC_TOKEN", "GIT_AUTHOR_DATE", "GIT_DIR"]) {
    if (saved[k] === undefined) delete process.env[k];
    else process.env[k] = saved[k];
  }
  rmSync(dir, { recursive: true, force: true });
});

test("the child environment drops inherited GIT_* and ASC_* variables and pins the identity", () => {
  const asc = new Asc({ bin: "unused", home: dir, cwd: dir });
  const env = asc.env();
  expect(env.ASC_TOKEN).toBeUndefined();
  expect(env.GIT_DIR).toBeUndefined();
  expect(env.GIT_AUTHOR_DATE).toBe(`${EPOCH} +0000`);
  expect(env.GIT_CONFIG_GLOBAL?.startsWith(dir)).toBe(true);
  expect(env.GIT_TERMINAL_PROMPT).toBe("0");
  expect(env.ASC_HOME?.startsWith(dir)).toBe(true);
});

test("the clock moves one second per call and is shared by every directory", () => {
  const a = new Asc({ bin: "unused", home: dir, cwd: dir });
  const b = a.at(join(dir, "other"));
  expect([a.tick(), b.tick(), a.tick()]).toEqual([EPOCH + 1, EPOCH + 2, EPOCH + 3]);
});

test("a token given for one call reaches the child environment and not the next", () => {
  const asc = new Asc({ bin: "unused", home: dir, cwd: dir });
  expect(asc.env({ ASC_TOKEN: "t" }).ASC_TOKEN).toBe("t");
  expect(asc.env().ASC_TOKEN).toBeUndefined();
});
