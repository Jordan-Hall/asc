// Actions tree (SPEC 24.4.7): the CI runs recorded against the current change, their jobs, and a
// read-only `asc-actions:` log per job, over the host-neutral `asc.v1.Ci` service (feature `ci.v1`).
// Any runner or forge adapter reports runs to asc serve; the plugin only reads them, so there is no
// re-run (reporting is a service-credential call, not a user action).
// Models are editor-free; `registerActions` is the vscode user.
import { callUnary, changeIdFromString, changeIdToString } from "@asc/asc-client";
import { Ci, CiStatus } from "@asc/asc-client/ci";
import { Workspace } from "@asc/asc-client/workspace";
import type * as vscodeTypes from "vscode";
import type { BridgeManager, ServerTarget } from "../bridge";
import {
  ActionsLogDocs,
  type LogRange,
  type LogRangeRequest,
  type LogReadApi,
  SCHEME,
  toActionsUri,
} from "../fs/actions-log";
import { wsIdFor } from "./worktrees";

export interface Disposable {
  dispose(): unknown;
}

export interface JobRow {
  id: number;
  runId: number;
  name: string;
  status: string;
}

export interface RunRow {
  id: number;
  title: string;
  status: string;
  /** Who reported the run (a runner or adapter name). */
  source: string;
  /** The change version the run is for; 0 when unknown. */
  version: number;
  jobs: JobRow[];
}

/** `owner/repo` and the current change (32 letters k-z) of an asc root. */
export interface RemoteTarget {
  owner: string;
  repo: string;
  change?: string;
}

export interface ActionsApi extends LogReadApi {
  /** Runs of `change` (every change when undefined), newest first, each with its jobs. */
  listRuns(change: string | undefined, signal?: AbortSignal): Promise<RunRow[]>;
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
    case "queued":
      return "pending";
    default:
      return "neutral";
  }
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
  const bits = [r.status, r.source, r.version > 0 ? `v${r.version}` : ""].filter(Boolean);
  return {
    label: r.title || `Run ${r.id}`,
    description: bits.join("  "),
    tooltip: `Run ${r.id}: ${r.status}`,
  };
}

// --- model ------------------------------------------------------------------------------------------

export interface ActionsModelOptions {
  api: ActionsApi;
  /** The repository and current change; undefined when the root is not an asc-server workspace. */
  target: () => Promise<RemoteTarget | undefined>;
  onError?: (e: unknown) => void;
}

export class ActionsModel {
  runs: RunRow[] = [];
  /** The target of the last refresh. */
  current: RemoteTarget | undefined;
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
    try {
      const t = await this.o.target();
      const runs = t ? await this.o.api.listRuns(t.change, ac.signal) : [];
      if (mine !== this.seq) return;
      this.current = t;
      this.runs = runs;
      this.stale = false;
      for (const l of this.listeners) l();
    } catch (e) {
      if (mine === this.seq && !ac.signal.aborted) this.o.onError?.(e);
    }
  }

  dispose(): void {
    this.abort?.abort();
    this.listeners.clear();
  }
}

// --- the bridge adapter ---------------------------------------------------------------------------

const STATUS_NAMES = new Map<CiStatus, string>([
  [CiStatus.QUEUED, "queued"],
  [CiStatus.RUNNING, "running"],
  [CiStatus.SUCCESS, "success"],
  [CiStatus.FAILURE, "failure"],
  [CiStatus.CANCELLED, "cancelled"],
  [CiStatus.SKIPPED, "skipped"],
]);
const statusName = (s: CiStatus): string => STATUS_NAMES.get(s) ?? "";

/** The most one `JobLog` answer holds. */
const MAX_LOG_BYTES = 1024 * 1024;

/** The change the working copy is on, from the `Stacks` snapshot; undefined when it is on no stack. */
export async function currentChange(mgr: BridgeManager, root: string): Promise<string | undefined> {
  const wsId = await wsIdFor(mgr, root);
  const r = await callUnary(mgr.node(), Workspace.method.stacks, { wsId }, { root });
  for (const st of r.stacks) {
    const hit = st.changes.find((c) => c.current);
    if (hit) return changeIdToString(hit.change);
  }
  return undefined;
}

/** `owner/repo` split from the Hello repo name. */
export function splitRepo(name: string): { owner: string; repo: string } | undefined {
  const i = name.indexOf("/");
  return i > 0 && i < name.length - 1
    ? { owner: name.slice(0, i), repo: name.slice(i + 1) }
    : undefined;
}

/** Ci reads for one asc-server root. A job id maps to its run from the last `ListRuns`. */
export function bridgeActionsApi(mgr: BridgeManager, t: ServerTarget): ActionsApi {
  const runOf = new Map<number, number>();
  const jobLog = (
    runId: number,
    jobId: number,
    maxBytes: number,
    sinceOffset: number,
    signal?: AbortSignal,
  ) =>
    callUnary(
      mgr.node(),
      Ci.method.jobLog,
      {
        repo: { name: t.repo },
        runId: BigInt(runId),
        jobId: BigInt(jobId),
        maxBytes,
        sinceOffset: BigInt(sinceOffset),
      },
      { root: t.root, ...(signal ? { signal } : {}) },
    );
  return {
    async listRuns(change, signal) {
      const r = await callUnary(
        mgr.node(),
        Ci.method.listRuns,
        {
          repo: { name: t.repo },
          ...(change === undefined ? {} : { change: changeIdFromString(change) }),
          limit: 30,
        },
        { root: t.root, ...(signal ? { signal } : {}) },
      );
      return r.runs.map((run) => {
        const id = Number(run.id);
        return {
          id,
          title: run.name,
          status: statusName(run.status),
          source: run.source,
          version: run.version,
          jobs: run.jobs.map((j) => {
            runOf.set(Number(j.id), id);
            return { id: Number(j.id), runId: id, name: j.name, status: statusName(j.status) };
          }),
        };
      });
    },

    async readRange(jobId, range: LogRangeRequest, signal): Promise<LogRange> {
      const runId = runOf.get(jobId);
      if (runId === undefined) {
        throw new Error("this job's run is not loaded; refresh the Actions view");
      }
      if ("suffix" in range) {
        const r = await jobLog(runId, jobId, Math.min(range.suffix, MAX_LOG_BYTES), 0, signal);
        return { bytes: r.data, start: Number(r.offset), total: Number(r.size) };
      }
      const want = range.end - range.start + 1;
      if (range.start > 0) {
        const r = await jobLog(runId, jobId, Math.min(want, MAX_LOG_BYTES), range.start, signal);
        return { bytes: r.data, start: Number(r.offset), total: Number(r.size) };
      }
      // Offset 0 cannot be asked for by offset (0 means "the tail"): read the whole log when it fits,
      // then cut. A log past 1 MiB comes back from a later offset and the caller keeps what it has.
      const probe = await jobLog(runId, jobId, 1, 0, signal);
      const r = await jobLog(runId, jobId, Math.min(Number(probe.size), MAX_LOG_BYTES), 0, signal);
      const bytes = Number(r.offset) === 0 ? r.data.subarray(0, want) : r.data;
      return { bytes, start: Number(r.offset), total: Number(r.size) };
    },
  };
}

// --- the editor adapter -----------------------------------------------------------------------------

export interface ActionsHost {
  /** The repository and current change of an asc root; undefined when it has no asc server. */
  target(root: string): Promise<RemoteTarget | undefined>;
  api(root: string): ActionsApi | undefined;
  /** Head or operation events for a root (the `Workspace.Watch` pump). */
  onStale(root: string, fn: () => void): Disposable;
}

type Node =
  | { kind: "run"; target: RemoteTarget; run: RunRow }
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

  const models = roots.map((root) => {
    const model = new ActionsModel({
      api: {
        listRuns: (change, s) => {
          const api = host.api(root);
          if (!api) throw new Error("the server offers no CI for this workspace");
          return api.listRuns(change, s);
        },
        readRange: (id, r, s) => {
          const api = host.api(root);
          if (!api) throw new Error("the server offers no CI for this workspace");
          return api.readRange(id, r, s);
        },
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
    for (const { root, model } of models) {
      const t = model.current;
      if (t && t.owner === owner && t.repo === repo) return host.api(root);
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
    getChildren(el) {
      if (!el) {
        return models.flatMap(({ model }) => {
          const target = model.current;
          return target ? model.runs.map((run) => ({ kind: "run" as const, target, run })) : [];
        });
      }
      if (el.kind !== "run") return [];
      return el.run.jobs.map((job) => ({ kind: "job" as const, target: el.target, job }));
    },
    getTreeItem(n) {
      if (n.kind === "run") {
        const d = describeRun(n.run);
        const item = new vscode.TreeItem(
          d.label,
          n.run.jobs.length > 0
            ? vscode.TreeItemCollapsibleState.Collapsed
            : vscode.TreeItemCollapsibleState.None,
        );
        item.description = d.description;
        item.tooltip = d.tooltip;
        item.contextValue = "asc.run";
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
        void vscode.window.showErrorMessage(`ASC: could not open the log: ${String(e)}`);
      }
    }),
    vscode.commands.registerCommand("asc.actions.loadEarlier", async () => {
      const uri = vscode.window.activeTextEditor?.document.uri;
      if (!uri || uri.scheme !== SCHEME) return;
      try {
        await docs.loadEarlier(uri.path);
        docChanged.fire(uri);
      } catch (e) {
        void vscode.window.showErrorMessage(`ASC: could not load earlier output: ${String(e)}`);
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
