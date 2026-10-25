// Live updates in the status bar (SPEC 21.3.8, 24.4, P59). Live updates are off by default, so the plugin
// shows "trunk moved N commits - Restack" (N from `WsStack.behind`, API addition A1) and the user runs the
// restack. A toggle runs `live pause|resume`; a live-conflict notice shows while the worktree is paused by a
// conflict, until `live resume`. The plugin never moves the working copy itself: its only verbs are
// `restack`, `live pause` and `live resume`, each run through the bridge allowlist. Everything the verb
// printed is untrusted: the status JSON is read defensively and no text from it is shown.
// Editor-free; `registerLive` is the only `vscode` user.
// TODO(P10): the bridge allowlist has no `live` entry yet; `LiveApi.run`/`status` are the seam, and the
// extension supplies `Run live ...` once the allowlist adds it.
import type * as vscodeTypes from "vscode";

export const TOGGLE_COMMAND = "asc.live.toggle";
export const RESTACK_COMMAND = "asc.restackOntoTrunk";
/** Same cap as the status bar (`WsStack.behind` is capped at 999). */
export const BEHIND_CAP = 999;

export interface LiveStatus {
  paused: boolean;
  /** `conflict` (the daemon paused it) or `user`; null when running or unknown. */
  reason: "conflict" | "user" | null;
}

export interface LiveApi {
  /** Runs one asc verb in the root (through the allowlist); resolves the exit code. */
  run(root: string, verb: string, args: string[]): Promise<{ exit: number }>;
  /** The encoded `asc live status --json` of the root, or null when it could not be read. */
  status(root: string, signal: AbortSignal): Promise<Uint8Array | null>;
}

/** Reads `live status --json` defensively. Malformed input gives null. */
export function parseLiveStatus(bytes: Uint8Array | null): LiveStatus | null {
  if (bytes === null || bytes.byteLength === 0 || bytes.byteLength > 65_536) return null;
  let v: unknown;
  try {
    v = JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    return null;
  }
  if (typeof v !== "object" || v === null || Array.isArray(v)) return null;
  const o = v as Record<string, unknown>;
  if (typeof o.paused !== "boolean") return null;
  if (!o.paused) return { paused: false, reason: null };
  return { paused: true, reason: o.pauseReason === "conflict" ? "conflict" : "user" };
}

export interface LiveView {
  /** The "trunk moved" item; hidden at N = 0. */
  readonly restack: { visible: boolean; text: string; tooltip: string };
  /** The conflict notice; visible only while a conflict pauses live updates. */
  readonly notice: { visible: boolean; text: string };
  /** The toggle's label: the verb a click runs. */
  readonly toggle: { text: string; verb: "pause" | "resume" };
}

export function liveView(behind: number, s: LiveStatus | null): LiveView {
  const n = Number.isFinite(behind) ? Math.min(Math.max(Math.trunc(behind), 0), BEHIND_CAP) : 0;
  const paused = s?.paused === true;
  const conflict = paused && s?.reason === "conflict";
  return {
    restack: {
      visible: n > 0,
      text: n > 0 ? `trunk moved ${n} ${n === 1 ? "commit" : "commits"} - Restack` : "",
      tooltip: "Run asc restack onto the new trunk. Live updates never move your working copy.",
    },
    notice: {
      visible: conflict,
      text: conflict ? "Live updates paused by a conflict. Resolve it, then resume." : "",
    },
    toggle: paused
      ? { text: "Resume live updates", verb: "resume" }
      : { text: "Pause live updates", verb: "pause" },
  };
}

export interface LiveModelOptions {
  api: LiveApi;
  root: string;
  onError?: (e: unknown) => void;
}

/** Holds the last status and `behind`; refreshes on events only (no timer). */
export class LiveModel {
  status: LiveStatus | null = null;
  behind = 0;
  private seq = 0;
  private abort: AbortController | null = null;
  private disposed = false;
  private readonly listeners = new Set<() => void>();
  constructor(private readonly o: LiveModelOptions) {}

  view(): LiveView {
    return liveView(this.behind, this.status);
  }

  onChange(l: () => void): { dispose(): void } {
    this.listeners.add(l);
    return { dispose: () => void this.listeners.delete(l) };
  }

  /** `behind` comes from the `Stacks` snapshot the status bar already reads. */
  setBehind(n: number): void {
    if (n === this.behind) return;
    this.behind = n;
    this.emit();
  }

  async refresh(): Promise<void> {
    if (this.disposed) return;
    this.abort?.abort();
    const ac = new AbortController();
    this.abort = ac;
    const mine = ++this.seq;
    let parsed: LiveStatus | null;
    try {
      parsed = parseLiveStatus(await this.o.api.status(this.o.root, ac.signal));
    } catch (e) {
      if (!ac.signal.aborted) this.o.onError?.(e);
      return;
    }
    if (this.disposed || mine !== this.seq) return;
    this.status = parsed;
    this.emit();
  }

  /** Runs `live pause` or `live resume` (whichever the view offers), then re-reads the status. */
  async toggle(): Promise<boolean> {
    const verb = this.view().toggle.verb;
    const r = await this.o.api.run(this.o.root, "live", [verb]);
    await this.refresh();
    return r.exit === 0;
  }

  /** Runs `restack`; the verb moves the stack, the plugin does nothing else. */
  async restack(): Promise<boolean> {
    const r = await this.o.api.run(this.o.root, "restack", []);
    return r.exit === 0;
  }

  dispose(): void {
    this.disposed = true;
    this.abort?.abort();
    this.listeners.clear();
  }

  private emit(): void {
    for (const l of this.listeners) l();
  }
}

export function registerLive(vscode: typeof vscodeTypes, model: LiveModel): { dispose(): void } {
  const restack = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 48);
  restack.name = "asc trunk moved";
  restack.command = RESTACK_COMMAND;
  const notice = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 47);
  notice.name = "asc live conflict";
  notice.backgroundColor = new vscode.ThemeColor("statusBarItem.warningBackground");
  notice.command = TOGGLE_COMMAND;
  const paint = () => {
    const v = model.view();
    restack.text = v.restack.text === "" ? "" : `$(arrow-down) ${v.restack.text}`;
    restack.tooltip = v.restack.tooltip;
    if (v.restack.visible) restack.show();
    else restack.hide();
    notice.text = v.notice.text === "" ? "" : `$(warning) ${v.notice.text}`;
    notice.tooltip = v.toggle.text;
    if (v.notice.visible) notice.show();
    else notice.hide();
  };
  const subs = [
    restack,
    notice,
    model.onChange(paint),
    vscode.commands.registerCommand(TOGGLE_COMMAND, () => model.toggle()),
  ];
  paint();
  return {
    dispose() {
      model.dispose();
      for (const s of subs) s.dispose();
    },
  };
}
