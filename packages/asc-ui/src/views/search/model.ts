// Search view model (SPEC §24.3.5, §24.4.7). Pure: modes, scope picker, feature gating,
// index coverage and result flattening. Result text is untrusted file content; the DOM view
// only ever sets it through `textContent`.

export type SearchMode = "exact" | "regex" | "symbol" | "semantic";
export type ScopeKind = "workspace" | "revision" | "change" | "stack";

export const MODES: readonly SearchMode[] = ["exact", "regex", "symbol", "semantic"];
export const SCOPES: readonly ScopeKind[] = ["workspace", "revision", "change", "stack"];

/** Panel cap on rendered hits; the server's `truncated` flag is shown separately. */
export const MAX_HITS = 500;

export const REASON_NO_SEARCH_V1 =
  "Needs an asc server with code intelligence (search.v1 is not advertised)";
export const REASON_EMPTY = "Enter a search term";
export const REASON_SCOPE_VALUE = "Enter a revision, change or stack id";

export interface Features {
  /** `Hello.features` contains `search.v1`. Absent for every local-only workspace. */
  readonly searchV1: boolean;
}

export interface ScopeChoice {
  readonly kind: ScopeKind;
  /** Revision spec, change id or stack-top change id; unused for workspace. */
  readonly value: string;
}

export interface SearchQuery {
  readonly mode: SearchMode;
  readonly text: string;
  readonly scope: ScopeChoice;
  readonly caseSensitive: boolean;
}

export interface Availability {
  readonly enabled: boolean;
  readonly reason?: string;
}

export function modeAvailability(mode: SearchMode, f: Features): Availability {
  if (mode === "exact" || f.searchV1) return { enabled: true };
  return { enabled: false, reason: REASON_NO_SEARCH_V1 };
}

/** Without search.v1 only `Repo.Grep` is available, which reads a revision or the workspace. */
export function scopeAvailability(scope: ScopeKind, f: Features): Availability {
  if (f.searchV1 || scope === "workspace" || scope === "revision") return { enabled: true };
  return { enabled: false, reason: REASON_NO_SEARCH_V1 };
}

export const scopeNeedsValue = (k: ScopeKind): boolean => k !== "workspace";

/** Reason the query cannot run, or undefined when it can. */
export function queryBlockedReason(q: SearchQuery, f: Features): string | undefined {
  const m = modeAvailability(q.mode, f);
  if (!m.enabled) return m.reason;
  const s = scopeAvailability(q.scope.kind, f);
  if (!s.enabled) return s.reason;
  if (q.text.trim() === "") return REASON_EMPTY;
  if (scopeNeedsValue(q.scope.kind) && q.scope.value.trim() === "") return REASON_SCOPE_VALUE;
  return undefined;
}

/** Which backend serves the query. */
export type Engine = "codeintel" | "grep";
export const engineFor = (q: SearchQuery, f: Features): Engine =>
  f.searchV1 || q.mode !== "exact" ? "codeintel" : "grep";

export interface IndexCoverage {
  readonly index: string;
  /** 0..1 */
  readonly coverage: number;
  readonly lagSeconds: number;
}

export interface CoverageSummary {
  readonly text: string;
  readonly partial: boolean;
}

export function coverageSummary(
  indexes: readonly IndexCoverage[] | undefined,
  f: Features,
): CoverageSummary {
  if (!f.searchV1) return { text: "No index: exact search reads files directly", partial: false };
  if (indexes === undefined) return { text: "Index status unknown", partial: false };
  if (indexes.length === 0) return { text: "No index for this scope", partial: true };
  const parts = indexes.map((i) => {
    const pct = Math.floor(Math.min(1, Math.max(0, i.coverage)) * 100);
    const lag = i.lagSeconds > 0 ? `, ${i.lagSeconds}s behind` : "";
    return `${i.index} ${pct}%${lag}`;
  });
  return { text: `Index: ${parts.join(" · ")}`, partial: indexes.some((i) => i.coverage < 1) };
}

/** One rendered result row, whatever engine produced it. */
export interface ResultItem {
  readonly path: string;
  readonly line: number;
  readonly text: string;
  /** 0-based match start columns within `text` (exact and regex only). */
  readonly cols: readonly number[];
  /** Match length for highlighting; 0 when unknown. */
  readonly length: number;
  /** Replaces the highlighted text (symbol and semantic rows). */
  readonly label?: string;
  readonly redacted: boolean;
}

export interface FileHitLike {
  readonly path: string;
  readonly redacted?: boolean;
  readonly matches: readonly { line: number; cols?: readonly number[]; text: string }[];
}

export function flattenFileHits(files: readonly FileHitLike[], matchLength: number): ResultItem[] {
  const out: ResultItem[] = [];
  for (const f of files) {
    for (const m of f.matches) {
      out.push({
        path: f.path,
        line: m.line,
        text: f.redacted ? "" : m.text,
        cols: f.redacted ? [] : (m.cols ?? []),
        length: matchLength,
        redacted: f.redacted === true,
      });
    }
  }
  return out;
}

/** Split `text` into plain and highlighted runs; offsets are clamped, overlaps merged. */
export function highlightRuns(
  text: string,
  cols: readonly number[],
  length: number,
): { text: string; hit: boolean }[] {
  if (length <= 0 || cols.length === 0) return [{ text, hit: false }];
  const runs: { text: string; hit: boolean }[] = [];
  let pos = 0;
  for (const c of cols.toSorted((a, b) => a - b)) {
    const start = Math.max(pos, Math.min(c, text.length));
    const end = Math.min(text.length, c + length);
    if (end <= start) continue;
    if (start > pos) runs.push({ text: text.slice(pos, start), hit: false });
    runs.push({ text: text.slice(start, end), hit: true });
    pos = end;
  }
  if (pos < text.length) runs.push({ text: text.slice(pos), hit: false });
  return runs.length === 0 ? [{ text, hit: false }] : runs;
}

/** Appends a batch, dropping anything past the cap; reports whether the cap was hit. */
export function appendCapped(
  items: readonly ResultItem[],
  batch: readonly ResultItem[],
  cap = MAX_HITS,
): { items: ResultItem[]; capped: boolean } {
  const room = Math.max(0, cap - items.length);
  return { items: [...items, ...batch.slice(0, room)], capped: batch.length > room };
}

export const resultCountText = (n: number, truncated: boolean): string =>
  `${n} ${n === 1 ? "result" : "results"}${truncated ? " (more available, narrow the search)" : ""}`;
