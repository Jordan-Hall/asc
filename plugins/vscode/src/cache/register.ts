// The build cache's editor adapter (docs/designs/build-cache.md §6): the status item, the Build
// cache view, the read-only `cache show` documents, the `asc-build` task provider, the admin
// commands and the terminal variables. The models are in `model.ts` and `tasks.ts`.
import { readFile, stat } from "node:fs/promises";
import { join } from "node:path";
import type * as vscodeTypes from "vscode";
import { CACHE_SETUP_TOOLS, type CacheSetupTool } from "../cli-direct";
import {
  type CacheApi,
  type CacheEntry,
  CacheModel,
  type CacheStatus,
  ago,
  delta,
  humanSize,
  type LastBuild,
  statusView,
} from "./model";
import {
  BUILD_TASK_TYPE,
  type BuildSpec,
  type BuildTaskDefinition,
  declaredRun,
  definitionOf,
  detectBuilds,
  detectSetupTools,
  type RootFiles,
} from "./tasks";

export const VIEW_ID = "asc.buildCache";
export const SHOW_SCHEME = "asc-cache";
export const COMMANDS = {
  refresh: "asc.cache.refresh",
  show: "asc.cache.show",
  remove: "asc.cache.delete",
  clear: "asc.cache.clear",
  gc: "asc.cache.gc",
  warm: "asc.cache.warm",
  setup: "asc.cache.setup",
} as const;
const MAX_READ = 1024 * 1024;

/** `RootFiles` over the local disk. */
export function diskFiles(root: string): RootFiles {
  return {
    async exists(rel) {
      try {
        await stat(join(root, rel));
        return true;
      } catch {
        return false;
      }
    },
    async read(rel) {
      try {
        const p = join(root, rel);
        if ((await stat(p)).size > MAX_READ) return null;
        return await readFile(p, "utf8");
      } catch {
        return null;
      }
    },
  };
}

export interface CacheEnv {
  api: CacheApi;
  roots: readonly string[];
  /** The absolute asc executable, or null when none was found. */
  ascPath: () => string | null;
  /** `asc.cache.terminalEnv`. */
  terminalEnv: () => boolean;
}

type Node =
  | { kind: "root"; model: CacheModel }
  | { kind: "tool"; model: CacheModel; tool: string; status: CacheStatus["tools"][number] }
  | { kind: "entry"; model: CacheModel; entry: CacheEntry }
  | { kind: "none"; text: string };

function toolsOf(m: CacheModel): Node[] {
  if (m.status === null) return [{ kind: "none", text: "The build cache could not be read" }];
  if (m.status.tools.length === 0) return [{ kind: "none", text: "No builds cached yet" }];
  return m.status.tools.map((t) => ({ kind: "tool", model: m, tool: t.name, status: t }));
}

const error = (e: unknown): string => (e instanceof Error ? e.message : String(e));

export function registerBuildCache(
  vscode: typeof vscodeTypes,
  ctx: vscodeTypes.ExtensionContext,
  e: CacheEnv,
): { dispose(): void } {
  const subs: { dispose(): unknown }[] = [];
  const models = e.roots.map((root) => new CacheModel({ api: e.api, root }));
  const byRoot = new Map(models.map((m) => [m.root, m]));
  const first = models[0];
  let last: LastBuild | null = null;

  // --- the status item: the first root's store (one store per user, so every root shares it) -----
  const item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 46);
  item.name = "asc build cache";
  item.command = `${VIEW_ID}.focus`;
  const paint = () => {
    const v = statusView(first?.status ?? null, Date.now() / 1000, last);
    item.text = v.text;
    item.tooltip = v.tooltip;
    item.backgroundColor = v.warning
      ? new vscode.ThemeColor("statusBarItem.warningBackground")
      : undefined;
    if (v.visible) item.show();
    else item.hide();
  };
  subs.push(item);

  // --- the view ---------------------------------------------------------------------------------
  const changed = new vscode.EventEmitter<void>();
  subs.push(changed);
  for (const m of models) {
    subs.push(
      m.onChange(() => {
        changed.fire();
        if (m === first) paint();
      }),
    );
  }
  const provider: vscodeTypes.TreeDataProvider<Node> = {
    onDidChangeTreeData: changed.event,
    async getChildren(n) {
      if (n === undefined) {
        return models.length === 1 && first !== undefined
          ? toolsOf(first)
          : models.map((model) => ({ kind: "root", model }));
      }
      if (n.kind === "root") return toolsOf(n.model);
      if (n.kind === "tool") {
        const list = await n.model.entries(n.tool);
        return list.length === 0
          ? [{ kind: "none", text: "No entries" }]
          : list.map((entry) => ({ kind: "entry", model: n.model, entry }));
      }
      return [];
    },
    getTreeItem(n) {
      const C = vscode.TreeItemCollapsibleState;
      if (n.kind === "none") return new vscode.TreeItem(n.text, C.None);
      if (n.kind === "root") {
        const ti = new vscode.TreeItem(n.model.root, C.Expanded);
        ti.contextValue = "asc.cache.root";
        return ti;
      }
      const now = Date.now() / 1000;
      if (n.kind === "tool") {
        const ti = new vscode.TreeItem(n.tool, C.Collapsed);
        const d = n.status.day;
        ti.description = `${d.hits} hits, ${d.misses} misses today`;
        ti.tooltip = `${n.tool}: ${Math.round(n.status.week.hitRate * 100)}% hits this week, ${n.status.week.restored} restored`;
        ti.contextValue = "asc.cache.tool";
        return ti;
      }
      const en = n.entry;
      const ti = new vscode.TreeItem(en.label === "" ? en.key.slice(0, 12) : en.label, C.None);
      ti.description = `${humanSize(en.size)}, used ${ago(en.lastUse, now)}`;
      ti.tooltip = [
        `key ${en.key}`,
        `${en.outputs} outputs, exit ${en.exit}, created ${ago(en.created, now)}`,
        en.worktree === "" ? "" : `written from ${en.worktree}`,
        en.workflow === "" ? "" : `workflow ${en.workflow}`,
      ]
        .filter(Boolean)
        .join("\n");
      ti.contextValue = "asc.cache.entry";
      ti.command = { command: COMMANDS.show, title: "Show", arguments: [n] };
      return ti;
    },
  };
  const view = vscode.window.createTreeView(VIEW_ID, { treeDataProvider: provider });
  const refreshAll = async () => {
    await Promise.all(models.map((m) => m.refresh()));
  };
  subs.push(
    view,
    view.onDidChangeVisibility((ev) => {
      if (ev.visible) void refreshAll();
    }),
    vscode.window.onDidChangeWindowState((s) => {
      if (s.focused) void refreshAll();
    }),
  );

  // --- `cache show` as a read-only document -----------------------------------------------------
  const docs: vscodeTypes.TextDocumentContentProvider = {
    async provideTextDocumentContent(uri) {
      const key = uri.path.replace(/^\//, "").replace(/\.json$/, "");
      const m = byRoot.get(new URLSearchParams(uri.query).get("root") ?? "");
      if (m === undefined) return "";
      try {
        return await e.api.show(m.root, key);
      } catch (err) {
        return `${error(err)}\n`;
      }
    },
  };
  subs.push(vscode.workspace.registerTextDocumentContentProvider(SHOW_SCHEME, docs));

  // --- admin commands ---------------------------------------------------------------------------
  const pickRoot = async (): Promise<CacheModel | undefined> => {
    if (models.length <= 1) return first;
    const got = await vscode.window.showQuickPick(
      models.map((m) => ({ label: m.root, model: m })),
      { placeHolder: "Work tree" },
    );
    return got?.model;
  };
  const busy = <T>(title: string, fn: () => Promise<T>) =>
    vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title }, fn);
  const fail = (err: unknown) => void vscode.window.showErrorMessage(error(err));
  const cmd = (id: string, fn: (...a: never[]) => unknown) =>
    subs.push(vscode.commands.registerCommand(id, fn as (...a: unknown[]) => unknown));

  cmd(COMMANDS.refresh, () => refreshAll());
  cmd(COMMANDS.show, async (n?: Node) => {
    if (n?.kind !== "entry") return;
    const q = new URLSearchParams({ root: n.model.root }).toString();
    const uri = vscode.Uri.from({ scheme: SHOW_SCHEME, path: `/${n.entry.key}.json`, query: q });
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc, { preview: true });
  });
  cmd(COMMANDS.remove, async (n?: Node, sel?: Node[]) => {
    const picked = (sel ?? (n ? [n] : [])).filter(
      (x): x is Extract<Node, { kind: "entry" }> => x.kind === "entry",
    );
    const m = picked[0]?.model;
    if (m === undefined) return;
    const keys = picked.filter((x) => x.model === m).map((x) => x.entry.key);
    const what =
      keys.length === 1 ? "this build cache entry" : `${keys.length} build cache entries`;
    const ok = await vscode.window.showWarningMessage(
      `Delete ${what}? The next build that needs it runs again.`,
      { modal: true },
      "Delete",
    );
    if (ok !== "Delete") return;
    try {
      const removed = await e.api.remove(m.root, keys);
      void vscode.window.showInformationMessage(`Deleted ${removed.length} build cache entries.`);
    } catch (err) {
      fail(err);
    }
    await m.refresh();
  });
  cmd(COMMANDS.clear, async (n?: Node) => {
    let m: CacheModel | undefined;
    let tool: string | undefined;
    if (n?.kind === "tool") {
      m = n.model;
      tool = n.tool;
    } else {
      m = await pickRoot();
      if (m === undefined) return;
      const all = { label: "All tools", tool: undefined as string | undefined };
      const got = await vscode.window.showQuickPick(
        [
          all,
          ...(m.status?.tools ?? []).map((t) => ({
            label: t.name,
            tool: t.name as string | undefined,
          })),
        ],
        { placeHolder: "Clear the build cache of" },
      );
      if (got === undefined) return;
      tool = got.tool;
    }
    const ok = await vscode.window.showWarningMessage(
      tool === undefined
        ? "Clear the whole build cache? Every build runs again once."
        : `Clear the build cache of ${tool}? Its builds run again once.`,
      { modal: true },
      "Clear",
    );
    if (ok !== "Clear") return;
    try {
      const r = await busy("Clearing the build cache", () => e.api.clear(m.root, tool));
      void vscode.window.showInformationMessage(
        `Cleared ${r.entries} entries, ${humanSize(r.freed)} freed.`,
      );
    } catch (err) {
      fail(err);
    }
    await m.refresh();
  });
  cmd(COMMANDS.gc, async () => {
    const m = await pickRoot();
    if (m === undefined) return;
    try {
      const r = await busy("Cleaning up the build cache", () => e.api.gc(m.root));
      void vscode.window.showInformationMessage(
        `Removed ${r.entries} entries, ${humanSize(r.freed)} freed.`,
      );
    } catch (err) {
      fail(err);
    }
    await m.refresh();
  });
  cmd(COMMANDS.warm, async () => {
    const m = await pickRoot();
    if (m === undefined) return;
    try {
      const r = await busy("Warming the build cache for this revision", () => e.api.warm(m.root));
      void vscode.window.showInformationMessage(
        `Fetched ${r.fetched} entries (${humanSize(r.bytes)}); ${r.local} were local already.`,
      );
    } catch (err) {
      fail(err);
    }
    await m.refresh();
  });
  cmd(COMMANDS.setup, async () => {
    if (!vscode.workspace.isTrusted) {
      void vscode.window.showWarningMessage("Trust this workspace to set up the build cache.");
      return;
    }
    const m = await pickRoot();
    if (m === undefined) return;
    const found = new Set(await detectSetupTools(diskFiles(m.root)));
    const picks = await vscode.window.showQuickPick(
      CACHE_SETUP_TOOLS.map((tool) => ({
        label: tool,
        description: found.has(tool) ? "used here" : "",
        picked: found.has(tool),
        tool,
      })),
      { canPickMany: true, placeHolder: "Tools to cache (asc cache setup)" },
    );
    if (picks === undefined || picks.length === 0) return;
    const done: string[] = [];
    const failed: string[] = [];
    await busy("Setting up the build cache", async () => {
      for (const p of picks) {
        try {
          await e.api.setup(m.root, p.tool as CacheSetupTool, false);
          done.push(p.tool);
        } catch (err) {
          failed.push(error(err));
        }
      }
    });
    if (done.length > 0) {
      void vscode.window.showInformationMessage(`Build cache set up for ${done.join(", ")}.`);
    }
    for (const f of failed) void vscode.window.showErrorMessage(f);
    await applyEnv();
  });

  // --- the tasks ----------------------------------------------------------------------------------
  const envs = new Map<string, Record<string, string>>();
  const envFor = async (root: string): Promise<Record<string, string>> => {
    const have = envs.get(root);
    if (have !== undefined) return have;
    let got: Record<string, string>;
    try {
      got = await e.api.env(root);
    } catch {
      got = {};
    }
    envs.set(root, got);
    return got;
  };
  const toTask = async (
    root: string,
    folder: vscodeTypes.WorkspaceFolder | vscodeTypes.TaskScope,
    def: BuildTaskDefinition,
    s: BuildSpec,
  ): Promise<vscodeTypes.Task | undefined> => {
    const asc = e.ascPath();
    if (asc === null) return undefined;
    const env = { ...(await envFor(root)), ...s.env };
    const exec =
      s.command === "asc"
        ? new vscode.ProcessExecution(asc, s.args, { cwd: root, env })
        : new vscode.ShellExecution(
            s.command,
            s.args.map((value) => ({ value, quoting: vscode.ShellQuoting.Strong })),
            { cwd: root, env },
          );
    const task = new vscode.Task(def, folder, s.label, "asc build cache", exec, s.matchers);
    task.detail = s.detail;
    task.group = vscode.TaskGroup.Build;
    return task;
  };
  const folderOf = (root: string): vscodeTypes.WorkspaceFolder | vscodeTypes.TaskScope =>
    vscode.workspace.getWorkspaceFolder(vscode.Uri.file(root)) ?? vscode.TaskScope.Workspace;
  const rootOf = (scope: vscodeTypes.Task["scope"]): string | undefined => {
    if (typeof scope === "object" && scope !== null && "uri" in scope) {
      const p = scope.uri.fsPath;
      if (byRoot.has(p)) return p;
    }
    return first?.root;
  };
  subs.push(
    vscode.tasks.registerTaskProvider(BUILD_TASK_TYPE, {
      async provideTasks() {
        const asc = e.ascPath();
        if (!vscode.workspace.isTrusted || asc === null) return [];
        const out: vscodeTypes.Task[] = [];
        for (const m of models) {
          for (const s of await detectBuilds(diskFiles(m.root), asc)) {
            const t = await toTask(m.root, folderOf(m.root), definitionOf(s), s);
            if (t !== undefined) out.push(t);
          }
        }
        return out;
      },
      async resolveTask(task) {
        const asc = e.ascPath();
        if (!vscode.workspace.isTrusted || asc === null) return undefined;
        const root = rootOf(task.scope);
        if (root === undefined) return undefined;
        const d = task.definition as Partial<BuildTaskDefinition>;
        let s: BuildSpec | undefined;
        if (typeof d.build === "string") {
          s = (await detectBuilds(diskFiles(root), asc)).find((x) => x.id === d.build);
        } else {
          s = declaredRun(d);
        }
        if (s === undefined) return undefined;
        const scope = task.scope ?? vscode.TaskScope.Workspace;
        return toTask(
          root,
          scope as vscodeTypes.WorkspaceFolder | vscodeTypes.TaskScope,
          d as BuildTaskDefinition,
          {
            ...s,
            label: task.name,
          },
        );
      },
    }),
  );
  // The hits and misses of one task: the store's counters before and after it.
  const started = new Map<vscodeTypes.TaskExecution, CacheStatus | null>();
  subs.push(
    vscode.tasks.onDidStartTask((ev) => {
      if (ev.execution.task.definition.type !== BUILD_TASK_TYPE || first === undefined) return;
      started.set(ev.execution, first.status);
      void first.refresh().then((s) => {
        if (started.has(ev.execution)) started.set(ev.execution, s);
      });
    }),
    vscode.tasks.onDidEndTask((ev) => {
      if (first === undefined) return;
      const own = started.has(ev.execution);
      const before = started.get(ev.execution) ?? null;
      started.delete(ev.execution);
      void first.refresh().then((after) => {
        if (!own) return;
        const d = delta(before, after);
        last = { label: ev.execution.task.name, ...d };
        paint();
      });
    }),
  );

  // --- terminal variables: turbo, Nx and Maven in a terminal reach the local fronts --------------
  const collection = (ctx as Partial<vscodeTypes.ExtensionContext>).environmentVariableCollection;
  const applyEnv = async (): Promise<void> => {
    if (collection === undefined || first === undefined) return;
    envs.delete(first.root);
    if (!e.terminalEnv() || !vscode.workspace.isTrusted) {
      collection.clear();
      return;
    }
    const vars = await envFor(first.root);
    collection.clear();
    collection.description = "The asc build cache: turbo, Nx and Maven use the local cache fronts.";
    for (const [k, v] of Object.entries(vars)) collection.replace(k, v);
  };
  subs.push(
    vscode.workspace.onDidChangeConfiguration((ev) => {
      if (ev.affectsConfiguration("asc.cache.terminalEnv")) void applyEnv();
    }),
  );

  paint();
  void refreshAll();
  void applyEnv();
  return {
    dispose() {
      for (const m of models) m.dispose();
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
