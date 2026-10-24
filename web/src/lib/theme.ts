// Theme preference: light, dark or system. `public/theme-init.js` applies the same rule before first
// paint; keep THEME_KEY and the class name in step with it.
import { useCallback, useEffect, useSyncExternalStore } from "react";

export type ThemePref = "light" | "dark" | "system";
export type Theme = "light" | "dark";

export const THEME_KEY = "asc-theme";

export function parsePref(raw: string | null | undefined): ThemePref {
  return raw === "light" || raw === "dark" ? raw : "system";
}

export function resolveTheme(pref: ThemePref, systemDark: boolean): Theme {
  return pref === "system" ? (systemDark ? "dark" : "light") : pref;
}

export function readPref(storage: Pick<Storage, "getItem"> | undefined): ThemePref {
  try {
    return parsePref(storage?.getItem(THEME_KEY));
  } catch {
    return "system";
  }
}

export function writePref(
  storage: Pick<Storage, "setItem" | "removeItem"> | undefined,
  pref: ThemePref,
): void {
  try {
    if (pref === "system") storage?.removeItem(THEME_KEY);
    else storage?.setItem(THEME_KEY, pref);
  } catch {
    // Storage can be blocked; the choice then lasts for this page view only.
  }
}

export function applyTheme(root: HTMLElement, theme: Theme): void {
  root.classList.toggle("dark", theme === "dark");
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
  const pref = useSyncExternalStore(subscribe, snapshot, () => "system" as const);
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
  useEffect(() => applyTheme(document.documentElement, theme), [theme]);
  const setPref = useCallback((p: ThemePref) => {
    current = p;
    writePref(typeof localStorage === "undefined" ? undefined : localStorage, p);
    for (const l of listeners) l();
  }, []);
  return { pref, theme, setPref };
}
