// The Vite plugin (docs/designs/build-cache.md §4.2): `vite build` as one cached action. On a hit
// the outputs are restored before Vite reads a module, and the build that follows has one empty
// virtual module and writes nothing. On a miss Vite builds and the outputs are stored when the
// last bundle is written.

import { resolve } from "node:path";

import type { BuildEnvironmentOptions, Plugin, UserConfig } from "vite";

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

const VIRTUAL = "asc-build-cache:hit";

/** The plugin names, in order; the plugins' code is in the inputs or the lockfile. */
function pluginNames(plugins: UserConfig["plugins"]): string[] {
  const names: string[] = [];
  const walk = (p: unknown) => {
    if (Array.isArray(p)) {
      p.forEach(walk);
    } else if (p && typeof p === "object" && typeof (p as { name?: unknown }).name === "string") {
      names.push((p as { name: string }).name);
    }
  };
  walk(plugins ?? []);
  return names;
}

export function ascBuildCache(opts: AscBuildCacheOptions = {}): Plugin {
  let build: Build | undefined;
  let key: string | undefined;
  let failed = false;
  let wrote = false;
  let hit = false;
  return {
    name: "asc-build-cache",
    apply: "build",
    enforce: "pre",
    config(user, env) {
      if (!enabled(opts) || watching() || user.build?.watch) {
        return undefined;
      }
      const root = resolve(user.root ?? process.cwd());
      const outDirs = new Set([resolve(root, user.build?.outDir ?? "dist")]);
      for (const e of Object.values(user.environments ?? {})) {
        if (e.build?.outDir) {
          outDirs.add(resolve(root, e.build.outDir));
        }
      }
      const { plugins, ...rest } = user;
      const prefixes = user.envPrefix ?? "VITE_";
      build = {
        tool: "vite",
        root,
        outputs: [...outDirs],
        parts: {
          version: this.meta.viteVersion,
          mode: env.mode,
          plugins: pluginNames(plugins).join(","),
          config: digest(rest, [workTree(root) ?? root]),
        },
        envPrefixes: Array.isArray(prefixes) ? prefixes : [prefixes],
      };
      const got = restore(build, opts);
      if (!got) {
        build = undefined;
        return undefined;
      }
      key = got.key;
      if (!got.hit) {
        return undefined;
      }
      hit = true;
      note("vite", got.key);
      const empty: BuildEnvironmentOptions = {
        write: false,
        emptyOutDir: false,
        copyPublicDir: false,
        lib: false,
        rollupOptions: { input: VIRTUAL },
        ...(user.build?.ssr ? { ssr: VIRTUAL } : {}),
      };
      const environments: Record<string, { build: BuildEnvironmentOptions }> = {};
      for (const name of Object.keys(user.environments ?? {})) {
        environments[name] = { build: empty };
      }
      return { build: empty, ...(user.environments ? { environments } : {}) };
    },
    resolveId(id) {
      return hit && id === VIRTUAL ? `\0${VIRTUAL}` : undefined;
    },
    load(id) {
      return hit && id === `\0${VIRTUAL}` ? "export {};\n" : undefined;
    },
    buildEnd(err) {
      if (err) {
        failed = true;
      }
    },
    writeBundle() {
      wrote = true;
    },
    // Called once per build (each environment); the last call stores every output directory.
    closeBundle() {
      if (build && key && !hit && wrote && !failed) {
        save(build, opts, key);
      }
    },
  };
}

export default ascBuildCache;
