// Working-copy commands (SPEC 24.3.1): create, amend, reword, absorb with preview, stage and unstage
// ranges and hunks, discard with a modal confirmation, undo and redo. `CommitCommands` is editor-free so
// it runs under Bun; `registerCommands` is the only `vscode` user. Every mutation goes through the
// allowlisted `Bridge.Run` (typed flags, no shell, nothing secret in argv) or `Workspace.Stage`.
import { bridge, callStream, callUnary, fromHex } from "@asc/asc-client";
import { Workspace } from "@asc/asc-client/workspace";
import { readFile } from "node:fs/promises";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "../bridge";
import type { AscAt, ReadApi } from "../fs/ascfs";
import { buildHunkPatch, buildPatch, type LineRange, type Side } from "./stage-ranges";

/** `Workspace.Stage` carries at most this many patch bytes (allowlist `limits.patchBytes`). */
export const MAX_PATCH_BYTES = 1_048_576;
/** `restore` takes at most this many paths per run, so the 64-argument cap is never reached. */
export const RESTORE_CHUNK = 50;

export interface RunOutcome {
  exit: number;
  stderr: string[];
  /** The encoded `asc.v1.OpResult`, when the bridge sent one. */
  opResult: Uint8Array | null;
}

/** The bridge calls the commands need; tests supply a fake. */
export interface CommandsApi extends ReadApi {
  run(root: string, verb: string, args: string[], signal?: AbortSignal): Promise<RunOutcome>;
  stage(root: string, req: { add?: string[]; remove?: string[]; patch?: string }): Promise<void>;
  /** `RootInfo.branchless` from `Hello`. */
  branchless(root: string): Promise<boolean>;
  /** True when `Hello.features` allow mutating commands (`checkFeatures` level `full`). */
  canMutate(): Promise<boolean>;
  /** The working-tree text of a file; null when it does not exist. */
  readWorking(root: string, path: string): Promise<string | null>;
}

export interface PickItem {
  label: string;
  detail?: string;
}

/** What the commands need from the editor. */
export interface CommandsUi {
  output(line: string): void;
  info(message: string): void;
  warn(message: string): void;
  /** A failure; `lines` is the stderr tail (hook output) to show with it. */
  error(message: string, lines: string[]): void;
  /** A modal confirmation; resolves true only when the person chose `button`. */
  confirm(message: string, detail: string, button: string): Promise<boolean>;
  /** A read-only quick pick of `items`; resolves true when the person accepts. */
  review(title: string, items: PickItem[], accept: string): Promise<boolean>;
}

export interface OpSummary {
  status: string;
  lines: PickItem[];
}

/**
 * TODO(M02): `cli.proto` (`OpResult`) is not in the descriptor set, so the bridge sends no
 * `op_result` yet. Until then this reads the bytes as the JSON the verb printed, and a miss gives null.
 * Field names follow `crates/asc/src/stack/json.rs`.
 */
export function summariseOpResult(bytes: Uint8Array | null): OpSummary | null {
  if (bytes === null || bytes.byteLength === 0) return null;
  let v: unknown;
  try {
    v = JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    return null;
  }
  if (typeof v !== "object" || v === null) return null;
  const o = v as Record<string, unknown>;
  const arr = (k: string): Record<string, unknown>[] =>
    Array.isArray(o[k])
      ? (o[k] as Record<string, unknown>[]).filter((x) => typeof x === "object" && x !== null)
      : [];
  const str = (x: unknown): string => (typeof x === "string" ? x : "");
  const lines: PickItem[] = [];
  for (const r of arr("rewritten")) {
    lines.push({
      label: str(r.change) || str(r.new) || "change",
      ...(str(r.subject) ? { detail: str(r.subject) } : {}),
    });
  }
  for (const s of arr("skipped")) {
    lines.push({ label: `${str(s.change) || "change"} (skipped)`, detail: str(s.reason) });
  }
  for (const c of arr("conflicts")) {
    lines.push({ label: `${str(c.change) || "change"} (conflict)`, detail: str(c.path) });
  }
  for (const h of Array.isArray(o.hints) ? o.hints : []) {
    if (typeof h === "string") lines.push({ label: h });
  }
  return { status: str(o.status), lines };
}

export type CommandResult =
  | { ok: true; nothing?: boolean }
  | {
      ok: false;
      reason: "disabled" | "invalid" | "cancelled" | "conflict" | "ambiguous" | "failed";
    };

const UPDATE_ASC = "asc is too old for this command. Update asc.";

/** Paths starting with `-` would read as flags. */
function arg(path: string): string {
  return path.startsWith("-") ? `./${path}` : path;
}

export class CommitCommands {
  private readonly locks = new Map<string, Promise<unknown>>();

  constructor(
    private readonly api: CommandsApi,
    private readonly ui: CommandsUi,
  ) {}

  /** One mutation per root at a time, in call order. */
  private serial<T>(root: string, fn: () => Promise<T>): Promise<T> {
    const prev = this.locks.get(root) ?? Promise.resolve();
    const next = prev.then(fn, fn);
    this.locks.set(
      root,
      next.catch(() => undefined),
    );
    return next;
  }

  private async gate(): Promise<boolean> {
    if (await this.api.canMutate()) return true;
    this.ui.warn(UPDATE_ASC);
    return false;
  }

  /** Runs one verb and reports it. Hook output goes to the output channel; a failure shows the tail. */
  private async runVerb(root: string, verb: string, args: string[]): Promise<RunOutcome> {
    this.ui.output(`asc ${verb}`);
    const r = await this.api.run(root, verb, args);
    for (const l of r.stderr) this.ui.output(l);
    return r;
  }

  private outcome(verb: string, r: RunOutcome): CommandResult {
    if (r.exit === 0) {
      return {
        ok: true,
        ...(summariseOpResult(r.opResult)?.status === "nothing" ? { nothing: true } : {}),
      };
    }
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

  /** Create a change from the working copy: `create --no-branch` in branchless mode, `create` otherwise. */
  create(root: string, message: string): Promise<CommandResult> {
    return this.serial(root, async () => {
      if (message.trim() === "") {
        this.ui.warn("Type a message to create a change.");
        return { ok: false, reason: "invalid" };
      }
      if (!(await this.gate())) return { ok: false, reason: "disabled" };
      const branchless = await this.api.branchless(root);
      const args = [...(branchless ? ["--no-branch"] : []), `-m=${message}`];
      return this.outcome("create", await this.runVerb(root, "create", args));
    });
  }

  /** Amend the current change with the staged changes; an optional message replaces its message. */
  amend(root: string, message?: string): Promise<CommandResult> {
    return this.serial(root, async () => {
      if (!(await this.gate())) return { ok: false, reason: "disabled" };
      const args = message !== undefined && message.trim() !== "" ? [`-m=${message}`] : [];
      return this.outcome("modify", await this.runVerb(root, "modify", args));
    });
  }

  reword(root: string, message: string, rev?: string): Promise<CommandResult> {
    return this.serial(root, async () => {
      if (message.trim() === "") {
        this.ui.warn("Type a message to reword the change.");
        return { ok: false, reason: "invalid" };
      }
      if (!(await this.gate())) return { ok: false, reason: "disabled" };
      return this.outcome(
        "reword",
        await this.runVerb(root, "reword", [`-m=${message}`, ...(rev === undefined ? [] : [rev])]),
      );
    });
  }

  /** Preview with `absorb -n`, let the person review it, then apply with `absorb -f`. */
  absorb(root: string): Promise<CommandResult> {
    return this.serial(root, async () => {
      if (!(await this.gate())) return { ok: false, reason: "disabled" };
      const preview = await this.runVerb(root, "absorb", ["-n"]);
      if (preview.exit !== 0) return this.outcome("absorb", preview);
      const sum = summariseOpResult(preview.opResult);
      if (sum?.status === "nothing") {
        this.ui.info("ASC: nothing to absorb.");
        return { ok: true, nothing: true };
      }
      // TODO(M02): without an OpResult the preview is the verb's stderr; the human preview text is on
      // stdout, which the bridge does not relay, so the list can be empty.
      const items: PickItem[] =
        sum && sum.lines.length > 0
          ? sum.lines
          : preview.stderr.filter((l) => l.trim() !== "").map((label) => ({ label }));
      if (!(await this.ui.review("Absorb into the stack", items, "Absorb"))) {
        return { ok: false, reason: "cancelled" };
      }
      return this.outcome("absorb", await this.runVerb(root, "absorb", ["-f"]));
    });
  }

  undo(root: string): Promise<CommandResult> {
    return this.simple(root, "undo");
  }

  redo(root: string): Promise<CommandResult> {
    return this.simple(root, "redo");
  }

  private simple(root: string, verb: string): Promise<CommandResult> {
    return this.serial(root, async () => {
      if (!(await this.gate())) return { ok: false, reason: "disabled" };
      return this.outcome(verb, await this.runVerb(root, verb, []));
    });
  }

  /** Discard staged and working changes of tracked files after a modal confirmation. Untracked paths
   * (`??`) are left alone: `restore` cannot remove them. */
  discard(root: string, files: { path: string; xy: string }[]): Promise<CommandResult> {
    return this.serial(root, async () => {
      const tracked = files.filter((f) => f.xy !== "??" && f.xy !== "!!");
      const skipped = files.length - tracked.length;
      if (tracked.length === 0) {
        this.ui.info("ASC: untracked files cannot be discarded here.");
        return { ok: false, reason: "invalid" };
      }
      if (!(await this.gate())) return { ok: false, reason: "disabled" };
      const names = tracked.slice(0, 5).map((f) => f.path);
      const more = tracked.length > 5 ? `\n...and ${tracked.length - 5} more` : "";
      const ok = await this.ui.confirm(
        tracked.length === 1
          ? `Discard changes to ${names[0]}?`
          : `Discard changes to ${tracked.length} files?`,
        `${names.join("\n")}${more}\n\nThis cannot be undone.${skipped > 0 ? ` ${skipped} untracked file(s) are skipped.` : ""}`,
        "Discard",
      );
      if (!ok) return { ok: false, reason: "cancelled" };
      for (let i = 0; i < tracked.length; i += RESTORE_CHUNK) {
        const chunk = tracked.slice(i, i + RESTORE_CHUNK).map((f) => arg(f.path));
        const r = await this.runVerb(root, "restore", ["--staged", "--worktree", ...chunk]);
        if (r.exit !== 0) return this.outcome("restore", r);
      }
      return { ok: true };
    });
  }

  /** Stage whole files (`add`) or unstage them (`remove`). */
  stageFiles(root: string, paths: string[], mode: "stage" | "unstage"): Promise<CommandResult> {
    return this.serial(root, async () => {
      if (paths.length === 0) return { ok: false, reason: "invalid" };
      try {
        await this.api.stage(root, mode === "stage" ? { add: paths } : { remove: paths });
        return { ok: true };
      } catch (e) {
        this.ui.error(`ASC: ${mode} failed: ${e instanceof Error ? e.message : String(e)}`, []);
        return { ok: false, reason: "failed" };
      }
    });
  }

  private async text(root: string, path: string, at: AscAt): Promise<string | null> {
    try {
      return new TextDecoder().decode(await this.api.readFile(root, path, at));
    } catch {
      return null;
    }
  }

  /** The patch for a selection, or the whole-file fallback. Staging numbers working-file lines;
   * unstaging numbers index lines (the right side of the staged diff). */
  private async patchFor(
    root: string,
    path: string,
    mode: "stage" | "unstage",
    make: (base: string, target: string, side: Side) => string | null,
  ): Promise<{ patch: string } | { whole: true } | { none: true }> {
    const index = await this.text(root, path, { kind: "stage", stage: 0 });
    if (index === null) return { whole: true }; // not in the index yet: the whole file is the change
    const target =
      mode === "stage"
        ? await this.api.readWorking(root, path)
        : await this.text(root, path, { kind: "rev", rev: "HEAD" });
    if (target === null) return { whole: true }; // deleted in the working tree, or new in the index
    const patch = make(index, target, mode === "stage" ? "target" : "base");
    return patch === null ? { none: true } : { patch };
  }

  private async apply(
    root: string,
    path: string,
    mode: "stage" | "unstage",
    r: { patch: string } | { whole: true } | { none: true },
  ): Promise<CommandResult> {
    try {
      if ("none" in r) {
        this.ui.info("ASC: no changes in the selection.");
        return { ok: true, nothing: true };
      }
      if ("whole" in r) {
        await this.api.stage(root, mode === "stage" ? { add: [path] } : { remove: [path] });
        return { ok: true };
      }
      if (new TextEncoder().encode(r.patch).byteLength > MAX_PATCH_BYTES) {
        this.ui.warn("ASC: the selection is too large to stage by lines; stage the whole file.");
        return { ok: false, reason: "invalid" };
      }
      await this.api.stage(root, { patch: r.patch });
      return { ok: true };
    } catch (e) {
      this.ui.error(`ASC: ${mode} failed: ${e instanceof Error ? e.message : String(e)}`, []);
      return { ok: false, reason: "failed" };
    }
  }

  /** Stage or unstage the selected line ranges of one file as a host-built `git apply --cached` patch. */
  stageRanges(
    root: string,
    path: string,
    ranges: readonly LineRange[],
    mode: "stage" | "unstage",
  ): Promise<CommandResult> {
    return this.serial(root, async () => {
      if (ranges.length === 0) return { ok: false, reason: "invalid" };
      const r = await this.patchFor(root, path, mode, (b, t, side) =>
        buildPatch(path, b, t, ranges, side),
      );
      return this.apply(root, path, mode, r);
    });
  }

  /** Stage or unstage one hunk (change block) by its index in the file's diff. */
  stageHunk(
    root: string,
    path: string,
    hunk: number,
    mode: "stage" | "unstage",
  ): Promise<CommandResult> {
    return this.serial(root, async () => {
      const r = await this.patchFor(root, path, mode, (b, t) => buildHunkPatch(path, b, t, hunk));
      return this.apply(root, path, mode, r);
    });
  }
}

// --- the bridge-backed API ----------------------------------------------------------------------

const enc = new TextEncoder();

export function bridgeCommandsApi(mgr: BridgeManager): CommandsApi {
  const wsIdOf = async (root: string): Promise<string> => {
    const h = await mgr.hello();
    const r = h.roots.find((x) => x.root === root);
    if (!r?.wsId) throw new Error("asc root is not known to the bridge");
    return r.wsId;
  };
  return {
    async readFile(root, path, at, signal) {
      const wsId = await wsIdOf(root);
      const p = enc.encode(path);
      const sel =
        at.kind === "rev"
          ? { case: "revPath" as const, value: { rev: at.rev, path: p } }
          : at.kind === "stage"
            ? { case: "stage" as const, value: { stage: at.stage, path: p } }
            : { case: "fileId" as const, value: fromHex(at.fileId) };
      const chunks: Uint8Array[] = [];
      for await (const r of callStream(
        mgr.node(),
        Workspace.method.readFile,
        { wsId, at: sel },
        { root, ...(signal === undefined ? {} : { signal }) },
      )) {
        chunks.push(r.data);
      }
      let n = 0;
      for (const c of chunks) n += c.byteLength;
      const out = new Uint8Array(n);
      let o = 0;
      for (const c of chunks) {
        out.set(c, o);
        o += c.byteLength;
      }
      return out;
    },
    async run(root, verb, args, signal) {
      const stderr: string[] = [];
      let exit = -1;
      let opResult: Uint8Array | null = null;
      for await (const e of callStream(
        mgr.node(),
        bridge.Bridge.method.run,
        { verb, args, root },
        { root, ...(signal === undefined ? {} : { signal }) },
      )) {
        const ev = e.event;
        if (ev.case === "stderrLine") stderr.push(ev.value);
        else if (ev.case === "opResult") opResult = ev.value;
        else if (ev.case === "exit") exit = ev.value;
      }
      return { exit, stderr, opResult };
    },
    async stage(root, req) {
      const wsId = await wsIdOf(root);
      await callUnary(
        mgr.node(),
        Workspace.method.stage,
        {
          wsId,
          add: (req.add ?? []).map((p) => enc.encode(p)),
          remove: (req.remove ?? []).map((p) => enc.encode(p)),
          ...(req.patch === undefined ? {} : { patch: enc.encode(req.patch) }),
        },
        { root },
      );
    },
    async branchless(root) {
      const r = await callUnary(mgr.node(), bridge.Bridge.method.hello, {});
      return r.roots.find((x) => x.root === root)?.branchless ?? false;
    },
    async canMutate() {
      return (await mgr.hello()).compat.level === "full";
    },
    async readWorking(root, path) {
      try {
        return await readFile(`${root}/${path}`, "utf8");
      } catch {
        return null;
      }
    },
  };
}

// --- the editor adapter -------------------------------------------------------------------------

export interface CommandsRegistration {
  dispose(): void;
}

interface RootArg {
  rootUri?: vscodeTypes.Uri;
  inputBox?: { value: string };
}
interface QuickDiffChange {
  modifiedStartLineNumber: number;
  modifiedEndLineNumber: number;
}

/** The editor side of `CommandsUi`: messages, a modal confirmation and a read-only quick pick. */
export function vscodeCommandsUi(
  vscode: typeof vscodeTypes,
  log: (line: string) => void,
): CommandsUi {
  return {
    output: log,
    info: (m) => void vscode.window.showInformationMessage(m),
    warn: (m) => void vscode.window.showWarningMessage(m),
    error: (m, lines) => {
      for (const l of lines) log(l);
      void vscode.window.showErrorMessage(m, "Show output").then((c) => {
        if (c === "Show output") void vscode.commands.executeCommand("asc.showOutput");
      });
    },
    confirm: async (message, detail, button) =>
      (await vscode.window.showWarningMessage(message, { modal: true, detail }, button)) === button,
    review: async (title, items, accept) => {
      const list = items.length > 0 ? items : [{ label: "No hunks to show" }];
      const picked = await vscode.window.showQuickPick(
        [
          { label: `$(check) ${accept}`, accept: true },
          ...list.map((i) => ({ ...i, accept: false })),
        ],
        { title, placeHolder: `${accept}? Pick the first row to continue, Esc to cancel.` },
      );
      return picked?.accept === true;
    },
  };
}

/** Registers `asc.create`, `asc.amend`, `asc.reword`, `asc.absorb`, `asc.stageSelectedRanges`,
 * `asc.unstageSelectedRanges`, `asc.stageChange`, `asc.unstageChange`, `asc.stageFile`, `asc.unstageFile`,
 * `asc.discard`, `asc.undo` and `asc.redo`. Call after the feature check passes, next to `registerScm`. */
export function registerCommands(
  vscode: typeof vscodeTypes,
  mgr: BridgeManager,
  roots: readonly string[],
  log: (line: string) => void,
): CommandsRegistration {
  const ui = vscodeCommandsUi(vscode, log);
  const cmds = new CommitCommands(bridgeCommandsApi(mgr), ui);
  const norm = (s: string) => (process.platform === "win32" ? s.toLowerCase() : s);
  const slash = (s: string) => s.replace(/\\/g, "/").replace(/\/+$/, "");
  const rel = (root: string, fsPath: string): string | null => {
    const r = slash(root);
    const p = fsPath.replace(/\\/g, "/");
    return norm(p).startsWith(`${norm(r)}/`) ? p.slice(r.length + 1) : null;
  };
  const rootOfUri = (u: vscodeTypes.Uri): string | undefined =>
    roots.find((r) => rel(r, u.fsPath) !== null);

  const pickRoot = async (a?: unknown): Promise<string | undefined> => {
    const asRoot = a as RootArg | undefined;
    if (asRoot?.rootUri) {
      const r = rootOfUri(vscode.Uri.joinPath(asRoot.rootUri, "x"));
      if (r) return r;
    }
    const uri =
      (a as { resourceUri?: vscodeTypes.Uri } | undefined)?.resourceUri ??
      (a instanceof vscode.Uri ? a : undefined) ??
      vscode.window.activeTextEditor?.document.uri;
    const byUri = uri ? rootOfUri(uri) : undefined;
    if (byUri) return byUri;
    if (roots.length === 1) return roots[0];
    return vscode.window.showQuickPick([...roots], { placeHolder: "Choose an ASC root" });
  };

  const subs: { dispose(): unknown }[] = [];
  const reg = (id: string, fn: (...a: never[]) => unknown) =>
    subs.push(vscode.commands.registerCommand(id, fn as (...a: unknown[]) => unknown));

  reg("asc.create", async (a?: unknown) => {
    const root = await pickRoot(a);
    if (!root) return;
    const box = (a as RootArg | undefined)?.inputBox;
    const message =
      box?.value ??
      (await vscode.window.showInputBox({ prompt: "Message for the new change" })) ??
      "";
    const r = await cmds.create(root, message);
    if (r.ok && box) box.value = "";
  });
  reg("asc.amend", async (a?: unknown) => {
    const root = await pickRoot(a);
    if (root) await cmds.amend(root);
  });
  reg("asc.reword", async (a?: unknown) => {
    const root = await pickRoot(a);
    if (!root) return;
    const box = (a as RootArg | undefined)?.inputBox;
    const message =
      box?.value ||
      (await vscode.window.showInputBox({ prompt: "New message for the current change" })) ||
      "";
    const r = await cmds.reword(root, message);
    if (r.ok && box) box.value = "";
  });
  reg("asc.absorb", async (a?: unknown) => {
    const root = await pickRoot(a);
    if (root) await cmds.absorb(root);
  });
  reg("asc.undo", async (a?: unknown) => {
    const root = await pickRoot(a);
    if (root) await cmds.undo(root);
  });
  reg("asc.redo", async (a?: unknown) => {
    const root = await pickRoot(a);
    if (root) await cmds.redo(root);
  });

  const resources = (a: unknown, rest: unknown[]): { root: string; path: string; xy: string }[] => {
    const all = [a, ...rest.flat()].filter(
      (x): x is { resourceUri: vscodeTypes.Uri; contextValue?: string } =>
        typeof x === "object" && x !== null && "resourceUri" in x,
    );
    const out: { root: string; path: string; xy: string }[] = [];
    for (const r of all) {
      const root = rootOfUri(r.resourceUri);
      const path = root ? rel(root, r.resourceUri.fsPath) : null;
      if (root && path !== null) out.push({ root, path, xy: r.contextValue ?? "" });
    }
    return out;
  };
  const perRoot = async <T extends { root: string }>(
    items: T[],
    fn: (root: string, items: T[]) => Promise<unknown>,
  ) => {
    const by = new Map<string, T[]>();
    for (const i of items) by.set(i.root, [...(by.get(i.root) ?? []), i]);
    for (const [root, list] of by) await fn(root, list);
  };

  reg("asc.discard", async (a?: unknown, ...rest: unknown[]) => {
    await perRoot(resources(a, rest), (root, l) => cmds.discard(root, l));
  });
  reg("asc.stageFile", async (a?: unknown, ...rest: unknown[]) => {
    await perRoot(resources(a, rest), (root, l) =>
      cmds.stageFiles(
        root,
        l.map((x) => x.path),
        "stage",
      ),
    );
  });
  reg("asc.unstageFile", async (a?: unknown, ...rest: unknown[]) => {
    await perRoot(resources(a, rest), (root, l) =>
      cmds.stageFiles(
        root,
        l.map((x) => x.path),
        "unstage",
      ),
    );
  });

  const selectionRanges = (ed: vscodeTypes.TextEditor): LineRange[] =>
    ed.selections.map((s) => {
      // A selection ending at column 0 of a later line does not include that line.
      const end = s.end.character === 0 && s.end.line > s.start.line ? s.end.line : s.end.line + 1;
      return { start: s.start.line + 1, end };
    });
  const stageSelection = (mode: "stage" | "unstage") => async () => {
    const ed = vscode.window.activeTextEditor;
    if (!ed) return;
    const root = rootOfUri(ed.document.uri);
    const path = root ? rel(root, ed.document.uri.fsPath) : null;
    if (!root || path === null) return;
    await cmds.stageRanges(root, path, selectionRanges(ed), mode);
  };
  reg("asc.stageSelectedRanges", stageSelection("stage"));
  reg("asc.unstageSelectedRanges", stageSelection("unstage"));

  // `scm/change/title` passes (uri, changes, index): the gutter peek's change.
  const stageChange =
    (mode: "stage" | "unstage") =>
    async (uri?: vscodeTypes.Uri, changes?: QuickDiffChange[], index?: number) => {
      const c = changes?.[index ?? 0];
      if (!uri || !c) return;
      const root = rootOfUri(uri);
      const path = root ? rel(root, uri.fsPath) : null;
      if (!root || path === null) return;
      // A pure deletion reports an empty modified range after line `start`: a point at the next line.
      const range: LineRange =
        c.modifiedEndLineNumber === 0
          ? { start: c.modifiedStartLineNumber + 1, end: c.modifiedStartLineNumber }
          : { start: c.modifiedStartLineNumber, end: c.modifiedEndLineNumber };
      await cmds.stageRanges(root, path, [range], mode);
    };
  reg("asc.stageChange", stageChange("stage"));
  reg("asc.unstageChange", stageChange("unstage"));

  return {
    dispose() {
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
