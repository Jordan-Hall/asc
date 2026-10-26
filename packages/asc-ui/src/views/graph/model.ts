// Commit graph model (SPEC §24.4.5 "Graph rendering"): rows arrive in pages of 200 with the
// server's `column` and `edges`, so the client does no layout. This file holds the pure parts:
// the bounded page store (at most 5,000 rows), lane continuation for the canvas, the minimap
// projection, keyboard navigation and the cherry-pick/revert menu rules. No DOM here.

export const PAGE_SIZE = 200;
/** Rows kept in memory at once (SPEC §24.4.10). */
export const MAX_ROWS = 5000;
export const MAX_LANES = 24;

export type GraphPreset = "all" | "current" | "smart" | "stack";

export interface GraphEdge {
  readonly toColumn: number;
  /** Hex id of the parent this edge leads to. */
  readonly parent: string;
}

export interface GraphReview {
  readonly number: number;
  readonly state: string;
  readonly draft: boolean;
}

export interface GraphRow {
  readonly id: string;
  readonly subject: string;
  readonly author: string;
  /** Milliseconds since the epoch; 0 when unknown. */
  readonly time: number;
  readonly parents: readonly string[];
  readonly column: number;
  readonly edges: readonly GraphEdge[];
  readonly refs: readonly string[];
  /** The server capped the lanes at this row. */
  readonly overflow: boolean;
  readonly wip: boolean;
  readonly review?: GraphReview;
  /** Filled by the store: columns of lanes that pass through this row. */
  readonly pass?: readonly number[];
  /** Filled by the store: columns of lanes that end at this row's node. */
  readonly incoming?: readonly number[];
}

export interface RefMarker {
  readonly name: string;
  readonly row: number;
}

export interface GraphPage {
  readonly rows: readonly GraphRow[];
  /** Empty or absent on the last page. */
  readonly next?: string;
  /** First page only (API addition A2); absent when the server did not compute it. */
  readonly total?: number;
  readonly refMarkers?: readonly RefMarker[];
}

export interface GraphQuery {
  readonly preset: GraphPreset;
  readonly include: readonly string[];
  readonly exclude: readonly string[];
  readonly pageToken: string;
  readonly limit: number;
}

/** Split a comma or space separated ref list from a filter input. */
export function parseRefList(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

/**
 * Pages keyed by page index, bounded to `maxRows` rows. The page tokens form a chain (each
 * page's `next` opens the following one), so tokens are kept for every page reached; only rows
 * are evicted, farthest from the focus page first.
 */
export class PageStore {
  private readonly pages = new Map<number, readonly GraphRow[]>();
  /** tokens[i] opens page i; tokens[0] is the empty first-page token. */
  private tokens: string[] = [""];
  private lastPage: number | undefined;
  private totalRows: number | undefined;
  private markers: readonly RefMarker[] | undefined;
  private derived = new Map<number, readonly GraphRow[]>();
  private index = new Map<string, number>();
  private focus = 0;

  constructor(readonly maxRows: number = MAX_ROWS) {}

  reset(): void {
    this.pages.clear();
    this.derived.clear();
    this.index.clear();
    this.tokens = [""];
    this.lastPage = undefined;
    this.totalRows = undefined;
    this.markers = undefined;
    this.focus = 0;
  }

  get total(): number | undefined {
    return this.totalRows;
  }
  get refMarkers(): readonly RefMarker[] {
    return this.markers ?? [];
  }
  /** Rows currently held in memory. */
  get heldRows(): number {
    let n = 0;
    for (const p of this.pages.values()) n += p.length;
    return n;
  }
  get heldPages(): number[] {
    return [...this.pages.keys()].toSorted((a, b) => a - b);
  }
  /** The row count to size the scroller with: exact when known, else loaded pages plus one. */
  get rowCount(): number {
    if (this.totalRows !== undefined) return this.totalRows;
    let known = 0;
    for (const [p, rows] of this.pages) known = Math.max(known, p * PAGE_SIZE + rows.length);
    const maxPage = this.pages.size === 0 ? -1 : Math.max(...this.pages.keys());
    return this.lastPage !== undefined && maxPage >= this.lastPage ? known : known + PAGE_SIZE;
  }

  hasPage(p: number): boolean {
    return this.pages.has(p);
  }
  /** Token to fetch page `p`, or undefined while the chain has not reached it. */
  tokenFor(p: number): string | undefined {
    return this.tokens[p];
  }
  /** Highest page whose token is known. */
  get reachablePage(): number {
    return this.tokens.length - 1;
  }
  isBeyondEnd(p: number): boolean {
    return this.lastPage !== undefined && p > this.lastPage;
  }

  setFocus(row: number): void {
    this.focus = Math.max(0, Math.floor(row / PAGE_SIZE));
  }

  /** Add page `p` as fetched with `tokens[p]`. Returns the page indexes evicted. */
  add(p: number, page: GraphPage): number[] {
    this.pages.set(p, page.rows);
    if (page.next) this.tokens[p + 1] = page.next;
    else this.lastPage = p;
    if (p === 0) {
      if (page.total !== undefined) this.totalRows = page.total;
      if (page.refMarkers !== undefined) this.markers = page.refMarkers;
    }
    const evicted: number[] = [];
    while (this.heldRows > this.maxRows && this.pages.size > 1) {
      let far = -1;
      let dist = -1;
      for (const k of this.pages.keys()) {
        const d = Math.abs(k - this.focus);
        if (k !== p && d > dist) {
          far = k;
          dist = d;
        }
      }
      if (far < 0) break;
      this.pages.delete(far);
      evicted.push(far);
    }
    this.rebuild();
    return evicted;
  }

  /**
   * `Watch` handling: only the first page is refetched and spliced by commit id. When no row
   * was added and the chain is unchanged the later pages stay; otherwise row offsets moved, so
   * they are dropped and reloaded on demand.
   */
  spliceHead(fresh: GraphPage): "spliced" | "reset" {
    const old = this.pages.get(0);
    if (old === undefined) return "reset";
    const oldIds = new Set(old.map((r) => r.id));
    let added = 0;
    for (const r of fresh.rows) if (!oldIds.has(r.id)) added++;
    const chainKept = (fresh.next ?? "") === (this.tokens[1] ?? "");
    this.pages.set(0, fresh.rows);
    if (fresh.total !== undefined) this.totalRows = fresh.total;
    if (fresh.refMarkers !== undefined) this.markers = fresh.refMarkers;
    const same = added === 0 && chainKept && fresh.rows.length === old.length;
    if (!same) {
      for (const k of this.pages.keys()) if (k !== 0) this.pages.delete(k);
      this.tokens = fresh.next ? ["", fresh.next] : [""];
      this.lastPage = fresh.next ? undefined : 0;
    }
    this.rebuild();
    return same ? "spliced" : "reset";
  }

  rowAt(i: number): GraphRow | undefined {
    if (i < 0) return undefined;
    const p = Math.floor(i / PAGE_SIZE);
    return this.derived.get(p)?.[i - p * PAGE_SIZE];
  }

  indexOf(id: string): number | undefined {
    return this.index.get(id);
  }

  /** Held row count of page `p`. */
  pageLength(p: number): number {
    return this.pages.get(p)?.length ?? 0;
  }

  private rebuild(): void {
    this.derived.clear();
    this.index.clear();
    let prev = -2;
    let active: ReadonlyMap<number, string> = new Map();
    for (const p of this.heldPages) {
      if (p !== prev + 1) active = new Map();
      prev = p;
      const out: GraphRow[] = [];
      const rows = this.pages.get(p) ?? [];
      rows.forEach((row, k) => {
        const step = laneStep(active, row);
        active = step.next;
        out.push({ ...row, pass: step.pass, incoming: step.incoming });
        this.index.set(row.id, p * PAGE_SIZE + k);
      });
      this.derived.set(p, out);
    }
  }
}

/**
 * Lane continuation for one row. `active` maps a column to the commit id its lane waits for.
 * Lanes waiting for this row's id end at its node; the others pass through. The row's own
 * edges then open lanes toward its parents. This only continues the server's lanes so the
 * canvas can draw pass-through lines; it never assigns a column.
 */
export function laneStep(
  active: ReadonlyMap<number, string>,
  row: GraphRow,
): { next: Map<number, string>; pass: number[]; incoming: number[] } {
  const next = new Map(active);
  const incoming: number[] = [];
  const pass: number[] = [];
  for (const [col, waiting] of active) {
    if (waiting === row.id) {
      incoming.push(col);
      next.delete(col);
    } else {
      pass.push(col);
    }
  }
  for (const e of row.edges) next.set(e.toColumn, e.parent);
  return { next, pass, incoming };
}

/** Widest column among rows, clamped: the width of the lane canvas. */
export function laneCount(rows: Iterable<GraphRow | undefined>): number {
  let max = 0;
  for (const r of rows) {
    if (!r) continue;
    max = Math.max(max, r.column);
    for (const e of r.edges) max = Math.max(max, e.toColumn);
    if (r.pass) for (const c of r.pass) max = Math.max(max, c);
  }
  return Math.min(max + 1, MAX_LANES);
}

export interface VisibleRange {
  readonly first: number;
  /** Exclusive. */
  readonly end: number;
}

/** Virtual window: rows intersecting the viewport plus `overscan` rows on both sides. */
export function visibleRange(
  scrollTop: number,
  viewport: number,
  rowHeight: number,
  count: number,
  overscan = 20,
): VisibleRange {
  const first = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  const end = Math.min(count, Math.ceil((scrollTop + viewport) / rowHeight) + overscan);
  return { first, end: Math.max(first, end) };
}

export interface MinimapTick {
  readonly name: string;
  /** 0..1 position along the minimap. */
  readonly at: number;
}

export interface MinimapModel {
  /** Rows the minimap spans: the server total, else only what is loaded. */
  readonly span: number;
  readonly exact: boolean;
  readonly ticks: readonly MinimapTick[];
  /** Loaded ranges as 0..1 fractions. */
  readonly loaded: readonly { readonly from: number; readonly to: number }[];
}

/**
 * Minimap from the first page's `total` and `ref_markers` (A2). Without them the minimap spans
 * only the loaded pages and draws no ref ticks.
 */
export function minimapModel(store: PageStore): MinimapModel {
  const exact = store.total !== undefined;
  const pages = store.heldPages;
  let heldEnd = 0;
  for (const p of pages) heldEnd = Math.max(heldEnd, p * PAGE_SIZE + store.pageLength(p));
  const span = Math.max(1, exact ? (store.total as number) : heldEnd);
  const ticks: MinimapTick[] = [];
  if (exact) {
    for (const m of store.refMarkers)
      if (m.row >= 0 && m.row < span) ticks.push({ name: m.name, at: m.row / span });
  }
  const loaded = pages.map((p) => ({
    from: (p * PAGE_SIZE) / span,
    to: Math.min(1, (p * PAGE_SIZE + store.pageLength(p)) / span),
  }));
  return { span, exact, ticks, loaded };
}

/** Minimap click position (0..1) to a row index. */
export function minimapRow(fraction: number, span: number): number {
  return Math.max(0, Math.min(span - 1, Math.floor(fraction * span)));
}

export type NavKey =
  | "down"
  | "up"
  | "parent"
  | "child"
  | "nextFork"
  | "prevFork"
  | "nextRef"
  | "prevRef"
  | "first"
  | "last";

const isFork = (r: GraphRow): boolean => r.edges.length > 1 || r.parents.length > 1;
const hasRef = (r: GraphRow): boolean => r.refs.length > 0;

/**
 * Target row for a navigation key from `from`, searching only held rows. Returns `from` when
 * nothing matches. A scan that reaches an unloaded row stops there so the view loads it.
 * "child" is the nearest newer row that lists this commit as a parent.
 */
export function navigate(store: PageStore, from: number, key: NavKey, count: number): number {
  const scan = (dir: 1 | -1, pred: (r: GraphRow) => boolean): number => {
    for (let i = from + dir; i >= 0 && i < count; i += dir) {
      const r = store.rowAt(i);
      if (r === undefined || pred(r)) return i;
    }
    return from;
  };
  switch (key) {
    case "down":
      return Math.min(count - 1, from + 1);
    case "up":
      return Math.max(0, from - 1);
    case "first":
      return 0;
    case "last":
      return Math.max(0, count - 1);
    case "parent": {
      const id = store.rowAt(from)?.parents[0];
      const idx = id === undefined ? undefined : store.indexOf(id);
      return idx ?? from;
    }
    case "child": {
      const r = store.rowAt(from);
      if (!r) return from;
      for (let i = from - 1; i >= 0; i--) {
        const c = store.rowAt(i);
        if (c === undefined) return from;
        if (c.parents.includes(r.id)) return i;
      }
      return from;
    }
    case "nextFork":
      return scan(1, isFork);
    case "prevFork":
      return scan(-1, isFork);
    case "nextRef":
      return scan(1, hasRef);
    case "prevRef":
      return scan(-1, hasRef);
  }
}

/** Next held row after `from` (wrapping once) that matches the search text. */
export function searchRow(
  store: PageStore,
  from: number,
  text: string,
  dir: 1 | -1 = 1,
): number | undefined {
  const q = text.trim().toLowerCase();
  if (q === "") return undefined;
  const count = store.rowCount;
  for (let step = 1; step <= count; step++) {
    const i = (((from + dir * step) % count) + count) % count;
    const r = store.rowAt(i);
    if (r === undefined) continue;
    if (
      r.subject.toLowerCase().includes(q) ||
      r.author.toLowerCase().includes(q) ||
      r.id.startsWith(q) ||
      r.refs.some((x) => x.toLowerCase().includes(q))
    ) {
      return i;
    }
  }
  return undefined;
}

export type MenuId = "cherry-pick" | "revert" | "copy-id";

export interface MenuItem {
  readonly id: MenuId;
  readonly label: string;
  readonly enabled: boolean;
  /** Set exactly when `enabled` is false. */
  readonly reason?: string;
}

export const REASON_WIP = "Uncommitted changes row: nothing to pick";
export const REASON_MERGE = "Merge commit: needs a mainline, run it in the terminal with -m";

export function menuItems(row: GraphRow): MenuItem[] {
  const reason = row.wip ? REASON_WIP : row.parents.length > 1 ? REASON_MERGE : undefined;
  const item = (id: MenuId, label: string): MenuItem =>
    reason === undefined ? { id, label, enabled: true } : { id, label, enabled: false, reason };
  return [
    item("cherry-pick", "Cherry-pick"),
    item("revert", "Revert"),
    row.wip
      ? { id: "copy-id", label: "Copy id", enabled: false, reason: REASON_WIP }
      : { id: "copy-id", label: "Copy id", enabled: true },
  ];
}

export interface GraphVerbCall {
  readonly verb: "cherry-pick" | "revert";
  /** A full hex id from a loaded row, never free text. */
  readonly rev: string;
}

export function menuCall(id: "cherry-pick" | "revert", row: GraphRow): GraphVerbCall {
  return { verb: id, rev: row.id };
}

export function shortId(id: string): string {
  return id.slice(0, 8);
}
