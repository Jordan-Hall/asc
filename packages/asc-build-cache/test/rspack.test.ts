// The Rspack and webpack plugin under a real Rspack build: a project built in one work tree is
// restored, not built, in a second work tree with the same sources; a changed source builds again;
// a failed, watched or disabled build stores nothing. webpack is driven through a compiler of the
// same shape. Needs the asc binary (`ASC_BIN`, else the workspace's debug build); without it the
// tests are skipped.

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { AscBuildCachePlugin, type CompilerLike } from "../src/rspack.js";

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
  sandbox = mkdtempSync(join(process.env.ASC_TEST_TMP ?? tmpdir(), "asc-rspack-cache-"));
  process.env.ASC_HOME = join(sandbox, "asc-home");
  process.env.HOME = join(sandbox, "home");
  process.env.USERPROFILE = join(sandbox, "home");
  process.env.XDG_CONFIG_HOME = join(sandbox, "home", ".config");
  process.env.GIT_CONFIG_NOSYSTEM = "1";
  process.env.ASC_BUILD_CACHE_QUIET = "1";
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

/** `rspack build` of `src/index.js` into `dist`; resolves with whether it had errors. */
async function rspack(root: string): Promise<boolean> {
  const { rspack: create } = await import("@rsbuild/core");
  process.chdir(root);
  const compiler = create({
    context: root,
    mode: "production",
    entry: "./src/index.js",
    output: { path: join(root, "dist"), clean: true },
    plugins: [new AscBuildCachePlugin()],
    infrastructureLogging: { level: "none" },
  });
  return new Promise((done, fail) =>
    compiler.run((err, stats) => {
      compiler.close(() => (err ? fail(err) : done(!!stats?.hasErrors())));
    }),
  );
}

const files = {
  "package.json": '{ "name": "bundle" }\n',
  "src/index.js": 'import { greet } from "./greet.js";\nconsole.log(greet());\n',
  "src/greet.js": 'export const greet = () => "hi from rspack";\n',
};

const runIf = ASC ? describe : describe.skip;

runIf("rspack", () => {
  test("a second work tree restores the build; a changed source builds again", async () => {
    const a = workTree("rspack-a", files);
    expect(await rspack(a)).toBe(false);
    const built = tree(join(a, "dist"));
    expect(Object.values(built).some((t) => t.includes("hi from rspack"))).toBe(true);
    expect(entries(a, "rspack")).toBe(1);

    const b = workTree("rspack-b", files);
    const before = hits(b, "rspack");
    expect(await rspack(b)).toBe(false);
    expect(hits(b, "rspack")).toBe(before + 1);
    expect(tree(join(b, "dist"))).toEqual(built);
    // The empty build went to the scratch directory, not over the restored outputs.
    expect(existsSync(join(b, "node_modules", ".cache", "asc-build-cache", "rspack", "out"))).toBe(
      true,
    );

    write(b, "src/greet.js", 'export const greet = () => "changed";\n');
    expect(await rspack(b)).toBe(false);
    expect(entries(b, "rspack")).toBe(2);
    expect(Object.values(tree(join(b, "dist"))).some((t) => t.includes("changed"))).toBe(true);
  }, 120_000);

  test("a built miss learns the sources it read into a prefetch profile", async () => {
    const app = workTree("rspack-learn", {
      ...files,
      "src/greet.js": 'export const greet = () => "learned";\n',
    });
    const top = join(app, "..");
    asc(top, "config", "user.name", "T");
    asc(top, "config", "user.email", "t@example.com");
    asc(top, "add", ".");
    asc(top, "commit", "-q", "-m", "base");
    expect(await rspack(app)).toBe(false);
    const shown = asc(top, "prefetch", "--show", "rspack:bundle").trim().split("\n").toSorted();
    expect(shown).toEqual(["app/package.json", "app/src/greet.js", "app/src/index.js"]);
  }, 120_000);

  test("a failed, watched or disabled build stores nothing", async () => {
    const broken = workTree("rspack-broken", {
      ...files,
      "src/index.js": 'import { nope } from "./missing.js";\nconsole.log(nope);\n',
    });
    // One store serves every work tree of the user, so count what each build adds.
    const stored = entries(broken, "rspack");
    expect(await rspack(broken)).toBe(true);
    expect(entries(broken, "rspack")).toBe(stored);

    // A compiler in watch mode is left alone: nothing restored, nothing hooked.
    const watched = workTree("rspack-watched", files);
    const taps: string[] = [];
    new AscBuildCachePlugin().apply({
      options: { context: watched, watch: true },
      rspack: { rspackVersion: "1.7.12" },
      hooks: {
        watchRun: { tapAsync: (n) => taps.push(n) },
        done: { tap: (n) => taps.push(n) },
      },
    });
    expect(taps).toEqual([]);
    expect(entries(watched, "rspack")).toBe(stored);

    const off = workTree("rspack-off", {
      ...files,
      "src/greet.js": "export const greet = () => 2;\n",
    });
    process.env.ASC_BUILD_CACHE = "0";
    try {
      expect(await rspack(off)).toBe(false);
    } finally {
      delete process.env.ASC_BUILD_CACHE;
    }
    expect(entries(off, "rspack")).toBe(stored);
  }, 120_000);

  test("plugin options and EnvironmentPlugin variables key the build", async () => {
    const { rspack: core } = await import("@rsbuild/core");
    const root = workTree("rspack-define", files);
    const keyOf = (plugins: unknown[]): string => {
      const debug = process.env.ASC_BUILD_CACHE_DEBUG;
      const stderr = process.stderr.write.bind(process.stderr);
      process.env.ASC_BUILD_CACHE_DEBUG = "1";
      let line = "";
      process.stderr.write = ((chunk: string) => {
        line += chunk;
        return true;
      }) as typeof process.stderr.write;
      try {
        new AscBuildCachePlugin().apply({
          options: { context: root, mode: "production", plugins },
          rspack: { rspackVersion: "1.7.12" },
          hooks: { watchRun: { tapAsync: () => {} }, done: { tap: () => {} } },
        });
      } finally {
        process.stderr.write = stderr;
        if (debug === undefined) delete process.env.ASC_BUILD_CACHE_DEBUG;
        else process.env.ASC_BUILD_CACHE_DEBUG = debug;
      }
      return line;
    };
    const a = keyOf([new core.DefinePlugin({ FLAG: JSON.stringify("a") })]);
    const b = keyOf([new core.DefinePlugin({ FLAG: JSON.stringify("b") })]);
    expect(a).not.toBe(b);
    expect(a).toContain("--part pluginOptions=");
    const env = keyOf([new core.EnvironmentPlugin(["ASC_TEST_API_URL"])]);
    expect(env).not.toContain("--env ASC_TEST_API_URL");
    process.env.ASC_TEST_API_URL = "https://api.example.org";
    try {
      expect(keyOf([new core.EnvironmentPlugin(["ASC_TEST_API_URL"])])).toContain(
        "--env ASC_TEST_API_URL",
      );
    } finally {
      delete process.env.ASC_TEST_API_URL;
    }
  });

  test("a webpack compiler is keyed as webpack and stored after a clean run", () => {
    const root = workTree("webpack-a", files);
    const done: ((stats: { hasErrors(): boolean }) => void)[] = [];
    const compiler: CompilerLike = {
      options: {
        context: root,
        mode: "production",
        entry: { main: { import: ["./src/index.js"] } },
        output: {},
      },
      webpack: { version: "5.99.0" },
      hooks: {
        watchRun: { tapAsync: () => {} },
        done: { tap: (_n, fn) => done.push(fn) },
      },
    };
    const rspacks = entries(root, "rspack");
    new AscBuildCachePlugin().apply(compiler);
    expect(done.length).toBe(1);
    write(root, "dist/main.js", "console.log(1);\n");
    done[0]?.({ hasErrors: () => false });
    expect(entries(root, "webpack")).toBe(1);
    expect(entries(root, "rspack")).toBe(rspacks);
  });
});
