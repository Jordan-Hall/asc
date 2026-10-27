// Query definitions: the cache key, the runner and the freshness class of every read, in one place
// so a route loader and a component name the same query. Reads under a resolved id are
// content-addressed and cached for good (docs/WEB-UI.md section 4.2).
import type { BridgeTransport } from "@asc/asc-client";
import { createContext, useContext } from "react";
import type { AuthStore } from "../lib/auth";
import {
  type Entry,
  keyOf,
  type QueryCache,
  type QueryOptions,
  type Runner,
  useQuery,
} from "../lib/query";
import { loadChangeList } from "./changes";
import { listThreads, type ThreadsView } from "./review";
import type { InboxCounts, InboxView } from "./inbox";
import { lastCommit, listRefs, readFile, readTree, resolve } from "./repo";
import type { ChangeListView, CommitView, FileView, RefsView, TreeView } from "./types";

export interface Ctx {
  transport: BridgeTransport;
  cache: QueryCache;
}

export interface Query<T> {
  key: string;
  run: Runner<T>;
  opts: QueryOptions;
}

const REF_STALE_MS = 10_000;
const FOREVER = { staleMs: Number.POSITIVE_INFINITY } as const;

export interface Queries {
  refs(repo: string): Query<RefsView>;
  tree(repo: string, rev: string, path: string): Query<TreeView>;
  lastCommit(repo: string, rev: string, path: string): Query<CommitView | undefined>;
  file(repo: string, rev: string, path: string): Query<FileView>;
  changes(repo: string): Query<ChangeListView>;
  /** Needs a user identity: a credential without one answers PERMISSION_DENIED. */
  inboxCount(repo: string): Query<InboxCounts>;
  inbox(repo: string): Query<InboxView>;
  /** The review threads of one change. */
  threads(repo: string, change: string): Query<ThreadsView>;
}

/**
 * Runs `inner` once under a content-addressed key and mirrors its partial results to `outer`.
 * A mutable query (a ref lookup) wraps one immutable read; if the mutable run is replaced while the
 * read is still streaming (the page remounted, or a second tab of the same key), the new run joins
 * the read in flight and still shows the first screenful instead of waiting for the end.
 */
export function mirror<T>(
  cache: QueryCache,
  key: string,
  inner: Runner<T>,
  outer: { signal: AbortSignal; emit(data: T): void },
): Promise<T> {
  const done = cache.fetch(key, inner, FOREVER);
  const seen = cache.peek<T>(key).data;
  if (seen !== undefined) outer.emit(seen);
  const stop = cache.subscribe(key, () => {
    const data = cache.peek<T>(key).data;
    if (data !== undefined && !outer.signal.aborted) outer.emit(data);
  });
  return done.finally(stop);
}

export function createQueries(ctx: Ctx): Queries {
  const { transport: t, cache } = ctx;

  /** Resolves a ref to its id (short-lived), so reads below it can be cached forever. */
  const idOf = (repo: string, rev: string, signal: AbortSignal): Promise<string> =>
    cache.fetch(
      keyOf("resolve", repo, rev),
      async () => (await resolve(t, repo, rev, { signal })) ?? rev,
      { staleMs: REF_STALE_MS },
    );

  return {
    refs: (repo) => ({
      key: keyOf("refs", repo),
      run: ({ signal }) => listRefs(t, repo, { signal }),
      opts: { staleMs: REF_STALE_MS },
    }),

    tree: (repo, rev, path) => ({
      key: keyOf("tree", repo, rev, path),
      opts: { staleMs: REF_STALE_MS, abortOnRelease: true },
      run: async ({ signal, emit }) => {
        const id = await idOf(repo, rev, signal);
        // The listing of an id never changes: keep it for good.
        return mirror(
          cache,
          keyOf("tree-at", repo, id, path),
          ({ emit: inner, signal: s }) => readTree(t, repo, id, path, inner, { signal: s }),
          { signal, emit },
        );
      },
    }),

    lastCommit: (repo, rev, path) => ({
      key: keyOf("last", repo, rev, path),
      opts: { staleMs: REF_STALE_MS },
      run: async ({ signal }) => {
        const id = await idOf(repo, rev, signal);
        return cache.fetch(
          keyOf("last-at", repo, id, path),
          () => lastCommit(t, repo, id, path, { signal }),
          FOREVER,
        );
      },
    }),

    file: (repo, rev, path) => ({
      key: keyOf("file", repo, rev, path),
      opts: { staleMs: REF_STALE_MS, abortOnRelease: true },
      run: async ({ signal, emit }) => {
        const id = await idOf(repo, rev, signal);
        return mirror(
          cache,
          keyOf("file-at", repo, id, path),
          ({ emit: inner, signal: s }) => readFile(t, repo, id, path, inner, { signal: s }),
          { signal, emit },
        );
      },
    }),

    changes: (repo) => ({
      key: keyOf("changes", repo),
      opts: { staleMs: 30_000 },
      run: ({ signal }) => loadChangeList(t, repo, { signal }),
    }),

    inboxCount: (repo) => ({
      key: keyOf("inbox-count", repo),
      opts: { staleMs: 30_000 },
      // The inbox service descriptors load with the first call, not with the entry chunk.
      run: async ({ signal }) => (await import("./inbox")).countInbox(t, repo, "", { signal }),
    }),

    inbox: (repo) => ({
      key: keyOf("inbox", repo),
      opts: { staleMs: 30_000 },
      run: async ({ signal }) => (await import("./inbox")).listInbox(t, repo, {}, { signal }),
    }),

    threads: (repo, change) => ({
      key: keyOf("threads", repo, change),
      opts: { staleMs: 30_000 },
      run: ({ signal }) => listThreads(t, repo, change, {}, { signal }),
    }),
  };
}

export interface AppCtx extends Ctx {
  auth: AuthStore;
  queries: Queries;
}

const AppContext = createContext<AppCtx | null>(null);
export const AppProvider = AppContext.Provider;

export function useApp(): AppCtx {
  const c = useContext(AppContext);
  if (!c) throw new Error("AppProvider is missing");
  return c;
}

/** Subscribes to a query. */
export function useQ<T>(q: Query<T>): Entry<T> {
  return useQuery(q.key, q.run, q.opts);
}

/** Warms a query without a subscriber (route loaders, link prefetch). */
export function warm<T>(ctx: Ctx, q: Query<T>): void {
  void ctx.cache.prefetch(q.key, q.run, q.opts);
}
