// Worktrees tree (SPEC 24.4.4): `Workspace.Worktrees`, refreshed by `Workspace.Watch` on any root and
// fetched only while visible. Actions: open in a new window, add (`worktree add --ephemeral`), remove
// (modal text composed by the host), lock. `WorktreesModel` is editor-free so it runs under Bun;
// `registerWorktrees` is the only `vscode` user. Also holds the pieces the other views share: the
// `ViewApi`, the `Watch` pump and `Bridge.Run`.
import {
  bridge,
  type BridgeTransport,
  type CallParams,
  callStream,
  callUnary,
} from "@asc/asc-client";
import { WatchEvent_Kind, Workspace } from "@asc/asc-client/workspace";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "../bridge";

export interface WorktreeInfo {
  wsId: string;
  path: string;
  mode: string;
  headRef: string;
  dirty: number;
  staged: number;
  ahead: number;
  behind: number;
  ephemeral: boolean;
  locked: string;
  inProgress: string;
}

export type WatchKindName =
  | "paths"
  | "head"
  | "ref"
  | "stage"
  | "op-started"
  | "op-finished"
  | "other";

const KINDS: Record<number, WatchKindName> = {
  [WatchEvent_Kind.PATHS]: "paths",
  [WatchEvent_Kind.HEAD]: "head",
  [WatchEvent_Kind.REF]: "ref",
  [WatchEvent_Kind.STAGE]: "stage",
  [WatchEvent_Kind.OP_STARTED]: "op-started",
  [WatchEvent_Kind.OP_FINISHED]: "op-finished",
};

export interface RunResult {
  exit: number;
  /** An encoded `OpResult` when the child printed one. TODO(M02): `cli.proto` is not generated yet. */
  opResult: Uint8Array | null;
  stderr: string[];
}

export interface WatchApi {
  /** Event kinds only; `Watch` is the one refresh trigger (IP-D9). */
  watch(root: string, signal: AbortSignal): AsyncIterable<WatchKindName>;
}

export interface ViewApi extends WatchApi {
  worktrees(root: string, signal?: AbortSignal): Promise<WorktreeInfo[]>;
  /** `Bridge.Run` of an allowlisted verb. Stdin is the null device. */
  run(root: string, verb: string, args: string[], signal?: AbortSignal): Promise<RunResult>;
}

/** Calls `fn` for each watch event until aborted; reconnects after an error with a growing delay. */
export async function pump(
  api: WatchApi,
  root: string,
  signal: AbortSignal,
  fn: (k: WatchKindName) => void,
  onError?: (e: unknown) => void,
  delays: readonly number[] = [500, 2000, 10000],
): Promise<void> {
  let attempt = 0;
  while (!signal.aborted) {
    try {
      for await (const k of api.watch(root, signal)) {
        attempt = 0;
        fn(k);
      }
    } catch (e) {
      if (signal.aborted) return;
      onError?.(e);
    }
    if (signal.aborted) return;
    const d = delays[Math.min(attempt++, delays.length - 1)] ?? 10000;
    await new Promise<void>((resolve) => {
      const t = setTimeout(resolve, d);
      signal.addEventListener(
        "abort",
        () => {
          clearTimeout(t);
          resolve();
        },
        { once: true },
      );
    });
  }
}

export interface WorktreesModelOptions {
  root: string;
  api: ViewApi;
  onError?: (e: unknown) => void;
}

export class WorktreesModel {
  items: WorktreeInfo[] = [];
  private visible = false;
  private stale = true;
  private seq = 0;
  private abort: AbortController | null = null;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly o: WorktreesModelOptions) {}

  get root(): string {
    return this.o.root;
  }
  get isStale(): boolean {
    return this.stale;
  }

  onChange(fn: () => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }

  setVisible(v: boolean): void {
    this.visible = v;
    if (v && this.stale) void this.refresh();
  }

  /** Called for every `Watch` event on the root. */
  onWatch(): void {
    this.stale = true;
    if (this.visible) void this.refresh();
  }

  async refresh(): Promise<void> {
    const mine = ++this.seq;
    this.abort?.abort();
    const ac = new AbortController();
    this.abort = ac;
    try {
      const list = await this.o.api.worktrees(this.o.root, ac.signal);
      if (mine !== this.seq) return;
      this.items = list;
      this.stale = false;
      for (const l of this.listeners) l();
    } catch (e) {
      if (mine === this.seq && !ac.signal.aborted) this.o.onError?.(e);
    }
  }

  add(path: string, rev?: string): Promise<RunResult> {
    return this.o.api.run(this.o.root, "worktree add", [
      "--ephemeral",
      "--end-of-options",
      path,
      ...(rev ? [rev] : []),
    ]);
  }

  remove(wt: WorktreeInfo): Promise<RunResult> {
    return this.o.api.run(this.o.root, "worktree remove", ["--end-of-options", wt.path]);
  }

  dispose(): void {
    this.abort?.abort();
    this.listeners.clear();
  }
}

export function describeWorktree(w: WorktreeInfo): {
  label: string;
  description: string;
  tooltip: string;
} {
  const bits: string[] = [];
  if (w.headRef) bits.push(w.headRef);
  if (w.dirty > 0) bits.push(`${w.dirty} changed`);
  if (w.staged > 0) bits.push(`${w.staged} staged`);
  if (w.ahead > 0) bits.push(`ahead ${w.ahead}`);
  if (w.behind > 0) bits.push(`behind ${w.behind}`);
  if (w.inProgress) bits.push(w.inProgress);
  if (w.ephemeral) bits.push("ephemeral");
  if (w.locked) bits.push("locked");
  const label = w.path.split(/[\\/]/).filter(Boolean).pop() ?? w.path;
  return {
    label,
    description: bits.join("  "),
    tooltip: w.locked ? `${w.path}\nLocked: ${w.locked}` : w.path,
  };
}

/** The removal prompt, composed from the parsed worktree and never from caller text. */
export function removeConfirmText(w: WorktreeInfo): string {
  const warn = w.dirty + w.staged > 0 ? " It has uncommitted changes." : "";
  return `Remove worktree ${describeWorktree(w).label} (${w.path})?${warn}`;
}

// --- the bridge adapter ---------------------------------------------------------------------------

const callParams = (root: string, signal?: AbortSignal): CallParams => ({
  root,
  ...(signal === undefined ? {} : { signal }),
});

/** `Bridge.Run`: the bridge validates the argv against its schema and is the authority. */
export async function runVerb(
  node: BridgeTransport,
  root: string,
  verb: string,
  args: string[],
  signal?: AbortSignal,
): Promise<RunResult> {
  const out: RunResult = { exit: -1, opResult: null, stderr: [] };
  for await (const ev of callStream(
    node,
    bridge.Bridge.method.run,
    { verb, args, root },
    callParams(root, signal),
  )) {
    const e = ev.event;
    if (e.case === "stderrLine") out.stderr.push(e.value);
    else if (e.case === "opResult") out.opResult = e.value;
    else if (e.case === "exit") out.exit = e.value;
  }
  return out;
}

export async function wsIdFor(mgr: BridgeManager, root: string): Promise<string> {
  const h = await mgr.hello();
  const r = h.roots.find((x) => x.root === root);
  if (!r) throw new Error("asc root is not known to the bridge");
  return r.wsId;
}

export function bridgeWatch(mgr: BridgeManager): WatchApi["watch"] {
  return async function* (root, signal) {
    const wsId = await wsIdFor(mgr, root);
    for await (const e of callStream(
      mgr.node(),
      Workspace.method.watch,
      { wsId, sinceToken: "" },
      callParams(root, signal),
    )) {
      yield KINDS[e.kind] ?? "other";
    }
  };
}

export function bridgeViewApi(mgr: BridgeManager): ViewApi {
  return {
    async worktrees(root, signal) {
      await wsIdFor(mgr, root);
      const r = await callUnary(
        mgr.node(),
        Workspace.method.worktrees,
        {},
        callParams(root, signal),
      );
      return r.worktrees.map((w) => ({
        wsId: w.wsId,
        path: w.path,
        mode: w.mode,
        headRef: w.headRef,
        dirty: w.dirty,
        staged: w.staged,
        ahead: w.ahead,
        behind: w.behind,
        ephemeral: w.ephemeralUntil > 0n,
        locked: w.locked,
        inProgress: w.inProgress,
      }));
    },
    watch: bridgeWatch(mgr),
    run: (root, verb, args, signal) => runVerb(mgr.node(), root, verb, args, signal),
  };
}

// --- the editor adapter ---------------------------------------------------------------------------

export function registerWorktrees(
  vscode: typeof vscodeTypes,
  api: ViewApi,
  roots: readonly string[],
  log: (line: string) => void,
): { dispose(): void } {
  const subs: { dispose(): unknown }[] = [];
  const ac = new AbortController();
  const models = roots.map(
    (root) => new WorktreesModel({ root, api, onError: (e) => log(`asc worktrees: ${String(e)}`) }),
  );
  const changed = new vscode.EventEmitter<void>();
  subs.push(changed);
  for (const m of models) {
    subs.push(m.onChange(() => changed.fire()));
    void pump(
      api,
      m.root,
      ac.signal,
      () => m.onWatch(),
      (e) => log(`asc watch: ${String(e)}`),
    );
  }
  interface Node {
    model: WorktreesModel;
    wt: WorktreeInfo;
  }
  const provider: vscodeTypes.TreeDataProvider<Node> = {
    onDidChangeTreeData: changed.event,
    getChildren: (el) =>
      el ? [] : models.flatMap((model) => model.items.map((wt) => ({ model, wt }))),
    getTreeItem({ wt }) {
      const d = describeWorktree(wt);
      const item = new vscode.TreeItem(d.label, vscode.TreeItemCollapsibleState.None);
      item.description = d.description;
      item.tooltip = d.tooltip;
      item.contextValue = wt.locked ? "asc.worktree.locked" : "asc.worktree";
      item.iconPath = new vscode.ThemeIcon(wt.locked ? "lock" : "folder");
      return item;
    },
  };
  const view = vscode.window.createTreeView("asc.worktrees", { treeDataProvider: provider });
  subs.push(
    view,
    view.onDidChangeVisibility((e) => {
      for (const m of models) m.setVisible(e.visible);
    }),
  );
  for (const m of models) m.setVisible(view.visible);

  const report = (what: string, r: RunResult) => {
    if (r.exit !== 0) {
      void vscode.window.showErrorMessage(
        `asc ${what} failed${r.stderr[0] ? `: ${r.stderr[0]}` : ""}`,
      );
    }
  };
  subs.push(
    vscode.commands.registerCommand("asc.worktrees.open", (n?: Node) => {
      if (!n) return;
      void vscode.commands.executeCommand("vscode.openFolder", vscode.Uri.file(n.wt.path), {
        forceNewWindow: true,
      });
    }),
    vscode.commands.registerCommand("asc.worktrees.add", async () => {
      const m = models[0];
      if (!m) return;
      const p = await vscode.window.showInputBox({ prompt: "Path for the new ephemeral worktree" });
      if (p) report("worktree add", await m.add(p));
    }),
    vscode.commands.registerCommand("asc.worktrees.remove", async (n?: Node) => {
      if (!n) return;
      const ok = await vscode.window.showWarningMessage(
        removeConfirmText(n.wt),
        { modal: true },
        "Remove",
      );
      if (ok === "Remove") report("worktree remove", await n.model.remove(n.wt));
    }),
    // TODO(C31): the bridge schema has no `worktree lock` verb yet.
    vscode.commands.registerCommand("asc.worktrees.lock", () => {
      void vscode.window.showInformationMessage("ASC: locking worktrees needs a newer asc.");
    }),
  );
  return {
    dispose() {
      ac.abort();
      for (const m of models) m.dispose();
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
