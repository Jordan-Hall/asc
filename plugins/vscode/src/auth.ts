// SPEC §24.6: the `asc-forgejo` AuthenticationProvider core.
// The VS Code API is injected through `AuthHost` so the flow is testable without an
// extension host. TODO(P24): extension.ts constructs `AuthHost` from `vscode` and
// registers this class with `authentication.registerAuthenticationProvider`.

export const AUTH_PROVIDER_ID = "asc-forgejo";
export const CLIENT_ID = "asc-ide";
export const REDIRECT_AUTHORITY = "asc.asc";
export const REDIRECT_PATH = "/auth";
export const SCOPES = [
  "read:user",
  "read:repository",
  "write:repository",
  "read:issue",
  "write:issue",
] as const;
export const STATE_TTL_MS = 5 * 60 * 1000;

export const secretKey = (host: string): string => `asc.asc.token.${host}`;

/** Bridge calls (SPEC §24.6, P01c). `token` resolves undefined on NOT_FOUND. */
export interface AuthBridge {
  token(host: string): Promise<string | undefined>;
  /** Rejects with `code === "NO_CREDENTIAL_HELPER"` when no helper is configured. */
  storeToken(host: string, token: string): Promise<void>;
}

export interface SecretsLike {
  get(key: string): Promise<string | undefined>;
  store(key: string, value: string): Promise<void>;
  delete(key: string): Promise<void>;
}

export interface AuthHost {
  uriScheme: string;
  secrets: SecretsLike;
  /** `env.asExternalUri`; returns the URI string to use as the redirect. */
  asExternalUri(uri: string): Promise<string>;
  openExternal(url: string): Promise<boolean>;
  /** The URI handler (`onUri`); returns a disposer. */
  onUri(handler: (uri: URL) => void): () => void;
  /** Ask for a PAT with masked input; undefined when cancelled. */
  promptPat(host: string): Promise<string | undefined>;
  fetch(
    url: string,
    init: { method: string; headers: Record<string, string>; body: string },
  ): Promise<{ ok: boolean; status: number; json(): Promise<unknown> }>;
  randomBytes(n: number): Uint8Array;
  sha256(data: Uint8Array): Promise<Uint8Array>;
  now(): number;
  /** Returns a canceller. */
  setTimeout(fn: () => void, ms: number): () => void;
}

export interface AuthSession {
  id: string;
  accessToken: string;
  account: { id: string; label: string };
  scopes: readonly string[];
}

export class AuthError extends Error {
  constructor(
    readonly code: "TIMEOUT" | "DENIED" | "EXCHANGE_FAILED" | "INVALID_HOST" | "CANCELLED",
    message: string,
  ) {
    super(message);
    this.name = "AuthError";
  }
}

const b64url = (b: Uint8Array): string =>
  btoa(String.fromCharCode(...b))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");

/** Forge host: bare host[:port]. Rejects anything that could change the URL shape. */
export function assertForgeHost(host: string): void {
  if (!/^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?(:\d{1,5})?$/.test(host)) {
    throw new AuthError("INVALID_HOST", "invalid forge host");
  }
}

interface Pending {
  expires: number;
  resolve(u: URL): void;
}

export class AscForgejoAuth {
  private readonly listeners = new Set<() => void>();
  private readonly inflight = new Map<string, Pending>();

  constructor(
    private readonly host: AuthHost,
    private readonly bridge: AuthBridge,
    private readonly forgeHost: string,
    /** Called when the fallback token changes so the caller respawns the bridge with ASC_TOKEN. */
    private readonly onFallbackTokenChanged: (token: string | undefined) => void = () => {},
  ) {
    assertForgeHost(forgeHost);
  }

  onDidChangeSessions(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private fire(): void {
    for (const l of this.listeners) l();
  }

  private session(token: string): AuthSession {
    return {
      id: `${AUTH_PROVIDER_ID}:${this.forgeHost}`,
      accessToken: token,
      account: { id: this.forgeHost, label: this.forgeHost },
      scopes: SCOPES,
    };
  }

  /** `getSessions` calls `Bridge.Token`, then reads the SecretStorage fallback. */
  async getSessions(): Promise<AuthSession[]> {
    const t =
      (await this.bridge.token(this.forgeHost)) ??
      (await this.host.secrets.get(secretKey(this.forgeHost)));
    return t ? [this.session(t)] : [];
  }

  async createSession(): Promise<AuthSession> {
    return this.save(await this.oauth());
  }

  /** The alternative to OAuth: a pasted personal access token. */
  async createSessionWithPat(): Promise<AuthSession> {
    const pat = (await this.host.promptPat(this.forgeHost))?.trim();
    if (!pat) throw new AuthError("CANCELLED", "no token entered");
    return this.save(pat);
  }

  async removeSession(): Promise<void> {
    await this.host.secrets.delete(secretKey(this.forgeHost));
    this.onFallbackTokenChanged(undefined);
    this.fire();
  }

  private async save(token: string): Promise<AuthSession> {
    try {
      await this.bridge.storeToken(this.forgeHost, token);
      // A helper now holds it; drop any stale fallback so the helper is the single source.
      await this.host.secrets.delete(secretKey(this.forgeHost));
    } catch (e) {
      if ((e as { code?: string }).code !== "NO_CREDENTIAL_HELPER") throw e;
      await this.host.secrets.store(secretKey(this.forgeHost), token);
      this.onFallbackTokenChanged(token);
    }
    this.fire();
    return this.session(token);
  }

  private async oauth(): Promise<string> {
    const verifier = b64url(this.host.randomBytes(32));
    const challenge = b64url(await this.host.sha256(new TextEncoder().encode(verifier)));
    const state = b64url(this.host.randomBytes(16)); // 128 bits
    const redirect = await this.host.asExternalUri(
      `${this.host.uriScheme}://${REDIRECT_AUTHORITY}${REDIRECT_PATH}`,
    );

    const callback = new Promise<URL>((resolve, reject) => {
      const cancel = this.host.setTimeout(() => {
        this.inflight.delete(state);
        reject(new AuthError("TIMEOUT", "sign-in timed out"));
      }, STATE_TTL_MS);
      this.inflight.set(state, {
        expires: this.host.now() + STATE_TTL_MS,
        resolve: (u) => {
          cancel();
          resolve(u);
        },
      });
    });
    callback.catch(() => {});
    const dispose = this.host.onUri((uri) => this.handleUri(uri));
    try {
      const q = new URLSearchParams({
        response_type: "code",
        client_id: CLIENT_ID,
        redirect_uri: redirect,
        scope: SCOPES.join(" "),
        state,
        code_challenge: challenge,
        code_challenge_method: "S256",
      });
      await this.host.openExternal(`https://${this.forgeHost}/login/oauth/authorize?${q}`);
      const uri = await callback;
      const err = uri.searchParams.get("error");
      if (err) throw new AuthError("DENIED", `authorization denied: ${err}`);
      const code = uri.searchParams.get("code");
      if (!code) throw new AuthError("EXCHANGE_FAILED", "no authorization code");
      return await this.exchange(code, verifier, redirect);
    } finally {
      this.inflight.delete(state);
      dispose();
    }
  }

  /** Single-use state: a callback is ignored unless the state is known, unexpired and unused. */
  private handleUri(uri: URL): void {
    if (uri.pathname !== REDIRECT_PATH) return;
    const state = uri.searchParams.get("state");
    if (!state) return;
    const entry = this.inflight.get(state);
    if (!entry) return;
    this.inflight.delete(state);
    if (this.host.now() > entry.expires) return;
    entry.resolve(uri);
  }

  private async exchange(code: string, verifier: string, redirect: string): Promise<string> {
    const body = new URLSearchParams({
      grant_type: "authorization_code",
      client_id: CLIENT_ID,
      code,
      code_verifier: verifier,
      redirect_uri: redirect,
    }).toString();
    const res = await this.host.fetch(`https://${this.forgeHost}/login/oauth/access_token`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded", accept: "application/json" },
      body,
    });
    if (!res.ok) throw new AuthError("EXCHANGE_FAILED", `token endpoint returned ${res.status}`);
    const j = (await res.json()) as { access_token?: unknown };
    if (typeof j.access_token !== "string" || !j.access_token) {
      throw new AuthError("EXCHANGE_FAILED", "token endpoint returned no access_token");
    }
    return j.access_token;
  }
}
