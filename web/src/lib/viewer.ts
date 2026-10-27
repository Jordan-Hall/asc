// Who is signed in, for the avatar and the greeting. The RPC surface has no "who am I" call, so the
// name comes from where the person typed it: the optional display name on the sign-in form is kept
// next to the token (sessionStorage, same lifetime) and never sent anywhere. Without one the UI
// shows a neutral avatar and a greeting without a name. The mock build answers a demo viewer so the
// screens can be reviewed against the boards.
import { useSyncExternalStore } from "react";

export const VIEWER_KEY = "asc-viewer";

export interface Viewer {
  /** Display name, for example `Jordan Upton`. */
  name: string;
  /** Up to two letters for the avatar. */
  initials: string;
  /** Name used in "Good afternoon, Jordan". */
  first: string;
}

export function parseViewer(raw: string | null | undefined): Viewer | undefined {
  const name = (raw ?? "").replace(/\s+/g, " ").trim().slice(0, 60);
  if (!name) return undefined;
  const words = name.split(" ");
  const first = words[0] ?? name;
  const initials = (
    words.length > 1 ? `${first[0] ?? ""}${words[words.length - 1]?.[0] ?? ""}` : name.slice(0, 2)
  ).toUpperCase();
  return { name, first, initials };
}

function store(): Pick<Storage, "getItem" | "setItem" | "removeItem"> | undefined {
  try {
    return typeof sessionStorage === "undefined" ? undefined : sessionStorage;
  } catch {
    return undefined;
  }
}

export function writeViewer(name: string): void {
  try {
    const v = parseViewer(name);
    if (v) store()?.setItem(VIEWER_KEY, v.name);
    else store()?.removeItem(VIEWER_KEY);
  } catch {
    // Storage can be blocked; the avatar then stays neutral.
  }
  cached = undefined;
  for (const l of listeners) l();
}

const listeners = new Set<() => void>();
let cached: { v: Viewer | undefined } | undefined;

function snapshot(): Viewer | undefined {
  if (!cached) {
    let raw: string | null = null;
    try {
      raw = store()?.getItem(VIEWER_KEY) ?? null;
    } catch {
      raw = null;
    }
    cached = { v: parseViewer(raw) };
  }
  return cached.v;
}

export function useViewer(): Viewer | undefined {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    snapshot,
    () => undefined,
  );
}

/** "Good morning", "Good afternoon" or "Good evening" for the hour of day. */
export function greeting(hour: number): string {
  if (hour < 5) return "Good evening";
  if (hour < 12) return "Good morning";
  if (hour < 18) return "Good afternoon";
  return "Good evening";
}
