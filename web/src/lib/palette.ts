// Open state of the command palette (Ctrl+K). The palette itself is a lazy chunk; this tiny store is
// in the entry so the header, the shortcuts and the menus can open it without importing it.
import { useSyncExternalStore } from "react";

export interface PaletteState {
  open: boolean;
  /** Text the palette starts with, for example `owner/` from the open-repository menu item. */
  query: string;
}

let state: PaletteState = { open: false, query: "" };
const listeners = new Set<() => void>();

function set(next: PaletteState): void {
  state = next;
  for (const l of listeners) l();
}

export function openPalette(query = ""): void {
  set({ open: true, query });
}

export function closePalette(): void {
  if (state.open) set({ open: false, query: "" });
}

export function usePalette(): PaletteState {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => state,
    () => state,
  );
}

/** Subsequence match: every character of `needle` appears in order. Lower is better; -1 is no match. */
export function fuzzy(needle: string, hay: string): number {
  const n = needle.toLowerCase();
  const h = hay.toLowerCase();
  if (n === "") return 0;
  const at = h.indexOf(n);
  if (at >= 0) return at === 0 ? 0 : 1 + at / 100;
  let i = 0;
  let gaps = 0;
  let last = -1;
  for (const ch of n) {
    const j = h.indexOf(ch, last + 1);
    if (j < 0) return -1;
    if (last >= 0 && j > last + 1) gaps += 1;
    last = j;
    i += 1;
  }
  return i === n.length ? 2 + gaps : -1;
}
