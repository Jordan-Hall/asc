// Hydration guard (SPEC 24.4.11). On a virtual workspace, reading a file body hydrates it, and IDE
// search, indexers and watchers are the usual cause. Two pieces: (1) a one-time exclusions prompt
// that writes `files.watcherExclude`, `search.exclude`, `files.exclude` and
// `search.followSymlinks: false` at workspace scope (checked in with .vscode/settings.json), and
// (2) a poll of `Workspace.Stats` that shows one notification per 24 hours when a single process
// exceeded `asc.vfs.hydrationBudget` hydrations. This module never reads working files, never
// walks the tree and never forwards stats to a webview: its only data source is `HydrationApi`.
// The logic is editor-free; `registerHydration` is the only `vscode` user.
import type * as vscodeTypes from "vscode";

export const EXCLUDE_FOLDERS = [
  "**/node_modules",
  "**/dist",
  "**/.turbo",
  "**/target",
  "**/.next",
  "**/.asc",
] as const;
export const EXCLUDE_KEYS = ["files.watcherExclude", "search.exclude", "files.exclude"] as const;

export const DEFAULT_HYDRATION_BUDGET = 1000;
export const POLL_MS = 60_000;
export const NOTIFY_INTERVAL_MS = 24 * 60 * 60 * 1000;
export const STATE_EXCLUSIONS_ANSWERED = "asc.hydration.exclusionsAnswered";
export const STATE_LAST_NOTIFIED = "asc.hydration.lastNotified";
export const COMMAND_SEARCH = "asc.searchCode";

/** One `HydrationCount` row of `Workspace.Stats`. */
export interface HydrationCount {
  key: string;
  count: number;
}

export interface HydrationStats {
  byPath: readonly HydrationCount[];
  byProcess: readonly HydrationCount[];
}

/** The only call the guard makes. Tests supply a fake. */
export interface HydrationApi {
  stats(root: string, signal?: AbortSignal): Promise<HydrationStats>;
}

/** The workspace-scope settings the prompt reads and writes. */
export interface SettingsApi {
  /** The workspace-scope value only (not user or default), or undefined. */
  getWorkspace(key: string): Record<string, boolean> | boolean | undefined;
  updateWorkspace(key: string, value: unknown): Promise<void>;
}

export interface Memento {
  get<T>(key: string): T | undefined;
  update(key: string, value: unknown): PromiseLike<void>;
}

/** Settings to add, leaving every entry the user already set untouched. */
export function missingExclusions(settings: SettingsApi): Map<string, unknown> {
  const out = new Map<string, unknown>();
  for (const key of EXCLUDE_KEYS) {
    const cur = settings.getWorkspace(key);
    const base = cur !== null && typeof cur === "object" ? cur : {};
    const next: Record<string, boolean> = { ...base };
    let changed = false;
    for (const f of EXCLUDE_FOLDERS) {
      if (!(f in next)) {
        next[f] = true;
        changed = true;
      }
    }
    if (changed) out.set(key, next);
  }
  if (settings.getWorkspace("search.followSymlinks") !== false) {
    out.set("search.followSymlinks", false);
  }
  return out;
}

export interface PromptApi {
  notify(message: string, actions: readonly string[]): Promise<string | undefined>;
}

/**
 * Prompts once per workspace. "Add exclusions" writes at workspace scope; dismissing the prompt
 * (undefined) asks again next session, while "Not now" and "Add exclusions" are remembered.
 */
export async function maybePromptExclusions(
  settings: SettingsApi,
  state: Memento,
  prompt: PromptApi,
): Promise<"written" | "declined" | "skipped" | "dismissed"> {
  if (state.get<boolean>(STATE_EXCLUSIONS_ANSWERED) === true) return "skipped";
  const patch = missingExclusions(settings);
  if (patch.size === 0) {
    await state.update(STATE_EXCLUSIONS_ANSWERED, true);
    return "skipped";
  }
  const picked = await prompt.notify(
    "asc: exclude generated folders (node_modules, dist, target, .asc) from watching and search so they are not hydrated?",
    ["Add exclusions", "Not now"],
  );
  if (picked === "Add exclusions") {
    for (const [k, v] of patch) await settings.updateWorkspace(k, v);
    await state.update(STATE_EXCLUSIONS_ANSWERED, true);
    return "written";
  }
  if (picked === "Not now") {
    await state.update(STATE_EXCLUSIONS_ANSWERED, true);
    return "declined";
  }
  return "dismissed";
}

/** Process images and paths come from the daemon; strip control characters and cap the length. */
export function sanitizeLabel(s: string, max = 80): string {
  let t = "";
  for (const ch of s) {
    const c = ch.codePointAt(0) ?? 0;
    t += c < 0x20 || c === 0x7f || c === 0x2028 || c === 0x2029 ? " " : ch;
  }
  t = t.trim();
  return t.length > max ? `${t.slice(0, max - 1)}…` : t;
}

/** Parent folder of a path key, normalised to forward slashes ("" for a root-level file). */
export function folderOf(key: string): string {
  const p = key.replace(/\\/g, "/").replace(/\/+$/, "");
  const i = p.lastIndexOf("/");
  return i <= 0 ? "" : p.slice(0, i);
}

export function topFolders(byPath: readonly HydrationCount[], n = 3): string[] {
  const m = new Map<string, number>();
  for (const r of byPath) {
    const f = folderOf(r.key);
    if (f !== "") m.set(f, (m.get(f) ?? 0) + r.count);
  }
  return [...m.entries()]
    .toSorted((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : 1))
    .slice(0, n)
    .map(([f]) => f);
}

export interface Offender {
  process: string;
  count: number;
  folders: string[];
}

/** The worst process over budget, or null. */
export function findOffender(s: HydrationStats, budget: number): Offender | null {
  let worst: HydrationCount | null = null;
  for (const p of s.byProcess) {
    if (p.count > budget && (worst === null || p.count > worst.count)) worst = p;
  }
  if (worst === null) return null;
  return { process: sanitizeLabel(worst.key), count: worst.count, folders: topFolders(s.byPath) };
}

export function hydrationMessage(o: Offender): string {
  const where =
    o.folders.length > 0 ? ` Top folders: ${o.folders.map((f) => sanitizeLabel(f)).join(", ")}.` : "";
  return `Search is hydrating files. Use asc: Search code, or exclude folders. Process: ${o.process}.${where}`;
}

/** One poll. Notifies at most once per 24 hours, whatever the number of roots. */
export async function pollHydration(opts: {
  api: HydrationApi;
  roots: readonly string[];
  budget: number;
  state: Memento;
  now: () => number;
  notify(message: string, actions: readonly string[]): Promise<string | undefined>;
  onAction(action: string): void;
  signal?: AbortSignal;
}): Promise<boolean> {
  const last = opts.state.get<number>(STATE_LAST_NOTIFIED);
  const now = opts.now();
  if (typeof last === "number" && now - last >= 0 && now - last < NOTIFY_INTERVAL_MS) return false;
  for (const root of opts.roots) {
    let stats: HydrationStats;
    try {
      stats = await opts.api.stats(root, opts.signal);
    } catch {
      continue; // a stopped daemon is not a hydration problem
    }
    const o = findOffender(stats, opts.budget);
    if (o === null) continue;
    await opts.state.update(STATE_LAST_NOTIFIED, now);
    void opts.notify(hydrationMessage(o), ["Search code", "Add exclusions"]).then((a) => {
      if (a !== undefined) opts.onAction(a);
    });
    return true;
  }
  return false;
}

export interface HydrationRegistration {
  dispose(): void;
}

/**
 * Wires the prompt and the poll. `statsApi` is a bridge-backed `Workspace.Stats` caller; the poll
 * runs once a minute while the window is focused. Nothing here reads a file.
 */
export function registerHydration(
  vscode: typeof vscodeTypes,
  statsApi: HydrationApi,
  roots: readonly string[],
  state: Memento,
  budget: number = DEFAULT_HYDRATION_BUDGET,
): HydrationRegistration {
  const cfg = () => vscode.workspace.getConfiguration();
  const settings: SettingsApi = {
    getWorkspace: (key) => cfg().inspect<Record<string, boolean> | boolean>(key)?.workspaceValue,
    updateWorkspace: (key, value) =>
      Promise.resolve(cfg().update(key, value, vscode.ConfigurationTarget.Workspace)),
  };
  const notify = async (m: string, actions: readonly string[]) =>
    (await vscode.window.showInformationMessage(m, ...actions)) as string | undefined;

  void maybePromptExclusions(settings, state, { notify });

  const ctl = new AbortController();
  const timer = setInterval(() => {
    if (!vscode.window.state.focused) return;
    void pollHydration({
      api: statsApi,
      roots,
      budget,
      state,
      now: Date.now,
      notify,
      signal: ctl.signal,
      onAction: (a) => {
        if (a === "Search code") void vscode.commands.executeCommand(COMMAND_SEARCH);
        else if (a === "Add exclusions") {
          void (async () => {
            for (const [k, v] of missingExclusions(settings)) await settings.updateWorkspace(k, v);
          })();
        }
      },
    });
  }, POLL_MS);
  return {
    dispose() {
      clearInterval(timer);
      ctl.abort();
    },
  };
}
