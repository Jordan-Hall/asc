// Stash commands (SPEC 24.3.1): push (with untracked), pop, apply, drop and list through the
// allowlisted `Bridge.Run` verbs `stash push|pop|apply|list|drop`. Editor-free; `registerStashCommands`
// is the only `vscode` user. The list feeds the "Stashes" group of the Worktrees tree.
import type * as vscodeTypes from "vscode";
import { decodeOpResult, stashEntries } from "../opresult";
import type { CommandResult, CommandsApi, CommandsUi } from "./commands";
import { VerbRunner } from "./statusbar";

/** Allowlist `stash push -m` limit. */
export const STASH_MESSAGE_MAX = 1024;

export interface StashEntry {
  index: number;
  message: string;
}

/** Reads the `stash list` result (`OpResult.stash`); anything else gives []. */
export function parseStashList(bytes: Uint8Array | null): StashEntry[] {
  return stashEntries(decodeOpResult(bytes));
}

export class StashCommands {
  private readonly r: VerbRunner;

  constructor(
    api: CommandsApi,
    private readonly ui: CommandsUi,
  ) {
    this.r = new VerbRunner(api, ui);
  }

  /** `stash push`, with `-u` to include untracked files and `-m=` for a message. */
  push(
    root: string,
    opts: { message?: string; untracked?: boolean; paths?: readonly string[] } = {},
  ): Promise<CommandResult> {
    return this.r.serial(root, async () => {
      const msg = opts.message?.trim() ?? "";
      if (msg.length > STASH_MESSAGE_MAX || msg.includes("\0")) {
        this.ui.warn("ASC: the stash message is too long.");
        return { ok: false, reason: "invalid" };
      }
      if (!(await this.r.gate())) return { ok: false, reason: "disabled" };
      const args = [
        ...(opts.untracked === true ? ["-u"] : []),
        ...(msg === "" ? [] : [`-m=${msg}`]),
        ...(opts.paths ?? []).map((p) => (p.startsWith("-") ? `./${p}` : p)),
      ];
      return this.r.outcome("stash push", await this.r.run(root, "stash push", args));
    });
  }

  pop(root: string): Promise<CommandResult> {
    return this.r.simple(root, "stash pop");
  }

  apply(root: string): Promise<CommandResult> {
    return this.r.simple(root, "stash apply");
  }

  /** Drops the top stash after a modal confirmation; the verb is modal in the allowlist. */
  drop(root: string, label = "the latest stash"): Promise<CommandResult> {
    return this.r.serial(root, async () => {
      if (!(await this.r.gate())) return { ok: false, reason: "disabled" };
      const ok = await this.ui.confirm(
        `Drop ${label}?`,
        "The stash is deleted and cannot be restored.",
        "Drop",
      );
      if (!ok) return { ok: false, reason: "cancelled" };
      return this.r.outcome("stash drop", await this.r.run(root, "stash drop", []));
    });
  }

  async list(root: string): Promise<StashEntry[]> {
    const r = await this.r.run(root, "stash list", []);
    if (r.exit !== 0) {
      this.r.outcome("stash list", r);
      return [];
    }
    return parseStashList(r.opResult);
  }
}

/** Registers `asc.stashPush`, `asc.stashPushUntracked`, `asc.stashPop`, `asc.stashApply`, `asc.stashDrop`. */
export function registerStashCommands(
  vscode: typeof vscodeTypes,
  api: CommandsApi,
  ui: CommandsUi,
  roots: readonly string[],
  after: (root: string) => void,
): { dispose(): void } {
  const cmds = new StashCommands(api, ui);
  const subs: { dispose(): unknown }[] = [];
  const pick = async (a: unknown): Promise<string | undefined> => {
    const rootUri = (a as { rootUri?: vscodeTypes.Uri } | undefined)?.rootUri;
    const hit = rootUri ? roots.find((r) => r === rootUri.fsPath) : undefined;
    if (hit) return hit;
    if (roots.length === 1) return roots[0];
    return vscode.window.showQuickPick([...roots], { placeHolder: "Choose an ASC root" });
  };
  const reg = (id: string, fn: (root: string) => Promise<unknown>) =>
    subs.push(
      vscode.commands.registerCommand(id, async (a?: unknown) => {
        const root = await pick(a);
        if (!root) return;
        await fn(root);
        after(root);
      }),
    );
  const push = (untracked: boolean) => async (root: string) => {
    const message = await vscode.window.showInputBox({ prompt: "Stash message (optional)" });
    if (message === undefined) return;
    await cmds.push(root, { message, untracked });
  };
  reg("asc.stashPush", push(false));
  reg("asc.stashPushUntracked", push(true));
  reg("asc.stashPop", (r) => cmds.pop(r));
  reg("asc.stashApply", (r) => cmds.apply(r));
  reg("asc.stashDrop", (r) => cmds.drop(r));
  return {
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
