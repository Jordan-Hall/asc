// The part every plugin shares (docs/designs/build-cache.md §4.2): the inputs of a project, the
// key parts, and the `asc cache restore` / `asc cache save` calls. A build that cannot reach asc
// (no binary, not in a work tree) builds as if the plugin were not there, with one warning.

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

export interface AscBuildCacheOptions {
  /** The project's name in `asc cache ls` (default: the `name` in package.json, else the directory). */
  name?: string;
  /** More paths that key the build, from the project root. The project directory, the lockfile and
   *  the workspace packages it depends on are always inputs. */
  inputs?: string[];
  /** More environment variables that key the build. */
  env?: string[];
  /** The asc executable (default: `ASC_BIN`, else `asc` on `PATH`). */
  asc?: string;
  /** Neither fetch from nor push to the server tier. */
  remote?: boolean;
  /** Turn the plugin off (default: on unless `ASC_BUILD_CACHE=0`). */
  enabled?: boolean;
  /** Send the files a built miss read to `asc prefetch --learn`, so a later switch or clone warms
   *  them (default: on unless `ASC_PREFETCH_LEARN=0`). */
  learn?: boolean;
}

export interface Build {
  tool: string;
  root: string;
  outputs: string[];
  parts: Record<string, string>;
  envPrefixes: string[];
}

export interface Restored {
  hit: boolean;
  key: string;
}

const LOCKFILES = [
  "bun.lock",
  "bun.lockb",
  "package-lock.json",
  "npm-shrinkwrap.json",
  "pnpm-lock.yaml",
  "yarn.lock",
];

/** This package's version: a new plugin may store outputs differently. */
const PLUGIN_VERSION = String(
  readJson(fileURLToPath(new URL("../package.json", import.meta.url)))?.version ?? "unknown",
);

let warned = false;
function warn(message: string): void {
  if (!warned) {
    warned = true;
    process.stderr.write(`asc build cache: ${message}; building without it\n`);
  }
}

export function enabled(opts: AscBuildCacheOptions): boolean {
  return opts.enabled ?? process.env.ASC_BUILD_CACHE !== "0";
}

/** The watch flag of the command line: a watched build is not cached. */
export function watching(): boolean {
  return process.argv.some((a) => a === "--watch" || a === "-w");
}

function ascBin(opts: AscBuildCacheOptions): string {
  return opts.asc ?? process.env.ASC_BIN ?? "asc";
}

function asc(opts: AscBuildCacheOptions, cwd: string, args: string[], input?: string) {
  return spawnSync(ascBin(opts), args, {
    cwd,
    encoding: "utf8",
    windowsHide: true,
    maxBuffer: 1 << 20,
    ...(input === undefined ? {} : { input }),
  });
}

function readJson(path: string): Record<string, unknown> | undefined {
  try {
    return JSON.parse(readFileSync(path, "utf8")) as Record<string, unknown>;
  } catch {
    return undefined;
  }
}

/** The work tree's root: the nearest directory with `.asc` or `.git`. */
export function workTree(from: string): string | undefined {
  for (let dir = resolve(from); ; dir = dirname(dir)) {
    if (existsSync(join(dir, ".asc")) || existsSync(join(dir, ".git"))) {
      return dir;
    }
    if (dirname(dir) === dir) {
      return undefined;
    }
  }
}

function inside(dir: string, path: string): boolean {
  const rel = relative(dir, path);
  return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
}

/** The lockfiles from the project up to the work tree's root. */
export function lockfiles(root: string, top: string): string[] {
  const found: string[] = [];
  for (let dir = resolve(root); inside(top, dir); dir = dirname(dir)) {
    for (const name of LOCKFILES) {
      const p = join(dir, name);
      if (existsSync(p)) {
        found.push(p);
      }
    }
    if (dir === top || dirname(dir) === dir) {
      break;
    }
  }
  return found;
}

function dependencies(pkg: Record<string, unknown>): string[] {
  const names = new Set<string>();
  for (const field of ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"]) {
    const deps = pkg[field];
    if (deps && typeof deps === "object") {
      for (const name of Object.keys(deps)) {
        names.add(name);
      }
    }
  }
  return [...names];
}

/** The workspace packages the project depends on, directly or through each other: dependencies
 *  whose `node_modules` entry links to a directory of the work tree outside any `node_modules`. */
export function workspacePackages(root: string, top: string): string[] {
  const seen = new Set<string>();
  const queue = [resolve(root)];
  while (queue.length > 0) {
    const dir = queue.pop() as string;
    const pkg = readJson(join(dir, "package.json"));
    if (!pkg) {
      continue;
    }
    for (const name of dependencies(pkg)) {
      let target: string | undefined;
      for (let d = dir; inside(top, d); d = dirname(d)) {
        const link = join(d, "node_modules", name);
        if (existsSync(link)) {
          try {
            target = realpathSync(link);
          } catch {
            target = undefined;
          }
          break;
        }
        if (d === top || dirname(d) === d) {
          break;
        }
      }
      if (
        target &&
        inside(top, target) &&
        !target.split(sep).includes("node_modules") &&
        target !== resolve(root) &&
        !seen.has(target)
      ) {
        seen.add(target);
        queue.push(target);
      }
    }
  }
  return [...seen].toSorted();
}

/** The version of a package as the project resolves it. */
export function versionOf(root: string, name: string): string {
  try {
    const req = createRequire(join(root, "package.json"));
    const pkg = readJson(req.resolve(`${name}/package.json`));
    return typeof pkg?.version === "string" ? pkg.version : "unknown";
  } catch {
    return "unknown";
  }
}

/** A digest of a value, stable across runs and work trees: object keys sorted, functions by their
 *  source, and each of `roots` in a string replaced by a placeholder, so the same configuration in
 *  another work tree has the same digest. */
export function digest(value: unknown, roots: string[] = []): string {
  const seen = new WeakSet<object>();
  const spellings = roots
    .flatMap((r) => [r, r.replaceAll("\\", "/"), r.replaceAll("/", "\\")])
    .toSorted((a, b) => b.length - a.length);
  const norm = (v: unknown): unknown => {
    if (typeof v === "string") {
      return spellings.reduce((s, r) => s.replaceAll(r, "<root>"), v);
    }
    if (typeof v === "function") {
      return `fn:${v.toString()}`;
    }
    if (v instanceof RegExp) {
      return `re:${v.toString()}`;
    }
    if (v && typeof v === "object") {
      if (seen.has(v)) {
        return "[cycle]";
      }
      seen.add(v);
      if (Array.isArray(v)) {
        return v.map(norm);
      }
      const out: Record<string, unknown> = {};
      for (const k of Object.keys(v).toSorted()) {
        out[k] = norm((v as Record<string, unknown>)[k]);
      }
      return out;
    }
    return v;
  };
  return createHash("sha256").update(JSON.stringify(norm(value)) ?? "undefined").digest("hex");
}

/** The environment variables that key the build: the tool's public prefixes, `NODE_ENV` and the
 *  ones the options name. */
export function envNames(prefixes: string[], extra: string[] = []): string[] {
  const names = new Set<string>(["NODE_ENV", ...extra]);
  for (const name of Object.keys(process.env)) {
    if (prefixes.some((p) => name.startsWith(p))) {
      names.add(name);
    }
  }
  return [...names].toSorted();
}

export function projectName(root: string, opts: AscBuildCacheOptions): string {
  if (opts.name) {
    return opts.name;
  }
  const pkg = readJson(join(root, "package.json"));
  return typeof pkg?.name === "string" ? pkg.name : (root.split(sep).pop() ?? "app");
}

/** The arguments of `asc cache restore` for `build`, from the project root. */
export function restoreArgs(build: Build, opts: AscBuildCacheOptions, top: string): string[] {
  const inputs = [
    ".",
    ...lockfiles(build.root, top),
    ...workspacePackages(build.root, top),
    ...(opts.inputs ?? []).map((p) => resolve(build.root, p)),
  ];
  const args = [
    "cache",
    "restore",
    "--json",
    "--tool",
    build.tool,
    "--name",
    projectName(build.root, opts),
    "--inputs",
    ...inputs,
    "--outputs",
    ...build.outputs,
  ];
  const parts = { ...build.parts, plugin: PLUGIN_VERSION };
  for (const [k, v] of Object.entries(parts).toSorted(([a], [b]) => a.localeCompare(b))) {
    args.push("--part", `${k}=${v}`);
  }
  for (const name of envNames(build.envPrefixes, opts.env)) {
    args.push("--env", name);
  }
  if (opts.remote === false) {
    args.push("--no-remote");
  }
  return args;
}

/** Restores the build's outputs on a hit; `undefined` when asc cannot be used. */
export function restore(build: Build, opts: AscBuildCacheOptions): Restored | undefined {
  const top = workTree(build.root);
  if (!top) {
    warn(`${build.root} is not in a work tree`);
    return undefined;
  }
  if (build.outputs.some((o) => !inside(top, resolve(build.root, o)) || resolve(build.root, o) === top)) {
    warn("an output directory is outside the work tree");
    return undefined;
  }
  const args = restoreArgs(build, opts, top);
  if (process.env.ASC_BUILD_CACHE_DEBUG === "1") {
    process.stderr.write(`asc build cache: ${ascBin(opts)} ${args.join(" ")}\n`);
  }
  const r = asc(opts, build.root, args);
  if (r.error || r.status !== 0) {
    warn(r.error ? `cannot run ${ascBin(opts)}: ${r.error.message}` : (r.stderr || "asc cache restore failed").trim());
    return undefined;
  }
  try {
    const out = JSON.parse(r.stdout.trim()) as Restored;
    if (typeof out.hit === "boolean" && typeof out.key === "string") {
      return out;
    }
  } catch {
    // Falls through to the warning.
  }
  warn(`unexpected reply from asc cache restore: ${r.stdout.trim()}`);
  return undefined;
}

/** Stores the outputs of a successful build under the key `restore` gave. */
export function save(build: Build, opts: AscBuildCacheOptions, key: string): void {
  const args = [
    "cache",
    "save",
    "--key",
    key,
    "--tool",
    build.tool,
    "--name",
    projectName(build.root, opts),
    "--outputs",
    ...build.outputs,
  ];
  if (opts.remote === false) {
    args.push("--no-remote");
  }
  const r = asc(opts, build.root, args);
  if (r.error || r.status !== 0) {
    process.stderr.write(
      `asc build cache: the build was not stored: ${(r.error?.message ?? r.stderr ?? "").trim()}\n`,
    );
  }
}

export function note(tool: string, key: string): void {
  if (process.env.ASC_BUILD_CACHE_QUIET !== "1") {
    process.stdout.write(`asc build cache: ${tool} hit ${key.slice(0, 12)}, outputs restored\n`);
  }
}

/** The prefetch profile a build learns into: `<tool>:<project>`, without spaces. */
export function profileName(build: Build, opts: AscBuildCacheOptions): string {
  return `${build.tool}:${projectName(build.root, opts)}`.replace(/\s+/g, "-").slice(0, 200);
}

/** The files among `ids` a profile can hold: absolute paths inside the work tree `top`, outside
 *  `node_modules`, without a query; NUL separated, as `asc prefetch --learn` reads them. */
export function learnInput(ids: Iterable<string>, top: string): string {
  const out = new Set<string>();
  for (const raw of ids) {
    if (raw.startsWith("\0")) {
      continue;
    }
    const id = raw.split("?")[0] ?? "";
    if (!isAbsolute(id) || !inside(top, id) || id.split(/[\\/]/).includes("node_modules")) {
      continue;
    }
    out.add(id);
  }
  return [...out].join("\0");
}

/** Teaches the build's profile the files it read (SPEC §10.2 item 5b). Best effort: a failure
 *  is one line on stderr and never fails the build. */
export function learn(build: Build, opts: AscBuildCacheOptions, ids: Iterable<string>): void {
  if (!(opts.learn ?? process.env.ASC_PREFETCH_LEARN !== "0")) {
    return;
  }
  const top = workTree(build.root);
  if (!top) {
    return;
  }
  const input = learnInput(ids, top);
  if (!input) {
    return;
  }
  const r = asc(opts, build.root, ["prefetch", "--learn", profileName(build, opts), "--replace", "-q"], input);
  if (r.error || r.status !== 0) {
    process.stderr.write(
      `asc build cache: the read files were not learned: ${(r.error?.message ?? r.stderr ?? "").trim()}\n`,
    );
  }
}
