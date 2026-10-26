import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type BridgeNode, BridgeError, bridge } from "@asc/asc-client";
import { create, toBinary } from "@bufbuild/protobuf";
import {
  AuthCommands,
  type AuthBridge,
  AuthService,
  type AuthUi,
  bridgeAuth,
  cleanToken,
  hostOfRemote,
  hostsFromSetting,
  NoCredentialHelper,
  normalizeHost,
  SECRET_STORAGE_NOTE,
  type TokenStore,
} from "../src/auth";
import { cliArgv } from "../src/cli-direct";
import { activateWith, type Host } from "../src/extension";

describe("auth: host names", () => {
  test("accepts bare hosts and https origins and normalises them", () => {
    expect(normalizeHost("Forge.Example.org")).toBe("forge.example.org");
    expect(normalizeHost(" forge.example.org:8443 ")).toBe("forge.example.org:8443");
    expect(normalizeHost("https://forge.example.org/")).toBe("forge.example.org");
    expect(normalizeHost("https://forge.example.org:3000")).toBe("forge.example.org:3000");
    expect(normalizeHost("a-b.c1.dev")).toBe("a-b.c1.dev");
  });
  test("refuses anything that could change where a token goes", () => {
    for (const bad of [
      "",
      " ",
      "http://forge.example.org",
      "ssh://forge.example.org",
      "https://user@forge.example.org",
      "https://user:pw@forge.example.org",
      "forge.example.org/path",
      "https://forge.example.org/path",
      "https://forge.example.org?x=1",
      "https://forge.example.org#f",
      "forge.example.org:0",
      "forge.example.org:99999",
      "forge.example.org:80:80",
      "-forge.example.org",
      "forge-.example.org",
      "forge..example.org",
      "forge example.org",
      "forge.example.org\nevil.org",
      "forge\\example.org",
      "evil.org@forge.example.org",
      "127.0.0.1",
      "10.0.0.1:3000",
      "[::1]",
      "a".repeat(300),
      `${"a".repeat(64)}.org`,
      "--exec=x",
      "-c",
    ]) {
      expect(normalizeHost(bad)).toBeNull();
    }
  });
  test("a remote URL gives its https host, and ssh or local remotes give none", () => {
    expect(hostOfRemote("https://forge.example.org/o/r.git")).toBe("forge.example.org");
    expect(hostOfRemote("https://forge.example.org:8443/o/r")).toBe("forge.example.org:8443");
    expect(hostOfRemote("HTTPS://Forge.Example.org")).toBe("forge.example.org");
    expect(hostOfRemote("ssh://git@forge.example.org/o/r")).toBeNull();
    expect(hostOfRemote("git@forge.example.org:o/r.git")).toBeNull();
    expect(hostOfRemote("http://forge.example.org/o/r")).toBeNull();
    expect(hostOfRemote("https://u:p@forge.example.org/o/r")).toBeNull();
    expect(hostOfRemote("/srv/repo")).toBeNull();
    expect(hostOfRemote("")).toBeNull();
  });
  test("the setting keeps valid, unique hosts", () => {
    expect(hostsFromSetting(["A.example.org", "a.example.org", "bad host", 3, "http://x.org"])).toEqual([
      "a.example.org",
    ]);
    expect(hostsFromSetting("a.example.org")).toEqual([]);
    expect(hostsFromSetting(undefined)).toEqual([]);
  });
  test("a token is one non-empty line of bounded length", () => {
    expect(cleanToken("  abc123  ")).toBe("abc123");
    for (const bad of ["", "   ", "a\nb", "a\rb", "a\0b", "x".repeat(16 * 1024 + 1)]) {
      expect(cleanToken(bad)).toBeNull();
    }
  });
});

function memoryStore() {
  const m = new Map<string, string>();
  const store: TokenStore = {
    get: async (h) => m.get(h),
    set: async (h, t) => void m.set(h, t),
    delete: async (h) => void m.delete(h),
  };
  return { m, store };
}

function fakeBridge(opts: { helper: boolean; stored?: Record<string, string> }) {
  const stored = new Map(Object.entries(opts.stored ?? {}));
  const calls: { host: string; token: string }[] = [];
  const b: AuthBridge = {
    token: async (h) => stored.get(h) ?? null,
    storeToken: async (host, token) => {
      calls.push({ host, token });
      if (!opts.helper) throw new NoCredentialHelper();
      stored.set(host, token);
    },
  };
  return { b, calls, stored };
}

describe("auth: service", () => {
  test("a credential helper takes the token and nothing is kept in secret storage", async () => {
    const { m, store } = memoryStore();
    const f = fakeBridge({ helper: true });
    let changed = 0;
    const s = new AuthService({ bridge: f.b, store, onFallbackChanged: () => void changed++ });
    expect(await s.signIn("Forge.example.org", " tok-1 ")).toBe("helper");
    expect(f.calls).toEqual([{ host: "forge.example.org", token: "tok-1" }]);
    expect(m.size).toBe(0);
    expect(changed).toBe(0);
    expect(await s.state("forge.example.org")).toBe("helper");
  });
  test("with no helper the token goes to secret storage under its own host", async () => {
    const { m, store } = memoryStore();
    const f = fakeBridge({ helper: false });
    let changed = 0;
    const s = new AuthService({ bridge: f.b, store, onFallbackChanged: () => void changed++ });
    expect(await s.signIn("forge.example.org", "tok")).toBe("secret-storage");
    expect([...m]).toEqual([["forge.example.org", "tok"]]);
    expect(changed).toBe(1);
    expect(await s.state("forge.example.org")).toBe("secret-storage");
    expect(await s.state("other.example.org")).toBe("signed-out");
  });
  test("a helper sign-in replaces an older fallback copy", async () => {
    const { m, store } = memoryStore();
    m.set("forge.example.org", "old");
    const f = fakeBridge({ helper: true });
    let changed = 0;
    const s = new AuthService({ bridge: f.b, store, onFallbackChanged: () => void changed++ });
    await s.signIn("forge.example.org", "new");
    expect(m.size).toBe(0);
    expect(changed).toBe(1);
  });
  test("a bad host or token is refused before anything is sent", async () => {
    const { m, store } = memoryStore();
    const f = fakeBridge({ helper: true });
    const s = new AuthService({ bridge: f.b, store });
    await expect(s.signIn("http://forge.example.org", "t")).rejects.toThrow(RangeError);
    await expect(s.signIn("forge.example.org", "a\nb")).rejects.toThrow(RangeError);
    await expect(s.signIn("forge.example.org", "")).rejects.toThrow(RangeError);
    expect(f.calls).toHaveLength(0);
    expect(m.size).toBe(0);
  });
  test("other bridge errors propagate and store nothing", async () => {
    const { m, store } = memoryStore();
    const s = new AuthService({
      bridge: {
        token: async () => null,
        storeToken: async () => {
          throw new Error("asc could not store the credential");
        },
      },
      store,
    });
    await expect(s.signIn("forge.example.org", "t")).rejects.toThrow("could not store");
    expect(m.size).toBe(0);
  });
  test("sign-out forgets the fallback token and asks the helpers to forget", async () => {
    const { m, store } = memoryStore();
    const forgotten: string[] = [];
    const f = fakeBridge({ helper: false });
    let changed = 0;
    const s = new AuthService({
      bridge: f.b,
      store,
      forget: async (h) => void forgotten.push(h),
      onFallbackChanged: () => void changed++,
    });
    await s.signIn("forge.example.org", "tok");
    await s.signOut("Forge.example.org");
    expect(m.size).toBe(0);
    expect(forgotten).toEqual(["forge.example.org"]);
    expect(changed).toBe(2);
    await expect(s.signOut("http://x.org")).rejects.toThrow(RangeError);
  });
});

describe("auth: the fallback token reaches the bridge only for the host it was given for", () => {
  async function loaded(root: string[], stored: Record<string, string>) {
    const { m, store } = memoryStore();
    for (const [h, t] of Object.entries(stored)) m.set(h, t);
    const s = new AuthService({ bridge: fakeBridge({ helper: false }).b, store });
    const changed = await s.reconcile([], root);
    return { s, changed };
  }
  test("one host with a token: injected", async () => {
    const { s, changed } = await loaded(["a.example.org"], { "a.example.org": "ta" });
    expect(s.env()).toEqual({ ASC_TOKEN: "ta" });
    expect(changed).toBe(true);
  });
  test("a token for another host is not injected", async () => {
    const { s, changed } = await loaded(["a.example.org"], { "b.example.org": "tb" });
    expect(s.env()).toEqual({});
    expect(changed).toBe(false);
  });
  test("two hosts: nothing is injected, even when both have tokens", async () => {
    const { s } = await loaded(["a.example.org", "b.example.org"], {
      "a.example.org": "ta",
      "b.example.org": "tb",
    });
    expect(s.env()).toEqual({});
    expect(s.withheld).toBe(true);
  });
  test("two roots on one host: injected", async () => {
    const { s } = await loaded(["a.example.org", "a.example.org"], { "a.example.org": "ta" });
    expect(s.env()).toEqual({ ASC_TOKEN: "ta" });
    expect(s.withheld).toBe(false);
  });
  test("no https root: nothing", async () => {
    const { s } = await loaded([], { "a.example.org": "ta" });
    expect(s.env()).toEqual({});
  });
  test("a second reconcile with the same inputs reports no change", async () => {
    const { s } = await loaded(["a.example.org"], { "a.example.org": "ta" });
    expect(await s.reconcile([], ["a.example.org"])).toBe(false);
  });
});

function fakeUi(script: { host?: string | undefined; token?: string | undefined; confirm?: boolean }) {
  const log = { info: [] as string[], warn: [] as string[], error: [] as string[], tokenAsks: [] as string[], picks: [] as string[][] };
  const ui: AuthUi = {
    async pickHost(hosts) {
      log.picks.push([...hosts]);
      return script.host;
    },
    async askToken(host) {
      log.tokenAsks.push(host);
      return script.token;
    },
    async confirm() {
      return script.confirm ?? true;
    },
    info: (m) => void log.info.push(m),
    warn: (m) => void log.warn.push(m),
    error: (m) => void log.error.push(m),
  };
  return { ui, log };
}

function commands(script: Parameters<typeof fakeUi>[0], helper: boolean, setting: string[] = [], root: string[] = []) {
  const { m, store } = memoryStore();
  const f = fakeBridge({ helper });
  const u = fakeUi(script);
  const service = new AuthService({ bridge: f.b, store });
  const cmds = new AuthCommands({ service, ui: u.ui, settingHosts: () => setting, rootHosts: () => root });
  return { cmds, ...u, f, m, service };
}

describe("auth: commands", () => {
  test("sign-in asks for a token every time, for the chosen host", async () => {
    const c = commands({ host: "a.example.org", token: "t1" }, true, ["b.example.org"], ["a.example.org"]);
    expect(await c.cmds.signIn()).toBe(true);
    expect(await c.cmds.signIn()).toBe(true);
    expect(c.log.tokenAsks).toEqual(["a.example.org", "a.example.org"]);
    expect(c.log.picks[0]).toEqual(["b.example.org", "a.example.org"]);
    expect(c.f.calls.every((x) => x.host === "a.example.org")).toBe(true);
  });
  test("changing the host asks for the token again; the old token never moves to the new host", async () => {
    const c = commands({ host: "a.example.org", token: "ta" }, true);
    await c.cmds.signIn();
    const next = fakeUi({ host: "b.example.org", token: undefined });
    const cmds2 = new AuthCommands({ service: c.service, ui: next.ui, settingHosts: () => [], rootHosts: () => [] });
    expect(await cmds2.signIn()).toBe(false);
    expect(next.log.tokenAsks).toEqual(["b.example.org"]);
    expect(c.f.calls).toEqual([{ host: "a.example.org", token: "ta" }]);
  });
  test("dismissing either prompt does nothing", async () => {
    const a = commands({ host: undefined, token: "t" }, true);
    expect(await a.cmds.signIn()).toBe(false);
    expect(a.log.tokenAsks).toEqual([]);
    const b = commands({ host: "a.example.org", token: undefined }, true);
    expect(await b.cmds.signIn()).toBe(false);
    expect(b.f.calls).toEqual([]);
  });
  test("a host that is not a host name is an error and asks nothing", async () => {
    const c = commands({ host: "https://a.example.org/evil", token: "t" }, true);
    expect(await c.cmds.signIn()).toBe(false);
    expect(c.log.error).toHaveLength(1);
    expect(c.log.tokenAsks).toEqual([]);
  });
  test("the secret-storage note shows once, and no message carries the token", async () => {
    const c = commands({ host: "a.example.org", token: "s3cret-token-value" }, false);
    await c.cmds.signIn();
    await c.cmds.signIn();
    expect(c.log.warn).toEqual([SECRET_STORAGE_NOTE]);
    for (const line of [...c.log.info, ...c.log.warn, ...c.log.error]) {
      expect(line).not.toContain("s3cret-token-value");
    }
  });
  test("a failure shows fixed text without the token", async () => {
    const { store } = memoryStore();
    const service = new AuthService({
      bridge: {
        token: async () => null,
        storeToken: async () => {
          throw new Error("boom s3cret-token-value");
        },
      },
      store,
    });
    const u = fakeUi({ host: "a.example.org", token: "s3cret-token-value" });
    const cmds = new AuthCommands({ service, ui: u.ui, settingHosts: () => [], rootHosts: () => [] });
    expect(await cmds.signIn()).toBe(false);
    expect(u.log.error.join(" ")).not.toContain("s3cret-token-value");
    expect(u.log.error.join(" ")).not.toContain("boom");
  });
  test("sign-out needs a confirmation", async () => {
    const no = commands({ host: "a.example.org", confirm: false }, false);
    await no.service.signIn("a.example.org", "t");
    expect(await no.cmds.signOut()).toBe(false);
    expect(no.m.size).toBe(1);
    const yes = commands({ host: "a.example.org", confirm: true }, false);
    await yes.service.signIn("a.example.org", "t");
    expect(await yes.cmds.signOut()).toBe(true);
    expect(yes.m.size).toBe(0);
  });
  test("status names the source for each host and never the token", async () => {
    const c = commands({ host: "a.example.org", token: "tok" }, false, ["a.example.org", "b.example.org"]);
    await c.cmds.signIn();
    expect(await c.cmds.status()).toEqual([
      "a.example.org: signed in (VS Code secret storage)",
      "b.example.org: signed out",
    ]);
  });
});

describe("auth: bridge errors", () => {
  const node = (err: BridgeError): { node(): BridgeNode } => ({
    node: () =>
      ({
        state: "up",
        pid: undefined,
        stderrTail: () => [],
        unary: async () => {
          throw err;
        },
        stream: async function* () {},
        close() {},
      }) as BridgeNode,
  });
  const mgr = (err: BridgeError) => node(err) as never;
  test("NOT_FOUND on Token is no credential", async () => {
    expect(await bridgeAuth(mgr(new BridgeError(5, "NOT_FOUND: none"))).token("a.example.org")).toBeNull();
  });
  test("NO_CREDENTIAL_HELPER on StoreToken is the fallback signal", async () => {
    await expect(
      bridgeAuth(mgr(new BridgeError(9, "NO_CREDENTIAL_HELPER: set credential.helper"))).storeToken("a.example.org", "t"),
    ).rejects.toBeInstanceOf(NoCredentialHelper);
  });
  test("other errors become fixed text without the request fields", async () => {
    const err = new BridgeError(13, "internal: token=hunter2 failed");
    const e = await bridgeAuth(mgr(err))
      .storeToken("a.example.org", "hunter2")
      .then(() => new Error("no error"), (x: unknown) => x as Error);
    expect(e.message).toBe("asc could not store the credential");
    const t = await bridgeAuth(mgr(err))
      .token("a.example.org")
      .then(() => new Error("no error"), (x: unknown) => x as Error);
    expect(t.message).toBe("asc could not look up the credential");
  });
  test("the logout argv is built only from a bare host", () => {
    expect(cliArgv({ kind: "auth-logout", host: "a.example.org:8443" })).toEqual([
      "auth",
      "logout",
      "--host",
      "https://a.example.org:8443",
    ]);
    for (const host of ["", "-x", "a b", "a.org/x", "https://a.org", "A.org", "a.org@evil"]) {
      expect(cliArgv({ kind: "auth-logout", host })).toBeNull();
    }
  });
});

// The activation path: a fallback token makes the bridge start again once, with ASC_TOKEN set.
describe("auth: activation", () => {
  const dirs: string[] = [];
  const ascFolder = () => {
    const d = mkdtempSync(join(tmpdir(), "asc-auth-"));
    dirs.push(d);
    mkdirSync(join(d, ".git"));
    writeFileSync(join(d, ".git", "asc-workspace"), "");
    return d;
  };
  test("the bridge starts again once when the stored token changes its environment", async () => {
    const folder = ascFolder();
    const spawns: (Record<string, string | undefined> | undefined)[] = [];
    let token: string | undefined;
    let reconciles = 0;
    const host = {
      workspaceFolders: () => [folder],
      ascPathSetting: () => "",
      gitEnabled: () => false,
      disableBuiltinGit: async () => {},
      notify: async () => undefined,
      registerCommand: () => ({ dispose() {} }),
      setContext: async () => {},
      log() {},
      showOutput() {},
      memento: { get: () => true, set: async () => {} },
      env: { PATH: "/bin", ASC_TOKEN: "inherited" },
      platform: process.platform,
      bridgeEnv: () => (token === undefined ? {} : { ASC_TOKEN: token }),
      reconcileAuth: async () => {
        reconciles++;
        const changed = token === undefined;
        token = "from-secret-storage";
        return changed;
      },
    } as unknown as Host;
    const create_ = (o: { env?: Record<string, string | undefined> }): BridgeNode => {
      spawns.push(o.env);
      const reply = toBinary(
        bridge.HelloResponseSchema,
        create(bridge.HelloResponseSchema, { ascVersion: "1", features: ["plugin.v1", "json-verbs.v1"] }),
      );
      return {
        state: "up",
        pid: undefined,
        stderrTail: () => [],
        unary: async () => reply,
        stream: async function* () {},
        close() {},
      };
    };
    const a = await activateWith(host, create_ as never);
    expect(await a.started).toEqual({ level: "full" });
    expect(reconciles).toBe(1);
    expect(spawns).toHaveLength(2);
    expect(spawns[0]?.ASC_TOKEN).toBe("inherited");
    expect(spawns[1]?.ASC_TOKEN).toBe("from-secret-storage");
    a.dispose();
    for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
  });
});
