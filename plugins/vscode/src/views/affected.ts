// Affected tree (SPEC 24.4.7): the projects the working copy touches, with their reasons, from
// `Workspace.Affected`. Each project lists the targets that have an argv definition (API addition A4);
// a project without one offers no run action. asc never runs a target: `tasks.ts` hands the argv to the
// editor's terminal. Models are editor-free; `registerAffected` is the vscode user.
import { callUnary } from "@asc/asc-client";
import { Workspace } from "@asc/asc-client/workspace";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "../bridge";
import { type WatchApi, wsIdFor } from "./worktrees";

export interface AffectedTarget {
  name: string;
  argv: string[];
}

export interface AffectedItem {
  id: string;
  name: string;
  /** Repo-relative project root, forward slashes. */
  root: string;
  direct: boolean;
  reasons: string[];
  targets: AffectedTarget[];
}

export interface AffectedResult {
  /** Every project is affected (a root-level input changed). */
  all: boolean;
  projects: AffectedItem[];
}

export interface AffectedApi {
  affected(root: string, signal?: AbortSignal): Promise<AffectedResult>;
}

const TARGET_NAME = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,63}$/;

/** A target name and argv that are safe to show and to hand to a task; undefined otherwise. */
export function cleanTarget(name: string, argv: readonly string[]): AffectedTarget | undefined {
  if (!TARGET_NAME.test(name)) return undefined;
  if (argv.length === 0 || argv.length > 256) return undefined;
  if (argv[0] === "" || argv.some((a) => a.includes("\0"))) return undefined;
  return { name, argv: [...argv] };
}

export function describeAffected(p: AffectedItem): {
  label: string;
  description: string;
  tooltip: string;
} {
  const why = p.direct ? "changed" : "depends on a changed project";
  const first = p.reasons[0];
  return {
    label: p.name || p.id,
    description: first ? `${why}: ${first}` : why,
    tooltip: [p.root || ".", ...p.reasons].join("\n"),
  };
}

/** The `asc` command that lists what a task would cover; shown in the task detail. */
export function equivalentCommand(target: string): string {
  return `asc affected --target ${target}`;
}

export interface AffectedModelOptions {
  root: string;
  api: AffectedApi;
  onError?: (e: unknown) => void;
}

export class AffectedModel {
  result: AffectedResult = { all: false, projects: [] };
  private visible = false;
  private stale = true;
  private seq = 0;
  private abort: AbortController | null = null;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly o: AffectedModelOptions) {}

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
      const r = await this.o.api.affected(this.o.root, ac.signal);
      if (mine !== this.seq) return;
      this.result = r;
      this.stale = false;
      for (const l of this.listeners) l();
    } catch (e) {
      if (mine === this.seq && !ac.signal.aborted) this.o.onError?.(e);
    }
  }

  /** The latest result without waiting for the view; the task provider reads this. */
  async current(signal?: AbortSignal): Promise<AffectedResult> {
    if (this.stale) {
      const r = await this.o.api.affected(this.o.root, signal);
      this.result = r;
      this.stale = false;
    }
    return this.result;
  }

  dispose(): void {
    this.abort?.abort();
    this.listeners.clear();
  }
}

// --- the bridge adapter -----------------------------------------------------------------------------

const dec = new TextDecoder();
const text = (b: Uint8Array): string => dec.decode(b);

export function bridgeAffectedApi(mgr: BridgeManager): AffectedApi {
  return {
    async affected(root, signal) {
      const wsId = await wsIdFor(mgr, root);
      const r = await callUnary(
        mgr.node(),
        Workspace.method.affected,
        { wsId, uncommitted: true, query: {} },
        { root, ...(signal === undefined ? {} : { signal }) },
      );
      const res = r.result;
      const projects: AffectedItem[] = (res?.projects ?? []).map((p) => {
        const info = p.project;
        const targets: AffectedTarget[] = [];
        for (const [name, cmd] of Object.entries(p.targets)) {
          const t = cleanTarget(name, cmd.argv);
          if (t) targets.push(t);
        }
        targets.sort((a, b) => a.name.localeCompare(b.name));
        return {
          id: info?.id ?? "",
          name: info ? text(info.name) : "",
          root: info ? text(info.root).replaceAll("\\", "/") : "",
          direct: p.direct,
          reasons: p.reasons,
          targets,
        };
      });
      return { all: res?.all ?? false, projects };
    },
  };
}

// --- the editor adapter -----------------------------------------------------------------------------

type Node =
  | { kind: "project"; model: AffectedModel; project: AffectedItem }
  | { kind: "target"; model: AffectedModel; project: AffectedItem; target: AffectedTarget };

export function registerAffected(
  vscode: typeof vscodeTypes,
  models: readonly AffectedModel[],
  watch: WatchApi,
  pumpFn: (
    api: WatchApi,
    root: string,
    signal: AbortSignal,
    fn: () => void,
    onError: (e: unknown) => void,
  ) => Promise<void>,
  log: (line: string) => void,
): { dispose(): void } {
  const subs: { dispose(): unknown }[] = [];
  const ac = new AbortController();
  const changed = new vscode.EventEmitter<void>();
  subs.push(changed);
  for (const m of models) {
    subs.push(m.onChange(() => changed.fire()));
    void pumpFn(
      watch,
      m.root,
      ac.signal,
      () => m.onWatch(),
      (e) => log(`asc watch: ${String(e)}`),
    );
  }
  const provider: vscodeTypes.TreeDataProvider<Node> = {
    onDidChangeTreeData: changed.event,
    getChildren(el) {
      if (!el) {
        return models.flatMap((model) =>
          model.result.projects.map((project) => ({ kind: "project" as const, model, project })),
        );
      }
      if (el.kind !== "project") return [];
      return el.project.targets.map((target) => ({
        kind: "target" as const,
        model: el.model,
        project: el.project,
        target,
      }));
    },
    getTreeItem(n) {
      if (n.kind === "project") {
        const d = describeAffected(n.project);
        const item = new vscode.TreeItem(
          d.label,
          n.project.targets.length > 0
            ? vscode.TreeItemCollapsibleState.Collapsed
            : vscode.TreeItemCollapsibleState.None,
        );
        item.description = d.description;
        item.tooltip = d.tooltip;
        item.contextValue = "asc.affected.project";
        item.iconPath = new vscode.ThemeIcon(n.project.direct ? "edit" : "references");
        return item;
      }
      const item = new vscode.TreeItem(n.target.name);
      item.description = equivalentCommand(n.target.name);
      item.contextValue = "asc.affected.target";
      item.iconPath = new vscode.ThemeIcon("play");
      item.command = { command: "asc.affected.run", title: "Run target", arguments: [n] };
      return item;
    },
  };
  const view = vscode.window.createTreeView("asc.affected", { treeDataProvider: provider });
  subs.push(
    view,
    view.onDidChangeVisibility((e) => {
      for (const m of models) m.setVisible(e.visible);
    }),
    vscode.commands.registerCommand("asc.affected.refresh", () => {
      for (const m of models) void m.refresh();
    }),
    vscode.commands.registerCommand("asc.affected.run", async (n?: Node) => {
      if (n?.kind !== "target") return;
      const tasks = await vscode.tasks.fetchTasks({ type: "asc" });
      const hit = tasks.find(
        (t) =>
          t.definition.project === n.project.id &&
          t.definition.target === n.target.name &&
          t.definition.root === n.model.root,
      );
      if (hit) await vscode.tasks.executeTask(hit);
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
