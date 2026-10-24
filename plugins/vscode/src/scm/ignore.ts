// "Add to .ascignore" (SPEC 24.3.1): appends the pattern of an untracked or ignored file to the nearest
// `.ascignore` through a `WorkspaceEdit`, then `Status` refreshes through `Watch`. `planIgnore` is
// pure and editor-free; `registerIgnoreCommand` is the only `vscode` user.
import type * as vscodeTypes from "vscode";

/** Reads the text of `<root>/<dir>/.ascignore`; null when the file does not exist. */
export type ReadIgnore = (dir: string) => Promise<string | null>;

export interface IgnorePlan {
  /** Directory of the chosen `.ascignore`, relative to the root ("" for the root). */
  dir: string;
  /** Text to insert at the end of the file. */
  insert: string;
  /** The file exists already. */
  exists: boolean;
  /** The pattern is present, so nothing is inserted. */
  already: boolean;
  pattern: string;
}

/** Escapes glob specials and a leading `#` or `!`, so the pattern matches exactly this path. */
export function escapePattern(p: string): string {
  const body = p.replace(/[\\*?[\]]/g, (c) => `\\${c}`);
  return body.startsWith("#") || body.startsWith("!") ? `\\${body}` : body;
}

/** The anchored pattern for `rel` (relative to the `.ascignore` directory): `/name` or `/dir/`. */
export function patternFor(rel: string, isDir: boolean): string {
  return `/${escapePattern(rel)}${isDir ? "/" : ""}`;
}

/**
 * Walks from the file's directory up to the root and picks the first directory that holds a
 * `.ascignore`; with none, the root's file is created. `path` is relative to the root, with `/`.
 * Paths with `.`/`..` segments, a leading `/`, newlines or NUL are refused (null).
 */
export async function planIgnore(
  path: string,
  isDir: boolean,
  read: ReadIgnore,
): Promise<IgnorePlan | null> {
  const clean = path.replace(/\\/g, "/").replace(/\/+$/, "");
  if (clean === "" || clean.startsWith("/") || /[\0\r\n]/.test(clean)) return null;
  const parts = clean.split("/");
  if (parts.some((s) => s === ".." || s === "." || s === "")) return null;
  for (let n = parts.length - 1; n >= 0; n--) {
    const dir = parts.slice(0, n).join("/");
    const text = await read(dir);
    if (text === null && n > 0) continue;
    const pattern = patternFor(parts.slice(n).join("/"), isDir);
    const bare = pattern.replace(/\/$/, "");
    const lines = (text ?? "").split(/\r?\n/).map((l) => l.trim());
    if (text !== null && lines.some((l) => l === pattern || l === bare)) {
      return { dir, insert: "", exists: true, already: true, pattern };
    }
    const lead = text !== null && text !== "" && !text.endsWith("\n") ? "\n" : "";
    return { dir, insert: `${lead}${pattern}\n`, exists: text !== null, already: false, pattern };
  }
  return null;
}

/** Registers `asc.addToAscignore`; the argument is a resource state or a Uri. */
export function registerIgnoreCommand(
  vscode: typeof vscodeTypes,
  roots: readonly string[],
  log: (line: string) => void,
): { dispose(): void } {
  const slash = (s: string) => s.replace(/\\/g, "/").replace(/\/+$/, "");
  const norm = (s: string) => (process.platform === "win32" ? s.toLowerCase() : s);
  const relOf = (fsPath: string): { root: string; rel: string } | null => {
    const p = slash(fsPath);
    for (const r of roots) {
      const rr = slash(r);
      if (norm(p).startsWith(`${norm(rr)}/`)) return { root: r, rel: p.slice(rr.length + 1) };
    }
    return null;
  };
  const at = (root: vscodeTypes.Uri, dir: string): vscodeTypes.Uri =>
    vscode.Uri.joinPath(root, ...(dir === "" ? [] : dir.split("/")), ".ascignore");
  return vscode.commands.registerCommand("asc.addToAscignore", async (a?: unknown, ...rest: unknown[]) => {
    const uris = [a, ...rest.flat()]
      .map((x) =>
        x instanceof vscode.Uri ? x : (x as { resourceUri?: vscodeTypes.Uri } | undefined)?.resourceUri,
      )
      .filter((u): u is vscodeTypes.Uri => u !== undefined);
    const edit = new vscode.WorkspaceEdit();
    let n = 0;
    for (const uri of uris) {
      const hit = relOf(uri.fsPath);
      if (!hit) continue;
      let isDir = false;
      try {
        isDir = ((await vscode.workspace.fs.stat(uri)).type & vscode.FileType.Directory) !== 0;
      } catch {
        // Gone already: treat it as a file.
      }
      const rootUri = vscode.Uri.file(hit.root);
      const plan = await planIgnore(hit.rel, isDir, async (dir) => {
        try {
          return new TextDecoder().decode(await vscode.workspace.fs.readFile(at(rootUri, dir)));
        } catch {
          return null;
        }
      });
      if (!plan) {
        log(`asc: cannot add ${hit.rel} to .ascignore`);
        continue;
      }
      if (plan.already) continue;
      const file = at(rootUri, plan.dir);
      if (!plan.exists) edit.createFile(file, { ignoreIfExists: true });
      let end = new vscode.Position(0, 0);
      if (plan.exists) {
        const doc = await vscode.workspace.openTextDocument(file);
        end = doc.lineAt(doc.lineCount - 1).range.end;
      }
      edit.insert(file, end, plan.insert);
      n++;
    }
    if (n > 0) {
      await vscode.workspace.applyEdit(edit);
      await vscode.workspace.saveAll(false);
    }
  });
}
