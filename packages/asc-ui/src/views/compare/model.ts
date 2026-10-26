// Compare model (SPEC §24.3.3): two revisions, changes or stacks through `Compare` (C30b).
// Pure functions; the DOM lives in view.ts and the wire mapping in from-proto.ts.
import type { DetailFile } from "../details/model";

export type CompareKind = "revision" | "change" | "stack";

/** One side of a compare as the user picked it. */
export interface CompareSide {
  readonly kind: CompareKind;
  /** A rev expression, a change id, or the top change of a stack. */
  readonly value: string;
}

export interface CompareEntry {
  readonly id: string;
  readonly subject: string;
  readonly author: string;
  /** Unix seconds. */
  readonly time: number;
}

export interface CompareData {
  readonly a: CompareSide;
  readonly b: CompareSide;
  readonly mergeBase: string;
  readonly ahead: number;
  readonly behind: number;
  readonly aheadEntries: readonly CompareEntry[];
  readonly behindEntries: readonly CompareEntry[];
  readonly files: readonly DetailFile[];
  readonly aheadMore: boolean;
  readonly behindMore: boolean;
}

export interface CompareRequestArgs {
  readonly a: string;
  readonly b: string;
  readonly threeDot: boolean;
}

/**
 * Request for `Compare`. A change or a stack side resolves to its head revision first
 * (`ResolveChanges` is the caller's job and gives `heads`); a revision side is used as typed.
 * Two stacks and two changes compare as `a...b` (what b adds since the merge base), as does a
 * mixed pair; two plain revisions compare as `a..b`, the direct difference, unless the user asks
 * for the three-dot form.
 */
export function compareRequest(
  a: CompareSide,
  b: CompareSide,
  heads: Readonly<Record<string, string>>,
  threeDot?: boolean,
): CompareRequestArgs {
  const rev = (s: CompareSide): string =>
    s.kind === "revision" ? s.value : (heads[s.value] ?? s.value);
  const both = a.kind === "revision" && b.kind === "revision";
  return { a: rev(a), b: rev(b), threeDot: threeDot ?? !both };
}

export function isEmptySide(s: CompareSide): boolean {
  return s.value.trim() === "";
}

/** One-line summary: "b is 3 ahead, 1 behind a". */
export function summary(d: Pick<CompareData, "ahead" | "behind">): string {
  if (d.ahead === 0 && d.behind === 0) return "No differences in history";
  const parts: string[] = [];
  if (d.ahead > 0) parts.push(`${d.ahead} ahead`);
  if (d.behind > 0) parts.push(`${d.behind} behind`);
  return parts.join(", ");
}

export function sideLabel(s: CompareSide): string {
  const v = s.value.length > 16 && /^[0-9a-f]+$/.test(s.value) ? s.value.slice(0, 12) : s.value;
  return s.kind === "revision" ? v : `${s.kind} ${v}`;
}

/** Pairs for the multi-diff: the merge base (three-dot) or `a` against `b`, per text file. */
export function comparePairs(
  d: Pick<CompareData, "files" | "mergeBase">,
  req: CompareRequestArgs,
): { path: string; oldPath: string; leftRev: string; rightRev: string }[] {
  const left = req.threeDot && d.mergeBase !== "" ? d.mergeBase : req.a;
  return d.files
    .filter((f) => f.binary !== true)
    .toSorted((x, y) => (x.path < y.path ? -1 : x.path > y.path ? 1 : 0))
    .map((f) => ({ path: f.path, oldPath: f.oldPath ?? f.path, leftRev: left, rightRev: req.b }));
}
