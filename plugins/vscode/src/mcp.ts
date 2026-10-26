// MCP registration (SPEC 24.4.8). The extension registers `asc mcp` (stdio, one per asc root) or, for a
// workspace-less agent, `asc mcp --remote <server url>`. It injects no token and runs no OAuth: the
// `asc` process finds its credential through the git credential helper (SPEC 15.1/15.6), so the
// `asc-ide` token never reaches an agent channel. It never registers both a stdio definition and
// the server's `/mcp` for one repo: roots win, and the remote definition exists only without roots.
// Editor APIs come through `McpApi`, so the logic runs under Bun; `vscodeMcpApi` is the only adapter.
import { execFile } from "node:child_process";
import type * as vscodeTypes from "vscode";

export interface Disposable {
  dispose(): void;
}

/** One stdio server definition, in the editor-neutral form. Never carries `env` secrets. */
export interface McpStdio {
  label: string;
  command: string;
  args: string[];
  cwd?: string;
}

export interface McpApi {
  /** `lm.registerMcpServerDefinitionProvider` when the editor has it, else null. */
  registerProvider:
    | ((id: string, provide: () => McpStdio[], changed: (fire: () => void) => void) => Disposable)
    | null;
  /** Cursor's `cursor.mcp.registerServer` when present, else null. */
  cursorRegister: ((def: McpStdio) => Disposable | undefined) | null;
}

export interface McpOptions {
  api: McpApi;
  /** The resolved asc executable, or null when none was found. */
  ascPath: () => string | null;
  /** Folders that are asc workspaces. */
  roots: () => readonly string[];
  /** The configured server URL for a workspace-less agent, or "". */
  serverUrl: () => string;
}

function noop(): void {}

export const PROVIDER_ID = "asc";

/** True for an `https://` URL with a host and no credentials. Other schemes are refused. */
export function isSafeServerUrl(raw: string): boolean {
  try {
    const u = new URL(raw);
    return u.protocol === "https:" && u.hostname !== "" && u.username === "" && u.password === "";
  } catch {
    return false;
  }
}

/** The definitions to offer. Stdio per root; remote only when there is no root and the URL is safe. */
export function definitions(
  ascPath: string | null,
  roots: readonly string[],
  serverUrl: string,
): McpStdio[] {
  if (ascPath === null) return [];
  if (roots.length > 0) {
    return roots.map((root, i) => ({
      label: roots.length === 1 ? "asc" : `asc (${i + 1})`,
      command: ascPath,
      args: ["mcp"],
      cwd: root,
    }));
  }
  if (!isSafeServerUrl(serverUrl)) return [];
  return [{ label: "asc-remote", command: ascPath, args: ["mcp", "--remote", serverUrl] }];
}

export interface McpRegistration extends Disposable {
  /** Re-reads roots and settings and tells the editor the list changed. */
  refresh(): void;
  readonly mode: "provider" | "cursor" | "none";
}

export function registerMcp(opts: McpOptions): McpRegistration {
  const current = () => definitions(opts.ascPath(), opts.roots(), opts.serverUrl());
  const { api } = opts;

  if (api.registerProvider) {
    const box = { fire: noop };
    const sub = api.registerProvider(PROVIDER_ID, current, (f) => {
      box.fire = f;
    });
    return { mode: "provider", refresh: () => box.fire(), dispose: () => sub.dispose() };
  }

  if (api.cursorRegister) {
    const register = api.cursorRegister;
    let subs: Disposable[] = [];
    const apply = () => {
      for (const s of subs) s.dispose();
      subs = current().flatMap((d) => {
        const s = register(d);
        return s ? [s] : [];
      });
    };
    apply();
    return {
      mode: "cursor",
      refresh: apply,
      dispose() {
        for (const s of subs) s.dispose();
        subs = [];
      },
    };
  }

  return { mode: "none", refresh() {}, dispose() {} };
}

/** The command line for "Configure agents": `asc mcp install --scope project`, run in the root. */
export function installArgs(): string[] {
  return ["mcp", "install", "--scope", "project"];
}

export type RunAsc = (ascPath: string, args: string[], cwd: string) => Promise<void>;

export const nodeRunAsc: RunAsc = (ascPath, args, cwd) =>
  new Promise((resolve, reject) => {
    execFile(ascPath, args, { cwd, windowsHide: true, timeout: 60_000 }, (err) =>
      err ? reject(err) : resolve(),
    );
  });

/** Runs "Configure agents". Returns a user-facing message; never throws. */
export async function configureAgents(
  ascPath: string | null,
  roots: readonly string[],
  run: RunAsc = nodeRunAsc,
): Promise<string> {
  const root = roots[0];
  if (ascPath === null) return "asc was not found; set asc.path.";
  if (root === undefined) return "Open an ASC workspace to configure agents.";
  try {
    await run(ascPath, installArgs(), root);
    return "Agents configured for this project (asc mcp install --scope project).";
  } catch {
    return "asc mcp install failed; run it in a terminal to see why.";
  }
}

// --- the real adapter ---------------------------------------------------------------------------

type CursorApi = {
  mcp?: {
    registerServer?: (cfg: {
      name: string;
      server: { command: string; args: string[]; env?: Record<string, string> };
    }) => void;
    unregisterServer?: (name: string) => void;
  };
};

export function vscodeMcpApi(vscode: typeof vscodeTypes): McpApi {
  const lm = (vscode as { lm?: Partial<typeof vscodeTypes.lm> }).lm;
  const cursor = (vscode as { cursor?: CursorApi }).cursor;
  const registerProvider =
    typeof lm?.registerMcpServerDefinitionProvider === "function"
      ? (id: string, provide: () => McpStdio[], changed: (fire: () => void) => void) => {
          const emitter = new vscode.EventEmitter<void>();
          changed(() => emitter.fire());
          const reg = lm.registerMcpServerDefinitionProvider?.(id, {
            onDidChangeMcpServerDefinitions: emitter.event,
            provideMcpServerDefinitions: () =>
              provide().map((d) => {
                const def = new vscode.McpStdioServerDefinition(d.label, d.command, d.args, {});
                if (d.cwd !== undefined) def.cwd = vscode.Uri.file(d.cwd);
                return def;
              }),
          });
          return {
            dispose() {
              reg?.dispose();
              emitter.dispose();
            },
          };
        }
      : null;
  const cursorRegister =
    typeof cursor?.mcp?.registerServer === "function"
      ? (def: McpStdio): Disposable => {
          cursor.mcp?.registerServer?.({
            name: def.label,
            server: { command: def.command, args: def.args },
          });
          return { dispose: () => cursor.mcp?.unregisterServer?.(def.label) };
        }
      : null;
  return { registerProvider, cursorRegister };
}
