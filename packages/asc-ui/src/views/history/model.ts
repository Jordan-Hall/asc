// Visual file history model (SPEC §24.3.3): `Workspace.Log` with `paths`, `follow` and
// `with_stats` (`diffstat.v1`). Pure functions only; the DOM lives in view.ts.

export interface FileStatLite {
  readonly path: string;
  readonly added: number;
  readonly removed: number;
  readonly binary: boolean;
}

export interface HistoryEntry {
  readonly id: string;
  readonly subject: string;
  readonly author: string;
  /** Unix seconds. */
  readonly time: number;
  readonly change?: string;
  /** `diffstat.v1` rows of the entry; absent when the server lacks the feature. */
  readonly stats?: readonly FileStatLite[];
}

export interface HistoryNode {
  readonly entry: HistoryEntry;
  readonly lane: number;
  /** Path of the file at this revision (differs across a rename). */
  readonly path: string;
  readonly renamedFrom?: string;
  readonly churn: number;
  readonly size: number;
  readonly binary: boolean;
}

export interface HistoryLayout {
  readonly lanes: readonly string[];
  readonly nodes: readonly HistoryNode[];
  /** False when no entry carries stats: sizes are uniform and the view says why. */
  readonly hasStats: boolean;
}

export const MIN_SIZE = 8;
export const MAX_SIZE = 28;

/** Node diameter from churn, square-root scaled so a huge change does not hide the rest. */
export function sizeFor(churn: number, maxChurn: number): number {
  if (maxChurn <= 0) return MIN_SIZE + 4;
  const t = Math.sqrt(Math.max(0, churn) / maxChurn);
  return Math.round(MIN_SIZE + t * (MAX_SIZE - MIN_SIZE));
}

/**
 * Entries are newest first. Lanes are one per author, ordered by first appearance. The stat row
 * for the followed file is the one whose path is the current path; walking back, a commit whose
 * only stat row has a different path marks a rename.
 */
export function layoutHistory(entries: readonly HistoryEntry[], file: string): HistoryLayout {
  const lanes: string[] = [];
  const raw: { entry: HistoryEntry; path: string; churn: number; binary: boolean }[] = [];
  const hasStats = entries.some((e) => e.stats !== undefined);
  let path = file;
  for (const entry of entries) {
    const stats = entry.stats ?? [];
    let row = stats.find((s) => s.path === path);
    if (!row && stats.length === 1) {
      row = stats[0];
      if (row) path = row.path;
    }
    raw.push({
      entry,
      path,
      churn: row ? row.added + row.removed : 0,
      binary: row?.binary ?? false,
    });
    if (!lanes.includes(entry.author)) lanes.push(entry.author);
  }
  const max = Math.max(0, ...raw.map((r) => r.churn));
  const nodes = raw.map((r, i): HistoryNode => {
    const next = raw[i + 1];
    return {
      entry: r.entry,
      lane: lanes.indexOf(r.entry.author),
      path: r.path,
      ...(next && next.path !== r.path ? { renamedFrom: next.path } : {}),
      churn: r.churn,
      size: hasStats ? sizeFor(r.churn, max) : MIN_SIZE + 4,
      binary: r.binary,
    };
  });
  return { lanes, nodes, hasStats };
}
