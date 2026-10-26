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
/** Host-neutral asc serve services; each one is registered only when Hello advertises it. */
export const FEATURE_INBOX = "inbox.v1";
export const FEATURE_REVIEW = "review.v1";
export const FEATURE_CI = "ci.v1";

/** The context key each server feature sets, so contributions enable on the same condition the code registers on. */
export const FEATURE_CONTEXT_KEYS = {
  inbox: "asc.feature.inbox",
  review: "asc.feature.review",
  ci: "asc.feature.ci",
} as const;

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
  /**
   * The open workspace folders. A candidate inside one is never used: a repository can plant an
   * `asc.exe` (or `asc.cmd`) there, and Windows would run it.
   */
  workspaceRoots?: readonly string[];
}

/** An absolute path that does not depend on the current directory or the current drive. */
export function isAbsoluteExe(p: string, platform: NodeJS.Platform): boolean {
  if (p === "" || p.includes("\0")) return false;
  // A drive letter with a separator, or a UNC share; "\dir", "C:dir" and "dir" depend on the current directory.
  if (platform === "win32") return /^[A-Za-z]:[\\/]/.test(p) || /^\\\\[^\\/?]+[\\/][^\\/]+/.test(p);
  return p.startsWith("/");
}

function inside(child: string, parent: string, platform: NodeJS.Platform): boolean {
  const norm = platform === "win32" ? win32 : posix;
  const fold = (x: string) => (platform === "win32" ? x.toLowerCase() : x);
  const rel = fold(norm.relative(norm.resolve(parent), norm.resolve(child)));
  if (rel === "") return true;
  return !rel.startsWith("..") && !norm.isAbsolute(rel);
}

/**
 * The absolute path of the `asc` executable, or null when there is none to run. Only an absolute path
 * outside the workspace folders is ever returned, so a spawn never searches the current directory or a
 * folder a repository controls:
 *   - `asc.path` (machine scope) when it is absolute and a file; a bare name in it is looked up the way
 *     `PATH` is, below; any other form (relative, with a separator) is refused;
 *   - else the absolute `PATH` entries, then `$ASC_HOME/bin` (default `~/.asc/bin`).
 * Relative `PATH` entries (`.`, empty) are skipped.
 */
export function resolveAsc(i: AscPathInputs): string | null {
  const join = i.platform === "win32" ? win32.join : posix.join;
  const roots = i.workspaceRoots ?? [];
  const usable = (p: string) =>
    isAbsoluteExe(p, i.platform) && !roots.some((r) => inside(p, r, i.platform)) && i.isFile(p);
  const win = i.platform === "win32";
  const setting = i.setting.trim();
  let name = win ? "asc.exe" : "asc";
  if (setting !== "") {
    if (isAbsoluteExe(setting, i.platform)) return usable(setting) ? setting : null;
    if (/[\\/:]/.test(setting) || setting.includes("\0")) return null;
    name = win && !/\.exe$/i.test(setting) ? `${setting}.exe` : setting;
  }
  const dirs = (i.path ?? "")
    .split(win ? ";" : ":")
    .map((d) => (win ? d.replace(/^"(.*)"$/, "$1") : d))
    .filter((d) => isAbsoluteExe(d, i.platform));
  const ascHome = i.ascHome || (i.home ? join(i.home, ".asc") : "");
  if (isAbsoluteExe(ascHome, i.platform)) dirs.push(join(ascHome, "bin"));
  for (const d of dirs) {
    const candidate = join(d, name);
    if (usable(candidate)) return candidate;
  }
  return null;
}

export function nodeIsFile(p: string): boolean {
  try {
    return statSync(p).isFile();
  } catch {
    return false;
  }
}

export interface HelloRoot {
  root: string;
  wsId: string;
  /** `owner/repo` on the asc server; empty when the root has none. */
  repo: string;
  /** `none`, `asc` or `git`. */
  remoteKind: string;
  /** The remote URL with credentials removed; empty when the root has no remote. */
  remoteUrl: string;
}

export interface HelloInfo {
  ascVersion: string;
  features: string[];
  roots: HelloRoot[];
  compat: Compat;
}

/** A root whose remote is an asc server: the only place Inbox, Review and Ci calls can be answered. */
export interface ServerTarget {
  root: string;
  repo: string;
}

export function serverTargets(h: Pick<HelloInfo, "roots">): ServerTarget[] {
  return h.roots
    .filter((r) => r.wsId !== "" && r.remoteKind === "asc" && r.repo !== "")
    .map((r) => ({ root: r.root, repo: r.repo }));
}

/**
 * The `owner/repo` an asc server knows the root by, for the `Repo.*` calls; null when the root has no asc
 * server (a git remote or a local-only repository), where those calls cannot be answered.
 */
export async function serverRepoFor(
  mgr: Pick<BridgeManager, "hello">,
  root: string,
): Promise<string | null> {
  const h = await mgr.hello();
  return serverTargets(h).find((t) => t.root === root)?.repo ?? null;
}

export interface ServerFeatures {
  inbox: boolean;
  review: boolean;
  ci: boolean;
}

/** Which server services to register: the flag is in `Hello.features` and a root has an asc server. */
export function serverFeatures(h: Pick<HelloInfo, "features" | "roots">): ServerFeatures {
  const has = new Set(h.features);
  const server = serverTargets(h).length > 0;
  return {
    inbox: server && has.has(FEATURE_INBOX),
    review: server && has.has(FEATURE_REVIEW),
    ci: server && has.has(FEATURE_CI),
  };
}

export interface BridgeManagerOptions {
  /** Resolved on every start so a changed `asc.path` takes effect on restart. Null: no executable to run. */
  command: () => string | null;
  /** The child's whole environment, read on every spawn so a changed token takes effect on restart. */
  env?: () => Record<string, string | undefined>;
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
      const command = opts.command();
      // Spawn an absolute path or nothing: a bare name is looked up in the current directory first on Windows.
      if (command === null || !isAbsoluteExe(command, process.platform)) {
        throw new Error("asc was not found; set asc.path to the absolute path of the executable");
      }
      current = create({
        command,
        ...(opts.env === undefined ? {} : { env: opts.env() }),
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
        roots: r.roots.map((x) => ({
          root: x.root,
          wsId: x.wsId,
          repo: x.repo,
          remoteKind: x.remoteKind,
          remoteUrl: x.remoteUrl,
        })),
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
