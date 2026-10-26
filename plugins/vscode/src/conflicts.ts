// Conflicts (SPEC 24.4.6): the conflict list and the paused-operation banner, opening a conflict in
// the merge editor (`_open.mergeEditor`, internal and unversioned, so only when present) or as the
// diff3-marked working file (VS Code's built-in merge-conflict CodeLens handles it), "Mark resolved"
// through `Workspace.Stage{add}`, and `asc continue` / `asc abort` through `Bridge.Run`. The logic is
// editor-free so it runs under Bun; `registerConflicts` is the only `vscode` user.
import { bridge, type CallParams, callStream, callUnary } from "@asc/asc-client";
import { Workspace } from "@asc/asc-client/workspace";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "./bridge";
import { type AscAt, toAscUri } from "./fs/ascfs";

export type InProgress = "" | "merge" | "rebase" | "cherry-pick" | "revert" | "bisect" | "empty";

export interface ConflictEntry {
  path: string;
  /** The diff3-marked working text; empty when the engine sent none (binary, modify/delete). */
  marked: Uint8Array;
}

export interface ConflictsState {
  inProgress: InProgress;
  conflicts: ConflictEntry[];
}

/** The bridge calls conflicts need; tests supply a fake. */
export interface ConflictsApi {
  /** `Workspace.Status` for `in_progress`, plus `Workspace.Conflicts` when something is paused. */
  load(root: string, signal?: AbortSignal): Promise<ConflictsState>;
  /** `Workspace.Stage{add: [path]}`. */
  stageAdd(root: string, path: string): Promise<void>;
  /** `Bridge.Run` of `continue` or `abort` with no arguments; resolves to the exit code. */
  run(root: string, verb: "continue" | "abort", signal?: AbortSignal): Promise<number>;
}

const MARKER_START = /^<{7}(?: |$)/m;
const MARKER_END = /^>{7}(?: |$)/m;

/** True while the text still holds a conflict marker. */
export function hasConflictMarkers(text: string): boolean {
  return MARKER_START.test(text) || MARKER_END.test(text);
}

const OP_LABEL: Record<string, string> = {
  rebase: "Restack",
  merge: "Merge",
  "cherry-pick": "Cherry-pick",
  revert: "Revert",
};

export interface Banner {
  text: string;
  actions: readonly ("Continue" | "Abort")[];
}

/** "Restack paused: 2 conflicts · Continue · Abort"; null when nothing is paused or conflicted. */
export function bannerFor(state: ConflictsState): Banner | null {
  const n = state.conflicts.length;
  const op = OP_LABEL[state.inProgress];
  if (op === undefined && n === 0) return null;
  const noun = `${n} conflict${n === 1 ? "" : "s"}`;
  if (op === undefined) return { text: `Conflicts: ${noun}`, actions: [] };
  const head = n === 0 ? `${op} paused` : `${op} paused: ${noun}`;
  return { text: `${head} · Continue · Abort`, actions: ["Continue", "Abort"] };
}

/** True while an operation is paused, so Continue and Abort apply (the `asc.paused` context key). */
export function isPaused(state: ConflictsState): boolean {
  return OP_LABEL[state.inProgress] !== undefined;
}

export const PAUSED_CONTEXT = "asc.paused";

export interface MergeInput {
  uri: string;
  title: string;
  description: string;
  detail: string;
}

export interface MergeEditorArgs {
  base: string;
  input1: MergeInput;
  input2: MergeInput;
  output: string;
}

type UriOf = (path: string, at: AscAt) => string;

/** The `_open.mergeEditor` argument, as the built-in git builds it. During a restack the sides are
 * swapped: "ours" is the new base the change is replayed onto, so the user's change is input1. */
export function mergeEditorArgs(
  uriOf: UriOf,
  workingUri: string,
  path: string,
  inProgress: InProgress,
): MergeEditorArgs {
  const ours = {
    uri: uriOf(path, { kind: "stage", stage: 2 }),
    title: "Current",
    description: "Ours",
    detail: "",
  };
  const theirs = {
    uri: uriOf(path, { kind: "stage", stage: 3 }),
    title: "Incoming",
    description: "Theirs",
    detail: "",
  };
  const swap = inProgress === "rebase";
  return {
    base: uriOf(path, { kind: "stage", stage: 1 }),
    input1: swap ? theirs : ours,
    input2: swap ? ours : theirs,
    output: workingUri,
  };
}

export type OpenPlan =
  | { kind: "mergeEditor"; args: MergeEditorArgs }
  | { kind: "markers"; path: string };

/** The merge editor when the internal command exists, else the marked working file. */
export function planOpen(
  hasMergeEditor: boolean,
  uriOf: UriOf,
  workingUri: string,
  entry: ConflictEntry,
  inProgress: InProgress,
): OpenPlan {
  if (hasMergeEditor) {
    return {
      kind: "mergeEditor",
      args: mergeEditorArgs(uriOf, workingUri, entry.path, inProgress),
    };
  }
  return { kind: "markers", path: entry.path };
}

export class ConflictsModel {
  private st: ConflictsState = { inProgress: "", conflicts: [] };
  private readonly listeners = new Set<() => void>();
  private busy = false;

  constructor(
    readonly root: string,
    private readonly api: ConflictsApi,
  ) {}

  get state(): ConflictsState {
    return this.st;
  }
  get banner(): Banner | null {
    return bannerFor(this.st);
  }
  isConflicted(path: string): boolean {
    return this.st.conflicts.some((c) => c.path === path);
  }
  onDidChange(fn: () => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }

  async refresh(signal?: AbortSignal): Promise<void> {
    this.st = await this.api.load(this.root, signal);
    for (const l of this.listeners) l();
  }

  /** Stages `path` and refreshes. Refuses while markers remain unless `force` is set. */
  async markResolved(path: string, text: string | null, force = false): Promise<boolean> {
    if (!force && text !== null && hasConflictMarkers(text)) return false;
    await this.api.stageAdd(this.root, path);
    await this.refresh();
    return true;
  }

  /** Runs continue or abort; calls do not overlap. A continue that stops on new conflicts is not
   * an error. Throws with the exit code on any other failure. */
  async finish(verb: "continue" | "abort"): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    try {
      const code = await this.api.run(this.root, verb);
      await this.refresh();
      if (code !== 0 && !(verb === "continue" && this.st.conflicts.length > 0)) {
        throw new Error(`asc ${verb} exited with ${code}`);
      }
    } finally {
      this.busy = false;
    }
  }
}

// --- bridge adapter -----------------------------------------------------------------------------

const enc = new TextEncoder();
const dec = new TextDecoder();

function conflictedXy(xy: string): boolean {
  return xy === "DD" || xy === "AA" || xy.includes("U");
}

const params = (root: string, signal?: AbortSignal): CallParams => ({
  root,
  ...(signal === undefined ? {} : { signal }),
});

export function bridgeConflictsApi(mgr: BridgeManager): ConflictsApi {
  const wsIdOf = async (root: string): Promise<string> => {
    const h = await mgr.hello();
    const r = h.roots.find((x) => x.root === root);
    if (!r) throw new Error("asc root is not known to the bridge");
    return r.wsId;
  };
  return {
    async load(root, signal) {
      const wsId = await wsIdOf(root);
      const status = await callUnary(
        mgr.node(),
        Workspace.method.status,
        { wsId },
        params(root, signal),
      );
      const inProgress = status.inProgress as InProgress;
      if (inProgress === "" && !status.files.some((f) => conflictedXy(f.xy))) {
        return { inProgress, conflicts: [] };
      }
      const r = await callUnary(
        mgr.node(),
        Workspace.method.conflicts,
        { wsId },
        params(root, signal),
      );
      return {
        inProgress,
        conflicts: r.conflicts.map((c) => ({ path: dec.decode(c.path), marked: c.marked })),
      };
    },
    async stageAdd(root, path) {
      const wsId = await wsIdOf(root);
      await callUnary(
        mgr.node(),
        Workspace.method.stage,
        { wsId, add: [enc.encode(path)] },
        params(root),
      );
    },
    async run(root, verb, signal) {
      let exit = -1;
      // No stdin and no caller-supplied arguments: the verb is one of two literals.
      for await (const ev of callStream(
        mgr.node(),
        bridge.Bridge.method.run,
        { verb, args: [], root },
        params(root, signal),
      )) {
        if (ev.event.case === "exit") exit = ev.event.value;
      }
      return exit;
    },
  };
}

// --- the editor adapter -------------------------------------------------------------------------

export interface ConflictsRegistration {
  dispose(): void;
}

/** Registers the status-bar banner and the commands `asc.conflicts.open`, `asc.conflicts.markResolved`,
 * `asc.conflicts.pick`, `asc.continue` and `asc.abort`. The SCM "Conflicts" group is the provider's.
 * The `asc.paused` context key is true while any root has a paused operation; the `scm/title` entries
 * for Continue and Abort show on it. */
export function registerConflicts(
  vscode: typeof vscodeTypes,
  mgr: BridgeManager,
  roots: readonly string[],
  log: (line: string) => void,
  apiOverride?: ConflictsApi,
): ConflictsRegistration {
  const api = apiOverride ?? bridgeConflictsApi(mgr);
  const subs: { dispose(): unknown }[] = [];
  const models = new Map<string, ConflictsModel>();
  const item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  subs.push(item);

  const uriOf =
    (root: string): UriOf =>
    (path, at) => {
      const u = toAscUri({ root, path, at });
      return vscode.Uri.from({ scheme: "asc", path: u.path, query: u.query }).toString();
    };
  const pick = (
    arg?: vscodeTypes.Uri,
  ): { root: string; model: ConflictsModel; path: string | null } | null => {
    const uri = arg ?? vscode.window.activeTextEditor?.document.uri;
    if (uri?.scheme === "file") {
      const p = uri.fsPath.replace(/\\/g, "/");
      for (const [root, model] of models) {
        const r = root.replace(/\\/g, "/").replace(/\/+$/, "");
        if (p.toLowerCase().startsWith(`${r.toLowerCase()}/`)) {
          return { root, model, path: p.slice(r.length + 1) };
        }
      }
    }
    const first = models.entries().next().value;
    return first ? { root: first[0], model: first[1], path: null } : null;
  };
  let paused = false;
  const setPaused = () => {
    const now = [...models.values()].some((m) => isPaused(m.state));
    if (now === paused) return;
    paused = now;
    void vscode.commands.executeCommand("setContext", PAUSED_CONTEXT, now);
  };
  void vscode.commands.executeCommand("setContext", PAUSED_CONTEXT, false);
  const renderBanner = () => {
    setPaused();
    for (const m of models.values()) {
      const b = m.banner;
      if (!b) continue;
      item.text = `$(warning) ${b.text}`;
      item.tooltip = "Pick a conflict to open";
      item.command = "asc.conflicts.pick";
      item.show();
      return;
    }
    item.hide();
  };
  const fail = (what: string, e: unknown) => {
    log(`asc ${what}: ${String(e)}`);
    void vscode.window.showErrorMessage(`ASC ${what} failed. See the ASC output.`);
  };
  const refreshAll = () => {
    for (const m of models.values()) m.refresh().catch((e) => log(`asc conflicts: ${String(e)}`));
  };

  for (const root of roots) {
    const model = new ConflictsModel(root, api);
    models.set(root, model);
    subs.push(model.onDidChange(renderBanner));
  }
  refreshAll();
  subs.push(
    vscode.window.onDidChangeWindowState((s) => {
      if (s.focused) refreshAll();
    }),
  );

  subs.push(
    vscode.commands.registerCommand(
      "asc.conflicts.open",
      async (arg?: vscodeTypes.Uri | { resourceUri?: vscodeTypes.Uri }) => {
        const uri = arg && "resourceUri" in arg ? arg.resourceUri : (arg as vscodeTypes.Uri);
        const t = pick(uri);
        if (!t || t.path === null) return;
        const entry = t.model.state.conflicts.find((c) => c.path === t.path);
        if (!entry) return;
        const working = vscode.Uri.file(`${t.root}/${t.path}`);
        try {
          const present = (await vscode.commands.getCommands(true)).includes("_open.mergeEditor");
          const plan = planOpen(
            present,
            uriOf(t.root),
            working.toString(),
            entry,
            t.model.state.inProgress,
          );
          if (plan.kind === "mergeEditor") {
            const a = plan.args;
            await vscode.commands.executeCommand("_open.mergeEditor", {
              base: vscode.Uri.parse(a.base),
              input1: { ...a.input1, uri: vscode.Uri.parse(a.input1.uri) },
              input2: { ...a.input2, uri: vscode.Uri.parse(a.input2.uri) },
              output: vscode.Uri.parse(a.output),
            });
          } else {
            await vscode.window.showTextDocument(working);
          }
        } catch (e) {
          fail("open conflict", e);
        }
      },
    ),
    vscode.commands.registerCommand("asc.conflicts.markResolved", async (arg?: vscodeTypes.Uri) => {
      const t = pick(arg);
      if (!t || t.path === null) return;
      try {
        const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(`${t.root}/${t.path}`));
        const ok = await t.model.markResolved(t.path, doc.getText());
        if (!ok)
          void vscode.window.showWarningMessage("ASC: conflict markers remain in this file.");
      } catch (e) {
        fail("mark resolved", e);
      }
    }),
    vscode.commands.registerCommand("asc.conflicts.pick", async () => {
      const m = models.values().next().value;
      if (!m || m.state.conflicts.length === 0) return;
      const chosen = await vscode.window.showQuickPick(
        m.state.conflicts.map((c) => c.path),
        { placeHolder: "Open a conflict" },
      );
      if (chosen) {
        await vscode.commands.executeCommand(
          "asc.conflicts.open",
          vscode.Uri.file(`${m.root}/${chosen}`),
        );
      }
    }),
    vscode.commands.registerCommand("asc.continue", async () => {
      const t = pick();
      if (!t) return;
      try {
        await t.model.finish("continue");
      } catch (e) {
        fail("continue", e);
      }
    }),
    vscode.commands.registerCommand("asc.abort", async () => {
      const t = pick();
      if (!t) return;
      const yes = await vscode.window.showWarningMessage(
        "Abort restores the state from before the operation.",
        { modal: true },
        "Abort",
      );
      if (yes !== "Abort") return;
      try {
        await t.model.finish("abort");
      } catch (e) {
        fail("abort", e);
      }
    }),
    // Saving a conflicted file with no markers left offers "Mark resolved".
    vscode.workspace.onDidSaveTextDocument(async (doc) => {
      if (doc.uri.scheme !== "file") return;
      const t = pick(doc.uri);
      if (!t || t.path === null || !t.model.isConflicted(t.path)) return;
      if (hasConflictMarkers(doc.getText())) return;
      const choice = await vscode.window.showInformationMessage(
        `No conflict markers left in ${t.path}.`,
        "Mark resolved",
      );
      if (choice === "Mark resolved") {
        await vscode.commands.executeCommand("asc.conflicts.markResolved", doc.uri);
      }
    }),
  );

  return {
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
      void vscode.commands.executeCommand("setContext", PAUSED_CONTEXT, false);
    },
  };
}
