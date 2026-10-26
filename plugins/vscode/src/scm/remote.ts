// Fetch, pull and push for git-remote mode only (SPEC 24.3.1). In server mode (`remote_kind` "asc") and
// with no remote ("none") the commands are refused and hidden, because `sync` and `submit` apply
// there. Editor-free; `registerRemoteCommands` is the only `vscode` user. No URL, token or password
// is ever placed in argv: the verbs use the remote the repository already has.
import { bridge, callUnary } from "@asc/asc-client";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "../bridge";
import type { CommandResult, CommandsApi, CommandsUi } from "./commands";
import { VerbRunner } from "./statusbar";

export interface RemoteApi extends CommandsApi {
  /** `RootInfo.remote_kind` from `Hello`: "none", "asc" or "git". */
  remoteKind(root: string): Promise<string>;
}

export type PullMode = "ff-only" | "rebase" | "default";

/** True only for a git remote; the `asc.gitRemote` context key uses it to show the menu items. */
export const isGitRemote = (kind: string): boolean => kind === "git";

export class RemoteCommands {
  private readonly r: VerbRunner;

  constructor(
    private readonly api: RemoteApi,
    private readonly ui: CommandsUi,
  ) {
    this.r = new VerbRunner(api, ui);
  }

  private async guard(root: string): Promise<CommandResult | null> {
    if (!isGitRemote(await this.api.remoteKind(root))) {
      this.ui.info("ASC: fetch, pull and push apply to git remotes. Use sync and submit here.");
      return { ok: false, reason: "disabled" };
    }
    if (!(await this.r.gate())) return { ok: false, reason: "disabled" };
    return null;
  }

  fetch(root: string, opts: { prune?: boolean } = {}): Promise<CommandResult> {
    return this.r.serial(root, async () => {
      const g = await this.guard(root);
      if (g) return g;
      const args = opts.prune === true ? ["--prune"] : [];
      return this.r.outcome("fetch", await this.r.run(root, "fetch", args));
    });
  }

  pull(root: string, mode: PullMode = "ff-only"): Promise<CommandResult> {
    return this.r.serial(root, async () => {
      const g = await this.guard(root);
      if (g) return g;
      const args = mode === "rebase" ? ["--rebase"] : mode === "ff-only" ? ["--ff-only"] : [];
      return this.r.outcome("pull", await this.r.run(root, "pull", args));
    });
  }

  /** Push is modal in the allowlist: it asks first. Force flags are not in the schema. */
  push(root: string, opts: { setUpstream?: boolean } = {}): Promise<CommandResult> {
    return this.r.serial(root, async () => {
      const g = await this.guard(root);
      if (g) return g;
      const ok = await this.ui.confirm(
        "Push to the remote?",
        "Pushes the current branch. No force push.",
        "Push",
      );
      if (!ok) return { ok: false, reason: "cancelled" };
      const args = opts.setUpstream === true ? ["-u"] : [];
      return this.r.outcome("push", await this.r.run(root, "push", args));
    });
  }
}

export function bridgeRemoteApi(mgr: BridgeManager, base: CommandsApi): RemoteApi {
  return {
    ...base,
    async remoteKind(root) {
      const r = await callUnary(mgr.node(), bridge.Bridge.method.hello, {});
      return r.roots.find((x) => x.root === root)?.remoteKind ?? "none";
    },
  };
}

/** Registers `asc.fetch`, `asc.pull` and `asc.push` and sets the `asc.gitRemote` context key that the
 * menus use to hide them in server mode. */
export function registerRemoteCommands(
  vscode: typeof vscodeTypes,
  api: RemoteApi,
  ui: CommandsUi,
  roots: readonly string[],
  after: (root: string) => void,
): { dispose(): void } {
  const cmds = new RemoteCommands(api, ui);
  const subs: { dispose(): unknown }[] = [];
  void (async () => {
    const kinds = await Promise.all(roots.map((r) => api.remoteKind(r).catch(() => "none")));
    await vscode.commands.executeCommand("setContext", "asc.gitRemote", kinds.some(isGitRemote));
  })();
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
  reg("asc.fetch", (r) => cmds.fetch(r));
  reg("asc.pull", (r) => cmds.pull(r));
  reg("asc.push", (r) => cmds.push(r));
  return {
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
