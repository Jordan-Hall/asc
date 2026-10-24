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
        return cache.fetch(
          keyOf("tree-at", repo, id, path),
          ({ emit: inner, signal: s }) =>
            readTree(
              t,
              repo,
              id,
              path,
              (v) => {
                inner(v);
                emit(v);
              },
              { signal: s },
            ),
          FOREVER,
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
        return cache.fetch(
          keyOf("file-at", repo, id, path),
          ({ emit: inner, signal: s }) =>
            readFile(
              t,
              repo,
              id,
              path,
              (v) => {
                inner(v);
                emit(v);
              },
              { signal: s },
            ),
          FOREVER,
        );
      },
    }),

    changes: (repo) => ({
      key: keyOf("changes", repo),
      opts: { staleMs: 30_000 },
      run: ({ signal }) => loadChangeList(t, repo, { signal }),
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
