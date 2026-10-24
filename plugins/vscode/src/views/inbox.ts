// Inbox tree (SPEC 24.4.4, 24.4.7): the Forgejo inbox in the `asc` container. It has no event stream,
// so it fetches on show and on window focus, then every 60 s only while visible, with `If-None-Match`
// (a 304 keeps the last list). Pin and snooze go through `PUT /user/asc/inbox/{id}`. The token is
// attached by the guarded Forgejo client and never logged. `InboxModel` is editor-free;
// `registerInbox` is the only `vscode` user.
import { createForgejoClient, type FetchLike } from "@asc/asc-client/forgejo";
import type * as vscodeTypes from "vscode";
import type { CountApi, CountResult } from "../inbox-count";

export const REFRESH_MS = 60_000;

export interface InboxPull {
  number?: number;
  title?: string;
  draft?: boolean;
  state?: string;
}
export interface InboxEntry {
  id?: number;
  section?: string;
  pinned?: boolean;
  snoozed_until?: string;
  pull_request?: InboxPull;
}

export const SECTIONS = [
  ["needs_review", "Needs your review"],
  ["returned", "Returned to you"],
  ["ready", "Ready to merge"],
  ["blocked", "Blocked"],
  ["waiting", "Waiting"],
  ["drafts", "Drafts"],
  ["queued", "Queued"],
] as const;

export type ListResult =
  | { readonly status: "ok"; readonly entries: InboxEntry[]; readonly etag: string | undefined }
  | { readonly status: "not-modified" };

export interface InboxApi extends CountApi {
  list(etag: string | undefined, signal: AbortSignal): Promise<ListResult>;
  update(
    id: number,
    body: { pinned?: boolean; snoozed_until?: string },
    signal?: AbortSignal,
  ): Promise<void>;
}

export interface InboxApiOptions {
  baseUrl: string;
  getToken: () => string | undefined | Promise<string | undefined>;
  repos?: Iterable<string>;
  fetch?: FetchLike;
}

const cond = (etag: string | undefined): Record<string, string> =>
  etag ? { "If-None-Match": etag } : {};

/** The inbox API over the deny-by-default Forgejo client (https only, allowlisted pairs). */
export function forgejoInboxApi(o: InboxApiOptions): InboxApi {
  const { api } = createForgejoClient({
    baseUrl: o.baseUrl,
    getToken: o.getToken,
    ...(o.repos ? { repos: o.repos } : {}),
    ...(o.fetch ? { fetch: o.fetch } : {}),
  });
  return {
    async list(etag, signal) {
      const r = await api.GET("/user/asc/inbox", {
        params: { query: { limit: 50 } },
        headers: cond(etag),
        signal,
      });
      if (r.response.status === 304) return { status: "not-modified" };
      if (!r.response.ok || !r.data) throw new Error(`inbox: HTTP ${r.response.status}`);
      return {
        status: "ok",
        entries: r.data as InboxEntry[],
        etag: r.response.headers.get("etag") ?? undefined,
      };
    },
    async needsReviewCount(etag, signal): Promise<CountResult> {
      const r = await api.GET("/user/asc/inbox", {
        params: { query: { section: "needs_review", limit: 1 } },
        headers: cond(etag),
        signal,
      });
      if (r.response.status === 304) return { status: "not-modified" };
      if (!r.response.ok) throw new Error(`inbox count: HTTP ${r.response.status}`);
      const raw = r.response.headers.get("x-total-count");
      const total = raw === null ? Number.NaN : Number(raw);
      if (!Number.isSafeInteger(total) || total < 0) {
        throw new Error("inbox count: no X-Total-Count");
      }
      return { status: "ok", total, etag: r.response.headers.get("etag") ?? undefined };
    },
    async update(id, body, signal) {
      const r = await api.PUT("/user/asc/inbox/{id}", {
        params: { path: { id } },
        body,
        ...(signal ? { signal } : {}),
      });
      if (!r.response.ok) throw new Error(`inbox update: HTTP ${r.response.status}`);
    },
  };
}

export interface InboxItem {
  id: number;
  number: number | undefined;
  title: string;
  pinned: boolean;
  snoozedUntil: string | undefined;
}
export interface InboxGroup {
  id: string;
  title: string;
  items: InboxItem[];
}

export function isSnoozed(e: Pick<InboxEntry, "snoozed_until">, now: number): boolean {
  if (e.snoozed_until === undefined) return false;
  const t = Date.parse(e.snoozed_until);
  return Number.isFinite(t) && t > now;
}

/** Non-empty sections only; snoozed entries hidden; pinned first, then by PR number. */
export function group(entries: readonly InboxEntry[], now: number): InboxGroup[] {
  const out: InboxGroup[] = [];
  for (const [id, title] of SECTIONS) {
    const items = entries
      .filter((e) => e.section === id && e.id !== undefined && !isSnoozed(e, now))
      .map((e) => ({
        id: e.id as number,
        number: e.pull_request?.number,
        title: e.pull_request?.title ?? "(untitled)",
        pinned: e.pinned === true,
        snoozedUntil: e.snoozed_until,
      }))
      .toSorted((a, b) => Number(b.pinned) - Number(a.pinned) || (a.number ?? 0) - (b.number ?? 0));
    if (items.length > 0) out.push({ id, title, items });
  }
  return out;
}

export interface InboxModelOptions {
  api: InboxApi;
  signedIn: () => boolean;
  now?: () => number;
  setInterval?: (fn: () => void, ms: number) => unknown;
  clearInterval?: (h: unknown) => void;
  onError?: (e: unknown) => void;
}

export class InboxModel {
  entries: InboxEntry[] = [];
  private etag: string | undefined;
  private visible = false;
  private focused = true;
  private stale = true;
  private handle: unknown = null;
  private seq = 0;
  private abort: AbortController | null = null;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly o: InboxModelOptions) {}

  get isStale(): boolean {
    return this.stale;
  }
  get running(): boolean {
    return this.handle !== null;
  }
  get groups(): InboxGroup[] {
    return group(this.entries, this.now());
  }
  now(): number {
    return (this.o.now ?? Date.now)();
  }

  onChange(fn: () => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }

  setVisible(v: boolean): void {
    this.visible = v;
    this.sync(v && this.stale);
  }

  setFocused(f: boolean): void {
    this.focused = f;
    this.sync(f && this.visible);
  }

  private sync(fetchNow: boolean): void {
    const run = this.visible && this.focused && this.o.signedIn();
    if (!run) {
      this.stopTimer();
      if (!this.visible) this.stale = true;
      return;
    }
    if (this.handle === null) {
      const si = this.o.setInterval ?? ((fn, ms) => setInterval(fn, ms));
      this.handle = si(() => void this.refresh(), REFRESH_MS);
    }
    if (fetchNow) void this.refresh();
  }

  private stopTimer(): void {
    if (this.handle === null) return;
    const ci = this.o.clearInterval ?? ((h) => clearInterval(h as ReturnType<typeof setInterval>));
    ci(this.handle);
    this.handle = null;
  }

  async refresh(): Promise<void> {
    if (!this.o.signedIn()) return;
    const mine = ++this.seq;
    this.abort?.abort();
    const ac = new AbortController();
    this.abort = ac;
    try {
      const r = await this.o.api.list(this.etag, ac.signal);
      if (mine !== this.seq) return;
      if (r.status === "ok") {
        this.entries = r.entries;
        this.etag = r.etag;
      }
      this.stale = false;
      for (const l of this.listeners) l();
    } catch (e) {
      if (mine === this.seq && !ac.signal.aborted) this.o.onError?.(e);
    }
  }

  async pin(item: InboxItem, pinned: boolean): Promise<void> {
    await this.o.api.update(item.id, { pinned });
    this.etag = undefined;
    await this.refresh();
  }

  async snooze(item: InboxItem, presetMs: number): Promise<void> {
    await this.o.api.update(item.id, {
      snoozed_until: new Date(this.now() + presetMs).toISOString(),
    });
    this.etag = undefined;
    await this.refresh();
  }

  dispose(): void {
    this.stopTimer();
    this.abort?.abort();
    this.listeners.clear();
  }
}

export const SNOOZE_PRESETS = [
  { label: "1 hour", ms: 3_600_000 },
  { label: "Tomorrow", ms: 86_400_000 },
  { label: "1 week", ms: 7 * 86_400_000 },
] as const;

export function registerInbox(
  vscode: typeof vscodeTypes,
  model: InboxModel,
  log: (line: string) => void,
): { dispose(): void } {
  const changed = new vscode.EventEmitter<void>();
  type Node = { kind: "group"; g: InboxGroup } | { kind: "item"; i: InboxItem };
  const provider: vscodeTypes.TreeDataProvider<Node> = {
    onDidChangeTreeData: changed.event,
    getChildren: (el) =>
      !el
        ? model.groups.map((g): Node => ({ kind: "group", g }))
        : el.kind === "group"
          ? el.g.items.map((i): Node => ({ kind: "item", i }))
          : [],
    getTreeItem(n) {
      if (n.kind === "group") {
        const t = new vscode.TreeItem(
          `${n.g.title} (${n.g.items.length})`,
          vscode.TreeItemCollapsibleState.Expanded,
        );
        t.contextValue = "asc.inbox.group";
        return t;
      }
      const t = new vscode.TreeItem(
        n.i.number === undefined ? n.i.title : `#${n.i.number} ${n.i.title}`,
        vscode.TreeItemCollapsibleState.None,
      );
      t.contextValue = n.i.pinned ? "asc.inbox.pinned" : "asc.inbox.item";
      t.iconPath = new vscode.ThemeIcon(n.i.pinned ? "pinned" : "git-pull-request");
      return t;
    },
  };
  const view = vscode.window.createTreeView("asc.inbox", { treeDataProvider: provider });
  const fail = (what: string) => (e: unknown) => {
    log(`asc inbox ${what}: ${String(e)}`);
    void vscode.window.showErrorMessage(`asc: could not ${what} the inbox entry.`);
  };
  const item = (n?: Node) => (n?.kind === "item" ? n.i : undefined);
  const subs: { dispose(): unknown }[] = [
    changed,
    view,
    model.onChange(() => changed.fire()),
    view.onDidChangeVisibility((e) => model.setVisible(e.visible)),
    vscode.window.onDidChangeWindowState((s) => model.setFocused(s.focused)),
    vscode.commands.registerCommand("asc.inbox.refresh", () => void model.refresh()),
    vscode.commands.registerCommand("asc.inbox.pin", (n?: Node) => {
      const i = item(n);
      if (i) model.pin(i, true).catch(fail("pin"));
    }),
    vscode.commands.registerCommand("asc.inbox.unpin", (n?: Node) => {
      const i = item(n);
      if (i) model.pin(i, false).catch(fail("unpin"));
    }),
    vscode.commands.registerCommand("asc.inbox.snooze", async (n?: Node) => {
      const i = item(n);
      if (!i) return;
      const pick = await vscode.window.showQuickPick(SNOOZE_PRESETS.map((p) => p.label));
      const p = SNOOZE_PRESETS.find((x) => x.label === pick);
      if (p) await model.snooze(i, p.ms).catch(fail("snooze"));
    }),
  ];
  model.setFocused(vscode.window.state.focused);
  model.setVisible(view.visible);
  return {
    dispose() {
      model.dispose();
      for (const s of subs.splice(0)) s.dispose();
    },
  };
}
