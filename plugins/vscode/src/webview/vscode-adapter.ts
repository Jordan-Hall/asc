// The real VS Code adapter for the webview hosts (the only `vscode` user in this folder).
import { existsSync } from "node:fs";
import type * as vscodeTypes from "vscode";
import { type Disposable, type PanelLike, registerWebviews, type WebviewHost, type WebviewLike } from "./host";
import type { PanelKind } from "./panels";
import type { BridgeManager } from "../bridge";
import { nativeConfirm } from "./confirm";
import { createRelayHandler } from "./relay";

const COMMANDS: ReadonlyArray<readonly [string, PanelKind]> = [
  ["asc.openGraph", "graph"],
  ["asc.openReview", "review"],
  ["asc.openHistory", "history"],
  ["asc.openProject", "project"],
  ["asc.openSearch", "search"],
];

export function vscodeWebviews(
  vscode: typeof vscodeTypes,
  ctx: vscodeTypes.ExtensionContext,
  roots: readonly string[],
  log: (line: string) => void,
  bridge?: BridgeManager,
): WebviewHost {
  const dist = vscode.Uri.joinPath(ctx.extensionUri, "dist", "ui");
  const wrap = (w: vscodeTypes.Webview): WebviewLike => ({
    get html() {
      return w.html;
    },
    set html(v: string) {
      w.html = v;
    },
    cspSource: w.cspSource,
    get options() {
      return w.options;
    },
    set options(v) {
      w.options = { enableScripts: v.enableScripts ?? false, localResourceRoots: v.localResourceRoots as vscodeTypes.Uri[] };
    },
    distUri: (file) => w.asWebviewUri(vscode.Uri.joinPath(dist, file)).toString(),
    postMessage: (m) => w.postMessage(m),
    onDidReceiveMessage: (l) => w.onDidReceiveMessage(l),
  });
  const wrapPanel = (p: vscodeTypes.WebviewPanel): PanelLike => ({
    webview: wrap(p.webview),
    get title() {
      return p.title;
    },
    set title(v: string) {
      p.title = v;
    },
    reveal: () => p.reveal(),
    onDidDispose: (l) => p.onDidDispose(l),
    dispose: () => p.dispose(),
  });

  // The webview reaches asc only: the relay allowlist has no remote-service request kind here.
  const relayHandler = bridge
    ? createRelayHandler({
        upstream: () => bridge.node(),
        roots: () => roots,
        forgeRepos: () => [],
        confirm: nativeConfirm((message, options, action) => vscode.window.showWarningMessage(message, options, action)),
        log,
      })
    : undefined;
  const host = registerWebviews({
    roots,
    ...(relayHandler ? { onRequest: relayHandler } : {}),
    api: {
      distRoot: dist,
      distHasStyle: existsSync(vscode.Uri.joinPath(dist, "index.css").fsPath),
      log,
      createPanel: (viewType, title) =>
        wrapPanel(vscode.window.createWebviewPanel(viewType, title, vscode.ViewColumn.Active, { enableScripts: true, retainContextWhenHidden: false })),
      registerViewProvider: (id, resolve): Disposable =>
        vscode.window.registerWebviewViewProvider(id, {
          resolveWebviewView: (view) => resolve(wrap(view.webview)),
        }),
      registerSerializer: (viewType, restore): Disposable =>
        vscode.window.registerWebviewPanelSerializer(viewType, {
          deserializeWebviewPanel: (panel, state) => restore(wrapPanel(panel), state),
        }),
    },
  });
  const cmds = COMMANDS.map(([id, kind]) => vscode.commands.registerCommand(id, () => void host.open(kind)));
  return {
    open: (k) => host.open(k),
    dispose() {
      for (const c of cmds) c.dispose();
      relayHandler?.dispose();
      host.dispose();
    },
  };
}
