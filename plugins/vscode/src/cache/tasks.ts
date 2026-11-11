// Cached builds as tasks (docs/designs/build-cache.md §6): a `TaskProvider` of type `asc-build`.
// Detected tasks build the work tree with the cache in front of the tool: cargo with asc as
// `RUSTC_WRAPPER`, go with asc as `GOCACHEPROG`, and each package.json script that runs
// `rsbuild build`, `rslib build`, `rspack build`, `webpack` or `vite build` (the `@asc/build-cache`
// plugin in its config does the caching). A task the user declares in tasks.json with `inputs`, `outputs` and `command` runs
// through `asc cache run`. Each task carries the variables of `asc cache env`, so turbo, Nx and
// Maven reach the local fronts. Editor-free; `register.ts` turns a spec into a `vscode.Task`.
import { posix } from "node:path";
import type { CacheSetupTool } from "../cli-direct";

export const BUILD_TASK_TYPE = "asc-build";

/** What a detected or declared task runs. */
export interface BuildSpec {
  /** Stable within the root: `cargo`, `go`, `script:<name>` or `run:<name>`. */
  id: string;
  label: string;
  detail: string;
  /** The executable; `asc` is replaced by the absolute asc path when the task is made. */
  command: string;
  args: string[];
  env: Record<string, string>;
  /** The problem matchers, by their `$name`. */
  matchers: string[];
}

/** The tasks.json definition of type `asc-build`. */
export interface BuildTaskDefinition {
  type: typeof BUILD_TASK_TYPE;
  /** A detected build: `cargo`, `go` or `script:<name>`. */
  build?: string;
  /** A declared `asc cache run` action. */
  name?: string;
  inputs?: string[];
  outputs?: string[];
  command?: string[];
  env?: string[];
}

/** The reads detection needs, relative to the root. */
export interface RootFiles {
  exists(rel: string): Promise<boolean>;
  /** The file's text, or null when it is missing or too large. */
  read(rel: string): Promise<string | null>;
}

const TOOL_RUN = /(^|[\s;&|(])(rsbuild|rslib|rspack|webpack|vite)\s+build(\s|$)/;
/** webpack-cli builds without a command too (`webpack`, `webpack --mode production`). */
const WEBPACK_BARE = /(^|[\s;&|(])webpack(?=\s*$|\s*[;&|)]|\s+-)/;
/** A watch flag: that run stays up and is not cached. */
const WATCH_FLAG = /\s(?:--watch(?!=false)|-w)(?=\s|=|$)/;
const SCRIPT_NAME = /^[A-Za-z0-9:_.@/-]{1,64}$/;
const ACTION_NAME = /^[A-Za-z0-9:_.-]{1,64}$/;
const ENV_NAME = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;
const MAX_ARGS = 64;

/** The package manager the lockfile names; npm without one. */
export async function packageManager(files: RootFiles): Promise<"bun" | "pnpm" | "yarn" | "npm"> {
  if ((await files.exists("bun.lock")) || (await files.exists("bun.lockb"))) return "bun";
  if (await files.exists("pnpm-lock.yaml")) return "pnpm";
  if (await files.exists("yarn.lock")) return "yarn";
  return "npm";
}

/** The scripts of package.json that run a tool the build-cache plugin serves, by name. */
export function bundlerScripts(pkgJson: string | null): { name: string; tool: string }[] {
  if (pkgJson === null) return [];
  let v: unknown;
  try {
    v = JSON.parse(pkgJson);
  } catch {
    return [];
  }
  const scripts = (v as { scripts?: unknown } | null)?.scripts;
  if (typeof scripts !== "object" || scripts === null || Array.isArray(scripts)) return [];
  const out: { name: string; tool: string }[] = [];
  for (const [name, body] of Object.entries(scripts as Record<string, unknown>)) {
    if (!SCRIPT_NAME.test(name) || typeof body !== "string" || body.length > 2000) continue;
    const m = TOOL_RUN.exec(body);
    if (m?.[2] !== undefined) out.push({ name, tool: m[2] });
    else if (WEBPACK_BARE.test(body) && !WATCH_FLAG.test(body)) out.push({ name, tool: "webpack" });
  }
  return out.toSorted((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
}

/** The builds the root's files suggest. `asc` is the absolute asc path. */
export async function detectBuilds(files: RootFiles, asc: string): Promise<BuildSpec[]> {
  const out: BuildSpec[] = [];
  if (await files.exists("Cargo.toml")) {
    out.push({
      id: "cargo",
      label: "cargo build",
      detail: "cargo build with the build cache as RUSTC_WRAPPER",
      command: "cargo",
      args: ["build"],
      // Incremental crates are not cacheable: asc runs them uncached, so turn it off.
      env: { RUSTC_WRAPPER: asc, CARGO_INCREMENTAL: "0" },
      matchers: ["$asc-rustc"],
    });
  }
  if (await files.exists("go.mod")) {
    out.push({
      id: "go",
      label: "go build ./...",
      detail: "go build with the build cache as GOCACHEPROG",
      command: "go",
      args: ["build", "./..."],
      env: { GOCACHEPROG: `${asc} cache go` },
      matchers: ["$asc-go"],
    });
  }
  const scripts = bundlerScripts(await files.read("package.json"));
  if (scripts.length > 0) {
    const pm = await packageManager(files);
    for (const s of scripts) {
      out.push({
        id: `script:${s.name}`,
        label: `${pm} run ${s.name}`,
        detail: `${s.tool} build; @asc/build-cache in its config caches it`,
        command: pm,
        args: ["run", s.name],
        env: {},
        matchers: ["$asc-build"],
      });
    }
  }
  return out;
}

/** A repo-relative path that stays inside the root, normalised; undefined otherwise. */
export function insideRoot(rel: string): string | undefined {
  if (typeof rel !== "string" || rel === "" || rel.length > 1024 || rel.includes("\0"))
    return undefined;
  const r = rel.replaceAll("\\", "/");
  if (r.startsWith("/") || /^[A-Za-z]:/.test(r)) return undefined;
  const n = posix.normalize(r);
  if (n === ".." || n.startsWith("../")) return undefined;
  return n.replace(/\/$/, "");
}

const list = (v: unknown): string[] | undefined =>
  Array.isArray(v) && v.length <= MAX_ARGS && v.every((s) => typeof s === "string")
    ? (v as string[])
    : undefined;

/** The `asc cache run` spec of a declared task, or undefined when a field is out of range. */
export function declaredRun(d: Partial<BuildTaskDefinition>): BuildSpec | undefined {
  if (typeof d.name !== "string" || !ACTION_NAME.test(d.name)) return undefined;
  const command = list(d.command);
  const outputs = list(d.outputs)?.map(insideRoot);
  const inputs = list(d.inputs ?? ["."])?.map(insideRoot);
  const env = list(d.env ?? []);
  if (
    command === undefined ||
    command.length === 0 ||
    command.some((a) => a === "" || a.length > 4096)
  ) {
    return undefined;
  }
  if (
    outputs === undefined ||
    outputs.length === 0 ||
    outputs.some((p) => p === undefined || p === ".")
  ) {
    return undefined;
  }
  if (inputs === undefined || inputs.length === 0 || inputs.some((p) => p === undefined)) {
    return undefined;
  }
  if (env === undefined || !env.every((e) => ENV_NAME.test(e))) return undefined;
  const args = [
    "cache",
    "run",
    "--name",
    d.name,
    "--inputs",
    ...(inputs as string[]),
    "--outputs",
    ...(outputs as string[]),
    ...env.flatMap((e) => ["--env", e]),
    "--",
    ...command,
  ];
  return {
    id: `run:${d.name}`,
    label: d.name,
    detail: `asc cache run: ${command.join(" ")}`,
    command: "asc",
    args,
    env: {},
    matchers: ["$asc-rustc", "$asc-build"],
  };
}

/** The definition a detected spec is saved under in tasks.json. */
export function definitionOf(s: BuildSpec): BuildTaskDefinition {
  return { type: BUILD_TASK_TYPE, build: s.id };
}

/** The tools `asc cache setup` can configure that the root's files show it uses, in setup order. */
export async function detectSetupTools(files: RootFiles): Promise<CacheSetupTool[]> {
  const any = async (names: string[]) => {
    for (const n of names) if (await files.exists(n)) return true;
    return false;
  };
  const found: CacheSetupTool[] = [];
  if (await any(["Cargo.toml"])) found.push("cargo");
  if (await any(["go.mod", "go.work"])) found.push("go");
  if (await any(["turbo.json", "turbo.jsonc"])) found.push("turbo");
  if (await any(["nx.json"])) found.push("nx");
  if (await any(["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel"])) found.push("bazel");
  if (await any(["settings.gradle", "settings.gradle.kts", "build.gradle", "build.gradle.kts"])) {
    found.push("gradle");
  }
  if (await any(["pom.xml"])) found.push("maven");
  if (await any(["CMakeLists.txt", "meson.build", "Makefile"])) found.push("ccache");
  return found;
}
