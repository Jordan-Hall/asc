// Theme preference: asc-light, asc-dark or asc-auto (follows prefers-color-scheme). The choice is
// written to <html data-theme> and the resolved scheme to the `dark` class. `public/theme-init.js`
// applies the same rule before first paint; keep THEME_KEY, the names and the class in step with it.
import { useCallback, useEffect, useSyncExternalStore } from "react";

export type ThemePref = "asc-light" | "asc-dark" | "asc-auto";
export type Theme = "light" | "dark";

export const THEME_KEY = "asc-theme";

/** The three themes in menu order, with the words people see. */
export const THEME_CHOICES: readonly { value: ThemePref; label: string }[] = [
  { value: "asc-auto", label: "Match system" },
  { value: "asc-light", label: "Light" },
  { value: "asc-dark", label: "Dark" },
];

/** Unknown values mean asc-auto. The bare "light" and "dark" of earlier versions still parse. */
export function parsePref(raw: string | null | undefined): ThemePref {
  if (raw === "asc-light" || raw === "light") return "asc-light";
  if (raw === "asc-dark" || raw === "dark") return "asc-dark";
  return "asc-auto";
}

export function resolveTheme(pref: ThemePref, systemDark: boolean): Theme {
  if (pref === "asc-auto") return systemDark ? "dark" : "light";
  return pref === "asc-dark" ? "dark" : "light";
}

export function readPref(storage: Pick<Storage, "getItem"> | undefined): ThemePref {
  try {
    return parsePref(storage?.getItem(THEME_KEY));
  } catch {
    return "asc-auto";
  }
}

export function writePref(
  storage: Pick<Storage, "setItem" | "removeItem"> | undefined,
  pref: ThemePref,
): void {
  try {
    if (pref === "asc-auto") storage?.removeItem(THEME_KEY);
    else storage?.setItem(THEME_KEY, pref);
  } catch {
    // Storage can be blocked; the choice then lasts for this page view only.
  }
}

export function applyTheme(root: HTMLElement, theme: Theme, pref?: ThemePref): void {
  root.classList.toggle("dark", theme === "dark");
  if (pref) root.dataset.theme = pref;
  root.style.colorScheme = theme;
}

const listeners = new Set<() => void>();
let current: ThemePref | undefined;

function snapshot(): ThemePref {
  current ??= readPref(typeof localStorage === "undefined" ? undefined : localStorage);
  return current;
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

export function useTheme(): { pref: ThemePref; theme: Theme; setPref: (p: ThemePref) => void } {
  const pref = useSyncExternalStore(subscribe, snapshot, () => "asc-auto" as const);
  const systemDark = useSyncExternalStore(
    (cb) => {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      mq.addEventListener("change", cb);
      return () => mq.removeEventListener("change", cb);
    },
    () => window.matchMedia("(prefers-color-scheme: dark)").matches,
    () => false,
  );
  const theme = resolveTheme(pref, systemDark);
  useEffect(() => applyTheme(document.documentElement, theme, pref), [theme, pref]);
  const setPref = useCallback((p: ThemePref) => {
    current = p;
    writePref(typeof localStorage === "undefined" ? undefined : localStorage, p);
    for (const l of listeners) l();
  }, []);
  return { pref, theme, setPref };
}
