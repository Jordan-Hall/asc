import { describe, expect, test } from "bun:test";
import {
  type AuthBridge,
  type AuthHost,
  AuthError,
  AscForgejoAuth,
  secretKey,
} from "../src/auth";

const tick = () => new Promise((r) => setTimeout(r, 0));

function setup(opts: { helper: boolean }) {
  const store = new Map<string, string>();
  const stored: string[] = [];
  const handlers: ((u: URL) => void)[] = [];
  const opened: string[] = [];
  const fetched: { url: string; body: string }[] = [];
  const timers: (() => void)[] = [];
  const clock = { t: 0 };
  const host: AuthHost = {
    uriScheme: "cursor",
    secrets: {
      get: async (k) => store.get(k),
      store: async (k, v) => void store.set(k, v),
      delete: async (k) => void store.delete(k),
    },
    asExternalUri: async (u) => u,
    openExternal: async (u) => (opened.push(u), true),
    onUri: (h) => (handlers.push(h), () => void handlers.splice(handlers.indexOf(h), 1)),
    promptPat: async () => " pat123 ",
    fetch: async (url, init) => {
      fetched.push({ url, body: init.body });
      return { ok: true, status: 200, json: async () => ({ access_token: "tok1" }) };
    },
    randomBytes: (n) => crypto.getRandomValues(new Uint8Array(n)),
    sha256: async (d) => new Uint8Array(await crypto.subtle.digest("SHA-256", d)),
    now: () => clock.t,
    setTimeout: (fn) => (timers.push(fn), () => {}),
  };
  const bridge: AuthBridge = {
    token: async () => undefined,
    storeToken: async (_h, t) => {
      if (!opts.helper) throw Object.assign(new Error("x"), { code: "NO_CREDENTIAL_HELPER" });
      stored.push(t);
    },
  };
  const changed: (string | undefined)[] = [];
  const auth = new AscForgejoAuth(host, bridge, "forge.example", (t) => changed.push(t));
  const stateOf = () => new URL(opened[0] as string).searchParams.get("state") as string;
  const fire = (qs: string) => handlers[0]?.(new URL(`cursor://asc.asc/auth?${qs}`));
  return { auth, store, stored, opened, fetched, timers, changed, clock, stateOf, fire };
}

describe("auth", () => {
  test("OAuth PKCE stores via StoreToken when a helper exists", async () => {
    const s = setup({ helper: true });
    const p = s.auth.createSession();
    await tick();
    const q = new URL(s.opened[0] as string);
    expect(q.protocol).toBe("https:");
    expect(q.searchParams.get("client_id")).toBe("asc-ide");
    expect(q.searchParams.get("code_challenge_method")).toBe("S256");
    expect(q.searchParams.get("redirect_uri")).toBe("cursor://asc.asc/auth");
    expect(s.stateOf().length).toBeGreaterThanOrEqual(22);
    s.fire(`code=abc&state=${s.stateOf()}`);
    const sess = await p;
    expect(sess.accessToken).toBe("tok1");
    expect(s.stored).toEqual(["tok1"]);
    expect(s.store.size).toBe(0);
    expect(s.fetched[0]?.url.startsWith("https://")).toBe(true);
    expect(s.fetched[0]?.body).toContain("code_verifier=");
  });

  test("NO_CREDENTIAL_HELPER falls back to SecretStorage and notifies", async () => {
    const s = setup({ helper: false });
    const p = s.auth.createSession();
    await tick();
    s.fire(`code=abc&state=${s.stateOf()}`);
    await p;
    expect(s.store.get(secretKey("forge.example"))).toBe("tok1");
    expect(s.changed).toEqual(["tok1"]);
    expect((await s.auth.getSessions())[0]?.accessToken).toBe("tok1");
    await s.auth.removeSession();
    expect(s.store.size).toBe(0);
    expect(s.changed).toEqual(["tok1", undefined]);
  });

  test("wrong state is ignored; state is single use", async () => {
    const s = setup({ helper: true });
    const p = s.auth.createSession();
    await tick();
    s.fire("code=evil&state=nope");
    expect(s.fetched.length).toBe(0);
    s.fire(`code=abc&state=${s.stateOf()}`);
    s.fire(`code=again&state=${s.stateOf()}`);
    await p;
    expect(s.fetched.length).toBe(1);
  });

  test("expired state is ignored; timeout rejects", async () => {
    const s = setup({ helper: true });
    const p = s.auth.createSession();
    p.catch(() => {});
    await tick();
    s.clock.t = 5 * 60 * 1000 + 1;
    s.fire(`code=abc&state=${s.stateOf()}`);
    expect(s.fetched.length).toBe(0);
    s.timers[0]?.();
    await expect(p).rejects.toBeInstanceOf(AuthError);
  });

  test("denied authorization rejects", async () => {
    const s = setup({ helper: true });
    const p = s.auth.createSession();
    await tick();
    s.fire(`error=access_denied&state=${s.stateOf()}`);
    await expect(p).rejects.toMatchObject({ code: "DENIED" });
  });

  test("pasted PAT is trimmed and stored", async () => {
    const s = setup({ helper: true });
    const sess = await s.auth.createSessionWithPat();
    expect(sess.accessToken).toBe("pat123");
    expect(s.stored).toEqual(["pat123"]);
  });

  test("rejects hosts that could alter the URL", () => {
    for (const h of ["a/b", "evil.com@x", "http://x", "a b", ""]) {
      expect(() => new AscForgejoAuth({} as AuthHost, {} as AuthBridge, h)).toThrow();
    }
  });
});
