// File History tree (SPEC 24.4.4), in Explorer: `Workspace.Log{paths, follow, with_stats, limit: 50}` for
// the active editor, refreshed on active editor change and `KIND_HEAD`, fetched only while visible.
// File mode follows renames; line mode (the editor selection) uses `Repo.LineHistory`. Pages of 50 with a
// "Load more" item. `FileHistoryModel` is editor-free; `registerFileHistory` is the only `vscode` user.
import { callStream, toHex } from "@asc/asc-client";
import { Workspace } from "@asc/asc-client/workspace";
import { Repo } from "@asc/asc-client/repo";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "../bridge";
import { toAscUri } from "../fs/ascfs";
import { bridgeWatch, pump, type WatchApi, wsIdFor } from "./worktrees";

export const HISTORY_PAGE = 50;

export interface HistoryEntry {
  /** Full commit id (hex). */
  id: string;
  parent: string | null;
  subject: string;
  author: string;
  /** Unix seconds. */
  time: number;
  added: number;
  removed: number;
}

export interface LineRange {
  start: number;
  end: number;
}

export interface HistoryQuery {
  /** Repo-relative path, forward slashes. */
  path: string;
  lines: LineRange | null;
  limit: number;
  pageToken: string;
}

export interface HistoryPage {
  entries: HistoryEntry[];
  next: string;
}

export interface HistoryApi {
  history(root: string, q: HistoryQuery, signal?: AbortSignal): Promise<HistoryPage>;
}

export interface FileHistoryOptions {
  api: HistoryApi;
  onError?: (e: unknown) => void;
}

export interface Target {
  root: string;
  path: string;
}

export class FileHistoryModel {
  entries: HistoryEntry[] = [];
  mode: "file" | "line" = "file";
  private target: Target | null = null;
  private lines: LineRange | null = null;
  private next = "";
  private visible = false;
  private stale = true;
  private seq = 0;
  private abort: AbortController | null = null;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly o: FileHistoryOptions) {}

  get isStale(): boolean {
    return this.stale;
  }
  get hasMore(): boolean {
    return this.next !== "";
  }
  get current(): Target | null {
    return this.target;
  }

  onChange(fn: () => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }

  setVisible(v: boolean): void {
    this.visible = v;
    if (v && this.stale) void this.load(false);
  }

  /** The active editor changed (null: no asc file). */
  setTarget(t: Target | null): void {
    const same = this.target?.root === t?.root && this.target?.path === t?.path;
    if (same) return;
    this.target = t;
    this.lines = null;
    this.mode = "file";
    this.invalidate();
  }

  /** Line mode follows the selection; null returns to file mode. */
  setLines(r: LineRange | null): void {
    const same = this.lines?.start === r?.start && this.lines?.end === r?.end;
    if (same) return;
    this.lines = r;
    this.mode = r === null ? "file" : "line";
    this.invalidate();
  }

  /** `KIND_HEAD` or another reason the list may have moved. */
  onWatch(kind: string): void {
    if (kind === "head") this.invalidate();
  }

  loadMore(): void {
    if (this.next !== "") void this.load(true);
  }

  private invalidate(): void {
    this.stale = true;
    if (this.visible) void this.load(false);
  }

  private async load(more: boolean): Promise<void> {
    const t = this.target;
    const mine = ++this.seq;
    this.abort?.abort();
    if (t === null) {
      this.entries = [];
      this.next = "";
      this.stale = false;
      this.fire();
      return;
    }
    const ac = new AbortController();
    this.abort = ac;
    try {
      const page = await this.o.api.history(
        t.root,
        {
          path: t.path,
          lines: this.mode === "line" ? this.lines : null,
          limit: HISTORY_PAGE,
          pageToken: more ? this.next : "",
        },
        ac.signal,
      );
      if (mine !== this.seq) return;
      this.entries = more ? [...this.entries, ...page.entries] : page.entries;
      this.next = page.next;
      this.stale = false;
      this.fire();
    } catch (e) {
      if (mine === this.seq && !ac.signal.aborted) this.o.onError?.(e);
    }
  }

  private fire(): void {
    for (const l of this.listeners) l();
  }

  dispose(): void {
    this.abort?.abort();
    this.listeners.clear();
  }
}

export function describeEntry(e: HistoryEntry): {
  label: string;
  description: string;
  tooltip: string;
} {
  const when = e.time > 0 ? new Date(e.time * 1000).toISOString().slice(0, 10) : "";
  const stat = e.added + e.removed > 0 ? `+${e.added} -${e.removed}` : "";
  return {
    label: e.subject === "" ? e.id.slice(0, 12) : e.subject,
    description: [e.author, when, stat].filter(Boolean).join("  "),
    tooltip: `${e.id}\n${e.author}`,
  };
}

// --- the bridge adapter ---------------------------------------------------------------------------

const dec = new TextDecoder();
const enc = new TextEncoder();

interface RawEntry {
  id: Uint8Array;
  parents: Uint8Array[];
  author?: { name: Uint8Array; time: bigint } | undefined;
  message: Uint8Array;
  stats?: { files: { added: number; removed: number }[] } | undefined;
}

function toEntry(e: RawEntry): HistoryEntry {
  let added = 0;
  let removed = 0;
  for (const f of e.stats?.files ?? []) {
    added += f.added;
    removed += f.removed;
  }
  const first = e.parents[0];
  return {
    id: toHex(e.id),
    parent: first && first.byteLength > 0 ? toHex(first) : null,
    subject: dec.decode(e.message).split("\n", 1)[0] ?? "",
    author: e.author ? dec.decode(e.author.name) : "",
    time: e.author ? Number(e.author.time) : 0,
    added,
    removed,
  };
}

export function bridgeHistoryApi(mgr: BridgeManager): HistoryApi & WatchApi {
  return {
    watch: bridgeWatch(mgr),
    async history(root, q, signal) {
      const wsId = await wsIdFor(mgr, root);
      const p = { root, ...(signal === undefined ? {} : { signal }) };
      const entries: HistoryEntry[] = [];
      let next = "";
      if (q.lines === null) {
        for await (const r of callStream(
          mgr.node(),
          Workspace.method.log,
          {
            wsId,
            query: {
              paths: [q.path],
              follow: true,
              withStats: true,
              limit: q.limit,
              pageToken: q.pageToken,
            },
          },
          p,
        )) {
          for (const e of r.entries) entries.push(toEntry(e as RawEntry));
          if (r.nextPageToken) next = r.nextPageToken;
        }
      } else {
        // TODO(C30a): `Repo.LineHistory` needs a repo name the workspace does not expose yet.
        for await (const r of callStream(
          mgr.node(),
          Repo.method.lineHistory,
          {
            repo: { name: "" },
            rev: "HEAD",
            path: enc.encode(q.path),
            startLine: q.lines.start,
            endLine: q.lines.end,
            limit: q.limit,
            resumeToken: q.pageToken,
          },
          p,
        )) {
          for (const e of r.entries) entries.push(toEntry(e as RawEntry));
          if (r.truncated && r.resumeToken) next = r.resumeToken;
        }
      }
      return { entries, next };
    },
  };
}

// --- the editor adapter ---------------------------------------------------------------------------

function relPath(root: string, fsPath: string): string | null {
  const a = root.replace(/\\/g, "/").replace(/\/+$/, "");
  const b = fsPath.replace(/\\/g, "/");
  const win = /^[A-Za-z]:/.test(a);
  const [x, y] = win ? [a.toLowerCase(), b.toLowerCase()] : [a, b];
  return y.startsWith(`${x}/`) ? b.slice(a.length + 1) : null;
}

export function registerFileHistory(
  vscode: typeof vscodeTypes,
  api: HistoryApi & WatchApi,
  roots: readonly string[],
  log: (line: string) => void,
): { dispose(): void } {
  const subs: { dispose(): unknown }[] = [];
  const ac = new AbortController();
  const model = new FileHistoryModel({ api, onError: (e) => log(`asc file history: ${String(e)}`) });
  const changed = new vscode.EventEmitter<void>();
  subs.push(
    changed,
    model.onChange(() => changed.fire()),
  );

  const targetOf = (ed: vscodeTypes.TextEditor | undefined): Target | null => {
    if (!ed || ed.document.uri.scheme !== "file") return null;
    for (const root of roots) {
      const p = relPath(root, ed.document.uri.fsPath);
      if (p !== null) return { root, path: p };
    }
    return null;
  };
  model.setTarget(targetOf(vscode.window.activeTextEditor));
  for (const root of roots) {
    void pump(
      api,
      root,
      ac.signal,
      (k) => model.onWatch(k),
      (e) => log(`asc watch: ${String(e)}`),
    );
  }

  type Node = { kind: "entry"; entry: HistoryEntry } | { kind: "more" };
  const provider: vscodeTypes.TreeDataProvider<Node> = {
    onDidChangeTreeData: changed.event,
    getChildren(el) {
      if (el) return [];
      const out: Node[] = model.entries.map((entry) => ({ kind: "entry", entry }));
      if (model.hasMore) out.push({ kind: "more" });
      return out;
    },
    getTreeItem(n) {
      if (n.kind === "more") {
        const item = new vscode.TreeItem("Load more", vscode.TreeItemCollapsibleState.None);
        item.command = { command: "asc.fileHistory.loadMore", title: "Load more" };
        return item;
      }
      const d = describeEntry(n.entry);
      const item = new vscode.TreeItem(d.label, vscode.TreeItemCollapsibleState.None);
      item.description = d.description;
      item.tooltip = d.tooltip;
      item.contextValue = "asc.revision";
      const t = model.current;
      if (t) {
        const at = (rev: string) => {
          const u = toAscUri({ root: t.root, path: t.path, at: { kind: "rev", rev } });
          return vscode.Uri.from({ scheme: "asc", path: u.path, query: u.query });
        };
        const left = n.entry.parent ? at(n.entry.parent) : at(n.entry.id);
        item.command = {
          command: n.entry.parent ? "vscode.diff" : "vscode.open",
          title: "Open",
          arguments: n.entry.parent
            ? [left, at(n.entry.id), `${t.path} (${n.entry.id.slice(0, 8)})`]
            : [left],
        };
      }
      return item;
    },
  };
  const view = vscode.window.createTreeView("asc.fileHistory", { treeDataProvider: provider });
  subs.push(
    view,
    view.onDidChangeVisibility((e) => model.setVisible(e.visible)),
    vscode.window.onDidChangeActiveTextEditor((ed) => model.setTarget(targetOf(ed))),
    vscode.commands.registerCommand("asc.fileHistory.loadMore", () => model.loadMore()),
    vscode.commands.registerCommand("asc.fileHistory.fileMode", () => model.setLines(null)),
    vscode.commands.registerCommand("asc.fileHistory.lineMode", () => {
      const sel = vscode.window.activeTextEditor?.selection;
      if (sel) model.setLines({ start: sel.start.line + 1, end: sel.end.line + 1 });
    }),
  );
  model.setVisible(view.visible);
  return {
    dispose() {
      ac.abort();
      model.dispose();
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
