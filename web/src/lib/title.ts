import { useEffect } from "react";

/** Sets the document title for the screen; the shell announces it on navigation. */
export function useTitle(title: string): void {
  useEffect(() => {
    document.title = title ? `${title} · ASC Hub` : "ASC Hub";
  }, [title]);
}
