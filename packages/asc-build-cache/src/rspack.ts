// The Rspack and webpack plugin (docs/designs/build-cache.md §4.2): a plain `rspack build` or
// `webpack` run as one cached action, for projects that use neither Rsbuild nor Rslib. Both apply a
// configured plugin before they read their options, so on a hit the outputs are restored there and
// the compiler builds one empty entry into a scratch directory instead. On a miss it builds and the
// outputs are stored when the run is done. Watch runs and dev servers are never cached.

import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

import {
  type AscBuildCacheOptions,
  type Build,
  digest,
  enabled,
  note,
  restore,
  save,
  watching,
  workTree,
} from "./core.js";

/** The part of an Rspack or webpack `Compiler` the plugin uses; both have this shape. */
export interface CompilerLike {
  options: {
    context?: string;
    mode?: string;
    watch?: boolean;
    entry?: unknown;
    output?: { path?: string; clean?: unknown };
    plugins?: unknown[];
    [key: string]: unknown;
  };
  rspack?: { rspackVersion?: string };
  webpack?: { version?: string };
  hooks: {
    watchRun: {
      tapAsync(name: string, fn: (c: unknown, done: (err?: Error) => void) => void): void;
    };
    done: { tap(name: string, fn: (stats: { hasErrors(): boolean }) => void): void };
  };
}

const NAME = "AscBuildCachePlugin";

/** Options that do not change what a build writes, or that hold live objects (streams). */
const NOT_KEYED = new Set([
  "plugins",
  "infrastructureLogging",
  "stats",
  "watch",
  "watchOptions",
  "devServer",
  "cache",
  "snapshot",
  "profile",
  "parallelism",
]);

/** A dev server or watch run: its compiler stays up, so restored outputs would go stale. */
function serving(): boolean {
  const cmd = process.argv.slice(2).find((a) => !a.startsWith("-"));
  return (
    watching() ||
    cmd === "serve" ||
    cmd === "dev" ||
    cmd === "preview" ||
    process.env.WEBPACK_SERVE === "true" ||
    process.env.RSPACK_SERVE === "true"
  );
}

function pluginName(p: unknown): string {
  if (typeof p === "function") {
    return p.name || "fn";
  }
  if (p && typeof p === "object") {
    const named = (p as { name?: unknown }).name;
    return typeof named === "string" ? named : (p.constructor?.name ?? "");
  }
  return "";
}

/** The environment variables an `EnvironmentPlugin` reads at compile time: its `keys`. */
function environmentKeys(plugins: unknown[]): string[] {
  const names: string[] = [];
  for (const p of plugins) {
    if (p && typeof p === "object" && p.constructor?.name === "EnvironmentPlugin") {
      const keys = (p as { keys?: unknown }).keys;
      if (Array.isArray(keys)) {
        names.push(...keys.filter((k): k is string => typeof k === "string"));
      }
    }
  }
  return names;
}

export class AscBuildCachePlugin {
  readonly name = NAME;
  private readonly opts: AscBuildCacheOptions;

  constructor(opts: AscBuildCacheOptions = {}) {
    this.opts = opts;
  }

  apply(compiler: CompilerLike): void {
    const opts = this.opts;
    if (!enabled(opts) || compiler.options.watch || serving()) {
      return;
    }
    const tool = compiler.rspack?.rspackVersion ? "rspack" : "webpack";
    const version = compiler.rspack?.rspackVersion ?? compiler.webpack?.version ?? "unknown";
    const options = compiler.options;
    const root = resolve(options.context ?? process.cwd());
    const out = resolve(root, options.output?.path ?? "dist");
    const roots = [workTree(root) ?? root];
    const plugins = (options.plugins ?? []).filter((p) => p !== this);
    const keyed: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(options)) {
      if (!NOT_KEYED.has(k)) {
        keyed[k] = v;
      }
    }
    const build: Build = {
      tool,
      root,
      outputs: [out],
      parts: {
        version,
        mode: options.mode ?? process.env.NODE_ENV ?? "production",
        plugins: plugins.map(pluginName).join(","),
        // Each plugin's own options key the build too: DefinePlugin's values, a copy pattern.
        pluginOptions: digest(plugins, roots),
        config: digest(keyed, roots),
      },
      // An exact name is a prefix that matches itself.
      envPrefixes: environmentKeys(plugins),
    };
    const got = restore(build, opts);
    if (!got) {
      return;
    }
    if (got.hit) {
      note(tool, got.key);
      // The options are still as the user gave them (normalised), so the entry and output path set
      // here are the ones the compiler reads: one empty module, written where it does no harm.
      const scratch = join(root, "node_modules", ".cache", "asc-build-cache", tool);
      mkdirSync(scratch, { recursive: true });
      const entry = join(scratch, "empty.js");
      writeFileSync(entry, "export {};\n");
      options.entry = { main: { import: [entry] } };
      options.output = { ...options.output, path: join(scratch, "out"), clean: true };
      // A program that runs the same compiler again in watch mode would see the empty build.
      compiler.hooks.watchRun.tapAsync(NAME, (_c, done) =>
        done(
          new Error(
            "asc build cache: this compiler restored a cached build; run it once, or set ASC_BUILD_CACHE=0",
          ),
        ),
      );
      return;
    }
    // Only a single run is stored: a rebuild in watch mode may read files changed since the key.
    let watched = false;
    let stored = false;
    compiler.hooks.watchRun.tapAsync(NAME, (_c, done) => {
      watched = true;
      done();
    });
    compiler.hooks.done.tap(NAME, (stats) => {
      if (!watched && !stored && !stats.hasErrors()) {
        stored = true;
        save(build, opts, got.key);
      }
    });
  }
}

export default AscBuildCachePlugin;
