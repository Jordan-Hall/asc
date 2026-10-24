// Status bar model and stack-edit commands (SPEC 24.4.2, 24.3.1): the `$(git-commit) kxyzq... · 2/5 · ↓3`
// item from the `Stacks` snapshot (`WsStack.behind`, API addition A1; never `WsStatusResponse.behind`),
// the "needs restack" warning item, and uncommit, unamend and abandon. Editor-free so it runs under
// Bun; `registerStatusBar` and `registerStackEditCommands` are the only `vscode` users. Every mutation
// goes through the allowlisted `Bridge.Run` (typed flags, no shell, nothing secret in argv).
import { callUnary } from "@asc/asc-client";
import { Workspace } from "@asc/asc-client/workspace";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "../bridge";
import {
  bridgeCommandsApi,
  type CommandResult,
  type CommandsApi,
  type CommandsUi,
  type RunOutcome,
} from "./commands";

/** The least of `Stacks` the status bar needs: the stack that holds the current change. */
export interface StackSnap {
  /** Short change id of the current change (`StackEntry.short`). */
  short: string;
  /** 1-based position of the current change in its stack. */
  position: number;
  total: number;
  /** `WsStack.behind`: trunk commits not in the stack base, capped at 999. */
  behind: number;
  trunk: string;
  /** Any change of the stack is behind its parent. */
  needsRestack: boolean;
}

export interface StatusApi extends CommandsApi {
  /** The stack that holds the current change, or null when the working copy is on no stack. */
  stack(root: string): Promise<StackSnap | null>;
}

export interface StatusBarModel {
  text: string;
  tooltip: string;
  /** Non-zero shows `↓N`; the click offers a restack. */
  behind: number;
  /** The warning item is visible. */
  needsRestack: boolean;
  restackText: string;
}

export const BEHIND_CAP = 999;

/** `$(git-commit) kxyzq… · 2/5 · ↓3`; the `↓N` part is hidden at 0 (SPEC 24.4.2). */
export function statusBarModel(s: StackSnap | null): StatusBarModel | null {
  if (s === null) return null;
  const behind = Math.min(Math.max(Math.trunc(s.behind), 0), BEHIND_CAP);
  const parts = [`$(git-commit) ${s.short}`, `${s.position}/${s.total}`];
  if (behind > 0) parts.push(`↓${behind}`);
  return {
    text: parts.join(" · "),
    tooltip:
      behind > 0
        ? `Change ${s.short}, ${s.position} of ${s.total}. ${behind} commit(s) on ${s.trunk} are not in this stack. Click to restack.`
        : `Change ${s.short}, ${s.position} of ${s.total}. Click to open the smartlog.`,
    behind,
    needsRestack: s.needsRestack,
    restackText: "$(warning) needs restack",
  };
}

/** Maps a `WsStacksResponse` stack list to the snapshot of the current change. */
export function snapFromStacks(
  stacks: readonly {
    trunk: string;
    behind: number;
    changes: readonly { short: string; current: boolean; needsRestack: boolean }[];
  }[],
): StackSnap | null {
  for (const s of stacks) {
    const i = s.changes.findIndex((c) => c.current);
    if (i < 0) continue;
    return {
      short: s.changes[i]?.short ?? "",
      position: i + 1,
      total: s.changes.length,
      behind: s.behind,
      trunk: s.trunk,
      needsRestack: s.changes.some((c) => c.needsRestack),
    };
  }
  return null;
}

/** One mutation per root at a time, a feature gate and exit-code reporting, shared by the P26b commands. */
export class VerbRunner {
  private readonly locks = new Map<string, Promise<unknown>>();

  constructor(
    readonly api: CommandsApi,
    readonly ui: CommandsUi,
  ) {}

  serial<T>(root: string, fn: () => Promise<T>): Promise<T> {
    const prev = this.locks.get(root) ?? Promise.resolve();
    const next = prev.then(fn, fn);
    this.locks.set(
      root,
      next.catch(() => undefined),
    );
    return next;
  }

  async gate(): Promise<boolean> {
    if (await this.api.canMutate()) return true;
    this.ui.warn("asc is too old for this command. Update asc.");
    return false;
  }

  async run(root: string, verb: string, args: string[]): Promise<RunOutcome> {
    this.ui.output(`asc ${verb}`);
    const r = await this.api.run(root, verb, args);
    for (const l of r.stderr) this.ui.output(l);
    return r;
  }

  outcome(verb: string, r: RunOutcome): CommandResult {
    if (r.exit === 0) return { ok: true };
    if (r.exit === 1) {
      this.ui.warn(`asc ${verb} stopped on conflicts. Resolve them, then run asc continue.`);
      return { ok: false, reason: "conflict" };
    }
    if (r.exit === 2) {
      this.ui.warn(`asc ${verb} is ambiguous; nothing changed.`);
      return { ok: false, reason: "ambiguous" };
    }
    this.ui.error(`asc ${verb} failed (exit ${r.exit}).`, r.stderr.slice(-20));
    return { ok: false, reason: "failed" };
  }

  /** Gate, run one verb, report. */
  simple(root: string, verb: string, args: string[] = []): Promise<CommandResult> {
    return this.serial(root, async () => {
      if (!(await this.gate())) return { ok: false, reason: "disabled" };
      return this.outcome(verb, await this.run(root, verb, args));
    });
  }
}

/** `abandon` takes at most this many revisions per run (the bridge caps arguments at 64). */
export const ABANDON_MAX = 64;

/** A revision that reads as a flag or holds a control character is never passed to the bridge. */
export function validRev(rev: string): boolean {
  if (rev === "" || rev.startsWith("-") || rev.trim() !== rev) return false;
  for (const ch of rev) {
    const c = ch.charCodeAt(0);
    if (c < 0x20 || c === 0x7f) return false;
  }
  return true;
}

export class StackEditCommands {
  private readonly r: VerbRunner;

  constructor(
    api: StatusApi,
    private readonly ui: CommandsUi,
  ) {
    this.r = new VerbRunner(api, ui);
  }

  /** Undo the top change and keep its content in the working copy. */
  uncommit(root: string): Promise<CommandResult> {
    return this.r.simple(root, "uncommit");
  }

  /** Undo the last amend of the current change. */
  unamend(root: string): Promise<CommandResult> {
    return this.r.simple(root, "unamend");
  }

  /** Abandon changes (the current one when `revs` is empty) after a modal confirmation. */
  abandon(root: string, revs: readonly string[] = []): Promise<CommandResult> {
    return this.r.serial(root, async () => {
      if (revs.length > ABANDON_MAX || !revs.every(validRev)) {
        this.ui.warn("asc: those revisions cannot be abandoned from here.");
        return { ok: false, reason: "invalid" };
      }
      if (!(await this.r.gate())) return { ok: false, reason: "disabled" };
      const what = revs.length === 0 ? "the current change" : `${revs.length} change(s)`;
      const ok = await this.ui.confirm(
        `Abandon ${what}?`,
        `${revs.slice(0, 5).join("\n")}\n\nThe change leaves the stack. asc undo can bring it back.`.trim(),
        "Abandon",
      );
      if (!ok) return { ok: false, reason: "cancelled" };
      return this.r.outcome("abandon", await this.r.run(root, "abandon", [...revs]));
    });
  }

  /** The `↓N` click and the needs-restack item: restack onto the trunk tip. */
  restack(root: string): Promise<CommandResult> {
    return this.r.simple(root, "restack");
  }
}

// --- the bridge-backed API ----------------------------------------------------------------------

export function bridgeStatusApi(mgr: BridgeManager): StatusApi {
  const base = bridgeCommandsApi(mgr);
  return {
    ...base,
    async stack(root) {
      const h = await mgr.hello();
      const wsId = h.roots.find((x) => x.root === root)?.wsId;
      if (!wsId) return null;
      const r = await callUnary(mgr.node(), Workspace.method.stacks, { wsId }, { root });
      return snapFromStacks(r.stacks);
    },
  };
}

// --- the editor adapter -------------------------------------------------------------------------

export interface StatusBarRegistration {
  dispose(): void;
  /** Re-reads the `Stacks` snapshot of one root (or all). */
  refresh(root?: string): Promise<void>;
}

/** Shows the status bar items for the first root with a current change. `subscribe` lets the caller
 * wire `KIND_HEAD`/`KIND_REF`/`KIND_SWITCH_DONE`/`KIND_RESYNC` watch events; without it the items
 * refresh on window focus and after each command only. TODO(P25): wire `subscribe` to the watch loop. */
export function registerStatusBar(
  vscode: typeof vscodeTypes,
  api: StatusApi,
  roots: readonly string[],
  log: (line: string) => void,
  subscribe?: (cb: (root: string) => void) => { dispose(): void },
): StatusBarRegistration {
  const main = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  const warn = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 99);
  main.name = "asc change";
  warn.name = "asc needs restack";
  warn.backgroundColor = new vscode.ThemeColor("statusBarItem.warningBackground");
  const snaps = new Map<string, StatusBarModel | null>();

  const render = () => {
    const first = [...snaps.entries()].find(([, m]) => m !== null);
    const m = first?.[1] ?? null;
    if (!m || !first) {
      main.hide();
      warn.hide();
      return;
    }
    main.text = m.text;
    main.tooltip = m.tooltip;
    main.command =
      m.behind > 0
        ? { title: "Restack", command: "asc.restackOntoTrunk", arguments: [first[0]] }
        : "asc.openSmartlog";
    main.show();
    if (m.needsRestack) {
      warn.text = m.restackText;
      warn.tooltip = "Some changes are behind their parent. Click to restack.";
      warn.command = { title: "Restack", command: "asc.restack", arguments: [first[0]] };
      warn.show();
    } else warn.hide();
  };

  const refresh = async (root?: string) => {
    for (const r of root === undefined ? roots : [root]) {
      try {
        snaps.set(r, statusBarModel(await api.stack(r)));
      } catch (e) {
        log(`asc status bar: ${String(e)}`);
        snaps.set(r, null);
      }
    }
    render();
  };

  const subs: { dispose(): unknown }[] = [main, warn];
  subs.push(vscode.window.onDidChangeWindowState((s) => s.focused && void refresh()));
  if (subscribe) subs.push(subscribe((r) => void refresh(r)));
  void refresh();
  return {
    refresh,
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}

/** Registers `asc.uncommit`, `asc.unamend`, `asc.abandon`, `asc.restack`, `asc.restackOntoTrunk` and
 * `asc.openSmartlog` (reveals the SCM view until the smartlog view lands). */
export function registerStackEditCommands(
  vscode: typeof vscodeTypes,
  api: StatusApi,
  ui: CommandsUi,
  roots: readonly string[],
  after: (root: string) => void,
): { dispose(): void } {
  const cmds = new StackEditCommands(api, ui);
  const subs: { dispose(): unknown }[] = [];
  const pick = async (a: unknown): Promise<string | undefined> => {
    if (typeof a === "string" && roots.includes(a)) return a;
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
  reg("asc.uncommit", (r) => cmds.uncommit(r));
  reg("asc.unamend", (r) => cmds.unamend(r));
  reg("asc.abandon", (r) => cmds.abandon(r));
  reg("asc.restack", (r) => cmds.restack(r));
  reg("asc.restackOntoTrunk", async (r) => {
    const s = await api.stack(r);
    const trunk = s?.trunk || "master";
    const go = await vscode.window.showQuickPick([`Restack onto ${trunk}`], {
      placeHolder: `${s?.behind ?? 0} commit(s) on ${trunk} are not in this stack`,
    });
    if (go) await cmds.restack(r);
  });
  subs.push(
    vscode.commands.registerCommand("asc.openSmartlog", () =>
      vscode.commands.executeCommand("workbench.view.scm"),
    ),
  );
  return {
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
