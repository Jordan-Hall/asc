// Sign-in to any host (SPEC 24.6). The extension knows no forge: a host is a bare `host[:port]` the person
// names in `asc.hosts` or that an open root's remote uses. A token is stored by `Bridge.StoreToken` (the
// git credential helper, shared with the CLI and `asc mcp`); with no helper configured it is kept in VS Code
// SecretStorage and handed to the bridge as `ASC_TOKEN`. Rules that keep a token from reaching a host it was
// not given for:
//   - the token is typed for one host and keyed by that host; changing the host asks again;
//   - `ASC_TOKEN` is global in asc (it answers for every host), so the fallback token is injected only when
//     every open root that uses a token host names the same host as the token;
//   - nothing here logs, shows or puts a token in an argument; errors are fixed text.
// Editor-free; `registerAuth` is the only `vscode` user.
import { BridgeError, bridge, callUnary } from "@asc/asc-client";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "./bridge";
import type { CliDirect } from "./cli-direct";

/** Longest accepted token; the bridge refuses longer ones too. */
export const MAX_TOKEN = 16 * 1024;
const MAX_HOST = 253 + 6;
export const SECRET_PREFIX = "asc.asc.token.";

/** gRPC status numbers the bridge uses for these two cases. */
const GRPC_INVALID_ARGUMENT = 3;
const GRPC_NOT_FOUND = 5;

const LABEL = /^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/;

/**
 * A bare `host` or `host:port` from what a person typed or a setting holds: lowercase, https only, no user
 * information, path, query, fragment or whitespace. Returns null for anything else (fail closed).
 */
export function normalizeHost(input: string): string | null {
  let text = input.trim().toLowerCase();
  if (text === "" || text.length > MAX_HOST) return null;
  for (const ch of text) {
    const c = ch.charCodeAt(0);
    if (c <= 0x20 || c === 0x7f || ch === "\\" || ch === "@" || ch === "?" || ch === "#") return null;
  }
  if (text.includes("://")) {
    if (!text.startsWith("https://")) return null;
    text = text.slice("https://".length);
    if (text.endsWith("/")) text = text.slice(0, -1);
  }
  if (text.includes("/")) return null;
  let port = "";
  const colon = text.lastIndexOf(":");
  if (colon >= 0) {
    port = text.slice(colon + 1);
    text = text.slice(0, colon);
    if (!/^[1-9]\d{0,4}$/.test(port) || Number(port) > 65535) return null;
  }
  const labels = text.split(".");
  if (labels.length === 0 || labels.some((l) => l.length === 0 || l.length > 63 || !LABEL.test(l))) {
    return null;
  }
  // An all-numeric last label is an address, not a name; sign-in is by host name.
  if (/^\d+$/.test(labels[labels.length - 1] ?? "")) return null;
  return port === "" ? text : `${text}:${port}`;
}

/** The token host of a root's remote URL (`RootInfo.remote_url`); null for ssh, scp-like or local remotes. */
export function hostOfRemote(remoteUrl: string): string | null {
  if (!remoteUrl.toLowerCase().startsWith("https://")) return null;
  const rest = remoteUrl.slice("https://".length);
  const end = rest.search(/[/?#]/);
  const authority = end < 0 ? rest : rest.slice(0, end);
  // Credentials are removed in `remote_url`; a URL that still has any is not a host to trust.
  if (authority.includes("@")) return null;
  return normalizeHost(authority);
}

/** A pasted token: trimmed, non-empty, bounded, one line. Null when it is not usable. */
export function cleanToken(raw: string): string | null {
  const t = raw.trim();
  if (t === "" || t.length > MAX_TOKEN) return null;
  for (const ch of t) {
    const c = ch.charCodeAt(0);
    if (c < 0x20 || c === 0x7f) return null;
  }
  return t;
}

export class NoCredentialHelper extends Error {
  constructor() {
    super("no credential helper is configured");
  }
}

export interface AuthBridge {
  /** `Bridge.Token`; null when no credential is stored (NOT_FOUND). */
  token(host: string): Promise<string | null>;
  /** `Bridge.StoreToken`; throws `NoCredentialHelper` when none is configured. */
  storeToken(host: string, token: string): Promise<void>;
}

export interface TokenStore {
  get(host: string): Promise<string | undefined>;
  set(host: string, token: string): Promise<void>;
  delete(host: string): Promise<void>;
}

export function bridgeAuth(mgr: BridgeManager): AuthBridge {
  return {
    async token(host) {
      try {
        const r = await callUnary(mgr.node(), bridge.Bridge.method.token, { host });
        return r.token === "" ? null : r.token;
      } catch (e) {
        if (e instanceof BridgeError && e.code === GRPC_NOT_FOUND) return null;
        throw new Error("asc could not look up the credential");
      }
    },
    async storeToken(host, token) {
      try {
        await callUnary(mgr.node(), bridge.Bridge.method.storeToken, { host, token });
      } catch (e) {
        if (e instanceof BridgeError && e.message.includes("NO_CREDENTIAL_HELPER")) {
          throw new NoCredentialHelper();
        }
        // The bridge message can echo request fields; show a fixed text and never the token.
        throw new Error(
          e instanceof BridgeError && e.code === GRPC_INVALID_ARGUMENT
            ? "asc refused the host or the token"
            : "asc could not store the credential",
        );
      }
    },
  };
}

export type SignInWhere = "helper" | "secret-storage";
export type AuthState = "helper" | "secret-storage" | "signed-out";

export interface AuthServiceOptions {
  bridge: AuthBridge;
  store: TokenStore;
  /** Forgets the credential in the git credential helpers (`asc auth logout --host`). */
  forget?: (host: string) => Promise<void>;
  /** Called when the fallback token set changes, so the bridge can respawn with the right `ASC_TOKEN`. */
  onFallbackChanged?: () => void;
}

export class AuthService {
  /** Hosts whose fallback token is loaded; the bridge environment reads this synchronously. */
  private readonly cache = new Map<string, string>();
  /** The token hosts of the open roots; empty until `reconcile` runs. */
  private rootHosts: string[] = [];

  constructor(private readonly o: AuthServiceOptions) {}

  /** Stores `token` for `host`: the credential helper first, else SecretStorage. */
  async signIn(hostInput: string, tokenInput: string): Promise<SignInWhere> {
    const host = normalizeHost(hostInput);
    if (host === null) throw new RangeError("Enter a host such as forge.example.org (https only).");
    const token = cleanToken(tokenInput);
    if (token === null) throw new RangeError("Enter the token on one line.");
    try {
      await this.o.bridge.storeToken(host, token);
    } catch (e) {
      if (!(e instanceof NoCredentialHelper)) throw e;
      await this.o.store.set(host, token);
      this.cache.set(host, token);
      this.o.onFallbackChanged?.();
      return "secret-storage";
    }
    // A helper holds it now and is the single source; an older fallback copy would shadow a rotation.
    if (this.cache.delete(host) || (await this.o.store.get(host)) !== undefined) {
      await this.o.store.delete(host);
      this.o.onFallbackChanged?.();
    }
    return "helper";
  }

  /** Forgets the credential for `host` in SecretStorage and in the credential helpers. */
  async signOut(hostInput: string): Promise<void> {
    const host = normalizeHost(hostInput);
    if (host === null) throw new RangeError("Enter a host such as forge.example.org (https only).");
    const had = this.cache.delete(host);
    await this.o.store.delete(host);
    try {
      await this.o.forget?.(host);
    } finally {
      if (had) this.o.onFallbackChanged?.();
    }
  }

  /** Where a credential for `host` would come from; never the credential itself. */
  async state(hostInput: string): Promise<AuthState> {
    const host = normalizeHost(hostInput);
    if (host === null) return "signed-out";
    if ((await this.o.store.get(host)) !== undefined) return "secret-storage";
    return (await this.o.bridge.token(host)) !== null ? "helper" : "signed-out";
  }

  /**
   * Loads the fallback tokens of `candidates` and records the token hosts of the open roots. Returns true
   * when the environment the bridge must run with changed.
   */
  async reconcile(candidates: readonly string[], rootHosts: readonly string[]): Promise<boolean> {
    const before = this.injected();
    this.rootHosts = [...new Set(rootHosts)];
    for (const h of new Set([...candidates, ...rootHosts])) {
      if (this.cache.has(h)) continue;
      const t = await this.o.store.get(h);
      if (t !== undefined) this.cache.set(h, t);
    }
    return this.injected() !== before;
  }

  /**
   * `ASC_TOKEN` for the bridge: the fallback token of the one host the open roots use. `ASC_TOKEN` answers
   * for every host, so with roots on two hosts, or a root on a host with no fallback token, nothing is set.
   */
  env(): Record<string, string | undefined> {
    const t = this.injected();
    return t === undefined ? {} : { ASC_TOKEN: t };
  }

  private injected(): string | undefined {
    if (this.rootHosts.length !== 1) return undefined;
    const host = this.rootHosts[0];
    return host === undefined ? undefined : this.cache.get(host);
  }

  /** True when a fallback token exists for a host an open root uses, but is not injected (two hosts). */
  get withheld(): boolean {
    return this.rootHosts.length > 1 && this.rootHosts.some((h) => this.cache.has(h));
  }
}

/** `asc auth logout --host` through the fixed-argv runner; a failure is not an error (nothing to forget). */
export function cliForget(cli: CliDirect, root: string): (host: string) => Promise<void> {
  return async (host) => {
    await cli.run(root, { kind: "auth-logout", host }).catch(() => undefined);
  };
}

// --- commands ------------------------------------------------------------------------------------

/** What the commands need from the editor. */
export interface AuthUi {
  /** A choice among `hosts`, or free text; resolves the host the person chose or typed. */
  pickHost(hosts: readonly string[], placeholder: string): Promise<string | undefined>;
  /** A masked input for the token of `host`; resolves undefined when dismissed. */
  askToken(host: string): Promise<string | undefined>;
  /** A modal confirmation; true only when the person chose the button. */
  confirm(message: string, detail: string, button: string): Promise<boolean>;
  info(message: string): void;
  warn(message: string): void;
  error(message: string): void;
}

/** The `asc.hosts` setting as host names; entries that do not parse are dropped. */
export function hostsFromSetting(raw: unknown): string[] {
  if (!Array.isArray(raw)) return [];
  const out: string[] = [];
  for (const v of raw) {
    const h = typeof v === "string" ? normalizeHost(v) : null;
    if (h !== null && !out.includes(h)) out.push(h);
  }
  return out;
}

export interface AuthCommandsOptions {
  service: AuthService;
  ui: AuthUi;
  /** The `asc.hosts` setting. */
  settingHosts(): readonly string[];
  /** The token hosts of the open roots' remotes. */
  rootHosts(): readonly string[];
}

export const SECRET_STORAGE_NOTE =
  "No git credential helper is configured, so the token is kept in VS Code secret storage and the asc CLI does not share it. Set credential.helper to share one sign-in.";

export class AuthCommands {
  private noted = false;

  constructor(private readonly o: AuthCommandsOptions) {}

  /** Hosts to offer: the setting first, then the hosts the open roots use. */
  candidates(): string[] {
    return [...new Set([...this.o.settingHosts(), ...this.o.rootHosts()])];
  }

  /** Asks for a host and a token, then stores the token. The token is asked for on every sign-in. */
  async signIn(): Promise<boolean> {
    const typed = await this.o.ui.pickHost(this.candidates(), "Host to sign in to");
    if (typed === undefined) return false;
    const host = normalizeHost(typed);
    if (host === null) {
      this.o.ui.error("ASC: that is not a host name. Use a name such as forge.example.org (https only).");
      return false;
    }
    const token = await this.o.ui.askToken(host);
    if (token === undefined) return false;
    try {
      const where = await this.o.service.signIn(host, token);
      this.o.ui.info(`ASC: signed in to ${host}.`);
      if (where === "secret-storage" && !this.noted) {
        this.noted = true;
        this.o.ui.warn(SECRET_STORAGE_NOTE);
      }
      return true;
    } catch (e) {
      this.o.ui.error(`ASC: sign-in failed. ${e instanceof RangeError ? e.message : fixedMessage(e)}`);
      return false;
    }
  }

  /** Removes the stored credential of a host after a confirmation. */
  async signOut(): Promise<boolean> {
    const typed = await this.o.ui.pickHost(this.candidates(), "Host to sign out of");
    if (typed === undefined) return false;
    const host = normalizeHost(typed);
    if (host === null) {
      this.o.ui.error("ASC: that is not a host name.");
      return false;
    }
    const ok = await this.o.ui.confirm(
      `Sign out of ${host}?`,
      "The stored credential for this host is removed. Other hosts are not affected.",
      "Sign out",
    );
    if (!ok) return false;
    try {
      await this.o.service.signOut(host);
      this.o.ui.info(`ASC: signed out of ${host}.`);
      return true;
    } catch (e) {
      this.o.ui.error(`ASC: sign-out failed. ${fixedMessage(e)}`);
      return false;
    }
  }

  /** One line per candidate host: where its credential comes from, never the credential. */
  async status(): Promise<string[]> {
    const lines: string[] = [];
    for (const h of this.candidates()) {
      let s: AuthState;
      try {
        s = await this.o.service.state(h);
      } catch {
        s = "signed-out";
      }
      lines.push(
        `${h}: ${s === "helper" ? "signed in (credential helper)" : s === "secret-storage" ? "signed in (VS Code secret storage)" : "signed out"}`,
      );
    }
    return lines;
  }
}

/** A message safe to show: the text of an error this module threw, else a fixed line. */
function fixedMessage(e: unknown): string {
  return e instanceof Error && e.message.startsWith("asc ") ? `${e.message}.` : "Try again.";
}

// --- the editor adapter ----------------------------------------------------------------------------

/** SecretStorage under the key `asc.asc.token.<host>`. */
export function secretTokenStore(secrets: vscodeTypes.SecretStorage): TokenStore {
  return {
    get: async (host) => (await secrets.get(SECRET_PREFIX + host)) ?? undefined,
    set: (host, token) => Promise.resolve(secrets.store(SECRET_PREFIX + host, token)),
    delete: (host) => Promise.resolve(secrets.delete(SECRET_PREFIX + host)),
  };
}

const OTHER_HOST = "$(add) Another host...";

export function vscodeAuthUi(vscode: typeof vscodeTypes): AuthUi {
  return {
    async pickHost(hosts, placeholder) {
      if (hosts.length > 0) {
        const items = [...hosts.map((label) => ({ label })), { label: OTHER_HOST }];
        const hit = await vscode.window.showQuickPick(items, { placeHolder: placeholder });
        if (hit === undefined) return undefined;
        if (hit.label !== OTHER_HOST) return hit.label;
      }
      return vscode.window.showInputBox({
        prompt: `${placeholder} (a host name such as forge.example.org; https only)`,
        ignoreFocusOut: true,
        validateInput: (v) => (normalizeHost(v) === null ? "Enter a host name, for example forge.example.org" : undefined),
      });
    },
    askToken: (host) =>
      Promise.resolve(
        vscode.window.showInputBox({
          prompt: `Access token for ${host}`,
          placeHolder: "Paste a personal access token",
          password: true,
          ignoreFocusOut: true,
          validateInput: (v) => (v === "" || cleanToken(v) !== null ? undefined : "The token must be on one line"),
        }),
      ),
    async confirm(message, detail, button) {
      return (await vscode.window.showWarningMessage(message, { modal: true, detail }, button)) === button;
    },
    info: (m) => void vscode.window.showInformationMessage(m),
    warn: (m) => void vscode.window.showWarningMessage(m),
    error: (m) => void vscode.window.showErrorMessage(m),
  };
}

/** Registers `asc.signIn`, `asc.signOut` and `asc.authStatus`. */
export function registerAuth(
  vscode: typeof vscodeTypes,
  service: AuthService,
  rootHosts: () => readonly string[],
  log: (line: string) => void,
): { dispose(): void } {
  const cmds = new AuthCommands({
    service,
    ui: vscodeAuthUi(vscode),
    settingHosts: () => hostsFromSetting(vscode.workspace.getConfiguration("asc").get("hosts", [])),
    rootHosts,
  });
  const out = (what: string, ok: boolean) => log(`asc ${what}: ${ok ? "done" : "not done"}`);
  return vscode.Disposable.from(
    vscode.commands.registerCommand("asc.signIn", async () => out("sign in", await cmds.signIn())),
    vscode.commands.registerCommand("asc.signOut", async () => out("sign out", await cmds.signOut())),
    vscode.commands.registerCommand("asc.authStatus", async () => {
      const lines = await cmds.status();
      void vscode.window.showInformationMessage(
        lines.length === 0 ? "ASC: no host is set. Add one to asc.hosts, or sign in." : lines.join("\n"),
      );
    }),
  );
}
