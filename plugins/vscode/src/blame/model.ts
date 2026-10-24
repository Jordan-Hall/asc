// Editor-free blame logic (SPEC 24.4.3): the per-root LRU, line lookup, inline text, hover markdown,
// age quantiles for the heatmap, skip rules and CodeLens eligibility. `register.ts` is the only
// `vscode` user. Nothing here makes an RPC except through `BlameApi`, and `provide`-time code
// (`peek`) reads the cache only.

export const MAX_BLAME_LINES_LENS = 5000;
export const MAX_BLAME_BYTES = 2 * 1024 * 1024;
export const LRU_ENTRIES = 100;
export const INLINE_DEBOUNCE_MS = 150;
export const DIRTY_THROTTLE_MS = 500;

export interface BlameHunk {
  /** Lowercase hex commit id; "" for lines not yet committed (a dirty buffer). */
  origin: string;
  origPath: string;
  origStart: number;
  /** 0-based first line and line count. */
  start: number;
  len: number;
}

export interface BlameCommit {
  id: string;
  subject: string;
  message: string;
  author: string;
  /** Unix seconds. */
  time: number;
  /** k-z change id, or "". */
  changeId: string;
  gen: number;
}

export interface BlameReview {
  forge: string;
  number: number;
  url: string;
  state: string;
}

export interface BlameData {
  hunks: BlameHunk[];
  commits: Map<string, BlameCommit>;
  reviews: BlameReview[];
}

export interface Autolink {
  prefix: string;
  url: string;
}

/** The bridge calls blame needs; tests supply a fake. */
export interface BlameApi {
  /** `buffer` null blames HEAD; bytes blame the working copy with that content. */
  blame(
    root: string,
    path: string,
    buffer: Uint8Array | null,
    signal?: AbortSignal,
  ): Promise<BlameData>;
  /** Autolink rules from `RepoConfig`; may be empty. */
  autolinks(root: string): Promise<Autolink[]>;
}

export class Lru<V> {
  private readonly m = new Map<string, V>();
  constructor(private readonly cap: number) {}
  get(k: string): V | undefined {
    const v = this.m.get(k);
    if (v === undefined) return undefined;
    this.m.delete(k);
    this.m.set(k, v);
    return v;
  }
  set(k: string, v: V): void {
    this.m.delete(k);
    this.m.set(k, v);
    while (this.m.size > this.cap) {
      const oldest = this.m.keys().next();
      if (oldest.done) break;
      this.m.delete(oldest.value);
    }
  }
  clear(): void {
    this.m.clear();
  }
  get size(): number {
    return this.m.size;
  }
}

/** Skip rules: `asc:` and untitled documents, files outside roots, over 2 MiB, binary content. */
export function skipReason(i: {
  scheme: string;
  insideRoot: boolean;
  byteLength: number;
  head?: Uint8Array;
}): string | null {
  if (i.scheme !== "file") return "not a file document";
  if (!i.insideRoot) return "outside an asc root";
  if (i.byteLength > MAX_BLAME_BYTES) return "file over 2 MiB";
  if (i.head?.includes(0)) return "binary file";
  return null;
}

/** The hunk covering 0-based `line`, by binary search over hunks sorted by `start`. */
export function hunkAt(hunks: readonly BlameHunk[], line: number): BlameHunk | undefined {
  let lo = 0;
  let hi = hunks.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const h = hunks[mid];
    if (!h) return undefined;
    if (line < h.start) hi = mid - 1;
    else if (line >= h.start + h.len) lo = mid + 1;
    else return h;
  }
  return undefined;
}

export function relativeTime(thenSec: number, nowSec: number): string {
  const d = Math.max(0, nowSec - thenSec);
  const units: [string, number][] = [
    ["year", 31_536_000],
    ["month", 2_592_000],
    ["week", 604_800],
    ["day", 86_400],
    ["hour", 3600],
    ["minute", 60],
  ];
  for (const [name, secs] of units) {
    if (d >= secs) {
      const n = Math.floor(d / secs);
      return `${n} ${name}${n === 1 ? "" : "s"} ago`;
    }
  }
  return "just now";
}

/** Text after the line, or null when the line has no hunk. */
export function inlineText(data: BlameData, line: number, nowSec: number): string | null {
  const h = hunkAt(data.hunks, line);
  if (!h) return null;
  const c = h.origin === "" ? undefined : data.commits.get(h.origin);
  if (!c) return "You, not committed yet";
  return `${c.author}, ${relativeTime(c.time, nowSec)} • ${c.subject}`;
}

/**
 * Age quantile bucket 0 (oldest) .. buckets-1 (newest) per hunk, over the commit times present.
 * Uncommitted lines are the newest bucket.
 */
export function ageBuckets(data: BlameData, buckets = 5): Map<BlameHunk, number> {
  const times = [...new Set([...data.commits.values()].map((c) => c.time))].toSorted((a, b) => a - b);
  const out = new Map<BlameHunk, number>();
  for (const h of data.hunks) {
    const c = data.commits.get(h.origin);
    if (!c || times.length === 0) {
      out.set(h, buckets - 1);
      continue;
    }
    const rank = times.indexOf(c.time);
    out.set(h, times.length === 1 ? buckets - 1 : Math.floor((rank * buckets) / times.length));
  }
  return out;
}

export function subjectOf(message: string): string {
  const i = message.indexOf("\n");
  return (i < 0 ? message : message.slice(0, i)).trim();
}

/** Escapes text for a Markdown hover so commit messages cannot inject links or formatting. */
export function escapeMd(s: string): string {
  return s.replace(/[\\`*_{}[\]()#+\-.!|<>~&]/g, "\\$&").replace(/\r?\n/g, "  \n");
}

const SAFE_URL = /^https:\/\/[^\s)<>]+$/;

/** Autolinked issue references as Markdown. Only `https:` targets are linked (deny by default). */
export function autolinkMd(text: string, rules: readonly Autolink[]): string {
  let out = escapeMd(text);
  for (const r of rules) {
    if (r.prefix === "" || !r.url.includes("<num>")) continue;
    const prefix = escapeMd(r.prefix).replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const re = new RegExp(`${prefix}(\\d+)`, "g");
    out = out.replace(re, (m, n: string) => {
      const url = r.url.replace("<num>", n);
      return SAFE_URL.test(url) ? `[${m}](${url})` : m;
    });
  }
  return out;
}

export interface HoverAction {
  label: string;
  command: string;
  args: unknown[];
}

export interface HoverContent {
  markdown: string;
  actions: HoverAction[];
}

/** Hover for one line: commit, change, PR, autolinks and actions. Command args are plain data. */
export function hoverFor(
  data: BlameData,
  line: number,
  rules: readonly Autolink[],
  nowSec: number,
  ctx: { root: string; path: string },
): HoverContent | null {
  const h = hunkAt(data.hunks, line);
  if (!h) return null;
  const c = h.origin === "" ? undefined : data.commits.get(h.origin);
  if (!c) return { markdown: "**Not committed yet**", actions: [] };
  const lines = [
    `**${autolinkMd(c.subject, rules)}**`,
    "",
    `${escapeMd(c.author)}, ${relativeTime(c.time, nowSec)}`,
  ];
  const nl = c.message.indexOf("\n");
  const body = nl < 0 ? "" : c.message.slice(nl + 1).trim();
  if (body !== "") lines.push("", autolinkMd(body, rules));
  lines.push("", `Commit \`${c.id.slice(0, 12)}\``);
  if (c.changeId !== "") lines.push(`Change \`${c.changeId.slice(0, 12)}\` (gen ${c.gen})`);
  const actions: HoverAction[] = [
    { label: "Open changes", command: "asc.blame.openChanges", args: [ctx.root, ctx.path, c.id] },
    {
      label: "Blame previous revision",
      command: "asc.blame.previous",
      args: [ctx.root, ctx.path, c.id],
    },
  ];
  for (const r of data.reviews) {
    lines.push("", `PR #${r.number} (${escapeMd(r.state)})`);
    if (SAFE_URL.test(r.url)) {
      actions.push({ label: "Open PR", command: "asc.blame.openPr", args: [r.url] });
    }
  }
  if (c.changeId !== "") {
    actions.push({ label: "Copy change id", command: "asc.blame.copyChangeId", args: [c.changeId] });
  }
  return { markdown: lines.join("\n"), actions };
}

/** CodeLens is off for files over 5,000 lines. */
export function lensesEnabled(lineCount: number): boolean {
  return lineCount <= MAX_BLAME_LINES_LENS;
}

/** Lens title from the cached blame covering `[startLine, endLine]`: the most recent commit. */
export function lensTitle(
  data: BlameData,
  startLine: number,
  endLine: number,
  nowSec: number,
): string | null {
  let best: BlameCommit | undefined;
  const authors = new Set<string>();
  for (const h of data.hunks) {
    if (h.start > endLine || h.start + h.len <= startLine) continue;
    const c = data.commits.get(h.origin);
    if (!c) continue;
    authors.add(c.author);
    if (!best || c.time > best.time) best = c;
  }
  if (!best) return null;
  const more = authors.size > 1 ? ` +${authors.size - 1}` : "";
  return `${best.author}${more}, ${relativeTime(best.time, nowSec)}`;
}

export interface BlameServiceOptions {
  api: BlameApi;
  now?: () => number;
}

/** Per-root cache and in-flight de-duplication. Keyed by (path, head epoch, document version). */
export class BlameService {
  private readonly roots = new Map<string, { lru: Lru<BlameData>; epoch: number }>();
  private readonly inflight = new Map<string, Promise<BlameData>>();
  private readonly autolinkCache = new Map<string, Autolink[]>();
  constructor(private readonly o: BlameServiceOptions) {}

  private state(root: string) {
    let s = this.roots.get(root);
    if (!s) {
      s = { lru: new Lru<BlameData>(LRU_ENTRIES), epoch: 0 };
      this.roots.set(root, s);
    }
    return s;
  }

  private key(root: string, path: string, version: number | null): string {
    return `${path}\0${this.state(root).epoch}\0${version ?? "head"}`;
  }

  /** Cached blame, or undefined. Never calls the API: safe inside `provideCodeLenses`. */
  peek(root: string, path: string, version: number | null): BlameData | undefined {
    return this.state(root).lru.get(this.key(root, path, version));
  }

  /** Loads (or reuses) blame. `buffer` non-null blames the working copy at `version`. */
  load(
    root: string,
    path: string,
    version: number,
    buffer: Uint8Array | null,
    signal?: AbortSignal,
  ): Promise<BlameData> {
    const v = buffer === null ? null : version;
    const k = `${root}\0${this.key(root, path, v)}`;
    const hit = this.peek(root, path, v);
    if (hit) return Promise.resolve(hit);
    const pending = this.inflight.get(k);
    if (pending) return pending;
    const epoch = this.state(root).epoch;
    const p = this.o.api
      .blame(root, path, buffer, signal)
      .then((d) => {
        // A HEAD move while the request ran makes the answer stale: serve it, do not cache it.
        if (this.state(root).epoch === epoch) this.state(root).lru.set(this.key(root, path, v), d);
        return d;
      })
      .finally(() => this.inflight.delete(k));
    this.inflight.set(k, p);
    return p;
  }

  /** `KIND_HEAD` or a save: drop every entry of the root. */
  invalidate(root: string): void {
    const s = this.state(root);
    s.epoch++;
    s.lru.clear();
    this.autolinkCache.delete(root);
  }

  async autolinks(root: string): Promise<Autolink[]> {
    const c = this.autolinkCache.get(root);
    if (c) return c;
    const r = await this.o.api.autolinks(root).catch(() => [] as Autolink[]);
    this.autolinkCache.set(root, r);
    return r;
  }

  nowSec(): number {
    return Math.floor((this.o.now ? this.o.now() : Date.now()) / 1000);
  }
}

/** A trailing-edge debounce whose timer is injectable for tests. */
export function debounce<A extends unknown[]>(
  ms: number,
  fn: (...a: A) => void,
  timers: {
    set: (f: () => void, ms: number) => unknown;
    clear: (h: unknown) => void;
  } = { set: setTimeout, clear: (h) => clearTimeout(h as ReturnType<typeof setTimeout>) },
): ((...a: A) => void) & { cancel(): void } {
  let h: unknown;
  const d = (...a: A) => {
    if (h !== undefined) timers.clear(h);
    h = timers.set(() => {
      h = undefined;
      fn(...a);
    }, ms);
  };
  d.cancel = () => {
    if (h !== undefined) timers.clear(h);
    h = undefined;
  };
  return d;
}
