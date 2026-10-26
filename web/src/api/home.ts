// The data of the dashboard, the inbox and the navigation lists: for each repository this browser
// has opened (there is no "my repositories" RPC), its change list and inbox, and the newest thread
// of each returned item. Reads go through the shared query cache, so the three screens share them.
import { InboxSection } from "@asc/asc-client/inbox";
import { useMemo } from "react";
import { useQueries } from "../lib/query";
import { useRecent } from "../lib/recent";
import { useViewer } from "../lib/viewer";
import {
  buildNeeds,
  buildToday,
  type NeedRow,
  type RepoInputs,
  type StackBar,
  stackBars,
  type TodayEvent,
} from "./needs";
import { useApp } from "./queries";
import type { ThreadsView } from "./review";

/** How many remembered repositories the home screens read. */
export const HOME_REPOS = 6;

export interface HomeData {
  /** The repositories read, newest first. */
  repos: readonly string[];
  needs: readonly NeedRow[];
  stacks: readonly StackBar[];
  /** What others did in the last day. */
  today: readonly TodayEvent[];
  /** Rows by kind, for the sidebar counts. */
  reviewRequests: number;
  /** The first reads are still running. */
  loading: boolean;
  /** At least one repository could not be read. */
  failed: boolean;
}

export function useHome(enabled = true): HomeData {
  const { queries } = useApp();
  const recent = useRecent();
  const viewer = useViewer();
  const repos = useMemo(() => (enabled ? recent.slice(0, HOME_REPOS) : []), [recent, enabled]);
  const changes = useQueries(useMemo(() => repos.map((r) => queries.changes(r)), [repos, queries]));
  const inboxes = useQueries(useMemo(() => repos.map((r) => queries.inbox(r)), [repos, queries]));

  // Threads of the returned items: known only once the inboxes are.
  const wanted = useMemo(() => {
    const out: { repo: string; change: string }[] = [];
    repos.forEach((repo, i) => {
      for (const item of inboxes[i]?.data?.items ?? []) {
        if (item.section === InboxSection.RETURNED) out.push({ repo, change: item.change });
      }
    });
    return out;
  }, [repos, inboxes]);
  const threads = useQueries(
    useMemo(() => wanted.map((w) => queries.threads(w.repo, w.change)), [wanted, queries]),
  );

  return useMemo(() => {
    const inputs: RepoInputs[] = repos.map((repo, i) => {
      const map = new Map<string, ThreadsView>();
      wanted.forEach((w, j) => {
        const data = threads[j]?.data;
        if (w.repo === repo && data) map.set(w.change, data);
      });
      return { repo, changes: changes[i]?.data, inbox: inboxes[i]?.data, threads: map };
    });
    const needs = buildNeeds(inputs, viewer?.first ?? "");
    const all = [...changes, ...inboxes, ...threads];
    return {
      repos,
      needs,
      stacks: stackBars(inputs),
      today: buildToday(inputs, viewer?.first ?? "", Date.now() / 1000),
      reviewRequests: needs.filter((n) => n.kind === "review").length,
      loading: all.some((e) => e.status === "pending"),
      failed: all.some((e) => e.status === "error"),
    };
  }, [repos, changes, inboxes, threads, wanted, viewer]);
}
