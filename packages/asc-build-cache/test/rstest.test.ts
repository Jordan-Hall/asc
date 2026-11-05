// The Rstest runner over a stand-in `@rstest/core` (its command counts its runs, prints a report,
// writes coverage when asked and fails when the project has `fail.txt`): a run that passed in one
// work tree is restored and replayed in a second; a changed source, a failing run, watch mode and
// a snapshot update run the tests. Needs the asc binary (`ASC_BIN`, else the workspace's debug
// build); without it the cached cases are skipped.

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { cacheable, findRstest, parseArgs } from "../src/rstest.js";

function findAsc(): string | undefined {
  const exe = process.platform === "win32" ? "asc.exe" : "asc";
  const candidates = [
    process.env.ASC_BIN,
    process.env.CARGO_TARGET_DIR && join(process.env.CARGO_TARGET_DIR, "debug", exe),
    join(import.meta.dir, "..", "..", "..", "target", "debug", exe),
  ];
  return candidates.find((c): c is string => !!c && existsSync(c));
}

const ASC = findAsc();
const CLI = join(import.meta.dir, "..", "src", "rstest-cli.ts");
let sandbox = "";
let env: Record<string, string | undefined> = {};

const FAKE = `const fs = require("node:fs");
const path = require("node:path");
fs.appendFileSync(process.env.RSTEST_FAKE_RUNS, "run\\n");
const args = process.argv.slice(2);
if (args.includes("--coverage")) {
  fs.mkdirSync("coverage", { recursive: true });
  fs.writeFileSync(path.join("coverage", "lcov.info"), "SF:src/sum.js\\n");
}
process.stdout.write("Tests 3 passed\\n");
process.stderr.write("a warning\\n");
process.exitCode = fs.existsSync("fail.txt") ? 1 : 0;
`;

beforeAll(() => {
  sandbox = mkdtempSync(join(process.env.ASC_TEST_TMP ?? tmpdir(), "asc-rstest-"));
  mkdirSync(join(sandbox, "home"), { recursive: true });
  env = {
    ...process.env,
    ASC_HOME: join(sandbox, "asc-home"),
    HOME: join(sandbox, "home"),
    USERPROFILE: join(sandbox, "home"),
    XDG_CONFIG_HOME: join(sandbox, "home", ".config"),
    GIT_CONFIG_NOSYSTEM: "1",
    ASC_BUILD_CACHE_QUIET: "1",
    NODE_ENV: "test",
    RSTEST_FAKE_RUNS: join(sandbox, "runs"),
    ...(ASC ? { ASC_BIN: ASC } : {}),
  };
});

afterAll(() => {
  rmSync(sandbox, { recursive: true, force: true });
});

function write(root: string, rel: string, body: string): void {
  const p = join(root, rel);
  mkdirSync(join(p, ".."), { recursive: true });
  writeFileSync(p, body);
}

function asc(dir: string, ...args: string[]): string {
  const r = spawnSync(ASC as string, args, { cwd: dir, encoding: "utf8", env });
  if (r.status !== 0) {
    throw new Error(`asc ${args.join(" ")}: ${r.stderr}`);
  }
  return r.stdout;
}

/** A work tree with one tested project in `app/` and the stand-in rstest installed there. */
function workTree(name: string): string {
  const top = join(sandbox, name);
  mkdirSync(top, { recursive: true });
  write(top, ".gitignore", "coverage/\nnode_modules/\n");
  asc(top, "init", "-q");
  asc(top, "config", "asc.buildcache.remote", "off");
  const app = join(top, "app");
  write(app, "package.json", '{ "name": "app" }\n');
  write(app, "src/sum.js", "export const sum = (a, b) => a + b;\n");
  write(app, "test/sum.test.js", 'import { sum } from "../src/sum.js";\ntest("sum", () => expect(sum(1, 2)).toBe(3));\n');
  write(app, "node_modules/@rstest/core/package.json", '{ "name": "@rstest/core", "version": "0.6.2", "bin": { "rstest": "bin/rstest.js" } }\n');
  write(app, "node_modules/@rstest/core/bin/rstest.js", FAKE);
  return app;
}

function runs(): number {
  const p = join(sandbox, "runs");
  return existsSync(p) ? readFileSync(p, "utf8").split("\n").filter(Boolean).length : 0;
}

function rstest(root: string, ...args: string[]) {
  const r = spawnSync(process.execPath, [CLI, ...args], { cwd: root, encoding: "utf8", env });
  return { code: r.status, stdout: r.stdout, stderr: r.stderr };
}

function hits(dir: string): number {
  const v = JSON.parse(asc(dir, "cache", "status", "--json"));
  return v.tools?.rstest?.day?.hits ?? 0;
}

describe("the command line", () => {
  test("asc options come first and the rest is rstest's", () => {
    const p = parseArgs(["--asc-name", "web", "--asc-output", "coverage", "--asc-no-remote", "run", "--coverage"]);
    expect(p.opts).toEqual({ name: "web", outputs: ["coverage"], remote: false });
    expect(p.args).toEqual(["run", "--coverage"]);
    expect(() => parseArgs(["--asc-name"])).toThrow("needs a value");
    expect(() => parseArgs(["--asc-bogus", "run"])).toThrow("unknown option");
  });

  test("only a single run is cached", () => {
    expect(cacheable(["run"])).toBe(true);
    expect(cacheable(["run", "src/a.test.ts", "--coverage"])).toBe(true);
    expect(cacheable([])).toBe(false);
    expect(cacheable(["watch"])).toBe(false);
    expect(cacheable(["list"])).toBe(false);
    expect(cacheable(["run", "--watch"])).toBe(false);
    expect(cacheable(["run", "-u"])).toBe(false);
    expect(cacheable(["run", "--update"])).toBe(false);
  });

  test("rstest is found from the project up", () => {
    const top = mkdtempSync(join(sandbox, "find-"));
    write(top, "node_modules/@rstest/core/package.json", '{ "version": "1.0.0", "bin": "cli.js" }\n');
    mkdirSync(join(top, "pkg", "a"), { recursive: true });
    expect(findRstest(join(top, "pkg", "a"))).toEqual({ version: "1.0.0", bin: join(top, "node_modules", "@rstest", "core", "cli.js") });
    expect(findRstest(mkdtempSync(join(tmpdir(), "asc-none-")))).toBeUndefined();
  });
});

const runIf = ASC ? describe : describe.skip;

runIf("rstest", () => {
  test("a second work tree replays the passing run instead of running the tests", () => {
    const a = workTree("a");
    const first = rstest(a, "--asc-output", "coverage", "run", "--coverage");
    expect(first).toEqual({ code: 0, stdout: "Tests 3 passed\n", stderr: "a warning\n" });
    expect(runs()).toBe(1);

    const b = workTree("b");
    const before = hits(b);
    const again = rstest(b, "--asc-output", "coverage", "run", "--coverage");
    expect(again).toEqual({ code: 0, stdout: "Tests 3 passed\n", stderr: "a warning\n" });
    expect(runs()).toBe(1);
    expect(hits(b)).toBe(before + 1);
    expect(readFileSync(join(b, "coverage", "lcov.info"), "utf8")).toBe("SF:src/sum.js\n");

    // Other arguments are another run.
    rstest(b, "run");
    expect(runs()).toBe(2);
  }, 60_000);

  test("a changed source runs the tests", () => {
    const a = workTree("c");
    rstest(a, "run");
    const n = runs();
    write(a, "src/sum.js", "export const sum = (a, b) => b + a;\n");
    expect(rstest(a, "run").code).toBe(0);
    expect(runs()).toBe(n + 1);
    expect(rstest(a, "run").code).toBe(0);
    expect(runs()).toBe(n + 1);
  }, 60_000);

  test("a failing run is not stored", () => {
    const a = workTree("d");
    write(a, "fail.txt", "\n");
    const n = runs();
    expect(rstest(a, "run").code).toBe(1);
    expect(rstest(a, "run").code).toBe(1);
    expect(runs()).toBe(n + 2);
  }, 60_000);

  test("watch mode, snapshot updates and --asc-off always run", () => {
    const a = workTree("e");
    rstest(a, "run");
    const n = runs();
    rstest(a, "run", "-u");
    rstest(a, "--asc-off", "run");
    expect(runs()).toBe(n + 2);
  }, 60_000);
});
