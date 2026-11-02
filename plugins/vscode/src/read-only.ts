// The editor side of path-rule read-only paths (SPEC 15.5.1): a lock decoration in the explorer and
// editors opened read-only. The stored levels are reread when `asc fetch` rewrites them.
import type * as vscodeTypes from "vscode";
import { ACCESS_FILE, commonDir, loadAccess, type PathAccess, writable } from "./path-access";
import { nodeDetectFs, resolveGitDir } from "./repo";

const READ_ONLY_IN_SESSION = "workbench.action.files.setActiveEditorReadonlyInSession";

/** The asc root holding `fsPath` and the repo-relative path, or null. */
export function relTo(
  roots: readonly string[],
  fsPath: string,
  win = process.platform === "win32",
): { root: string; path: string } | null {
  const norm = (s: string) => (win ? s.toLowerCase() : s);
  const p = fsPath.replace(/\\/g, "/");
  for (const root of roots) {
    const r = root.replace(/\\/g, "/").replace(/\/+$/, "");
    if (norm(p).startsWith(`${norm(r)}/`)) return { root, path: p.slice(r.length + 1) };
  }
  return null;
}

export function registerReadOnly(
  vscode: typeof vscodeTypes,
  roots: readonly string[],
  log: (line: string) => void,
): vscodeTypes.Disposable {
  const access = new Map<string, PathAccess | null>();
  const subs: { dispose(): unknown }[] = [];
  const changed = new vscode.EventEmitter<undefined>();
  subs.push(changed);

  const locked = (uri: vscodeTypes.Uri): boolean => {
    if (uri.scheme !== "file") return false;
    const hit = relTo(roots, uri.fsPath);
    return hit !== null && !writable(access.get(hit.root) ?? null, hit.path);
  };
  const guardActive = (e: vscodeTypes.TextEditor | undefined) => {
    if (e && locked(e.document.uri)) {
      void vscode.commands.executeCommand(READ_ONLY_IN_SESSION);
    }
  };

  for (const root of roots) {
    void (async () => {
      const gitDir = await resolveGitDir(root, nodeDetectFs);
      if (!gitDir) return;
      const common = await commonDir(gitDir);
      const reload = async () => {
        access.set(root, await loadAccess(gitDir));
        changed.fire(undefined);
        guardActive(vscode.window.activeTextEditor);
      };
      const w = vscode.workspace.createFileSystemWatcher(
        new vscode.RelativePattern(vscode.Uri.file(common), ACCESS_FILE),
      );
      w.onDidCreate(reload);
      w.onDidChange(reload);
      w.onDidDelete(reload);
      subs.push(w);
      await reload();
    })().catch((e) => log(`asc path access ${root}: ${String(e)}`));
  }

  subs.push(
    vscode.window.registerFileDecorationProvider({
      onDidChangeFileDecorations: changed.event,
      provideFileDecoration(uri) {
        return locked(uri)
          ? new vscode.FileDecoration(
              "RO",
              "Read-only: the repository's path rules",
              new vscode.ThemeColor("disabledForeground"),
            )
          : undefined;
      },
    }),
    vscode.window.onDidChangeActiveTextEditor(guardActive),
  );
  return { dispose: () => subs.forEach((s) => s.dispose()) };
}
