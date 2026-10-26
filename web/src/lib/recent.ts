// Repositories this browser has opened, newest first. asc has no "list my repositories" RPC yet, so
// the dashboard, the palette and the header read this list (a per-viewer convenience: it can be
// empty or blocked and nothing breaks). Names are validated on the way in and on the way out.
import { useSyncExternalStore } from "react";

export const RECENT_KEY = "asc-recent";
export const MAX_RECENT = 12;

export function parseRepoName(input: string): { owner: string; repo: string } | undefined {
  const m = /^\/?([\w.-]+)\/([\w.-]+?)(?:\.git)?\/?$/.exec(input.trim());
  return m?.[1] && m[2] ? { owner: m[1], repo: m[2] } : undefined;
}

type Store = Pick<Storage, "getItem" | "setItem">;

function local(): Store | undefined {
  try {
    return typeof localStorage === "undefined" ? undefined : localStorage;
  } catch {
    return undefined;
  }
}

export function readRecent(storage: Store | undefined = local()): string[] {
  try {
    const raw: unknown = JSON.parse(storage?.getItem(RECENT_KEY) ?? "[]");
    if (!Array.isArray(raw)) return [];
    return raw
      .filter((x): x is string => typeof x === "string" && !!parseRepoName(x))
      .slice(0, MAX_RECENT);
  } catch {
    return [];
  }
}

const listeners = new Set<() => void>();
let snapshot: readonly string[] | undefined;

/** Puts `name` first. Storage may be blocked; the list then lasts for this page view only. */
export function remember(name: string, storage: Store | undefined = local()): void {
  const parsed = parseRepoName(name);
  if (!parsed) return;
  const key = `${parsed.owner}/${parsed.repo}`;
  const next = [key, ...(snapshot ?? readRecent(storage)).filter((r) => r !== key)].slice(
    0,
    MAX_RECENT,
  );
  if (snapshot && next[0] === snapshot[0] && next.length === snapshot.length) return;
  snapshot = next;
  try {
    storage?.setItem(RECENT_KEY, JSON.stringify(next));
  } catch {
    // Optional convenience.
  }
  for (const l of listeners) l();
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  const onStorage = (e: StorageEvent) => {
    if (e.key !== RECENT_KEY) return;
    snapshot = undefined;
    cb();
  };
  window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(cb);
    window.removeEventListener("storage", onStorage);
  };
}

const EMPTY: readonly string[] = [];

export function useRecent(): readonly string[] {
  return useSyncExternalStore(
    subscribe,
    () => (snapshot ??= readRecent()),
    () => EMPTY,
  );
}
