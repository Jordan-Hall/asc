// Stack actions (SPEC 24.3.2): the `scm/title` restack, sync and submit buttons. `submit` is a modal
// verb, so it asks first. Editor-free; `registerStackActions` is the only `vscode` user.
import type * as vscodeTypes from "vscode";
import type { CommandResult, CommandsApi, CommandsUi } from "../scm/commands";
import { VerbRunner } from "../scm/statusbar";

export class StackActions {
  private readonly r: VerbRunner;

  constructor(
    api: CommandsApi,
    private readonly ui: CommandsUi,
  ) {
    this.r = new VerbRunner(api, ui);
  }

  restack(root: string): Promise<CommandResult> {
    return this.r.simple(root, "restack");
  }

  sync(root: string): Promise<CommandResult> {
    return this.r.simple(root, "sync");
  }

  /** `submit -s` (the whole stack) after a modal confirmation. */
  submit(root: string): Promise<CommandResult> {
    return this.r.serial(root, async () => {
      if (!(await this.r.gate())) return { ok: false, reason: "disabled" };
      const ok = await this.ui.confirm(
        "Submit the stack?",
        "Each change is pushed and its review is created or updated as a new version.",
        "Submit",
      );
      if (!ok) return { ok: false, reason: "cancelled" };
      return this.r.outcome("submit", await this.r.run(root, "submit", ["-s"]));
    });
  }
}

/** Registers `asc.stackRestack`, `asc.stackSync`, `asc.stackSubmit` (the `scm/title` buttons). */
export function registerStackActions(
  vscode: typeof vscodeTypes,
  api: CommandsApi,
  ui: CommandsUi,
  roots: readonly string[],
  after: (root: string) => void,
): { dispose(): void } {
  const acts = new StackActions(api, ui);
  const subs: { dispose(): unknown }[] = [];
  const pick = async (a: unknown): Promise<string | undefined> => {
    const rootUri = (a as { rootUri?: vscodeTypes.Uri } | undefined)?.rootUri;
    const hit = rootUri ? roots.find((r) => r === rootUri.fsPath) : undefined;
    if (hit) return hit;
    if (roots.length === 1) return roots[0];
    return vscode.window.showQuickPick([...roots], { placeHolder: "Choose an asc root" });
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
  reg("asc.stackRestack", (r) => acts.restack(r));
  reg("asc.stackSync", (r) => acts.sync(r));
  reg("asc.stackSubmit", (r) => acts.submit(r));
  return {
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
