// Review panel view model (SPEC §24.3.5, §24.4.7): pure navigator rows, badges, version-picker
// defaults and the stated reason for every disabled action. No DOM, so the rules are unit-tested.
import type { Stack, StackChange, Stacks } from "../../state/model";
import { locate } from "../../state/model";

export interface VersionInfo {
  readonly v: number;
  /** Unix seconds. */
  readonly at: number;
  readonly actor: string;
}

/** `Changes.Mergeability` state (proto `Mergeable.State`) in text form. */
export type MergeState = "disjoint" | "clean" | "conflicts" | "needs_restack";

export interface OverlapInfo {
  /** The other change that touches the same paths. */
  readonly change: string;
  readonly pr?: number;
  readonly paths: readonly string[];
}

export interface ReviewComment {
  readonly id: string;
  readonly path: string;
  readonly line?: number;
  readonly author: string;
  readonly body: string;
  readonly resolved: boolean;
  /** The thread anchors to an older version than the one shown. */
  readonly outdated?: boolean;
}

/** What the host loaded for one change of the stack. */
export interface ChangeReviewData {
  readonly versions: readonly VersionInfo[];
  /** Highest version the user reviewed last (their latest submitted review), if any. */
  readonly lastReviewedV?: number;
  readonly mergeability?: MergeState;
  readonly conflictPaths?: readonly string[];
  readonly overlaps?: readonly OverlapInfo[];
  /** The signed-in user authored this change (cannot approve it). */
  readonly own?: boolean;
  readonly merged?: boolean;
}

export interface ReviewData {
  readonly stacks: Stacks;
  readonly changes: Readonly<Record<string, ChangeReviewData>>;
  readonly comments: Readonly<Record<string, readonly ReviewComment[]>>;
  readonly selected?: string;
}

export interface DiffFile {
  readonly path: string;
  readonly status: "M" | "A" | "D" | "R";
  readonly additions: number;
  readonly deletions: number;
}

export type BadgeKind = "pr" | "mergeability" | "overlap" | "restack" | "comments" | "merged";

export interface Badge {
  readonly kind: BadgeKind;
  readonly text: string;
  readonly title?: string;
  /** Extra css class suffix (the mergeability state). */
  readonly tone?: string;
}

export interface NavRow {
  readonly id: string;
  readonly subject: string;
  readonly selected: boolean;
  readonly badges: readonly Badge[];
}

export const MERGE_TEXT: Record<MergeState, string> = {
  disjoint: "no overlap",
  clean: "merges cleanly",
  conflicts: "conflicts",
  needs_restack: "needs restack",
};

/** The stack that contains `id`; absent when the change is unknown. */
export function stackOf(stacks: Stacks, id: string): Stack | undefined {
  const at = locate(stacks, id);
  return at === undefined ? undefined : stacks.stacks[at.stackIndex];
}

export function unresolvedCount(comments: readonly ReviewComment[] | undefined): number {
  return (comments ?? []).filter((c) => !c.resolved).length;
}

export function changeBadges(
  change: StackChange,
  data: ChangeReviewData | undefined,
  comments: readonly ReviewComment[] | undefined,
): Badge[] {
  const out: Badge[] = [];
  const r = change.review;
  if (r) out.push({ kind: "pr", text: `#${r.number} ${r.draft ? "draft" : r.state}` });
  if (data?.merged) out.push({ kind: "merged", text: "merged" });
  if (data?.mergeability !== undefined) {
    out.push({
      kind: "mergeability",
      text: MERGE_TEXT[data.mergeability],
      tone: data.mergeability,
      ...(data.conflictPaths?.length ? { title: data.conflictPaths.join("\n") } : {}),
    });
  }
  if (change.needsRestack) out.push({ kind: "restack", text: "restack" });
  const ov = data?.overlaps ?? [];
  if (ov.length > 0) {
    out.push({
      kind: "overlap",
      text: `overlaps ${ov.length}`,
      title: ov
        .map((o) => `${o.pr === undefined ? o.change : `#${o.pr}`}: ${o.paths.join(", ")}`)
        .join("\n"),
    });
  }
  const open = unresolvedCount(comments);
  if (open > 0) out.push({ kind: "comments", text: `${open} open` });
  return out;
}

/** Navigator rows: the selected change's stack, base to tip. */
export function buildNavigator(data: ReviewData): NavRow[] {
  const stack =
    (data.selected === undefined ? undefined : stackOf(data.stacks, data.selected)) ??
    data.stacks.stacks.find((s) => s.changes.some((c) => c.review !== undefined));
  if (!stack) return [];
  return stack.changes.map((c) => ({
    id: c.id,
    subject: c.subject,
    selected: c.id === data.selected,
    badges: changeBadges(c, data.changes[c.id], data.comments[c.id]),
  }));
}

export interface VersionPick {
  readonly from: number;
  readonly to: number;
}

export function latestVersion(versions: readonly VersionInfo[]): number {
  return versions.reduce((m, v) => Math.max(m, v.v), 0);
}

/** Default interdiff: previous version against the latest; a single version against itself. */
export function defaultPick(versions: readonly VersionInfo[]): VersionPick {
  const to = latestVersion(versions);
  return { from: Math.max(1, to - 1), to };
}

export const REASON_NO_LAST_REVIEW = "You have not reviewed this change yet";
export const REASON_UP_TO_DATE = "No new version since your last review";

/** "Since my last review": last reviewed version against the latest. */
export function sinceLastReview(
  data: ChangeReviewData | undefined,
): { pick: VersionPick } | { reason: string } {
  const last = data?.lastReviewedV;
  if (last === undefined) return { reason: REASON_NO_LAST_REVIEW };
  const to = latestVersion(data?.versions ?? []);
  if (to <= last) return { reason: REASON_UP_TO_DATE };
  return { pick: { from: last, to } };
}

export function pickError(versions: readonly VersionInfo[], pick: VersionPick): string | undefined {
  const known = new Set(versions.map((v) => v.v));
  if (!known.has(pick.from) || !known.has(pick.to)) return "Unknown version";
  if (pick.from > pick.to) return "From must not be newer than To";
  return undefined;
}

export interface ActionState {
  readonly enabled: boolean;
  /** Set exactly when `enabled` is false. */
  readonly reason?: string;
}

const ok: ActionState = { enabled: true };
const no = (reason: string): ActionState => ({ enabled: false, reason });

export function approveState(change: StackChange, data: ChangeReviewData | undefined): ActionState {
  const r = change.review;
  if (!r) return no("No pull request for this change");
  if (r.state !== "open") return no(`Pull request is ${r.state}`);
  if (r.draft) return no("Pull request is a draft");
  if (data?.own) return no("You cannot approve your own change");
  return ok;
}

/**
 * Merge is bottom-up (the stack guard answers 409 otherwise): every change below the selected one
 * must be merged first. Failing checks, missing approval, conflicts and a stale base block it.
 */
export function mergeState(stacks: Stacks, id: string, data: ReviewData["changes"]): ActionState {
  const at = locate(stacks, id);
  if (!at) return no("Unknown change");
  const stack = stacks.stacks[at.stackIndex];
  const c = at.change;
  const r = c.review;
  if (!r) return no("No pull request for this change");
  if (r.state !== "open") return no(`Pull request is ${r.state}`);
  if (r.draft) return no("Pull request is a draft");
  const below = (stack?.changes ?? []).slice(0, at.index).find((b) => data[b.id]?.merged !== true);
  if (below) {
    return no(`Merge ${below.review ? `#${below.review.number}` : below.subject} first (stack order)`);
  }
  const d = data[id];
  if (d?.mergeability === "conflicts") return no("Conflicts with the target");
  if (d?.mergeability === "needs_restack" || c.needsRestack) return no("Needs restack first");
  if (r.approvals < 1) return no("Needs an approval");
  if (r.checks !== "passing" && r.checks !== "") return no(`Checks are ${r.checks}`);
  return ok;
}

const label = (c: StackChange): string => (c.review ? `#${c.review.number}` : c.id);

export function mergeConfirmText(change: StackChange): string {
  return `Merge ${label(change)} "${change.subject}" into the target branch?`;
}

export function approveConfirmText(change: StackChange): string {
  return `Submit an approving review on ${label(change)} "${change.subject}"?`;
}

export function fileSummary(f: DiffFile): string {
  return `${f.status} ${f.path} +${f.additions} -${f.deletions}`;
}

/** Comments grouped by path, paths sorted, threads by line. */
export function groupComments(
  comments: readonly ReviewComment[],
): { path: string; comments: ReviewComment[] }[] {
  const byPath = new Map<string, ReviewComment[]>();
  for (const c of comments) {
    const list = byPath.get(c.path);
    if (list) list.push(c);
    else byPath.set(c.path, [c]);
  }
  return [...byPath.keys()].toSorted().map((path) => ({
    path,
    comments: (byPath.get(path) ?? []).toSorted((a, b) => (a.line ?? 0) - (b.line ?? 0)),
  }));
}
