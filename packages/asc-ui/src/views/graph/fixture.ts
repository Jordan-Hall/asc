// Synthetic history for the graph tests and Playwright specs.
import type { GraphRow } from "./model";

/** 40 hex digits whose first 8 identify the row, so short ids are unique. */
export const hexId = (i: number): string => i.toString(16).padStart(8, "0").padEnd(40, "0");
const hex = hexId;
const BLOCK = 25;

/** Row `i` of a synthetic history: a 25-row block with one merge, a side branch and a fork. */
export function genRow(i: number, total: number): GraphRow {
  const pos = i % BLOCK;
  const base = i - pos;
  const id = hex(i);
  const next = (n: number) => (n < total ? hex(n) : undefined);
  const refs = i === 0 ? ["main"] : i % 100 === 50 ? [`tag-${i}`] : [];
  const mk = (col: number, parents: (string | undefined)[], edges: [number, string | undefined][]): GraphRow => ({
    id,
    subject: `commit ${i}`,
    author: `author ${i % 7}`,
    time: 1_700_000_000_000 + (total - i) * 60_000,
    parents: parents.filter((p): p is string => p !== undefined),
    column: col,
    edges: edges.filter((e): e is [number, string] => e[1] !== undefined).map(([toColumn, parent]) => ({ toColumn, parent })),
    refs,
    overflow: false,
    wip: false,
  });
  const f = next(base + 4);
  switch (pos) {
    case 0:
      return mk(0, [next(base + 3), next(base + 1)], [[0, next(base + 3)], [1, next(base + 1)]]);
    case 1:
      return mk(1, [next(base + 2)], [[1, next(base + 2)]]);
    case 2:
      return mk(1, [f], [[1, f]]);
    case 3:
      return mk(0, [f], [[0, f]]);
    case 24:
      return mk(0, [next(base + BLOCK)], [[0, next(base + BLOCK)]]);
    default:
      return mk(0, [next(i + 1)], [[0, next(i + 1)]]);
  }
}

