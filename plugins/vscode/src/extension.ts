// ASC for VS Code: activation, detection, registration (SPEC 24.4.1). `activate` reads one
// small file and stats one marker per folder and returns; in an asc folder it registers synchronously
// and starts the bridge lazily, fire-and-forget. The editor API is reached through `Host` so the
// logic runs under Bun without VS Code; `vscodeHost` is the real adapter and the only `vscode` user.
import { homedir } from "node:os";
import { callUnary } from "@asc/asc-client";
import { Workspace } from "@asc/asc-client/workspace";
import {
  type BridgeManager,
  type BridgeManagerOptions,
  type Compat,
  createBridgeManager,
  FEATURE_CONTEXT_KEYS,
  type HelloInfo,
  nodeIsFile,
  resolveAsc,
  serverFeatures,
  serverTargets,
} from "./bridge";
import { createCliDirect } from "./cli-direct";
import { bridgeConnectivity, followConnectivity } from "./connectivity";
import { detectAscFolders } from "./repo";
import { registerBlame } from "./blame/register";
import { registerConflicts } from "./conflicts";
import { registerHydration } from "./hydration";
import { InboxCountPoller, registerInboxCount } from "./inbox-count";
import { configureAgents, registerMcp, vscodeMcpApi } from "./mcp";
import { StackCheckout } from "./review/checkout";
import { bridgeReviewApi, currentChangeRef, registerReviewComments } from "./review/comments";
import { registerRevNav } from "./revnav";
import { registerCommands, vscodeCommandsUi } from "./scm/commands";
import { registerIgnoreCommand } from "./scm/ignore";
import { registerScm } from "./scm/provider";
import { bridgeRemoteApi, registerRemoteCommands } from "./scm/remote";
import { registerStashCommands } from "./scm/stash";
import { bridgeStatusApi, registerStackEditCommands, registerStatusBar } from "./scm/statusbar";
import { registerSearch } from "./search";
import { registerStackActions } from "./stack/actions";
import { cliCloudApi, CloudStatusModel, registerCloud } from "./stack/cloud";
import { cliLiveApi, LiveModel, registerLive } from "./stack/live";
import { registerStackNav } from "./stack/nav";
import { registerShare } from "./stack/share";
import { registerTasks } from "./tasks";
import { bridgeActionsApi, currentChange, registerActions, splitRepo } from "./views/actions";
import { AffectedModel, bridgeAffectedApi, registerAffected } from "./views/affected";
import { bridgeHistoryApi, registerFileHistory } from "./views/file-history";
import { bridgeInboxApi, InboxModel, registerInbox } from "./views/inbox";
import { cliOpLogApi, registerOpLog } from "./views/oplog";
import {
  bridgeViewApi,
  pump,
  registerWorktrees,
  runVerb,
  wsIdFor,
} from "./views/worktrees";
import type { WebviewHost } from "./webview/host";
import { vscodeWebviews } from "./webview/vscode-adapter";

export interface Disposable {
  dispose(): void;
}

export interface Host {
  workspaceFolders(): string[];
  /** `asc.path` as configured. */
  ascPathSetting(): string;
  /** The effective `git.enabled` for a folder. */
  gitEnabled(folder: string): boolean;
  /** Writes `"git.enabled": false` at workspace scope. */
  disableBuiltinGit(): Promise<void>;
  /** Resolves to the chosen button or undefined when dismissed. */
  notify(message: string, ...buttons: string[]): Promise<string | undefined>;
  registerCommand(id: string, run: () => void): Disposable;
  setContext(key: string, value: boolean): Promise<void>;
  log(line: string): void;
  showOutput(): void;
  /** Workspace-state memory so the git prompt shows once. */
  memento: {
    get(key: string): boolean | undefined;
    set(key: string, v: boolean): Thenable<void> | Promise<void>;
  };
  env: Record<string, string | undefined>;
  platform: NodeJS.Platform;
  /** Registers the SCM provider, `asc:` file system and decorations (P25). Absent under tests. */
  registerScm?(bridge: BridgeManager, folders: readonly string[]): Disposable;
  /** Registers inline blame, hover, CodeLens and the heatmap (P27). Absent under tests. */
  registerBlame?(bridge: BridgeManager, folders: readonly string[]): Disposable;
  /** Registers the asc-ui webview hosts (P28a, P28b). Absent under tests. */
  registerWebviews?(folders: readonly string[], bridge?: BridgeManager): WebviewHost;
  /** Registers the commands, trees, status bar, search, MCP and hydration contributions
   * (P26-P34, P47a, P49a). The plugin talks to asc only; nothing here knows a forge. `hello` decides
   * which asc serve services (inbox, review, ci) register. Absent under tests. */
  registerFeatures?(
    bridge: BridgeManager,
    folders: readonly string[],
    hello: HelloInfo,
  ): Disposable;
}

export const GIT_PROMPT_BUTTON = "Use asc";
export const GIT_PROMPT_TEXT = "Use asc instead of built-in git for this workspace";
const GIT_PROMPT_KEY = "asc.gitPromptShown";

export interface Activation extends Disposable {
  /** Folders that are asc workspaces. */
  readonly ascFolders: readonly string[];
  readonly bridge: BridgeManager | null;
  /** Settles when the fire-and-forget start (Hello and the feature check) is done. */
  readonly started: Promise<Compat | null>;
}

const NOT_ASC: Activation = {
  ascFolders: [],
  bridge: null,
  started: Promise.resolve(null),
  dispose() {},
};

function updateNotice(compat: Compat, host: Host, version: string): void {
  if (compat.level === "full") return;
  const shown = version === "" ? "" : ` (found asc ${version})`;
  void host.notify(`Update asc: this plugin needs a newer asc${shown}.`);
}

export async function activateWith(
  host: Host,
  create?: BridgeManagerOptions["create"],
): Promise<Activation> {
  const ascFolders = await detectAscFolders(host.workspaceFolders());
  if (ascFolders.length === 0) return NOT_ASC;

  const subs: Disposable[] = [];
  const bridge = createBridgeManager({
    command: () =>
      resolveAsc({
        setting: host.ascPathSetting(),
        path: host.env.PATH,
        ascHome: host.env.ASC_HOME,
        home: host.env.HOME ?? homedir(),
        platform: host.platform,
        isFile: nodeIsFile,
      }),
    onStderr: (line) => host.log(line),
    ...(create === undefined ? {} : { create }),
  });

  // Contributions register synchronously, before any I/O. Mutating commands stay disabled until the
  // feature check passes.
  void host.setContext("asc.workspace", true);
  void host.setContext("asc.mutating", false);
  subs.push(
    host.registerCommand("asc.showOutput", () => host.showOutput()),
    host.registerCommand("asc.restartBridge", () => {
      bridge.restart();
      void start();
    }),
  );

  let scmReg: Disposable | null = null;
  let blameReg: Disposable | null = null;
  let registering = false;
  const start = async (): Promise<Compat | null> => {
    try {
      const hello = await bridge.hello();
      await host.setContext("asc.mutating", hello.compat.level === "full");
      updateNotice(hello.compat, host, hello.ascVersion);
      // The server services contribute on the same condition the code registers on (Hello.features).
      const server = serverFeatures(hello);
      for (const k of ["inbox", "review", "ci"] as const) {
        await host.setContext(FEATURE_CONTEXT_KEYS[k], server[k]);
      }
      // Read-only views need only plugin.v1, so the SCM registers for every level but too-old.
      if (hello.compat.level !== "too-old" && host.registerScm && !scmReg) {
        scmReg = host.registerScm(bridge, ascFolders);
        subs.push(scmReg);
      }
      if (hello.compat.level !== "too-old" && host.registerBlame && !blameReg) {
        blameReg = host.registerBlame(bridge, ascFolders);
        subs.push(blameReg);
      }
      if (hello.compat.level !== "too-old" && !registering) {
        registering = true;
        if (host.registerWebviews) subs.push(host.registerWebviews(ascFolders, bridge));
        if (host.registerFeatures) subs.push(host.registerFeatures(bridge, ascFolders, hello));
      }
      return hello.compat;
    } catch (e) {
      host.log(
        `the asc bridge did not start or a feature failed to register; check asc.path: ${String(e)}`,
      );
      return null;
    }
  };

  const started = start();
  void promptBuiltinGit(host, ascFolders);

  return {
    ascFolders,
    bridge,
    started,
    dispose() {
      for (const s of subs) s.dispose();
      bridge.dispose();
    },
  };
}

/** One notification per workspace when the built-in git is still on for an asc folder. */
async function promptBuiltinGit(host: Host, ascFolders: readonly string[]): Promise<void> {
  if (host.memento.get(GIT_PROMPT_KEY)) return;
  if (!ascFolders.some((f) => host.gitEnabled(f))) return;
  await host.memento.set(GIT_PROMPT_KEY, true);
  const choice = await host.notify(GIT_PROMPT_TEXT, GIT_PROMPT_BUTTON);
  if (choice === GIT_PROMPT_BUTTON) await host.disableBuiltinGit();
}

// --- the real adapter ---------------------------------------------------------------------------

import type * as vscodeTypes from "vscode";
let active: Activation | null = null;

type Sub = { dispose(): unknown };

interface FeatureEnv {
  vscode: typeof vscodeTypes;
  ctx: vscodeTypes.ExtensionContext;
  mgr: BridgeManager;
  roots: readonly string[];
  hello: HelloInfo;
  log(line: string): void;
  ascPath(): string;
  /** Variables for the asc children the extension starts itself, such as a token the host holds. */
  extraEnv?(): Record<string, string | undefined>;
}

function disposeAll(subs: Sub[]): void {
  for (const s of subs.splice(0).toReversed()) s.dispose();
}

/** Registers every contribution that is not the SCM provider, blame or the webview hosts. */
export function registerAll(e: FeatureEnv): Disposable {
  const { vscode, ctx, mgr, roots, hello, log } = e;
  const subs: Sub[] = [];
  const cmd = (id: string, fn: (...a: never[]) => unknown): void => {
    subs.push(vscode.commands.registerCommand(id, fn as (...a: unknown[]) => unknown));
  };
  const cfg = () => vscode.workspace.getConfiguration("asc");
  const ui = vscodeCommandsUi(vscode, log);
  const statusApi = bridgeStatusApi(mgr);
  const viewApi = bridgeViewApi(mgr);
  const historyApi = bridgeHistoryApi(mgr);
  const onWatchError = (err: unknown) => log(`asc watch: ${String(err)}`);
  // Read-mostly verbs the bridge allowlist does not carry (op log, cloud status, live).
  const cli = createCliDirect({
    command: e.ascPath,
    ...(e.extraEnv === undefined ? {} : { extraEnv: e.extraEnv }),
  });

  // P31a: stack checkout through `asc stack get`; the asc server decides what a remote supports.
  const checkout = new StackCheckout(statusApi, { info: ui.info, error: ui.error });

  // P26a: create, amend, reword, absorb, stage ranges and hunks, discard, undo, redo.
  subs.push(registerCommands(vscode, mgr, roots, log));

  // P26b: status bar, stack edits, stash, fetch/pull/push, .ascignore. The status bar refreshes on
  // head, ref and op-finished events and after each command.
  let bar: ReturnType<typeof registerStatusBar> | undefined;
  const after = (root: string): void => void bar?.refresh(root);
  bar = registerStatusBar(vscode, statusApi, roots, log, (cb) => {
    const ac = new AbortController();
    for (const root of roots) {
      void pump(
        viewApi,
        root,
        ac.signal,
        (k) => {
          if (
            k === "head" ||
            k === "ref" ||
            k === "op-finished" ||
            k === "switch-done" ||
            k === "resync" ||
            k === "stack"
          ) {
            cb(root);
          }
        },
        onWatchError,
      );
    }
    return { dispose: () => ac.abort() };
  });
  subs.push(
    bar,
    registerStackEditCommands(vscode, statusApi, ui, roots, after),
    registerStashCommands(vscode, statusApi, ui, roots, after),
    registerRemoteCommands(vscode, bridgeRemoteApi(mgr, statusApi), ui, roots, after),
    registerIgnoreCommand(vscode, roots, log),
    // P47a: stack navigation, restack/sync/submit and the share link.
    registerStackNav(vscode, statusApi, ui, roots, after),
    registerStackActions(vscode, statusApi, ui, roots, after),
    registerShare(vscode, (r) => statusApi.stack(r), roots),
    // P30: conflicts banner, open, mark resolved, continue, abort.
    registerConflicts(vscode, mgr, roots, log),
    // P29: trees and revision navigation.
    registerWorktrees(vscode, viewApi, roots, log),
    registerOpLog(vscode, viewApi, cliOpLogApi(cli), roots, log),
    registerFileHistory(vscode, historyApi, roots, log),
    registerRevNav(vscode, historyApi, roots, log),
    // P34: "ASC: Search code".
    registerSearch(vscode, mgr, roots, log),
  );

  // P32: Affected tree and the Run target task provider.
  const affected = roots.map(
    (root) =>
      new AffectedModel({
        root,
        api: bridgeAffectedApi(mgr),
        onError: (err) => log(`asc affected: ${String(err)}`),
      }),
  );
  subs.push(
    registerAffected(vscode, affected, viewApi, pump, log),
    registerTasks(vscode, affected, log),
  );

  // P47b and P59: cloud upload state and live updates, for the first root. Both refresh on events only:
  // the `Watch` stream, `Bridge.Events` connectivity and a finished op; there is no timer.
  const first = roots[0];
  if (first !== undefined) {
    const ac = new AbortController();
    const cloud = new CloudStatusModel({
      api: cliCloudApi(cli),
      root: first,
      onError: (err) => log(`asc cloud: ${String(err)}`),
    });
    const live = new LiveModel({
      api: cliLiveApi(cli, async (root) => ({
        exit: (await runVerb(mgr.node(), root, "restack", [])).exit,
      })),
      root: first,
      onError: (err) => log(`asc live: ${String(err)}`),
    });
    const behind = async (): Promise<void> => {
      try {
        live.setBehind((await statusApi.stack(first))?.behind ?? 0);
      } catch (err) {
        log(`asc live: ${String(err)}`);
      }
    };
    void pump(
      viewApi,
      first,
      ac.signal,
      (k) => {
        if (k === "cloud" || k === "op-finished" || k === "resync") void cloud.onEvent("cloud");
        if (k === "head" || k === "ref" || k === "op-finished" || k === "resync") {
          void live.refresh();
          void behind();
        }
      },
      onWatchError,
    );
    void followConnectivity(
      bridgeConnectivity(mgr),
      ac.signal,
      (ev) => {
        if (ev.root === first) void cloud.onEvent("connectivity", ev.server === "up");
      },
      onWatchError,
    );
    void cloud.refresh();
    void live.refresh();
    void behind();
    subs.push(
      { dispose: () => ac.abort() },
      registerCloud(vscode, cloud),
      registerLive(vscode, live),
    );
  }

  // P33b: MCP registration and "Configure agents".
  const mcp = registerMcp({
    api: vscodeMcpApi(vscode),
    ascPath: e.ascPath,
    roots: () => roots,
    serverUrl: () => cfg().get<string>("mcp.serverUrl", ""),
  });
  subs.push(
    mcp,
    vscode.workspace.onDidChangeConfiguration((ev) => {
      if (ev.affectsConfiguration("asc.mcp.serverUrl") || ev.affectsConfiguration("asc.path")) {
        mcp.refresh();
      }
    }),
  );
  cmd("asc.configureAgents", async () => {
    void vscode.window.showInformationMessage(await configureAgents(e.ascPath(), roots));
  });

  // P49a: hydration guard. Its only data source is `Workspace.Stats`.
  subs.push(
    registerHydration(
      vscode,
      {
        async stats(root, signal) {
          const r = await callUnary(
            mgr.node(),
            Workspace.method.stats,
            { wsId: await wsIdFor(mgr, root) },
            { root, ...(signal ? { signal } : {}) },
          );
          const rows = (l: typeof r.byPath) =>
            l.map((c) => ({ key: c.key, count: Number(c.count) }));
          return { byPath: rows(r.byPath), byProcess: rows(r.byProcess) };
        },
      },
      roots,
      ctx.workspaceState,
    ),
  );

  cmd("asc.review.checkoutStack", async () => {
    const t = await vscode.window.showInputBox({
      prompt: "Review number to check out as a stack",
      validateInput: (v) => (/^[1-9]\d{0,9}$/.test(v) ? undefined : "Enter a review number"),
    });
    if (!t) return;
    const n = Number(t);
    const root =
      roots.length === 1
        ? roots[0]
        : await vscode.window.showQuickPick([...roots], { placeHolder: "Choose an ASC root" });
    if (!root) return;
    await checkout.run(root, { kind: "pr", number: n });
    after(root);
  });

  // The asc serve services (inbox.v1, review.v1, ci.v1): each registers only when Hello advertises
  // it and a root has an asc server, the same condition that sets its `asc.feature.*` context key.
  const feat = serverFeatures(hello);
  const targets = serverTargets(hello);
  const targetOf = (root: string) => targets.find((t) => t.root === root);
  const serverRoots = targets.map((t) => t.root);
  if (feat.inbox) {
    const api = bridgeInboxApi(mgr, () => targets);
    const model = new InboxModel({
      api,
      signedIn: () => targets.length > 0,
      onError: (err) => log(`asc inbox: ${String(err)}`),
    });
    const poller = new InboxCountPoller({
      api,
      signedIn: () => targets.length > 0,
      enabled: () => cfg().get<boolean>("inbox.statusBar", true),
      onError: (err) => log(`asc inbox count: ${String(err)}`),
    });
    subs.push(
      registerInbox(vscode, model, log, async (item) => {
        await checkout.run(item.root, { kind: "change", id: item.id });
        after(item.root);
      }),
      registerInboxCount(vscode, poller),
    );
  }
  if (feat.review) {
    subs.push(
      registerReviewComments(
        vscode,
        bridgeReviewApi(mgr),
        {
          current: async (root) => {
            const t = targetOf(root);
            return t ? currentChangeRef(mgr, root, t.repo) : undefined;
          },
        },
        serverRoots,
        log,
      ),
    );
  }
  if (feat.ci) {
    const apis = new Map(targets.map((t) => [t.root, bridgeActionsApi(mgr, t)]));
    subs.push(
      registerActions(
        vscode,
        {
          async target(root) {
            const t = targetOf(root);
            const name = t ? splitRepo(t.repo) : undefined;
            if (!name) return undefined;
            const change = await currentChange(mgr, root).catch(() => undefined);
            return { ...name, ...(change === undefined ? {} : { change }) };
          },
          api: (root) => apis.get(root),
          onStale(root, fn) {
            const ac = new AbortController();
            void pump(
              viewApi,
              root,
              ac.signal,
              (k) => {
                if (k === "head" || k === "ref" || k === "op-finished") fn();
              },
              onWatchError,
            );
            return { dispose: () => ac.abort() };
          },
        },
        serverRoots,
        log,
      ),
    );
  }

  return { dispose: () => disposeAll(subs) };
}

export function vscodeHost(vscode: typeof vscodeTypes, ctx: vscodeTypes.ExtensionContext): Host {
  let channel: vscodeTypes.OutputChannel | undefined;
  const out = () => {
    channel ??= vscode.window.createOutputChannel("ASC", { log: true });
    ctx.subscriptions.push(channel);
    return channel;
  };
  const ascPathSetting = () => vscode.workspace.getConfiguration("asc").get<string>("path", "");
  const ascPath = () =>
    resolveAsc({
      setting: ascPathSetting(),
      path: process.env.PATH,
      ascHome: process.env.ASC_HOME,
      home: process.env.HOME ?? homedir(),
      platform: process.platform,
      isFile: nodeIsFile,
    });
  return {
    workspaceFolders: () =>
      (vscode.workspace.workspaceFolders ?? [])
        .filter((f) => f.uri.scheme === "file")
        .map((f) => f.uri.fsPath),
    ascPathSetting,
    gitEnabled: (folder) =>
      vscode.workspace
        .getConfiguration("git", vscode.Uri.file(folder))
        .get<boolean>("enabled", true),
    disableBuiltinGit: () =>
      Promise.resolve(
        vscode.workspace
          .getConfiguration("git")
          .update("enabled", false, vscode.ConfigurationTarget.Workspace),
      ),
    notify: (message, ...buttons) =>
      Promise.resolve(vscode.window.showInformationMessage(message, ...buttons)),
    registerCommand: (id, run) => vscode.commands.registerCommand(id, run),
    setContext: (key, value) =>
      Promise.resolve(vscode.commands.executeCommand("setContext", key, value)).then(() => {}),
    log: (line) => out().appendLine(line),
    showOutput: () => out().show(true),
    memento: {
      get: (k) => ctx.workspaceState.get<boolean>(k),
      set: (k, v) => ctx.workspaceState.update(k, v),
    },
    env: process.env,
    platform: process.platform,
    registerWebviews: (folders, bridge) =>
      vscodeWebviews(vscode, ctx, folders, (l) => out().appendLine(l), bridge),
    registerScm: (bridge, folders) => {
      const reg = registerScm(vscode, bridge, folders, (l) => out().appendLine(l));
      return reg;
    },
    registerBlame: (bridge, folders) =>
      registerBlame(vscode, bridge, folders, (l) => out().appendLine(l)),
    registerFeatures: (mgr, roots, hello) =>
      registerAll({ vscode, ctx, mgr, roots, hello, log: (l) => out().appendLine(l), ascPath }),
  };
}

export async function activate(ctx: vscodeTypes.ExtensionContext): Promise<void> {
  const vscode = require("vscode") as typeof vscodeTypes;
  active = await activateWith(vscodeHost(vscode, ctx));
  ctx.subscriptions.push({ dispose: () => active?.dispose() });
}

export function deactivate(): void {
  active?.dispose();
  active = null;
}
