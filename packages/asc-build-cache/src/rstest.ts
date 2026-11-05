// The Rstest runner (docs/designs/build-cache.md §4.2): `rstest run` as one cached action. A run
// whose inputs another work tree already passed restores what it wrote (coverage, reports) and
// replays its output instead of running the tests; a failing run is never stored, so it runs
// again every time. The command line is `asc-rstest [asc options] run [rstest options]`.

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

import { type AscBuildCacheOptions, type Build, enabled, note, restore, save } from "./core.js";

export interface AscRstestOptions extends AscBuildCacheOptions {
  /** More files or directories the run writes (for example `coverage`), restored on a hit. */
  outputs?: string[];
}

/** The output (and its log) is not stored beyond this many bytes per stream. */
const MAX_LOG = 16 << 20;
/** Where a run's output is kept, beside the project's other caches (ignored with node_modules). */
const LOG_DIR = join("node_modules", ".cache", "asc-rstest");
/** Variables a test run reads besides the `RSTEST_` ones. */
const ENV = ["NODE_ENV", "NODE_OPTIONS", "TZ", "CI"];

export interface Parsed {
  opts: AscRstestOptions;
  args: string[];
}

/** Splits the asc options (before the rstest command) from the rstest command line. */
export function parseArgs(argv: string[]): Parsed {
  const opts: AscRstestOptions = {};
  let i = 0;
  const value = (flag: string): string => {
    const v = argv[i + 1];
    if (v === undefined || v.startsWith("--")) {
      throw new Error(`asc-rstest: ${flag} needs a value`);
    }
    i += 2;
    return v;
  };
  for (let flag = argv[i]; flag?.startsWith("--asc-"); flag = argv[i]) {
    switch (flag) {
      case "--asc-name": {
        opts.name = value(flag);
        break;
      }
      case "--asc-output": {
        opts.outputs = [...(opts.outputs ?? []), value(flag)];
        break;
      }
      case "--asc-input": {
        opts.inputs = [...(opts.inputs ?? []), value(flag)];
        break;
      }
      case "--asc-env": {
        opts.env = [...(opts.env ?? []), value(flag)];
        break;
      }
      case "--asc-no-remote": {
        opts.remote = false;
        i += 1;
        break;
      }
      case "--asc-off": {
        opts.enabled = false;
        i += 1;
        break;
      }
      default: {
        throw new Error(`asc-rstest: unknown option ${flag}`);
      }
    }
  }
  return { opts, args: argv.slice(i) };
}

/** A single `rstest run`: not watch mode, not a snapshot update (which writes the sources). */
export function cacheable(args: string[]): boolean {
  if (args[0] !== "run") {
    return false;
  }
  return !args.some((a) => ["-w", "--watch", "-u", "--update", "--update=true"].includes(a));
}

/** The installed `@rstest/core` from `root` up: its version and its command's script. */
export function findRstest(root: string): { version: string; bin: string } | undefined {
  for (let dir = resolve(root); ; dir = dirname(dir)) {
    const pkgPath = join(dir, "node_modules", "@rstest", "core", "package.json");
    if (existsSync(pkgPath)) {
      const pkg = JSON.parse(readFileSync(pkgPath, "utf8")) as {
        version?: string;
        bin?: string | Record<string, string>;
      };
      const bin = typeof pkg.bin === "string" ? pkg.bin : pkg.bin?.rstest;
      return bin ? { version: String(pkg.version ?? "unknown"), bin: join(dirname(pkgPath), bin) } : undefined;
    }
    if (dirname(dir) === dir) {
      return undefined;
    }
  }
}

interface Ran {
  code: number;
  stdout: Buffer;
  stderr: Buffer;
  /** Both streams fit under the limit. */
  kept: boolean;
}

/** Runs rstest with its output shown as it comes and kept for the cache. */
function runTests(bin: string, args: string[], root: string): Promise<Ran> {
  return new Promise((done, fail) => {
    const child = spawn(process.execPath, [bin, ...args], {
      cwd: root,
      stdio: ["inherit", "pipe", "pipe"],
      windowsHide: true,
    });
    const kept = { stdout: [] as Buffer[], stderr: [] as Buffer[] };
    const sizes = { stdout: 0, stderr: 0 };
    const tee = (name: "stdout" | "stderr") => (chunk: Buffer) => {
      process[name].write(chunk);
      sizes[name] += chunk.length;
      if (sizes[name] <= MAX_LOG) {
        kept[name].push(chunk);
      }
    };
    child.stdout.on("data", tee("stdout"));
    child.stderr.on("data", tee("stderr"));
    child.on("error", fail);
    child.on("close", (code, signal) => {
      done({
        code: signal ? 1 : (code ?? 1),
        stdout: Buffer.concat(kept.stdout),
        stderr: Buffer.concat(kept.stderr),
        kept: sizes.stdout <= MAX_LOG && sizes.stderr <= MAX_LOG,
      });
    });
  });
}

/** `asc-rstest`: runs (or restores) the rstest command line; resolves to the exit code. */
export async function runRstest(argv: string[], root: string = process.cwd()): Promise<number> {
  const { opts, args } = parseArgs(argv);
  const rstest = findRstest(root);
  if (!rstest) {
    process.stderr.write(`asc-rstest: @rstest/core is not installed in ${root} or above\n`);
    return 1;
  }
  if (!enabled(opts) || !cacheable(args)) {
    return (await runTests(rstest.bin, args, root)).code;
  }
  const logs = resolve(root, LOG_DIR);
  const build: Build = {
    tool: "rstest",
    root,
    outputs: [logs, ...(opts.outputs ?? []).map((o) => resolve(root, o))],
    parts: {
      version: rstest.version,
      node: process.version,
      args: JSON.stringify(args),
    },
    envPrefixes: ["RSTEST_"],
  };
  const got = restore(build, { ...opts, env: [...ENV, ...(opts.env ?? [])] });
  const out = join(logs, "stdout");
  const err = join(logs, "stderr");
  if (got?.hit && existsSync(out) && existsSync(err)) {
    note("rstest", got.key);
    process.stdout.write(readFileSync(out));
    process.stderr.write(readFileSync(err));
    return 0;
  }
  rmSync(logs, { recursive: true, force: true });
  const ran = await runTests(rstest.bin, args, root);
  if (got && ran.code === 0 && ran.kept) {
    mkdirSync(logs, { recursive: true });
    writeFileSync(out, ran.stdout);
    writeFileSync(err, ran.stderr);
    save(build, opts, got.key);
  }
  return ran.code;
}
