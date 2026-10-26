// Inbox tree (SPEC 24.4.4, 24.4.7): the review inbox in the `asc` container, over the host-neutral
// `asc.v1.Inbox` service (feature `inbox.v1`). It has no event stream, so it fetches on show and on
// window focus, then every 60 s only while visible; a `Count` etag answers "nothing changed" without
// paging the list. The plugin knows no forge: items come from asc's own review requests, threads and
// check runs. `InboxModel` is editor-free; `registerInbox` is the only `vscode` user.
import { callUnary, changeIdFromString, changeIdToString } from "@asc/asc-client";
import { Inbox, InboxSection } from "@asc/asc-client/inbox";
import type * as vscodeTypes from "vscode";
import type { BridgeManager, ServerTarget } from "../bridge";
import type { CountApi, CountResult } from "../inbox-count";

export const REFRESH_MS = 60_000;
/** Most items one root contributes: pages of 200, at most 5 pages. */
export const MAX_ITEMS_PER_ROOT = 1000;

export interface InboxEntry {
  /** The bridge root the item belongs to. */
  root: string;
  /** The change id, 32 letters k-z. */
  id: string;
  section: string;
  latestVersion?: number;
  author?: string;
  /** Unix seconds of the newest event that put the item here. */
  updated?: number;
  openThreads?: number;
  requestedBy?: string;
  pinned?: boolean;
  /** Unix seconds; 0 or absent when not snoozed. */
  snoozedUntil?: number;
}

export const SECTIONS = [
  ["needs_review", "Needs your review"],
  ["returned", "Returned to you"],
  ["checks_failed", "Checks failed"],
  ["waiting", "Waiting"],
] as const;

export type ListResult =
  | { readonly status: "ok"; readonly entries: InboxEntry[]; readonly etag: string | undefined }
  | { readonly status: "not-modified" };

export interface InboxRef {
  root: string;
  id: string;
}

export interface InboxApi extends CountApi {
  list(etag: string | undefined, signal: AbortSignal): Promise<ListResult>;
  /** `pinned` and `snoozedUntil` (unix seconds, 0 clears); an absent field is left as it is. */
  update(
    ref: InboxRef,
    body: { pinned?: boolean; snoozedUntil?: number },
    signal?: AbortSignal,
  ): Promise<void>;
}

export interface InboxItem {
  root: string;
  id: string;
  label: string;
  description: string;
  pinned: boolean;
  snoozedUntil: number;
}
export interface InboxGroup {
  id: string;
  title: string;
  items: InboxItem[];
}

export function isSnoozed(e: Pick<InboxEntry, "snoozedUntil">, now: number): boolean {
  return e.snoozedUntil !== undefined && e.snoozedUntil > 0 && e.snoozedUntil * 1000 > now;
}

function describe(e: InboxEntry): string {
  const bits: string[] = [];
  if (e.section === "needs_review" && e.requestedBy) bits.push(`from ${e.requestedBy}`);
  if (e.openThreads) bits.push(`${e.openThreads} open thread${e.openThreads === 1 ? "" : "s"}`);
  if (e.latestVersion) bits.push(`v${e.latestVersion}`);
  return bits.join("  ");
}

/** Non-empty sections only; snoozed entries hidden; pinned first, then newest first. */
export function group(entries: readonly InboxEntry[], now: number): InboxGroup[] {
  const out: InboxGroup[] = [];
  for (const [id, title] of SECTIONS) {
    const items = entries
      .filter((e) => e.section === id && !isSnoozed(e, now))
      .toSorted(
        (a, b) =>
          Number(b.pinned === true) - Number(a.pinned === true) ||
          (b.updated ?? 0) - (a.updated ?? 0),
      )
      .map((e) => ({
        root: e.root,
        id: e.id,
        label: e.author ? `${e.id.slice(0, 8)} by ${e.author}` : e.id.slice(0, 8),
        description: describe(e),
        pinned: e.pinned === true,
        snoozedUntil: e.snoozedUntil ?? 0,
      }));
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

  async pin(item: InboxRef, pinned: boolean): Promise<void> {
    await this.o.api.update({ root: item.root, id: item.id }, { pinned });
    this.etag = undefined;
    await this.refresh();
  }

  async snooze(item: InboxRef, presetMs: number): Promise<void> {
    await this.o.api.update(
      { root: item.root, id: item.id },
      { snoozedUntil: Math.floor((this.now() + presetMs) / 1000) },
    );
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

// --- the bridge adapter ---------------------------------------------------------------------------

const SECTION_NAMES = new Map<InboxSection, string>([
  [InboxSection.NEEDS_REVIEW, "needs_review"],
  [InboxSection.RETURNED, "returned"],
  [InboxSection.CHECKS_FAILED, "checks_failed"],
  [InboxSection.WAITING, "waiting"],
]);

interface Cached<T> {
  etag: string;
  value: T;
}

type Probe<T> = { notModified: true } | { notModified: false; etag: string; value: T };

/**
 * Asks every root with its own `Count` etag. When all answer not-modified the caller keeps what it has;
 * otherwise the changed roots are read again and the rest come from the cache. `etag` undefined means
 * "read everything" (after a pin or snooze). The returned etag only has to differ when something did.
 */
async function pollRoots<T>(
  targets: readonly ServerTarget[],
  etag: string | undefined,
  cache: Map<string, Cached<T>>,
  one: (t: ServerTarget, ifNoneMatch: string) => Promise<Probe<T>>,
): Promise<{ changed: false } | { changed: true; values: T[]; etag: string }> {
  if (etag === undefined) cache.clear();
  const probes = await Promise.all(targets.map((t) => one(t, cache.get(t.root)?.etag ?? "")));
  let changed = false;
  const values: T[] = [];
  const tags: string[] = [];
  probes.forEach((p, i) => {
    const t = targets[i] as ServerTarget;
    if (p.notModified) {
      const c = cache.get(t.root);
      if (c) {
        values.push(c.value);
        tags.push(c.etag);
      }
      return;
    }
    changed = true;
    cache.set(t.root, { etag: p.etag, value: p.value });
    values.push(p.value);
    tags.push(p.etag);
  });
  return changed || etag === undefined
    ? { changed: true, values, etag: tags.join("|") || "-" }
    : { changed: false };
}

/** Inbox calls over the bridge, one set per asc-server root. `targets` is read on every call. */
export function bridgeInboxApi(
  mgr: BridgeManager,
  targets: () => readonly ServerTarget[],
): InboxApi {
  const listCache = new Map<string, Cached<InboxEntry[]>>();
  const countCache = new Map<string, Cached<number>>();

  const count = (
    t: ServerTarget,
    sections: InboxSection[],
    ifNoneMatch: string,
    signal: AbortSignal,
  ) =>
    callUnary(
      mgr.node(),
      Inbox.method.count,
      { repo: { name: t.repo }, sections, ifNoneMatch },
      { root: t.root, signal },
    );

  return {
    async needsReviewCount(etag, signal): Promise<CountResult> {
      const r = await pollRoots(targets(), etag, countCache, async (t, ifNoneMatch) => {
        const c = await count(t, [InboxSection.NEEDS_REVIEW], ifNoneMatch, signal);
        return c.notModified
          ? { notModified: true }
          : { notModified: false, etag: c.etag, value: c.total };
      });
      return r.changed
        ? { status: "ok", total: r.values.reduce((a, b) => a + b, 0), etag: r.etag }
        : { status: "not-modified" };
    },

    async list(etag, signal): Promise<ListResult> {
      const r = await pollRoots(targets(), etag, listCache, async (t, ifNoneMatch) => {
        const c = await count(t, [], ifNoneMatch, signal);
        if (c.notModified) return { notModified: true };
        const entries: InboxEntry[] = [];
        let token = "";
        do {
          const page = await callUnary(
            mgr.node(),
            Inbox.method.list,
            { repo: { name: t.repo }, limit: 200, pageToken: token },
            { root: t.root, signal },
          );
          for (const it of page.items) {
            const section = SECTION_NAMES.get(it.section);
            if (section === undefined) continue;
            entries.push({
              root: t.root,
              id: changeIdToString(it.change),
              section,
              latestVersion: it.latestVersion,
              author: it.author,
              updated: Number(it.updated),
              openThreads: it.openThreads,
              requestedBy: it.requestedBy,
              pinned: it.pinned,
              snoozedUntil: Number(it.snoozedUntil),
            });
          }
          token = page.nextPageToken;
        } while (token !== "" && entries.length < MAX_ITEMS_PER_ROOT);
        return { notModified: false, etag: c.etag, value: entries };
      });
      return r.changed
        ? { status: "ok", entries: r.values.flat(), etag: r.etag }
        : { status: "not-modified" };
    },

    async update(ref, body, signal) {
      const t = targets().find((x) => x.root === ref.root);
      if (!t) throw new Error("this folder has no asc server");
      await callUnary(
        mgr.node(),
        Inbox.method.setState,
        {
          repo: { name: t.repo },
          change: changeIdFromString(ref.id),
          ...(body.pinned === undefined ? {} : { pinned: body.pinned }),
          ...(body.snoozedUntil === undefined ? {} : { snoozedUntil: BigInt(body.snoozedUntil) }),
        },
        { root: t.root, ...(signal ? { signal } : {}) },
      );
    },
  };
}

// --- the editor adapter ---------------------------------------------------------------------------

export function registerInbox(
  vscode: typeof vscodeTypes,
  model: InboxModel,
  log: (line: string) => void,
  /** "Check out stack" for an item. */
  checkout: (item: InboxItem) => Promise<void>,
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
      const t = new vscode.TreeItem(n.i.label, vscode.TreeItemCollapsibleState.None);
      t.description = n.i.description;
      t.tooltip = `Change ${n.i.id}`;
      t.contextValue = n.i.pinned ? "asc.inbox.pinned" : "asc.inbox.item";
      t.iconPath = new vscode.ThemeIcon(n.i.pinned ? "pinned" : "git-pull-request");
      return t;
    },
  };
  const view = vscode.window.createTreeView("asc.inbox", { treeDataProvider: provider });
  const fail = (what: string) => (e: unknown) => {
    log(`asc inbox ${what}: ${String(e)}`);
    void vscode.window.showErrorMessage(`ASC: could not ${what} the inbox entry.`);
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
    vscode.commands.registerCommand("asc.inbox.checkout", (n?: Node) => {
      const i = item(n);
      if (i) checkout(i).catch(fail("check out"));
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
