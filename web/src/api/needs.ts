// What needs the viewer, and the viewer's stacks, derived from reads the app already makes: the
// change list of each repository, its inbox sections and the newest thread of the returned items.
// There is no server-side "needs you" list across repositories, so this is the one place that
// defines it. Pure functions, unit-tested (test/needs.test.ts).
import { InboxSection } from "@asc/asc-client/inbox";
import type { InboxView } from "./inbox";
import type { ThreadsView } from "./review";
import type { ChangeListView, ChangeRow, StackView } from "./types";
import { changeHref } from "../lib/paths";

export type NeedKind = "review" | "reply" | "merge" | "conflict" | "mention" | "checks";

export interface NeedRow {
  id: string;
  kind: NeedKind;
  /** `owner/name`. */
  repo: string;
  /** What the row says: "Wire retry policy into the client", "Reply from Priya on pagination cursor". */
  headline: string;
  /** After the repository: "stack 3 of 4", "2 approvals · checks green". Empty when there is none. */
  detail: string;
  /** Seconds since the epoch of the event that put the row here; 0 when it has none. */
  at: number;
  /** "asked", "" ...: the verb for `at` on the inbox page. */
  atVerb: string;
  /** A one-word risk chip, "Touches billing". */
  chip: string;
  /** "+84 −12 in 3 files". */
  stat: string;
  /** The newest comment, for replies and mentions. */
  quote: string;
  /** Link to the change, or to the repository's operations page for a merge. */
  href: string;
  /** The change this row is about (its id), for the inbox detail pane. */
  change: string;
  /** The agent that wrote the change, from its description; empty for a person. */
  agent: string;
}

export interface RepoInputs {
  repo: string;
  changes: ChangeListView | undefined;
  inbox: InboxView | undefined;
  /** Threads of the returned items, by change id. */
  threads: ReadonlyMap<string, ThreadsView>;
}

const lowerFirst = (s: string): string =>
  s.length > 1 && s[1] === (s[1] ?? "").toLowerCase() ? (s[0] ?? "").toLowerCase() + s.slice(1) : s;

export function humanize(ref: string): string {
  const name = ref.replace(/^refs\/(heads|for)\//, "").replace(/[-_]+/g, " ");
  return name.charAt(0).toUpperCase() + name.slice(1);
}

export function shortRef(ref: string): string {
  return ref.replace(/^refs\/(heads|for)\//, "");
}

export function statText(row: ChangeRow): string {
  const s = row.stat;
  if (!s) return "";
  return `+${s.added} −${s.removed} in ${s.files} ${s.files === 1 ? "file" : "files"}`;
}

function index(
  changes: ChangeListView | undefined,
): Map<string, { row: ChangeRow; stack: StackView }> {
  const out = new Map<string, { row: ChangeRow; stack: StackView }>();
  for (const stack of changes?.stacks ?? []) {
    for (const row of stack.changes) out.set(row.id, { row, stack });
  }
  return out;
}

function where(found: { row: ChangeRow; stack: StackView } | undefined): string {
  if (!found || found.stack.changes.length < 2) return "";
  return `stack ${found.row.position + 1} of ${found.stack.changes.length}`;
}

/** Breaks a tie in time: what asks for an answer comes before what only informs. */
const RANK: Record<NeedKind, number> = {
  review: 0,
  reply: 1,
  merge: 2,
  conflict: 3,
  mention: 4,
  checks: 5,
};

/** The stack is ready to merge: nothing in a draft, every change approved, every check green. */
export function readyToMerge(stack: StackView): { approvals: number } | undefined {
  if (stack.changes.length === 0) return undefined;
  let approvals = Number.POSITIVE_INFINITY;
  for (const c of stack.changes) {
    const r = c.review;
    if (c.landed) continue;
    if (!r || r.draft || r.approvals < 1 || r.checks !== "success") return undefined;
    approvals = Math.min(approvals, r.approvals);
  }
  return Number.isFinite(approvals) ? { approvals } : undefined;
}

export function mentions(body: string, first: string): boolean {
  if (first === "") return false;
  const esc = first.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`(^|[^\\w@])@${esc}\\b`, "i").test(body);
}

/** Everything that needs the viewer across the repositories, in the order of the inbox. */
export function buildNeeds(repos: readonly RepoInputs[], viewerFirst: string): NeedRow[] {
  const rows: NeedRow[] = [];
  for (const r of repos) {
    const found = index(r.changes);
    for (const item of r.inbox?.items ?? []) {
      const hit = found.get(item.change);
      const title = hit?.row.title ?? item.change.slice(0, 8);
      const base = {
        repo: r.repo,
        at: item.updated,
        change: item.change,
        href: changeHref(...(r.repo.split("/") as [string, string]), item.change),
        chip: "",
        stat: "",
        quote: "",
        detail: where(hit),
        atVerb: "",
        agent: hit?.row.description.agent ?? "",
      };
      switch (item.section) {
        case InboxSection.NEEDS_REVIEW:
          rows.push({
            ...base,
            id: `review:${r.repo}:${item.change}`,
            kind: "review",
            headline: title,
            atVerb: "asked",
            chip: hit?.row.description.riskArea ? `Touches ${hit.row.description.riskArea}` : "",
            stat: hit ? statText(hit.row) : "",
          });
          break;
        case InboxSection.RETURNED: {
          const last = r.threads
            .get(item.change)
            ?.threads.flatMap((t) => t.comments)
            .at(-1);
          const mention = last ? mentions(last.body, viewerFirst) : false;
          rows.push({
            ...base,
            id: `${mention ? "mention" : "reply"}:${r.repo}:${item.change}`,
            kind: mention ? "mention" : "reply",
            headline: mention
              ? `You were mentioned: ${lowerFirst(title)}`
              : `Reply from ${last?.author ?? "a reviewer"} on ${lowerFirst(title)}`,
            detail: "",
            quote: last ? `“${last.body}”` : "",
          });
          break;
        }
        case InboxSection.CHECKS_FAILED:
          rows.push({
            ...base,
            id: `checks:${r.repo}:${item.change}`,
            kind: "checks",
            headline: `Checks failed: ${lowerFirst(title)}`,
          });
          break;
        default:
          break;
      }
    }
    for (const stack of r.changes?.stacks ?? []) {
      const ready = readyToMerge(stack);
      if (ready) {
        const [owner, name] = r.repo.split("/") as [string, string];
        rows.push({
          id: `merge:${r.repo}:${stack.ref}`,
          kind: "merge",
          repo: r.repo,
          headline: `Ready to merge: ${humanize(stack.ref)}`,
          detail: `${ready.approvals} ${ready.approvals === 1 ? "approval" : "approvals"} · checks green`,
          at: Math.max(...stack.changes.map((c) => c.time)),
          atVerb: "",
          chip: "",
          stat: "",
          quote: "",
          href: `/${owner}/${name}/ops`,
          change: stack.changes[0]?.id ?? "",
          agent: "",
        });
      }
      for (const c of stack.changes) {
        if (c.needsRestack && !c.landed) {
          rows.push({
            id: `conflict:${r.repo}:${c.id}`,
            kind: "conflict",
            repo: r.repo,
            headline: `Restack needed: ${lowerFirst(c.title)}`,
            detail: "its parent moved",
            at: c.time,
            atVerb: "",
            chip: "",
            stat: "",
            quote: "",
            href: changeHref(...(r.repo.split("/") as [string, string]), c.id),
            change: c.id,
            agent: c.description.agent,
          });
        }
      }
    }
  }
  return rows.toSorted((a, b) => b.at - a.at || RANK[a.kind] - RANK[b.kind]);
}

export type SegmentState = "done" | "waiting" | "idle";

export interface StackBar {
  repo: string;
  name: string;
  ref: string;
  segments: SegmentState[];
  href: string;
}

export function segmentOf(c: ChangeRow): SegmentState {
  if (c.landed || (c.review?.approvals ?? 0) > 0) return "done";
  if (c.review && !c.review.draft && c.review.state === "open") return "waiting";
  return "idle";
}

/** One bar per stack the viewer can see: green merged or approved, amber waiting, grey a draft. */
export function stackBars(repos: readonly RepoInputs[]): StackBar[] {
  const out: StackBar[] = [];
  for (const r of repos) {
    for (const s of r.changes?.stacks ?? []) {
      const bottom = s.changes[0];
      const [owner, name] = r.repo.split("/") as [string, string];
      out.push({
        repo: r.repo,
        name: shortRef(s.ref),
        ref: s.ref,
        segments: s.changes.map(segmentOf),
        href: bottom ? changeHref(owner, name, bottom.id) : `/${owner}/${name}/changes`,
      });
    }
  }
  return out;
}

export interface TodayEvent {
  id: string;
  who: string;
  what: string;
  at: number;
}

const DAY = 86400;

/**
 * What others did in the last day on the viewer's repositories: pushes of new versions and new
 * comments. Newest first. The viewer's own pushes are left out (they know).
 */
export function buildToday(
  repos: readonly RepoInputs[],
  viewerFirst: string,
  now: number,
): TodayEvent[] {
  const out: TodayEvent[] = [];
  const mine = (who: string) =>
    viewerFirst !== "" && who.toLowerCase() === viewerFirst.toLowerCase();
  for (const r of repos) {
    for (const stack of r.changes?.stacks ?? []) {
      const byAuthor = new Map<string, { version: number; at: number }>();
      for (const c of stack.changes) {
        if (c.author === "" || mine(c.author) || now - c.time > DAY) continue;
        const prev = byAuthor.get(c.author);
        byAuthor.set(c.author, {
          version: Math.max(prev?.version ?? 0, c.version),
          at: Math.max(prev?.at ?? 0, c.time),
        });
      }
      for (const [who, v] of byAuthor) {
        out.push({
          id: `push:${r.repo}:${stack.ref}:${who}`,
          who,
          what: `pushed ${v.version} ${v.version === 1 ? "version" : "versions"} to ${shortRef(stack.ref)}`,
          at: v.at,
        });
      }
    }
    const found = index(r.changes);
    for (const [change, view] of r.threads) {
      const title = found.get(change)?.row.title ?? "a change";
      for (const th of view.threads) {
        for (const c of th.comments) {
          if (mine(c.author) || now - c.created > DAY) continue;
          out.push({
            id: `comment:${r.repo}:${change}:${c.id}`,
            who: c.author,
            what: `commented on ${lowerFirst(title)}`,
            at: c.created,
          });
        }
      }
    }
  }
  return out.toSorted((a, b) => b.at - a.at).slice(0, 4);
}
