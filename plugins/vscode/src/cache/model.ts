// The build cache in the editor (docs/designs/build-cache.md §6): the status item, the tree of
// entries and the admin verbs, over `asc cache ... --json` run directly (`cli-direct`). Editor-free;
// `register.ts` is the only `vscode` user. Everything a verb prints is untrusted: each JSON reply is
// read defensively, numbers are clamped and text is clipped. The model refreshes on events only
// (a task ended, the view shown, the window focused, an admin verb ran); there is no timer.
import type { CacheSetupTool, CliCall, CliDirect } from "../cli-direct";

/** Entries the tree lists per tool. */
export const LS_LIMIT = 200;
const MAX_TEXT = 500;
const clip = (s: string): string => (s.length > MAX_TEXT ? `${s.slice(0, MAX_TEXT)}...` : s);

const count = (v: unknown): number =>
  typeof v === "number" && Number.isFinite(v) && v > 0 ? Math.trunc(v) : 0;
const rate = (v: unknown): number =>
  typeof v === "number" && Number.isFinite(v) ? Math.min(Math.max(v, 0), 1) : 0;
const text = (v: unknown): string => (typeof v === "string" ? clip(v) : "");

function object(t: string | null): Record<string, unknown> | null {
  if (t === null || t === "" || t.length > 4 * 1024 * 1024) return null;
  let v: unknown;
  try {
    v = JSON.parse(t);
  } catch {
    return null;
  }
  return typeof v === "object" && v !== null && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : null;
}

export interface ToolStats {
  hits: number;
  misses: number;
  puts: number;
  restored: number;
  /** 0 to 1. */
  hitRate: number;
}

export interface CacheStatus {
  root: string;
  bytes: number;
  maxSize: number;
  entries: number;
  blobs: number;
  /** By tool name, sorted. */
  tools: { name: string; day: ToolStats; week: ToolStats }[];
  remote: {
    mode: "off" | "read" | "read-write";
    queued: number;
    /** The last exchange with the server tier: Unix seconds, and its error or null. */
    last: { at: number; error: string | null } | null;
  };
}

function stats(v: unknown): ToolStats {
  const o = (typeof v === "object" && v !== null ? v : {}) as Record<string, unknown>;
  return {
    hits: count(o.hits),
    misses: count(o.misses),
    puts: count(o.puts),
    restored: count(o.restored),
    hitRate: rate(o.hit_rate),
  };
}

/** `asc cache status --json`, or null when it is not that object. */
export function parseStatus(t: string | null): CacheStatus | null {
  const o = object(t);
  if (o === null || typeof o.root !== "string") return null;
  const tools: CacheStatus["tools"] = [];
  if (typeof o.tools === "object" && o.tools !== null && !Array.isArray(o.tools)) {
    for (const [name, v] of Object.entries(o.tools as Record<string, unknown>)) {
      if (!/^[a-z0-9][a-z0-9-]{0,31}$/.test(name) || typeof v !== "object" || v === null) continue;
      const s = v as Record<string, unknown>;
      tools.push({ name, day: stats(s.day), week: stats(s.week) });
    }
  }

  const r = (typeof o.remote === "object" && o.remote !== null ? o.remote : {}) as Record<
    string,
    unknown
  >;
  const mode = r.mode === "read" || r.mode === "read-write" ? r.mode : "off";
  let last: CacheStatus["remote"]["last"] = null;
  if (typeof r.last === "object" && r.last !== null) {
    const l = r.last as Record<string, unknown>;
    last = { at: count(l.at), error: typeof l.error === "string" ? clip(l.error) : null };
  }
  return {
    root: clip(o.root),
    bytes: count(o.bytes),
    maxSize: count(o.max_size),
    entries: count(o.entries),
    blobs: count(o.blobs),
    tools: tools.toSorted((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0)),
    remote: { mode, queued: count(r.queued), last },
  };
}

export interface CacheEntry {
  key: string;
  tool: string;
  label: string;
  size: number;
  outputs: number;
  exit: number;
  created: number;
  lastUse: number;
  worktree: string;
  workflow: string;
}

const KEY = /^[0-9a-f]{4,64}$/;

function entry(v: unknown): CacheEntry | null {
  if (typeof v !== "object" || v === null) return null;
  const e = v as Record<string, unknown>;
  if (typeof e.key !== "string" || !KEY.test(e.key)) return null;
  return {
    key: e.key,
    tool: text(e.tool),
    label: text(e.label),
    size: count(e.size),
    outputs: Array.isArray(e.outputs) ? e.outputs.length : count(e.outputs),
    exit: typeof e.exit === "number" && Number.isInteger(e.exit) ? e.exit : 0,
    created: count(e.created),
    lastUse: count(e.last_use),
    worktree: text(e.worktree),
    workflow: text(e.workflow),
  };
}

/** `asc cache ls --json`: an array of entries; rows without a hex key are dropped. */
export function parseEntries(t: string | null): CacheEntry[] {
  if (t === null || t === "" || t.length > 4 * 1024 * 1024) return [];
  let v: unknown;
  try {
    v = JSON.parse(t);
  } catch {
    return [];
  }
  if (!Array.isArray(v)) return [];
  const out: CacheEntry[] = [];
  for (const raw of v) {
    const e = entry(raw);
    if (e !== null) out.push(e);
  }
  return out;
}

/** `asc cache env --shell json`: the variables a tool reads to reach the local fronts. */
export function parseEnv(t: string | null): Record<string, string> {
  const o = object(t);
  const out: Record<string, string> = {};
  if (o === null) return out;
  for (const [k, v] of Object.entries(o)) {
    if (/^[A-Z][A-Z0-9_]{0,63}$/.test(k) && typeof v === "string" && v.length <= 4096) out[k] = v;
  }
  return out;
}

/** `asc cache gc --json` and `asc cache clear --json`. */
export function parseReport(t: string | null): { entries: number; freed: number } | null {
  const o = object(t);
  return o === null ? null : { entries: count(o.entries), freed: count(o.freed) };
}

/** `asc cache warm --json`. */
export function parseWarm(
  t: string | null,
): { fetched: number; local: number; bytes: number } | null {
  const o = object(t);
  return o === null
    ? null
    : { fetched: count(o.fetched), local: count(o.local), bytes: count(o.bytes) };
}

export function humanSize(n: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return i === 0 ? `${v} B` : `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

export function ago(at: number, now: number): string {
  const s = Math.max(0, Math.trunc(now - at));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.trunc(s / 60)}m ago`;
  if (s < 86_400) return `${Math.trunc(s / 3600)}h ago`;
  return `${Math.trunc(s / 86_400)}d ago`;
}

const percent = (r: number): string => `${Math.round(r * 100)}%`;

export interface StatusView {
  visible: boolean;
  text: string;
  tooltip: string;
  /** The server tier failed on its last exchange. */
  warning: boolean;
}

/** The hits and misses one build task had. */
export interface LastBuild {
  label: string;
  hits: number;
  misses: number;
}

/** The status item: the last build's hits and misses (else today's hit rate over every tool), and
 *  the detail in the tooltip. */
export function statusView(
  s: CacheStatus | null,
  now: number,
  last: LastBuild | null = null,
): StatusView {
  if (s === null) return { visible: false, text: "", tooltip: "", warning: false };
  const hits = s.tools.reduce((n, t) => n + t.day.hits, 0);
  const misses = s.tools.reduce((n, t) => n + t.day.misses, 0);
  const warning = s.remote.mode !== "off" && s.remote.last !== null && s.remote.last.error !== null;
  const lines = [
    `Build cache: ${humanSize(s.bytes)} of ${humanSize(s.maxSize)}, ${s.entries} entries`,
  ];
  for (const t of s.tools) {
    lines.push(
      `${t.name}: ${t.day.hits} hits, ${t.day.misses} misses today; ${percent(t.week.hitRate)} this week`,
    );
  }
  const r = s.remote;
  let server = `Server: ${r.mode}`;
  if (r.mode !== "off") {
    server += `, ${r.queued} queued`;
    if (r.last !== null) {
      server +=
        r.last.error === null
          ? `, reached ${ago(r.last.at, now)}`
          : `, failed ${ago(r.last.at, now)}: ${r.last.error}`;
    }
  }
  lines.push(server);
  if (last !== null)
    lines.push(`Last build: ${last.label}, ${last.hits} hits, ${last.misses} misses`);
  const rateText =
    last !== null
      ? `${last.hits} hits, ${last.misses} misses`
      : hits + misses === 0
        ? "no builds today"
        : `${percent(hits / (hits + misses))} hits today`;
  return {
    visible: true,
    text: `${warning ? "$(warning)" : "$(database)"} ${rateText}`,
    tooltip: lines.join("\n"),
    warning,
  };
}

/** The difference in hits and misses of every tool between two readings of the status. */
export function delta(
  before: CacheStatus | null,
  after: CacheStatus | null,
): { hits: number; misses: number } {
  if (before === null || after === null) return { hits: 0, misses: 0 };
  const sum = (s: CacheStatus, f: "hits" | "misses") => s.tools.reduce((n, t) => n + t.day[f], 0);
  return {
    hits: Math.max(0, sum(after, "hits") - sum(before, "hits")),
    misses: Math.max(0, sum(after, "misses") - sum(before, "misses")),
  };
}

export interface CacheApi {
  status(root: string, signal?: AbortSignal): Promise<CacheStatus | null>;
  list(root: string, tool: string): Promise<CacheEntry[]>;
  /** The pretty JSON of `cache show`, for a read-only document. */
  show(root: string, key: string): Promise<string>;
  remove(root: string, keys: readonly string[]): Promise<string[]>;
  gc(root: string): Promise<{ entries: number; freed: number }>;
  clear(root: string, tool?: string): Promise<{ entries: number; freed: number }>;
  warm(root: string): Promise<{ fetched: number; local: number; bytes: number }>;
  setup(root: string, tool: CacheSetupTool, undo: boolean): Promise<string>;
  env(root: string): Promise<Record<string, string>>;
}

/** The error of a verb that failed: its last stderr line, clipped. */
function failure(verb: string, r: { exit: number; stderr: string[] }): Error {
  const last = r.stderr.at(-1)?.replace(/^asc:\s*/, "") ?? `exited ${r.exit}`;
  return new Error(`asc cache ${verb}: ${clip(last)}`);
}

function report(t: string, verb: string): { entries: number; freed: number } {
  const r = parseReport(t);
  if (r === null) throw new Error(`asc cache ${verb}: unexpected reply`);
  return r;
}

export function cliCacheApi(cli: CliDirect): CacheApi {
  const run = async (root: string, call: CliCall, verb: string, signal?: AbortSignal) => {
    const r = await cli.run(root, call, signal);
    if (r.exit !== 0) throw failure(verb, r);
    return r.stdout;
  };
  return {
    async status(root, signal) {
      return parseStatus(await run(root, { kind: "cache-status" }, "status", signal));
    },
    async list(root, tool) {
      return parseEntries(await run(root, { kind: "cache-ls", tool, limit: LS_LIMIT }, "ls"));
    },
    async show(root, key) {
      const o = object(await run(root, { kind: "cache-show", key }, "show"));
      if (o === null) throw new Error("asc cache show: unexpected reply");
      return `${JSON.stringify(o, null, 2)}\n`;
    },
    async remove(root, keys) {
      const o = object(await run(root, { kind: "cache-rm", keys }, "rm"));
      const removed = Array.isArray(o?.removed) ? o.removed : [];
      return removed.filter((k): k is string => typeof k === "string" && KEY.test(k));
    },
    async gc(root) {
      return report(await run(root, { kind: "cache-gc" }, "gc"), "gc");
    },
    async clear(root, tool) {
      const call: CliCall =
        tool === undefined ? { kind: "cache-clear" } : { kind: "cache-clear", tool };
      return report(await run(root, call, "clear"), "clear");
    },
    async warm(root) {
      const w = parseWarm(await run(root, { kind: "cache-warm" }, "warm"));
      if (w === null) throw new Error("asc cache warm: unexpected reply");
      return w;
    },
    async setup(root, tool, undo) {
      return clip((await run(root, { kind: "cache-setup", tool, undo }, "setup")).trim());
    },
    async env(root) {
      return parseEnv(await run(root, { kind: "cache-env" }, "env"));
    },
  };
}

export interface CacheModelOptions {
  api: CacheApi;
  root: string;
}

/** The last status and the entries of each tool the tree opened. A failed read clears the status,
 *  which hides the item: the cache is optional, so a background failure is not reported. */
export class CacheModel {
  status: CacheStatus | null = null;
  private readonly lists = new Map<string, CacheEntry[]>();
  private seq = 0;
  private abort: AbortController | null = null;
  private disposed = false;
  private readonly listeners = new Set<() => void>();
  constructor(readonly o: CacheModelOptions) {}

  get root(): string {
    return this.o.root;
  }

  onChange(l: () => void): { dispose(): void } {
    this.listeners.add(l);
    return { dispose: () => void this.listeners.delete(l) };
  }

  /** Re-reads the status and forgets the listed entries; resolves the new status. */
  async refresh(): Promise<CacheStatus | null> {
    if (this.disposed) return null;
    this.abort?.abort();
    const ac = new AbortController();
    this.abort = ac;
    const mine = ++this.seq;
    let s: CacheStatus | null;
    try {
      s = await this.o.api.status(this.o.root, ac.signal);
    } catch {
      s = null;
    }
    if (this.disposed || mine !== this.seq) return this.status;
    this.status = s;
    this.lists.clear();
    this.emit();
    return s;
  }

  /** The entries of `tool`, read once per refresh; [] when the read fails. */
  async entries(tool: string): Promise<CacheEntry[]> {
    const have = this.lists.get(tool);
    if (have !== undefined) return have;
    let got: CacheEntry[];
    try {
      got = await this.o.api.list(this.o.root, tool);
    } catch {
      got = [];
    }
    if (!this.disposed) this.lists.set(tool, got);
    return got;
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
