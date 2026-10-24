// ASC for VS Code: activation, detection, registration (SPEC 24.4.1). `activate` reads one
// small file and stats one marker per folder and returns; in an asc folder it registers synchronously
// and starts the bridge lazily, fire-and-forget. The editor API is reached through `Host` so the
// logic runs under Bun without VS Code; `vscodeHost` is the real adapter and the only `vscode` user.
import { homedir } from "node:os";
import {
  type BridgeManager,
  type BridgeManagerOptions,
  type Compat,
  createBridgeManager,
  nodeIsFile,
  resolveAsc,
} from "./bridge";
import { detectAscFolders } from "./repo";

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

  const start = async (): Promise<Compat | null> => {
    try {
      const hello = await bridge.hello();
      await host.setContext("asc.mutating", hello.compat.level === "full");
      updateNotice(hello.compat, host, hello.ascVersion);
      return hello.compat;
    } catch {
      host.log("the asc bridge did not start; check asc.path");
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

function vscodeHost(vscode: typeof vscodeTypes, ctx: vscodeTypes.ExtensionContext): Host {
  let channel: vscodeTypes.OutputChannel | undefined;
  const out = () => {
    channel ??= vscode.window.createOutputChannel("asc", { log: true });
    ctx.subscriptions.push(channel);
    return channel;
  };
  return {
    workspaceFolders: () =>
      (vscode.workspace.workspaceFolders ?? [])
        .filter((f) => f.uri.scheme === "file")
        .map((f) => f.uri.fsPath),
    ascPathSetting: () => vscode.workspace.getConfiguration("asc").get<string>("path", ""),
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
