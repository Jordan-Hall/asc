// The Rsbuild and Rslib plugin (docs/designs/build-cache.md §4.2): `rsbuild build` and
// `rslib build` as one cached action. On a hit the outputs are restored before Rspack reads a
// module, and each environment builds one empty entry into a scratch directory instead. On a miss
// Rsbuild builds and the outputs are stored after the build.

import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

import type { OutputConfig, RsbuildPlugin } from "@rsbuild/core";

import {
  type AscBuildCacheOptions,
  type Build,
  digest,
  enabled,
  note,
  restore,
  save,
  versionOf,
  watching,
  workTree,
} from "./core.js";

function distRoot(output: OutputConfig | undefined): string | undefined {
  const d = output?.distPath;
  return typeof d === "string" ? d : d?.root;
}

export function pluginAscBuildCache(opts: AscBuildCacheOptions = {}): RsbuildPlugin {
  return {
    name: "asc:build-cache",
    setup(api) {
      if (!enabled(opts) || api.context.action !== "build" || watching()) {
        return;
      }
      const tool = api.context.callerName === "rslib" ? "rslib" : "rsbuild";
      let build: Build | undefined;
      let key: string | undefined;
      let hit = false;

      api.modifyRsbuildConfig((config) => {
        const root = resolve(api.context.rootPath);
        const outDirs = new Set([resolve(root, distRoot(config.output) ?? "dist")]);
        for (const e of Object.values(config.environments ?? {})) {
          const d = distRoot(e.output);
          if (d) {
            outDirs.add(resolve(root, d));
          }
        }
        const { plugins, ...rest } = config;
        build = {
          tool,
          root,
          outputs: [...outDirs],
          parts: {
            version: api.context.version,
            ...(tool === "rslib" ? { rslib: versionOf(root, "@rslib/core") } : {}),
            mode: process.env.NODE_ENV ?? "production",
            plugins: (plugins ?? [])
              .map((p) => (p && typeof p === "object" && "name" in p ? String(p.name) : ""))
              .join(","),
            config: digest(rest, [workTree(root) ?? root]),
          },
          envPrefixes: ["PUBLIC_"],
        };
        const got = restore(build, opts);
        if (!got) {
          build = undefined;
          return config;
        }
        key = got.key;
        if (got.hit) {
          hit = true;
          note(tool, got.key);
        }
        return config;
      });

      // A hit builds an empty entry where it does no harm: the restored outputs stay as they are.
      api.modifyEnvironmentConfig((config, { name }) => {
        if (!hit) {
          return config;
        }
        const scratch = join(api.context.rootPath, "node_modules", ".cache", "asc-build-cache", name);
        mkdirSync(scratch, { recursive: true });
        const entry = join(scratch, "empty.js");
        writeFileSync(entry, "export {};\n");
        // The environment's config is normalised: `distPath` is an object with every field.
        config.source.entry = { index: entry };
        config.output.distPath = { ...config.output.distPath, root: join(scratch, "out") };
        config.output.cleanDistPath = true;
        config.output.copy = [];
        return config;
      });

      api.onAfterBuild(({ isWatch, stats }) => {
        if (build && key && !hit && !isWatch && !stats?.hasErrors()) {
          save(build, opts, key);
        }
      });
    },
  };
}

export default pluginAscBuildCache;
