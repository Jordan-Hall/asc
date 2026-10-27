// Op log tree (SPEC 24.4.4, 24.4.12): `asc op log --json -n 50`, refreshed on `KIND_OP_FINISHED` with a 500 ms
// trailing debounce, only while visible (otherwise the view is marked stale and refreshes when shown),
// at most one `op log` child at a time and one per 2 s, and "Load more" up to 500 entries. Older
// entries: "open `asc op log` in a terminal". `OpLogModel` is editor-free; `registerOpLog` is the only
// `vscode` user.
import type * as vscodeTypes from "vscode";
import { type CliDirect, OP_LOG_MAX } from "../cli-direct";
import { pump, type ViewApi, type WatchApi } from "./worktrees";

export const PAGE = 50;
export const MAX_ENTRIES = OP_LOG_MAX;
export const DEBOUNCE_MS = 500;
export const MIN_INTERVAL_MS = 2000;

export interface OpEntry {
  id: string;
  /** The redacted argv of the operation, joined with spaces. */
  description: string;
  /** Unix seconds, or 0 when unknown. */
  time: number;
  /** The workspace the operation ran in. */
  user: string;
  undone: boolean;
}

const MAX_TEXT = 2000;
const clip = (s: string): string => (s.length > MAX_TEXT ? `${s.slice(0, MAX_TEXT)}...` : s);

/**
 * Decodes the stdout of `asc op log --json`: `{"version":1,"ops":[...]}` (`OpLogResponse`). Rows without an
 * id are dropped, text is clipped, and anything that is not that object gives [].
 */
export function parseOpLog(text: string | null): OpEntry[] {
  if (text === null || text === "") return [];
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    return [];
  }
  const list = (v as { ops?: unknown } | null)?.ops;
  if (!Array.isArray(list)) return [];
  const out: OpEntry[] = [];
  for (const raw of list) {
    if (typeof raw !== "object" || raw === null) continue;
    const e = raw as Record<string, unknown>;
    const id = typeof e.op === "string" ? e.op : "";
    if (id === "") continue;
    const argv = Array.isArray(e.argv) ? e.argv.filter((a): a is string => typeof a === "string") : [];
    out.push({
      id,
      description: clip(argv.join(" ")),
      time: typeof e.at === "number" && Number.isFinite(e.at) && e.at > 0 ? Math.trunc(e.at) : 0,
      user: typeof e.ws === "string" ? clip(e.ws) : "",
      undone: e.undone === true,
    });
    if (out.length >= MAX_ENTRIES) break;
  }
  return out;
}

export interface OpLogApi {
  /** The newest `limit` operations of the root. */
  read(root: string, limit: number, signal?: AbortSignal): Promise<OpEntry[]>;
}

/** `OpLogApi` over the direct `asc op log --json` read (the bridge cannot carry `op log`). */
export function cliOpLogApi(cli: CliDirect): OpLogApi {
  return {
    async read(root, limit, signal) {
      const r = await cli.run(root, { kind: "op-log", limit }, signal);
      if (r.exit !== 0) throw new Error(`op log exited ${r.exit}`);
      return parseOpLog(r.stdout);
    },
  };
}

export interface OpLogModelOptions {
  root: string;
  api: OpLogApi;
  debounceMs?: number;
  minIntervalMs?: number;
  now?: () => number;
  onError?: (e: unknown) => void;
}

export class OpLogModel {
  entries: OpEntry[] = [];
  private limit = PAGE;
  private visible = false;
  private stale = true;
  private running = false;
  private rerun = false;
  private lastStart = Number.NEGATIVE_INFINITY;
  private debounce: ReturnType<typeof setTimeout> | null = null;
  private gap: ReturnType<typeof setTimeout> | null = null;
  private disposed = false;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly o: OpLogModelOptions) {}

  get root(): string {
    return this.o.root;
  }
  get isStale(): boolean {
    return this.stale;
  }
  get requested(): number {
    return this.limit;
  }
  /** "Load more" shows while the last fetch filled the page and the cap is not reached. */
  get canLoadMore(): boolean {
    return this.limit < MAX_ENTRIES && this.entries.length >= this.limit;
  }
  /** At the cap with a full page: point the user at the terminal. */
  get atCap(): boolean {
    return this.limit >= MAX_ENTRIES && this.entries.length >= MAX_ENTRIES;
  }

  onChange(fn: () => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }

  setVisible(v: boolean): void {
    this.visible = v;
    if (!v) {
      this.clearTimers();
      return;
    }
    if (this.stale) this.schedule(0);
  }

  /** Called for each `Watch` event kind. */
  onWatch(kind: string): void {
    if (kind !== "op-finished") return;
    this.stale = true;
    if (this.visible) this.schedule(this.o.debounceMs ?? DEBOUNCE_MS);
  }

  loadMore(): void {
    if (this.limit >= MAX_ENTRIES) return;
    this.limit = Math.min(MAX_ENTRIES, this.limit + PAGE);
    this.stale = true;
    this.schedule(0);
  }

  private clearTimers(): void {
    if (this.debounce) clearTimeout(this.debounce);
    if (this.gap) clearTimeout(this.gap);
    this.debounce = this.gap = null;
  }

  /** Trailing debounce: each call restarts the wait. */
  private schedule(delay: number): void {
    if (this.disposed) return;
    if (this.debounce) clearTimeout(this.debounce);
    this.debounce = setTimeout(() => {
      this.debounce = null;
      this.tryRun();
    }, delay);
  }

  private tryRun(): void {
    if (this.disposed || !this.visible || !this.stale) return;
    if (this.running) {
      this.rerun = true;
      return;
    }
    const now = (this.o.now ?? Date.now)();
    const wait = (this.o.minIntervalMs ?? MIN_INTERVAL_MS) - (now - this.lastStart);
    if (wait > 0) {
      if (!this.gap) {
        this.gap = setTimeout(() => {
          this.gap = null;
          this.tryRun();
        }, wait);
      }
      return;
    }
    void this.fetch(now);
  }

  private async fetch(startedAt: number): Promise<void> {
    this.running = true;
    this.lastStart = startedAt;
    const asked = this.limit;
    let rows: OpEntry[] | null = null;
    try {
      rows = await this.o.api.read(this.o.root, asked);
    } catch (e) {
      this.o.onError?.(e);
    }
    this.running = false;
    if (this.disposed) return;
    if (rows !== null) {
      this.entries = rows.slice(0, asked);
      if (asked === this.limit && !this.rerun) this.stale = false;
      for (const l of this.listeners) l();
    }
    if (this.rerun) {
      this.rerun = false;
      this.tryRun();
    } else if (this.stale && this.visible && rows !== null) {
      this.schedule(0);
    }
  }

  dispose(): void {
    this.disposed = true;
    this.clearTimers();
    this.listeners.clear();
  }
}

export function describeOp(e: OpEntry): { label: string; description: string; tooltip: string } {
  const first = e.description.split("\n", 1)[0] ?? "";
  const when =
    e.time > 0 ? new Date(e.time * 1000).toISOString().replace("T", " ").slice(0, 16) : "";
  return {
    label: (first === "" ? e.id.slice(0, 12) : first) + (e.undone ? " (undone)" : ""),
    description: [e.id.slice(0, 12), e.user, when].filter(Boolean).join("  "),
    tooltip: e.description,
  };
}

// --- the editor adapter ---------------------------------------------------------------------------

export function registerOpLog(
  vscode: typeof vscodeTypes,
  api: ViewApi & WatchApi,
  opLog: OpLogApi,
  roots: readonly string[],
  log: (line: string) => void,
): { dispose(): void } {
  const subs: { dispose(): unknown }[] = [];
  const ac = new AbortController();
  const models = roots.map(
    (root) => new OpLogModel({ root, api: opLog, onError: (e) => log(`asc op log: ${String(e)}`) }),
  );
  const changed = new vscode.EventEmitter<void>();
  subs.push(changed);
  for (const m of models) {
    subs.push(m.onChange(() => changed.fire()));
    void pump(
      api,
      m.root,
      ac.signal,
      (k) => m.onWatch(k),
      (e) => log(`asc watch: ${String(e)}`),
    );
  }
  type Node =
    | { kind: "op"; entry: OpEntry }
    | { kind: "more"; model: OpLogModel }
    | { kind: "cap" };
  const provider: vscodeTypes.TreeDataProvider<Node> = {
    onDidChangeTreeData: changed.event,
    getChildren(el) {
      if (el) return [];
      const out: Node[] = [];
      for (const m of models) {
        for (const entry of m.entries) out.push({ kind: "op", entry });
        if (m.canLoadMore) out.push({ kind: "more", model: m });
        else if (m.atCap) out.push({ kind: "cap" });
      }
      return out;
    },
    getTreeItem(n) {
      if (n.kind === "more") {
        const item = new vscode.TreeItem("Load more", vscode.TreeItemCollapsibleState.None);
        item.command = { command: "asc.oplog.loadMore", title: "Load more", arguments: [n] };
        return item;
      }
      if (n.kind === "cap") {
        return new vscode.TreeItem(
          "Older entries: open `asc op log` in a terminal",
          vscode.TreeItemCollapsibleState.None,
        );
      }
      const d = describeOp(n.entry);
      const item = new vscode.TreeItem(d.label, vscode.TreeItemCollapsibleState.None);
      item.description = d.description;
      item.tooltip = d.tooltip;
      return item;
    },
  };
  const view = vscode.window.createTreeView("asc.oplog", { treeDataProvider: provider });
  subs.push(
    view,
    view.onDidChangeVisibility((e) => {
      for (const m of models) m.setVisible(e.visible);
    }),
    vscode.commands.registerCommand("asc.oplog.loadMore", (n?: Node) => {
      if (n?.kind === "more") n.model.loadMore();
    }),
  );
  for (const m of models) m.setVisible(view.visible);
  return {
    dispose() {
      ac.abort();
      for (const m of models) m.dispose();
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
