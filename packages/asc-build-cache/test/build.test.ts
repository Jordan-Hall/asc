// Each plugin under a real build: a project built in one work tree is restored, not built, in a
// second work tree with the same sources; a changed source builds again. Needs the asc binary
// (`ASC_BIN`, else the workspace's debug build); without it the tests are skipped.

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { digest, lockfiles, workspacePackages } from "../src/core.js";
import { pluginAscBuildCache } from "../src/rsbuild.js";
import { ascBuildCache } from "../src/vite.js";

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
let sandbox = "";
const saved = { ...process.env };
const cwd = process.cwd();

beforeAll(() => {
  sandbox = mkdtempSync(join(process.env.ASC_TEST_TMP ?? tmpdir(), "asc-build-cache-"));
  process.env.ASC_HOME = join(sandbox, "asc-home");
  process.env.HOME = join(sandbox, "home");
  process.env.USERPROFILE = join(sandbox, "home");
  process.env.XDG_CONFIG_HOME = join(sandbox, "home", ".config");
  process.env.GIT_CONFIG_NOSYSTEM = "1";
  process.env.ASC_BUILD_CACHE_QUIET = "1";
  // `NODE_ENV` keys a build; bun sets it to `test` and Vite to `production` after its first build.
  process.env.NODE_ENV = "production";
  if (ASC) {
    process.env.ASC_BIN = ASC;
  }
  mkdirSync(join(sandbox, "home"), { recursive: true });
});

afterAll(() => {
  process.chdir(cwd);
  process.env = saved;
  rmSync(sandbox, { recursive: true, force: true });
});

function asc(dir: string, ...args: string[]): string {
  const r = spawnSync(ASC as string, args, { cwd: dir, encoding: "utf8" });
  if (r.status !== 0) {
    throw new Error(`asc ${args.join(" ")}: ${r.stderr}`);
  }
  return r.stdout;
}

function write(root: string, rel: string, body: string): void {
  const p = join(root, rel);
  mkdirSync(join(p, ".."), { recursive: true });
  writeFileSync(p, body);
}

/** A work tree with one project in `app/`. */
function workTree(name: string, files: Record<string, string>): string {
  const top = join(sandbox, name);
  mkdirSync(top, { recursive: true });
  // Before `asc init`, which writes an `.ascignore` (shadowing `.gitignore`) when neither exists.
  write(top, ".gitignore", "dist/\nnode_modules/\n");
  asc(top, "init", "-q");
  asc(top, "config", "asc.buildcache.remote", "off");
  for (const [rel, body] of Object.entries(files)) {
    write(top, join("app", rel), body);
  }
  return join(top, "app");
}

function hits(dir: string, tool: string): number {
  const v = JSON.parse(asc(dir, "cache", "status", "--json"));
  return v.tools?.[tool]?.day?.hits ?? 0;
}

function entries(dir: string, tool: string): number {
  return JSON.parse(asc(dir, "cache", "ls", "--tool", tool, "--json")).length;
}

function tree(dir: string): Record<string, string> {
  const out: Record<string, string> = {};
  const walk = (d: string, rel: string) => {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      const r = rel ? `${rel}/${e.name}` : e.name;
      if (e.isDirectory()) {
        walk(join(d, e.name), r);
      } else {
        out[r] = readFileSync(join(d, e.name), "utf8");
      }
    }
  };
  walk(dir, "");
  return out;
}

async function vite(root: string): Promise<void> {
  const { build } = await import("vite");
  process.chdir(root);
  await build({ root, configFile: false, logLevel: "silent", plugins: [ascBuildCache()] });
}

async function rsbuild(root: string): Promise<void> {
  const { createRsbuild } = await import("@rsbuild/core");
  process.chdir(root);
  const r = await createRsbuild({
    cwd: root,
    rsbuildConfig: { plugins: [pluginAscBuildCache()], performance: { printFileSize: false } },
    loadEnv: false,
  });
  await r.build();
}

async function rslib(root: string): Promise<void> {
  const { build } = await import("@rslib/core");
  process.chdir(root);
  await build(
    {
      lib: [{ format: "esm", syntax: "es2022" }],
      source: { entry: { index: "./src/index.js" } },
      plugins: [pluginAscBuildCache()],
      performance: { printFileSize: false },
    },
    { root },
  );
}

const runIf = ASC ? describe : describe.skip;

runIf("vite", () => {
  const files = {
    "package.json": '{ "name": "app", "type": "module" }\n',
    "index.html": '<!doctype html><script type="module" src="/src/main.js"></script>\n',
    "src/main.js": 'import { greet } from "./greet.js";\ndocument.body.textContent = greet();\n',
    "src/greet.js": 'export const greet = () => "hi from greet";\n',
  };

  test("a second work tree restores the build", async () => {
    const a = workTree("vite-a", files);
    await vite(a);
    const built = tree(join(a, "dist"));
    expect(Object.keys(built).some((f) => f.endsWith(".js"))).toBe(true);
    expect(entries(a, "vite")).toBe(1);

    const b = workTree("vite-b", files);
    write(b, "dist/stale.txt", "left by an old build\n");
    const before = hits(b, "vite");
    await vite(b);
    expect(hits(b, "vite")).toBe(before + 1);
    expect(tree(join(b, "dist"))).toEqual(built);
    expect(entries(b, "vite")).toBe(1);

    write(b, "src/greet.js", 'export const greet = () => "changed";\n');
    await vite(b);
    expect(entries(b, "vite")).toBe(2);
    const changed = tree(join(b, "dist"));
    expect(Object.values(changed).some((t) => t.includes("changed"))).toBe(true);
  }, 120_000);
});

runIf("rsbuild", () => {
  const files = {
    "package.json": '{ "name": "web" }\n',
    "src/index.js": 'import { greet } from "./greet.js";\nconsole.log(greet());\n',
    "src/greet.js": 'export const greet = () => "hi from rsbuild";\n',
  };

  test("a second work tree restores the build", async () => {
    const a = workTree("rsbuild-a", files);
    await rsbuild(a);
    const built = tree(join(a, "dist"));
    expect(Object.keys(built).some((f) => f.endsWith(".js"))).toBe(true);
    expect(entries(a, "rsbuild")).toBe(1);

    const b = workTree("rsbuild-b", files);
    const before = hits(b, "rsbuild");
    await rsbuild(b);
    expect(hits(b, "rsbuild")).toBe(before + 1);
    expect(tree(join(b, "dist"))).toEqual(built);

    write(b, "src/greet.js", 'export const greet = () => "changed";\n');
    await rsbuild(b);
    expect(entries(b, "rsbuild")).toBe(2);
    expect(Object.values(tree(join(b, "dist"))).some((t) => t.includes("changed"))).toBe(true);
  }, 120_000);
});

runIf("rslib", () => {
  const files = {
    "package.json": '{ "name": "lib", "type": "module" }\n',
    "src/index.js": 'export const answer = () => "hi from rslib";\n',
  };

  test("a second work tree restores the library", async () => {
    const a = workTree("rslib-a", files);
    await rslib(a);
    const built = tree(join(a, "dist"));
    expect(Object.keys(built).length).toBeGreaterThan(0);
    expect(entries(a, "rslib")).toBe(1);

    const b = workTree("rslib-b", files);
    const before = hits(b, "rslib");
    await rslib(b);
    expect(hits(b, "rslib")).toBe(before + 1);
    expect(tree(join(b, "dist"))).toEqual(built);
  }, 120_000);
});

describe("inputs", () => {
  test("workspace packages the project links to are inputs; registry packages are not", () => {
    const top = mkdtempSync(join(process.env.ASC_TEST_TMP ?? tmpdir(), "asc-inputs-"));
    try {
      write(top, "bun.lock", "{}\n");
      write(top, "apps/web/package.json", '{ "dependencies": { "@x/ui": "workspace:*", "left-pad": "1" } }\n');
      write(top, "packages/ui/package.json", '{ "name": "@x/ui", "dependencies": { "@x/core": "workspace:*" } }\n');
      write(top, "packages/core/package.json", '{ "name": "@x/core" }\n');
      write(top, "node_modules/left-pad/package.json", '{ "name": "left-pad" }\n');
      mkdirSync(join(top, "node_modules", "@x"), { recursive: true });
      const link = (name: string, to: string) =>
        symlinkSync(join(top, to), join(top, "node_modules", "@x", name), "junction");
      link("ui", "packages/ui");
      link("core", "packages/core");
      const web = join(top, "apps", "web");
      expect(workspacePackages(web, top)).toEqual([join(top, "packages", "core"), join(top, "packages", "ui")]);
      expect(lockfiles(web, top)).toEqual([join(top, "bun.lock")]);
    } finally {
      rmSync(top, { recursive: true, force: true });
    }
  });

  test("the config digest ignores key order and sees functions", () => {
    expect(digest({ a: 1, b: { c: 2 } })).toBe(digest({ b: { c: 2 }, a: 1 }));
    expect(digest({ f: () => 1 })).not.toBe(digest({ f: () => 2 }));
  });
});
