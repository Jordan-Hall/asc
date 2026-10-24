// Status-bar inbox count (SPEC 24.4.7). A light poll that is independent of the Inbox view:
// `asc.v1.Inbox/Count` for the needs-review section with its `if_none_match` etag (feature
// `inbox.v1`). It runs once on window focus and after a review action, then every 5 minutes while
// the window is focused and an asc server is reachable, and stops on blur or when `asc.inbox.statusBar`
// is false. The item is stale (dimmed, "updated N min ago") when the last success is older than
// 15 minutes or the server is offline. Editor-free; `registerInboxCount` is the only `vscode` user.
import type * as vscodeTypes from "vscode";

export const POLL_MS = 5 * 60 * 1000;
export const STALE_MS = 15 * 60 * 1000;

export type CountResult =
  | { readonly status: "ok"; readonly total: number; readonly etag: string | undefined }
  | { readonly status: "not-modified" };

export interface CountApi {
  /** One conditional request. Rejects when the server is unreachable or the caller has no user identity. */
  needsReviewCount(etag: string | undefined, signal: AbortSignal): Promise<CountResult>;
}

export interface Timers {
  setInterval(fn: () => void, ms: number): unknown;
  clearInterval(h: unknown): void;
}

export interface InboxCountOptions {
  api: CountApi;
  /** Whether an asc server is configured for the workspace; polling stops when false. */
  signedIn: () => boolean;
  /** `asc.inbox.statusBar`. */
  enabled: () => boolean;
  now?: () => number;
  timers?: Timers;
  onError?: (e: unknown) => void;
}

export interface CountView {
  readonly text: string;
  readonly dimmed: boolean;
  readonly tooltip: string;
}

export class InboxCountPoller {
  total: number | undefined;
  private etag: string | undefined;
  private lastOk: number | undefined;
  private offline = false;
  private focused = false;
  private handle: unknown = null;
  private abort: AbortController | null = null;
  private seq = 0;
  private readonly listeners = new Set<() => void>();
  private readonly now: () => number;
  private readonly timers: Timers;

  constructor(private readonly o: InboxCountOptions) {
    this.now = o.now ?? Date.now;
    this.timers = o.timers ?? {
      setInterval: (fn, ms) => setInterval(fn, ms),
      clearInterval: (h) => clearInterval(h as ReturnType<typeof setInterval>),
    };
  }

  onChange(fn: () => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }

  get running(): boolean {
    return this.handle !== null;
  }

  /** Window focus changed. Focus polls once, then every 5 minutes; blur stops the timer. */
  setFocused(focused: boolean): void {
    this.focused = focused;
    if (focused) this.start();
    else this.stop();
  }

  /** Settings or sign-in changed: restart or stop as the conditions now say. */
  reevaluate(): void {
    if (this.focused) this.start();
    else this.stop();
    this.emit();
  }

  /** After approve, request changes, comment or merge. */
  afterReviewAction(): void {
    if (this.canPoll()) void this.poll();
  }

  private canPoll(): boolean {
    return this.focused && this.o.enabled() && this.o.signedIn();
  }

  private start(): void {
    this.stopTimer();
    if (!this.canPoll()) return;
    void this.poll();
    this.handle = this.timers.setInterval(() => void this.poll(), POLL_MS);
  }

  private stopTimer(): void {
    if (this.handle !== null) this.timers.clearInterval(this.handle);
    this.handle = null;
  }

  private stop(): void {
    this.stopTimer();
    this.seq++;
    this.abort?.abort();
    this.abort = null;
  }

  async poll(): Promise<void> {
    if (!this.canPoll()) return;
    const mine = ++this.seq;
    this.abort?.abort();
    const ac = new AbortController();
    this.abort = ac;
    try {
      const r = await this.o.api.needsReviewCount(this.etag, ac.signal);
      if (mine !== this.seq) return;
      if (r.status === "ok") {
        this.total = r.total;
        this.etag = r.etag;
      }
      this.lastOk = this.now();
      this.offline = false;
    } catch (e) {
      if (mine !== this.seq || ac.signal.aborted) return;
      this.offline = true;
      this.o.onError?.(e);
    }
    this.emit();
  }

  view(): CountView {
    const t = this.total;
    const ageMs = this.lastOk === undefined ? undefined : Math.max(0, this.now() - this.lastOk);
    const dimmed = this.offline || ageMs === undefined || ageMs > STALE_MS;
    const mins = ageMs === undefined ? undefined : Math.floor(ageMs / 60_000);
    const when =
      mins === undefined
        ? this.offline
          ? "server offline"
          : "not updated yet"
        : `updated ${mins} min ago${this.offline ? ", server offline" : ""}`;
    const n = t === undefined ? "-" : String(t);
    return {
      text: `$(git-pull-request) ${n}`,
      dimmed,
      tooltip: `asc: ${n} ${t === 1 ? "change needs" : "changes need"} your review (${when})`,
    };
  }

  dispose(): void {
    this.stop();
    this.listeners.clear();
  }

  private emit(): void {
    for (const l of this.listeners) l();
  }
}

export function registerInboxCount(
  vscode: typeof vscodeTypes,
  poller: InboxCountPoller,
): { dispose(): void } {
  const item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  item.command = "asc.inbox.focus";
  const paint = () => {
    const v = poller.view();
    item.text = v.text;
    item.tooltip = v.tooltip;
    item.color = v.dimmed ? new vscode.ThemeColor("disabledForeground") : undefined;
    if (vscode.workspace.getConfiguration("asc").get<boolean>("inbox.statusBar", true)) item.show();
    else item.hide();
  };
  const subs = [
    item,
    poller.onChange(paint),
    vscode.window.onDidChangeWindowState((s) => poller.setFocused(s.focused)),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("asc.inbox.statusBar")) {
        poller.reevaluate();
        paint();
      }
    }),
  ];
  poller.setFocused(vscode.window.state.focused);
  paint();
  return {
    dispose() {
      poller.dispose();
      for (const s of subs) s.dispose();
    },
  };
}
