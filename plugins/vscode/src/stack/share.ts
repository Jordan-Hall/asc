// Share (SPEC 24.3.2): copies an `asc:<id>` link for the current change. The link holds only the change
// id, never a path, host or credential. Editor-free; `registerShare` is the only `vscode` user.
import type * as vscodeTypes from "vscode";
import type { StackSnap } from "../scm/statusbar";

/** `asc:<id>` for a change id of 4 to 64 lower-case hex characters; null for anything else. */
export function shareLink(short: string): string | null {
  return /^[0-9a-f]{4,64}$/.test(short) ? `asc:${short}` : null;
}

export interface ShareUi {
  copy(text: string): Promise<void>;
  info(message: string): void;
  warn(message: string): void;
}

export async function shareCurrent(snap: StackSnap | null, ui: ShareUi): Promise<string | null> {
  const link = snap === null ? null : shareLink(snap.short);
  if (link === null) {
    ui.warn("ASC: there is no current change to share.");
    return null;
  }
  await ui.copy(link);
  ui.info(`Copied ${link}`);
  return link;
}

/** Registers `asc.stackShare`. */
export function registerShare(
  vscode: typeof vscodeTypes,
  stack: (root: string) => Promise<StackSnap | null>,
  roots: readonly string[],
): { dispose(): void } {
  const sub = vscode.commands.registerCommand("asc.stackShare", async () => {
    const root =
      roots.length === 1
        ? roots[0]
        : await vscode.window.showQuickPick([...roots], { placeHolder: "Choose an ASC root" });
    if (!root) return;
    await shareCurrent(await stack(root), {
      copy: async (t) => vscode.env.clipboard.writeText(t),
      info: (m) => void vscode.window.showInformationMessage(m),
      warn: (m) => void vscode.window.showWarningMessage(m),
    });
  });
  return { dispose: () => sub.dispose() };
}
