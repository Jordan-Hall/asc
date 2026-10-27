import { afterAll, afterEach, describe, expect, test } from "bun:test";
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
  commonTokenHost,
  hostOfRemote,
  hostsFromSetting,
  NoCredentialHelper,
  normalizeHost,
  type RootRemote,
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
    expect(
      hostsFromSetting(["A.example.org", "a.example.org", "bad host", 3, "http://x.org"]),
    ).toEqual(["a.example.org"]);
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

const root = (host: string, over: Partial<RootRemote> = {}): RootRemote => ({
  wsId: "ws1",
  remoteUrl: `https://${host}/o/r.git`,
  ...over,
});

describe("auth: remote hosts are checked strictly", () => {
  test("an address, a URL with credentials, an unparsable or odd URL gives no host", () => {
    for (const bad of [
      "https://127.0.0.1/o/r",
      "https://10.0.0.1:8443/o/r",
      "https://[::1]/o/r",
      "https://0x7f.1/o/r",
      "https://2130706433/o/r",
      "https://user@forge.example.org/o/r",
      "https://:pw@forge.example.org/o/r",
      "https://forge.example.org@evil.example.net/o/r",
      "https://forge.example.org\\@evil.example.net/o/r",
      "https://forge.example.org\n.evil.example.net/o/r",
      "https://forge.example.org\t/o/r",
      "https://",
      "https:///o/r",
      "https://forge.example.org:99999/o/r",
      "https://forge.example.org./o/r",
      "https:forge.example.org/o/r",
      " https://forge.example.org/o/r",
    ]) {
      expect(hostOfRemote(bad)).toBeNull();
    }
  });
  test("the default port is dropped so it equals the host a person signed in with", () => {
    expect(hostOfRemote("https://Forge.Example.org:443/o/r")).toBe("forge.example.org");
  });
  test("one root that does not qualify, or no root, gives no common host", () => {
    const ok = root("a.example.org");
    expect(commonTokenHost([ok, root("a.example.org")])).toBe("a.example.org");
    expect(commonTokenHost([])).toBeNull();
    expect(commonTokenHost([ok, root("b.example.org")])).toBeNull();
    expect(commonTokenHost([ok, root("a.example.org", { wsId: "" })])).toBeNull();
    expect(commonTokenHost([ok, { wsId: "ws2", remoteUrl: "" }])).toBeNull();
    expect(
      commonTokenHost([ok, { wsId: "ws2", remoteUrl: "ssh://git@a.example.org/o/r" }]),
    ).toBeNull();
    expect(
      commonTokenHost([ok, { wsId: "ws2", remoteUrl: "http://a.example.org/o/r" }]),
    ).toBeNull();
    expect(commonTokenHost([ok, { wsId: "ws2", remoteUrl: "https://10.0.0.1/o/r" }])).toBeNull();
    expect(
      commonTokenHost([ok, { wsId: "ws2", remoteUrl: "https://u:p@a.example.org/o/r" }]),
    ).toBeNull();
  });
});

describe("auth: the fallback token reaches the bridge only for the host it was given for", () => {
  async function loaded(roots: RootRemote[], stored: Record<string, string>) {
    const { m, store } = memoryStore();
    for (const [h, t] of Object.entries(stored)) m.set(h, t);
    const s = new AuthService({ bridge: fakeBridge({ helper: false }).b, store });
    const changed = await s.reconcile([], roots);
    return { s, changed };
  }
  test("one host with a token: injected with the host it is for", async () => {
    const { s, changed } = await loaded([root("a.example.org")], { "a.example.org": "ta" });
    expect(s.env()).toEqual({ ASC_TOKEN: "ta", ASC_TOKEN_HOST: "a.example.org" });
    expect(changed).toBe(true);
  });
  test("a non-default port is part of the host", async () => {
    const { s } = await loaded([root("a.example.org:8443")], {
      "a.example.org:8443": "ta",
      "a.example.org": "tb",
    });
    expect(s.env()).toEqual({ ASC_TOKEN: "ta", ASC_TOKEN_HOST: "a.example.org:8443" });
  });
  test("a token for another host is not injected", async () => {
    const { s, changed } = await loaded([root("a.example.org")], { "b.example.org": "tb" });
    expect(s.env()).toEqual({});
    expect(changed).toBe(false);
  });
  test("two hosts: nothing is injected, even when both have tokens", async () => {
    const { s } = await loaded([root("a.example.org"), root("b.example.org")], {
      "a.example.org": "ta",
      "b.example.org": "tb",
    });
    expect(s.env()).toEqual({});
    expect(s.withheld).toBe(true);
  });
  test("two roots on one host: injected", async () => {
    const { s } = await loaded([root("a.example.org"), root("a.example.org")], {
      "a.example.org": "ta",
    });
    expect(s.env()).toEqual({ ASC_TOKEN: "ta", ASC_TOKEN_HOST: "a.example.org" });
    expect(s.withheld).toBe(false);
  });
  test("no root: nothing", async () => {
    const { s } = await loaded([], { "a.example.org": "ta" });
    expect(s.env()).toEqual({});
  });
  test("one unqualified root among matching ones: nothing is injected", async () => {
    const stored = { "a.example.org": "ta" };
    for (const odd of [
      root("a.example.org", { wsId: "" }),
      { wsId: "ws2", remoteUrl: "" },
      { wsId: "ws2", remoteUrl: "ssh://git@a.example.org/o/r" },
      { wsId: "ws2", remoteUrl: "https://192.168.0.1/o/r" },
      { wsId: "ws2", remoteUrl: "https://u:p@a.example.org/o/r" },
      { wsId: "ws2", remoteUrl: "not a url" },
    ]) {
      const { s } = await loaded([root("a.example.org"), odd], stored);
      expect(s.env()).toEqual({});
    }
  });
  test("a second reconcile with the same inputs reports no change", async () => {
    const { s } = await loaded([root("a.example.org")], { "a.example.org": "ta" });
    expect(await s.reconcile([], [root("a.example.org")])).toBe(false);
  });
  test("a root that stops qualifying withdraws the token, and clear withdraws it too", async () => {
    const { s } = await loaded([root("a.example.org")], { "a.example.org": "ta" });
    expect(
      await s.reconcile([], [root("a.example.org"), root("a.example.org", { wsId: "" })]),
    ).toBe(true);
    expect(s.env()).toEqual({});
    expect(await s.reconcile([], [root("a.example.org")])).toBe(true);
    expect(s.clear()).toBe(true);
    expect(s.env()).toEqual({});
    expect(s.clear()).toBe(false);
  });
});

function fakeUi(script: {
  host?: string | undefined;
  token?: string | undefined;
  confirm?: boolean;
}) {
  const log = {
    info: [] as string[],
    warn: [] as string[],
    error: [] as string[],
    tokenAsks: [] as string[],
    picks: [] as string[][],
  };
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

function commands(
  script: Parameters<typeof fakeUi>[0],
  helper: boolean,
  setting: string[] = [],
  root: string[] = [],
) {
  const { m, store } = memoryStore();
  const f = fakeBridge({ helper });
  const u = fakeUi(script);
  const service = new AuthService({ bridge: f.b, store });
  const cmds = new AuthCommands({
    service,
    ui: u.ui,
    settingHosts: () => setting,
    rootHosts: () => root,
  });
  return { cmds, ...u, f, m, service };
}

describe("auth: commands", () => {
  test("sign-in asks for a token every time, for the chosen host", async () => {
    const c = commands(
      { host: "a.example.org", token: "t1" },
      true,
      ["b.example.org"],
      ["a.example.org"],
    );
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
    const cmds2 = new AuthCommands({
      service: c.service,
      ui: next.ui,
      settingHosts: () => [],
      rootHosts: () => [],
    });
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
    const cmds = new AuthCommands({
      service,
      ui: u.ui,
      settingHosts: () => [],
      rootHosts: () => [],
    });
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
    const c = commands({ host: "a.example.org", token: "tok" }, false, [
      "a.example.org",
      "b.example.org",
    ]);
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
    expect(
      await bridgeAuth(mgr(new BridgeError(5, "NOT_FOUND: none"))).token("a.example.org"),
    ).toBeNull();
  });
  test("NO_CREDENTIAL_HELPER on StoreToken is the fallback signal", async () => {
    await expect(
      bridgeAuth(mgr(new BridgeError(9, "NO_CREDENTIAL_HELPER: set credential.helper"))).storeToken(
        "a.example.org",
        "t",
      ),
    ).rejects.toBeInstanceOf(NoCredentialHelper);
  });
  test("other errors become fixed text without the request fields", async () => {
    const err = new BridgeError(13, "internal: token=hunter2 failed");
    const e = await bridgeAuth(mgr(err))
      .storeToken("a.example.org", "hunter2")
      .then(
        () => new Error("no error"),
        (x: unknown) => x as Error,
      );
    expect(e.message).toBe("asc could not store the credential");
    const t = await bridgeAuth(mgr(err))
      .token("a.example.org")
      .then(
        () => new Error("no error"),
        (x: unknown) => x as Error,
      );
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

// The activation path: the bridge starts again only when the injected environment differs from the one
// the running child was started with.
describe("auth: activation", () => {
  const dirs: string[] = [];
  const ascFolder = () => {
    const d = mkdtempSync(join(tmpdir(), "asc-auth-"));
    dirs.push(d);
    mkdirSync(join(d, ".git"));
    writeFileSync(join(d, ".git", "asc-workspace"), "");
    return d;
  };
  const exeDir = mkdtempSync(join(tmpdir(), "asc-auth-exe-"));
  const exe = join(exeDir, process.platform === "win32" ? "asc.exe" : "asc");
  writeFileSync(exe, "");
  afterAll(() => rmSync(exeDir, { recursive: true, force: true }));
  afterEach(() => {
    for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
  });

  type Env = Record<string, string | undefined>;
  function setup(opts: {
    reconcile: (n: number) => Env | Promise<Env>;
    helloFails?: (n: number) => boolean;
  }) {
    const folder = ascFolder();
    const spawns: (Env | undefined)[] = [];
    const closes: number[] = [];
    let helloCalls = 0;
    let wanted: Env = {};
    let reconciles = 0;
    let cleared = 0;
    const cmds = new Map<string, () => void>();
    const host = {
      workspaceFolders: () => [folder],
      ascPathSetting: () => exe,
      gitEnabled: () => false,
      disableBuiltinGit: async () => {},
      notify: async () => undefined,
      registerCommand: (id: string, run: () => void) => {
        cmds.set(id, run);
        return { dispose() {} };
      },
      setContext: async () => {},
      log() {},
      showOutput() {},
      memento: { get: () => true, set: async () => {} },
      env: { PATH: "/bin", ASC_TOKEN: "inherited" },
      platform: process.platform,
      bridgeEnv: () => wanted,
      reconcileAuth: async () => {
        reconciles++;
        wanted = await opts.reconcile(reconciles);
      },
      clearAuth: () => {
        cleared++;
        wanted = {};
      },
    } as unknown as Host;
    const create_ = (o: { env?: Env }): BridgeNode => {
      spawns.push(o.env);
      const n = spawns.length;
      const reply = toBinary(
        bridge.HelloResponseSchema,
        create(bridge.HelloResponseSchema, {
          ascVersion: "1",
          features: ["plugin.v1", "json-verbs.v1"],
        }),
      );
      return {
        state: "up",
        pid: undefined,
        stderrTail: () => [],
        unary: async () => {
          helloCalls++;
          if (opts.helloFails?.(helloCalls)) throw new Error("bridge gone");
          return reply;
        },
        stream: async function* () {},
        close() {
          closes.push(n);
        },
      };
    };
    return { host, create_, spawns, closes, cmds, state: () => ({ reconciles, cleared, wanted }) };
  }

  const TOKEN = { ASC_TOKEN: "from-secret-storage", ASC_TOKEN_HOST: "a.example.org" };

  test("the bridge starts again once when the stored token changes its environment", async () => {
    const t = setup({ reconcile: () => TOKEN });
    const a = await activateWith(t.host, t.create_ as never);
    expect(await a.started).toEqual({ level: "full" });
    expect(t.state().reconciles).toBe(2);
    expect(t.spawns).toHaveLength(2);
    expect(t.spawns[0]?.ASC_TOKEN).toBe("inherited");
    expect(t.spawns[0]?.ASC_TOKEN_HOST).toBeUndefined();
    expect(t.spawns[1]?.ASC_TOKEN).toBe("from-secret-storage");
    expect(t.spawns[1]?.ASC_TOKEN_HOST).toBe("a.example.org");
    expect(t.closes).toEqual([1]);
    a.dispose();
  });

  test("an unchanged environment does not restart the bridge, on any later start", async () => {
    const t = setup({ reconcile: () => ({}) });
    const a = await activateWith(t.host, t.create_ as never);
    await a.started;
    expect(t.spawns).toHaveLength(1);
    expect(t.state().reconciles).toBe(1);
    // asc.restartBridge closes the child and starts again: one new child, and reconcile runs again.
    t.cmds.get("asc.restartBridge")?.();
    await new Promise((r) => setTimeout(r, 20));
    expect(t.spawns).toHaveLength(2);
    expect(t.state().reconciles).toBe(2);
    expect(t.closes).toEqual([1]);
    a.dispose();
  });

  test("a token the roots no longer vouch for is dropped and the child restarted without it", async () => {
    let n = 0;
    const t = setup({ reconcile: () => (++n <= 2 ? TOKEN : {}) });
    const a = await activateWith(t.host, t.create_ as never);
    await a.started;
    // Start 0 spawns without a token, reconcile wants one, so spawn 2 carries it and keeps it.
    expect(t.spawns.at(-1)?.ASC_TOKEN).toBe("from-secret-storage");
    const before = t.spawns.length;
    t.cmds.get("asc.restartBridge")?.();
    await new Promise((r) => setTimeout(r, 20));
    // The restart's Hello is read with the token the child gets (still wanted at spawn time), the
    // check then finds the roots no longer qualify, and the child is started again without it.
    expect(t.spawns.length).toBe(before + 2);
    expect(t.spawns.at(-2)?.ASC_TOKEN).toBe("from-secret-storage");
    expect(t.spawns.at(-1)?.ASC_TOKEN).toBe("inherited");
    expect(t.spawns.at(-1)?.ASC_TOKEN_HOST).toBeUndefined();
    a.dispose();
  });

  test("when the hosts cannot be re-checked the injected token is cleared", async () => {
    let n = 0;
    const t = setup({
      reconcile: async () => {
        n++;
        if (n >= 3) throw new Error("hello did not parse");
        return TOKEN;
      },
    });
    const a = await activateWith(t.host, t.create_ as never);
    await a.started;
    expect(t.spawns.at(-1)?.ASC_TOKEN).toBe("from-secret-storage");
    const before = t.spawns.length;
    t.cmds.get("asc.restartBridge")?.();
    await new Promise((r) => setTimeout(r, 20));
    expect(t.state().cleared).toBeGreaterThan(0);
    expect(t.spawns.length).toBe(before + 2);
    expect(t.spawns.at(-1)?.ASC_TOKEN).toBe("inherited");
    a.dispose();
  });

  test("a Hello that fails clears the token and the child is not left holding it", async () => {
    let n = 0;
    const t = setup({ reconcile: () => TOKEN, helloFails: () => ++n === 3 });
    const a = await activateWith(t.host, t.create_ as never);
    await a.started;
    const before = t.spawns.length;
    const closed = t.closes.length;
    t.cmds.get("asc.restartBridge")?.();
    await new Promise((r) => setTimeout(r, 20));
    expect(t.state().cleared).toBeGreaterThan(0);
    expect(t.state().wanted).toEqual({});
    // restart() on the command, then the failed Hello's own restart: the token is not re-injected.
    expect(t.closes.length).toBeGreaterThan(closed);
    expect(t.spawns.length).toBe(before + 1);
    a.dispose();
  });

  test("a start that cannot settle gives up on the token instead of restarting forever", async () => {
    let n = 0;
    // The wanted token alternates every call, so it never matches the child's.
    const t = setup({
      reconcile: () =>
        ++n % 2 === 1 ? TOKEN : { ASC_TOKEN: "other", ASC_TOKEN_HOST: "b.example.org" },
    });
    const a = await activateWith(t.host, t.create_ as never);
    expect(await a.started).toEqual({ level: "full" });
    expect(t.spawns.length).toBeLessThanOrEqual(5);
    expect(t.state().cleared).toBeGreaterThan(0);
    expect(t.spawns.at(-1)?.ASC_TOKEN).toBe("inherited");
    a.dispose();
  });
});
