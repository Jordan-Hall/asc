// Revision navigation (SPEC 24.4.4): `editor/title` "previous version" and "next version" for the open
// file. The versions are the file's history (`Workspace.Log` with follow), newest first. A working-tree
// file sits before the newest version; stepping next from the newest version returns to it.
// `stepRevision` is editor-free; `registerRevNav` is the only `vscode` user.
import type * as vscodeTypes from "vscode";
import { parseAscUri, toAscUri } from "./fs/ascfs";
import type { HistoryApi } from "./views/file-history";

export const NAV_LIMIT = 200;

export type Direction = "previous" | "next";
export type NavTarget = { kind: "rev"; rev: string } | { kind: "working" };

/**
 * `ids` are commit ids newest first. `current` is the shown revision, or null for the working file.
 * Returns where to go, or null at either end (or when `current` is not in the list).
 */
export function stepRevision(
  ids: readonly string[],
  current: string | null,
  dir: Direction,
): NavTarget | null {
  if (current === null) {
    if (dir === "next") return null;
    const first = ids[0];
    return first === undefined ? null : { kind: "rev", rev: first };
  }
  const i = ids.indexOf(current);
  if (i < 0) return null;
  if (dir === "previous") {
    const older = ids[i + 1];
    return older === undefined ? null : { kind: "rev", rev: older };
  }
  const newer = ids[i - 1];
  if (newer !== undefined) return { kind: "rev", rev: newer };
  return { kind: "working" };
}

export interface NavInput {
  root: string;
  path: string;
  /** The revision of the shown `asc:` file; null for the working file. */
  rev: string | null;
}

export async function navigate(
  api: HistoryApi,
  input: NavInput,
  dir: Direction,
  signal?: AbortSignal,
): Promise<NavTarget | null> {
  const page = await api.history(
    input.root,
    { path: input.path, lines: null, limit: NAV_LIMIT, pageToken: "" },
    signal,
  );
  return stepRevision(
    page.entries.map((e) => e.id),
    input.rev,
    dir,
  );
}

function relPath(root: string, fsPath: string): string | null {
  const a = root.replace(/\\/g, "/").replace(/\/+$/, "");
  const b = fsPath.replace(/\\/g, "/");
  const win = /^[A-Za-z]:/.test(a);
  const [x, y] = win ? [a.toLowerCase(), b.toLowerCase()] : [a, b];
  return y.startsWith(`${x}/`) ? b.slice(a.length + 1) : null;
}

export function registerRevNav(
  vscode: typeof vscodeTypes,
  api: HistoryApi,
  roots: readonly string[],
  log: (line: string) => void,
): { dispose(): void } {
  const ascUri = (root: string, path: string, rev: string) => {
    const u = toAscUri({ root, path, at: { kind: "rev", rev } });
    return vscode.Uri.from({ scheme: "asc", path: u.path, query: u.query });
  };
  const input = (uri: vscodeTypes.Uri): NavInput | null => {
    if (uri.scheme === "asc") {
      try {
        const r = parseAscUri(uri.path, uri.query);
        return r.at.kind === "rev" ? { root: r.root, path: r.path, rev: r.at.rev } : null;
      } catch {
        return null;
      }
    }
    if (uri.scheme !== "file") return null;
    for (const root of roots) {
      const p = relPath(root, uri.fsPath);
      if (p !== null) return { root, path: p, rev: null };
    }
    return null;
  };
  const go = (dir: Direction) => async (arg?: vscodeTypes.Uri) => {
    const uri = arg ?? vscode.window.activeTextEditor?.document.uri;
    const inp = uri ? input(uri) : null;
    if (!inp) return;
    try {
      const t = await navigate(api, inp, dir);
      if (t === null) {
        void vscode.window.showInformationMessage(
          dir === "previous" ? "ASC: no earlier version." : "ASC: this is the newest version.",
        );
        return;
      }
      const target =
        t.kind === "working"
          ? vscode.Uri.file(`${inp.root.replace(/\\/g, "/")}/${inp.path}`)
          : ascUri(inp.root, inp.path, t.rev);
      await vscode.commands.executeCommand("vscode.open", target, { preview: true });
    } catch (e) {
      log(`asc revision navigation: ${String(e)}`);
    }
  };
  return vscode.Disposable.from(
    vscode.commands.registerCommand("asc.revnav.previous", go("previous")),
    vscode.commands.registerCommand("asc.revnav.next", go("next")),
  );
}
