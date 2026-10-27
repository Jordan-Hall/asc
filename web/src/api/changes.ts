// Change list over Repo.ListRefs, Changes.Stack and Repo.Log. There is no "list changes" RPC yet
// (docs/WEB-UI.md section 10), so the list is derived: every non-trunk branch (and the caller's
// own user refs, which ListRefs renames to refs/heads/*) is asked for its stack.
import {
  type BridgeTransport,
  callStream,
  callUnary,
  type CallParams,
  changeIdToString,
} from "@asc/asc-client";
import { Changes, type ChangeInfo } from "@asc/asc-client/changes";
import { Repo } from "@asc/asc-client/repo";
import { listRefs, shortId } from "./repo";
import type {
  ChangeDescription,
  ChangeListView,
  ChangeRow,
  DiffTotals,
  ReviewBadge,
  StackView,
} from "./types";

const MAX_STACKS = 50;
const CONCURRENCY = 6;
const text = new TextDecoder();

function badge(info: ChangeInfo): ReviewBadge | undefined {
  const r = info.review;
  if (!r || (r.number === 0n && r.state === "")) return undefined;
  return {
    state: r.state,
    number: Number(r.number),
    draft: r.draft,
    approvals: r.approvals,
    checks: r.checks,
    url: r.url,
  };
}

/**
 * Reads a change description: the first line is the title; the lines after it are the intent,
 * until the trailer block at the end (`Risk:`, `Risk-area:`, `Agent:`, one per line, like the
 * trailers of a commit message).
 */
export function parseDescription(message: string): ChangeDescription {
  const lines = message.replace(/\r\n?/g, "\n").split("\n");
  const title = (lines.shift() ?? "").trim();
  const trailers = new Map<string, string>();
  let end = lines.length;
  while (end > 0 && (lines[end - 1] ?? "").trim() === "") end--;
  let start = end;
  while (start > 0) {
    const m = /^(Risk|Risk-area|Agent):[ \t]*(.*)$/.exec(lines[start - 1] ?? "");
    if (!m) break;
    trailers.set((m[1] ?? "").toLowerCase(), (m[2] ?? "").trim());
    start--;
  }
  const intent = lines.slice(0, start).join("\n").trim();
  return {
    title,
    intent,
    risk: trailers.get("risk") ?? "",
    riskArea: trailers.get("risk-area") ?? "",
    agent: trailers.get("agent") ?? "",
  };
}

interface LogInfo {
  description: ChangeDescription;
  author: string;
  time: number;
  stat: DiffTotals | undefined;
}

/** The newest version's log entry of every change in a stack, by change id. */
async function details(
  t: BridgeTransport,
  repo: string,
  head: string,
  trunk: string,
  p?: CallParams,
): Promise<Map<string, LogInfo>> {
  const out = new Map<string, LogInfo>();
  const stream = callStream(
    t,
    Repo.method.log,
    { repo: { name: repo }, include: [head], exclude: [trunk], limit: 200 },
    p,
  );
  for await (const batch of stream) {
    for (const e of batch.entries) {
      if (e.change.length === 0) continue;
      const files = e.stats?.files ?? [];
      out.set(changeIdToString(e.change), {
        description: parseDescription(text.decode(e.message)),
        author: e.author ? text.decode(e.author.name) : "",
        time: e.author ? Number(e.author.time) : 0,
        stat:
          files.length === 0
            ? undefined
            : {
                added: files.reduce((n, f) => n + f.added, 0),
                removed: files.reduce((n, f) => n + f.removed, 0),
                files: files.length,
              },
      });
    }
  }
  return out;
}

async function loadStack(
  t: BridgeTransport,
  repo: string,
  ref: string,
  trunk: string,
  p?: CallParams,
): Promise<StackView | undefined> {
  const res = await callUnary(
    t,
    Changes.method.stack,
    { repo: { name: repo }, target: trunk, at: { case: "rev", value: ref } },
    p,
  );
  if (res.changes.length === 0) return undefined;
  const info = await details(t, repo, ref, trunk, p);
  const size = res.changes.length;
  const changes: ChangeRow[] = res.changes.map((c, i) => {
    const id = changeIdToString(c.change);
    const d = info.get(id);
    return {
      id,
      short: id.slice(0, 8),
      title: d?.description.title || shortId(id),
      author: d?.author ?? "",
      time: d?.time ?? 0,
      description: d?.description ?? parseDescription(""),
      stat: d?.stat,
      version: c.version,
      position: i,
      stackSize: size,
      ref,
      landed: c.landedAs.length > 0,
      divergent: c.visible.length > 1,
      needsRestack: !c.parentCurrent && c.parentChange.length > 0,
      review: badge(c),
    };
  });
  return { ref, trunk, changes };
}

async function mapLimit<T, R>(
  items: readonly T[],
  limit: number,
  fn: (x: T) => Promise<R>,
): Promise<R[]> {
  const out: R[] = [];
  let next = 0;
  const worker = async () => {
    for (let i = next++; i < items.length; i = next++) out[i] = await fn(items[i] as T);
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return out;
}

/** Drops a stack whose changes are all part of a longer stack. */
export function dedupeStacks(stacks: readonly StackView[]): StackView[] {
  const sorted = stacks.toSorted((a, b) => b.changes.length - a.changes.length);
  const kept: StackView[] = [];
  const seen = new Set<string>();
  for (const s of sorted) {
    if (s.changes.every((c) => seen.has(c.id))) continue;
    kept.push(s);
    for (const c of s.changes) seen.add(c.id);
  }
  return kept;
}

export async function loadChangeList(
  t: BridgeTransport,
  repo: string,
  p?: CallParams,
): Promise<ChangeListView> {
  const refs = await listRefs(t, repo, p);
  const trunk = refs.defaultBranch;
  const heads = refs.refs
    .filter((r) => r.kind === "branch" && r.short !== trunk)
    .slice(0, MAX_STACKS);
  const found = await mapLimit(heads, CONCURRENCY, (r) => loadStack(t, repo, r.name, trunk, p));
  const stacks = dedupeStacks(found.filter((s): s is StackView => s !== undefined));
  return { trunk, stacks };
}
