// A media query as a hook. The server render and the first client render answer `false`, so a
// screen that swaps whole layouts (desktop board / mobile board) starts on the desktop one and
// switches before paint on a phone only after hydration, never mid-render.
import { useSyncExternalStore } from "react";

/** The width below which the mobile boards apply (the boards are 1440 and 390 wide). */
export const MOBILE_QUERY = "(max-width: 767.98px)";

export function useMedia(query: string): boolean {
  return useSyncExternalStore(
    (cb) => {
      if (typeof matchMedia === "undefined") return () => undefined;
      const mq = matchMedia(query);
      mq.addEventListener("change", cb);
      return () => mq.removeEventListener("change", cb);
    },
    () => (typeof matchMedia === "undefined" ? false : matchMedia(query).matches),
    () => false,
  );
}

export function useMobile(): boolean {
  return useMedia(MOBILE_QUERY);
}
