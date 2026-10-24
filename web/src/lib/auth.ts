// The sign-in token (docs/WEB-UI.md section 9). It lives in memory, and in sessionStorage only when
// the user asks to stay signed in for the tab. Never localStorage, never a URL. The token is sent
// only as an Authorization header by the transport's `headers()` callback.
import { useSyncExternalStore } from "react";

export const TOKEN_KEY = "asc-token";

export interface AuthStore {
  /** The bearer token, or undefined when signed out. */
  token(): string | undefined;
  signIn(token: string, remember: boolean): void;
  signOut(): void;
  subscribe(cb: () => void): () => void;
  /** Header for the transport. */
  headers(): Record<string, string>;
}

export function createAuthStore(
  session: Pick<Storage, "getItem" | "setItem" | "removeItem"> | undefined,
): AuthStore {
  const listeners = new Set<() => void>();
  let value: string | undefined;
  try {
    value = session?.getItem(TOKEN_KEY) ?? undefined;
  } catch {
    value = undefined;
  }
  const emit = () => {
    for (const l of listeners) l();
  };
  return {
    token: () => value,
    signIn(token, remember) {
      value = token.trim() || undefined;
      try {
        if (value && remember) session?.setItem(TOKEN_KEY, value);
        else session?.removeItem(TOKEN_KEY);
      } catch {
        // Blocked storage: the token stays in memory for this page view.
      }
      emit();
    },
    signOut() {
      value = undefined;
      try {
        session?.removeItem(TOKEN_KEY);
      } catch {
        // Nothing stored that we could remove.
      }
      emit();
    },
    subscribe(cb) {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    headers: () => (value ? { authorization: `Bearer ${value}` } : {}),
  };
}

export function useToken(store: AuthStore): string | undefined {
  return useSyncExternalStore(store.subscribe, store.token, () => undefined);
}
