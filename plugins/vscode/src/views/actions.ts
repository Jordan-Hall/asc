// Actions tree (SPEC 24.4.7): runs of the current change's head commit, their jobs, and a read-only
// `asc-actions:` log per job. Data comes through `ActionsApi`, which an asc serve RPC implements; the
// plugin knows no forge, so this module is not wired until that RPC exists.
// "Re-run" is the only write. Models are editor-free; `registerActions` is the vscode user.
import type * as vscodeTypes from "vscode";
import {
  ActionsLogDocs,
  type LogReadApi,
  SCHEME,
  toActionsUri,
} from "../fs/actions-log";

export interface Disposable {
  dispose(): unknown;
}

export interface RunRow {
  id: number;
  title: string;
  status: string;
  event: string;
  commit: string;
}

export interface JobRow {
  id: number;
  runId: number;
  name: string;
  status: string;
}

/** `owner/repo` and the head commit of the current change (Actions context `asc_change`, C40). */
export interface RemoteTarget {
  owner: string;
  repo: string;
  headSha?: string;
}

export interface ActionsApi extends LogReadApi {
  listRuns(headSha: string | undefined, signal?: AbortSignal): Promise<RunRow[]>;
  listJobs(runId: number, signal?: AbortSignal): Promise<JobRow[]>;
  rerun(runId: number): Promise<void>;
}

export type Tone = "ok" | "fail" | "running" | "pending" | "neutral";

export function tone(status: string): Tone {
  switch (status) {
    case "success":
      return "ok";
    case "failure":
    case "cancelled":
      return "fail";
    case "running":
      return "running";
    case "waiting":
    case "queued":
    case "blocked":
      return "pending";
    default:
      return "neutral";
  }
}

/** Why "Re-run" is not offered; undefined when it is. */
export function rerunDisabledReason(status: string): string | undefined {
  const t = tone(status);
  return t === "running" || t === "pending" ? "The run has not finished" : undefined;
}

const ICONS: Record<Tone, string> = {
  ok: "pass",
  fail: "error",
  running: "sync~spin",
  pending: "clock",
  neutral: "circle-outline",
};
export const iconFor = (status: string): string => ICONS[tone(status)];

export function describeRun(r: RunRow): { label: string; description: string; tooltip: string } {
  const bits = [r.status, r.event, r.commit.slice(0, 8)].filter(Boolean);
  return {
    label: r.title || `Run ${r.id}`,
    description: bits.join("  "),
    tooltip: `Run ${r.id}: ${r.status}`,
  };
}

// --- model ------------------------------------------------------------------------------------------

export interface ActionsModelOptions {
  api: ActionsApi;
  target: () => RemoteTarget | undefined;
  onError?: (e: unknown) => void;
}

export class ActionsModel {
  runs: RunRow[] = [];
  private readonly jobs = new Map<number, JobRow[]>();
  private visible = false;
  private stale = true;
  private seq = 0;
  private abort: AbortController | null = null;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly o: ActionsModelOptions) {}

  get isStale(): boolean {
    return this.stale;
  }

  onChange(fn: () => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }

  /** Fetches only while visible; a view shown after changes refreshes once. */
  setVisible(v: boolean): void {
    this.visible = v;
    if (v && this.stale) void this.refresh();
  }

  /** Head moved or an operation finished. */
  markStale(): void {
    this.stale = true;
    if (this.visible) void this.refresh();
  }

  async refresh(): Promise<void> {
    const mine = ++this.seq;
    this.abort?.abort();
    const ac = new AbortController();
    this.abort = ac;
    const t = this.o.target();
    try {
      const runs = t ? await this.o.api.listRuns(t.headSha, ac.signal) : [];
      if (mine !== this.seq) return;
      this.runs = runs;
      this.jobs.clear();
      this.stale = false;
      for (const l of this.listeners) l();
    } catch (e) {
      if (mine === this.seq && !ac.signal.aborted) this.o.onError?.(e);
    }
  }

  async loadJobs(runId: number): Promise<JobRow[]> {
    const known = this.jobs.get(runId);
    if (known) return known;
    const list = await this.o.api.listJobs(runId, this.abort?.signal);
    this.jobs.set(runId, list);
    return list;
  }

  async rerun(run: RunRow): Promise<void> {
    const why = rerunDisabledReason(run.status);
    if (why) throw new Error(why);
    await this.o.api.rerun(run.id);
    this.markStale();
  }

  dispose(): void {
    this.abort?.abort();
    this.listeners.clear();
  }
}

// --- the editor adapter -----------------------------------------------------------------------------

export interface ActionsHost {
  /** The remote repository and head commit for an asc root; undefined when unknown. */
  target(root: string): RemoteTarget | undefined;
  api(t: RemoteTarget): ActionsApi | undefined;
  /** Head or operation events for a root (the `Workspace.Watch` pump). */
  onStale(root: string, fn: () => void): Disposable;
}

type Node =
  | { kind: "run"; model: ActionsModel; target: RemoteTarget; run: RunRow }
  | { kind: "job"; target: RemoteTarget; job: JobRow };

export function registerActions(
  vscode: typeof vscodeTypes,
  host: ActionsHost,
  roots: readonly string[],
  log: (line: string) => void,
): Disposable {
  const subs: { dispose(): unknown }[] = [];
  const changed = new vscode.EventEmitter<void>();
  const docChanged = new vscode.EventEmitter<vscodeTypes.Uri>();
  subs.push(changed, docChanged);

  const hostApi = (root: string): ActionsApi => {
    const t = host.target(root);
    const api = t ? host.api(t) : undefined;
    if (!api) throw new Error("the server offers no Actions for this workspace");
    return api;
  };
  const models = roots.map((root) => {
    const model = new ActionsModel({
      api: {
        listRuns: (sha, s) => hostApi(root).listRuns(sha, s),
        listJobs: (id, s) => hostApi(root).listJobs(id, s),
        rerun: (id) => hostApi(root).rerun(id),
        readRange: (id, r, s) => hostApi(root).readRange(id, r, s),
      },
      target: () => host.target(root),
      onError: (e) => log(`asc actions: ${String(e)}`),
    });
    subs.push(
      model.onChange(() => changed.fire()),
      host.onStale(root, () => model.markStale()),
    );
    return { root, model };
  });

  const docs = new ActionsLogDocs((owner, repo) => {
    for (const { root } of models) {
      const t = host.target(root);
      if (t && t.owner === owner && t.repo === repo) return host.api(t);
    }
    return undefined;
  });
  subs.push(
    vscode.workspace.registerTextDocumentContentProvider(SCHEME, {
      onDidChange: docChanged.event,
      provideTextDocumentContent: (uri, token) => {
        const ac = new AbortController();
        token.onCancellationRequested(() => ac.abort());
        return docs.provide(uri.path, ac.signal);
      },
    }),
    vscode.workspace.onDidCloseTextDocument((d) => {
      if (d.uri.scheme === SCHEME) docs.close(d.uri.path);
    }),
  );

  const provider: vscodeTypes.TreeDataProvider<Node> = {
    onDidChangeTreeData: changed.event,
    async getChildren(el) {
      if (!el) {
        return models.flatMap(({ root, model }) => {
          const target = host.target(root);
          return target
            ? model.runs.map((run) => ({ kind: "run" as const, model, target, run }))
            : [];
        });
      }
      if (el.kind !== "run") return [];
      try {
        const jobs = await el.model.loadJobs(el.run.id);
        return jobs.map((job) => ({ kind: "job" as const, target: el.target, job }));
      } catch (e) {
        log(`asc actions: ${String(e)}`);
        return [];
      }
    },
    getTreeItem(n) {
      if (n.kind === "run") {
        const d = describeRun(n.run);
        const item = new vscode.TreeItem(d.label, vscode.TreeItemCollapsibleState.Collapsed);
        item.description = d.description;
        item.tooltip = d.tooltip;
        item.contextValue = rerunDisabledReason(n.run.status) ? "asc.run.active" : "asc.run";
        item.iconPath = new vscode.ThemeIcon(iconFor(n.run.status));
        return item;
      }
      const item = new vscode.TreeItem(n.job.name || `Job ${n.job.id}`);
      item.description = n.job.status;
      item.iconPath = new vscode.ThemeIcon(iconFor(n.job.status));
      item.contextValue = "asc.job";
      item.command = { command: "asc.actions.openLog", title: "Open log", arguments: [n] };
      return item;
    },
  };
  const view = vscode.window.createTreeView("asc.actions", { treeDataProvider: provider });
  subs.push(
    view,
    view.onDidChangeVisibility((e) => {
      for (const m of models) m.model.setVisible(e.visible);
    }),
  );
  for (const m of models) m.model.setVisible(view.visible);

  subs.push(
    vscode.commands.registerCommand("asc.actions.refresh", () => {
      for (const m of models) void m.model.refresh();
    }),
    vscode.commands.registerCommand("asc.actions.openLog", async (n?: Node) => {
      if (n?.kind !== "job") return;
      try {
        const u = toActionsUri({ owner: n.target.owner, repo: n.target.repo, jobId: n.job.id });
        const doc = await vscode.workspace.openTextDocument(
          vscode.Uri.from({ scheme: u.scheme, path: u.path }),
        );
        await vscode.window.showTextDocument(doc, { preview: true });
      } catch (e) {
        void vscode.window.showErrorMessage(`asc: could not open the log: ${String(e)}`);
      }
    }),
    vscode.commands.registerCommand("asc.actions.loadEarlier", async () => {
      const uri = vscode.window.activeTextEditor?.document.uri;
      if (!uri || uri.scheme !== SCHEME) return;
      try {
        await docs.loadEarlier(uri.path);
        docChanged.fire(uri);
      } catch (e) {
        void vscode.window.showErrorMessage(`asc: could not load earlier output: ${String(e)}`);
      }
    }),
    vscode.commands.registerCommand("asc.actions.rerun", async (n?: Node) => {
      if (n?.kind !== "run") return;
      try {
        await n.model.rerun(n.run);
      } catch (e) {
        void vscode.window.showErrorMessage(`asc: re-run failed: ${String(e)}`);
      }
    }),
  );
  return {
    dispose() {
      for (const m of models) m.model.dispose();
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
