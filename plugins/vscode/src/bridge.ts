// One bridge per window (SPEC 24.4.1): a lazily spawned `asc api --stdio` child, `Hello` for roots and
// features, and the minimum-feature check. Gating uses `Hello.features`, never a version number.
// The command line carries no secret: the bridge finds credentials itself.
import { statSync } from "node:fs";
import { posix, win32 } from "node:path";
import {
  type BridgeNode,
  type BridgeNodeOptions,
  type BridgeState,
  bridge,
  bridgeNode,
  callUnary,
} from "@asc/asc-client";

/** Needed for any use; `plugin.v1` alone keeps read-only views working. */
export const FEATURE_PLUGIN = "plugin.v1";
export const FEATURE_JSON_VERBS = "json-verbs.v1";

export type Compat =
  /** Everything the plugin needs is present. */
  | { level: "full" }
  /** `plugin.v1` only: read-only views work, mutating commands are disabled. */
  | { level: "read-only"; missing: string[] }
  /** Not even `plugin.v1`: show "Update asc". */
  | { level: "too-old"; missing: string[] };

export function checkFeatures(features: readonly string[]): Compat {
  const has = new Set(features);
  const missing = [FEATURE_PLUGIN, FEATURE_JSON_VERBS].filter((f) => !has.has(f));
  if (missing.length === 0) return { level: "full" };
  if (has.has(FEATURE_PLUGIN)) return { level: "read-only", missing };
  return { level: "too-old", missing };
}

export interface AscPathInputs {
  setting: string;
  path: string | undefined;
  ascHome: string | undefined;
  home: string | undefined;
  platform: NodeJS.Platform;
  isFile: (p: string) => boolean;
}

/** `asc.path`, else `PATH`, else `$ASC_HOME/bin` (default `~/.asc`). Returns the bare name if none exists. */
export function resolveAsc(i: AscPathInputs): string {
  const join = i.platform === "win32" ? win32.join : posix.join;
  if (i.setting.trim() !== "") return i.setting.trim();
  const exe = i.platform === "win32" ? "asc.exe" : "asc";
  const dirs = (i.path ?? "").split(i.platform === "win32" ? ";" : ":").filter(Boolean);
  const ascHome = i.ascHome || (i.home ? join(i.home, ".asc") : "");
  if (ascHome) dirs.push(join(ascHome, "bin"));
  for (const d of dirs) {
    const candidate = join(d, exe);
    if (i.isFile(candidate)) return candidate;
  }
  return exe;
}

export function nodeIsFile(p: string): boolean {
  try {
    return statSync(p).isFile();
  } catch {
    return false;
  }
}

export interface HelloInfo {
  ascVersion: string;
  features: string[];
  roots: { root: string; wsId: string }[];
  compat: Compat;
}

export interface BridgeManagerOptions {
  /** Resolved on every start so a changed `asc.path` takes effect on restart. */
  command: () => string;
  env?: Record<string, string | undefined>;
  onState?: (s: BridgeState) => void;
  onStderr?: (line: string) => void;
  /** For tests: replaces the process-spawning transport. */
  create?: (o: BridgeNodeOptions) => BridgeNode;
}

export interface BridgeManager {
  readonly started: boolean;
  /** Spawns on first use; later calls share the node. */
  node(): BridgeNode;
  /** `Hello`, cached until `restart`. */
  hello(): Promise<HelloInfo>;
  restart(): void;
  dispose(): void;
}

export function createBridgeManager(opts: BridgeManagerOptions): BridgeManager {
  const create = opts.create ?? bridgeNode;
  let current: BridgeNode | null = null;
  let helloP: Promise<HelloInfo> | null = null;

  const node = (): BridgeNode => {
    if (!current) {
      current = create({
        command: opts.command(),
        ...(opts.env === undefined ? {} : { env: opts.env }),
        ...(opts.onState === undefined ? {} : { onState: opts.onState }),
        ...(opts.onStderr === undefined ? {} : { onStderr: opts.onStderr }),
      });
    }
    return current;
  };

  const stop = () => {
    // Closing the node ends the child's stdin (EOF), which is the bridge's shutdown signal.
    current?.close();
    current = null;
    helloP = null;
  };

  return {
    get started() {
      return current !== null;
    },
    node,
    hello() {
      helloP ??= callUnary(node(), bridge.Bridge.method.hello, {}).then((r) => ({
        ascVersion: r.ascVersion,
        features: r.features,
        roots: r.roots.map((x) => ({ root: x.root, wsId: x.wsId })),
        compat: checkFeatures(r.features),
      }));
      // A failed Hello must not stick: the next caller retries.
      helloP.catch(() => {
        helloP = null;
      });
      return helloP;
    },
    restart: stop,
    dispose: stop,
  };
}
