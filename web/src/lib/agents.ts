// "Show agent features": a per-browser switch. Off by default, so the product reads human first.
// When on, the front page gains its agents section and the app shows where agent work appears.
import { useCallback, useSyncExternalStore } from "react";

export const AGENTS_KEY = "asc-agents";

export function readAgents(storage: Pick<Storage, "getItem"> | undefined): boolean {
  try {
    return storage?.getItem(AGENTS_KEY) === "1";
  } catch {
    return false;
  }
}

export function writeAgents(
  storage: Pick<Storage, "setItem" | "removeItem"> | undefined,
  on: boolean,
): void {
  try {
    if (on) storage?.setItem(AGENTS_KEY, "1");
    else storage?.removeItem(AGENTS_KEY);
  } catch {
    // Storage can be blocked; the choice then lasts for this page view only.
  }
}

const listeners = new Set<() => void>();
let current: boolean | undefined;

const store = (): Storage | undefined => (typeof localStorage === "undefined" ? undefined : localStorage);

function snapshot(): boolean {
  current ??= readAgents(store());
  return current;
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

export function useAgents(): { on: boolean; setOn: (on: boolean) => void } {
  const on = useSyncExternalStore(subscribe, snapshot, () => false);
  const setOn = useCallback((next: boolean) => {
    current = next;
    writeAgents(store(), next);
    for (const l of listeners) l();
  }, []);
  return { on, setOn };
}
