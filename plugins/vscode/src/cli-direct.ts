// Read-mostly verbs the bridge does not carry. `Bridge.Run` refuses `op log` (its result is an
// `OpLogResponse`, which `RunEvent` cannot hold) and its argv schema has no `cloud status` or `live`
// entries, so the extension runs these few fixed argv itself, with the same rules as the bridge: no shell,
// the null device as stdin, an environment rebuilt from a short list, the workspace root as the working
// directory, a time limit and a cap on the output. The argv is built here from typed inputs and nowhere
// else; nothing a webview or a repository supplies reaches it.
import { spawn } from "node:child_process";

/** Longest a verb may run before it is killed. */
export const CLI_TIMEOUT_MS = 15_000;
/** Most stdout bytes read; a longer result is an error, never a partial parse. */
export const CLI_MAX_STDOUT = 2 * 1024 * 1024;
/** Most stderr bytes kept for the log. */
const MAX_STDERR = 16 * 1024;
/** Most entries `op log` is asked for (the allowlist cap of the bridge verb). */
export const OP_LOG_MAX = 500;

export type CliCall =
  | { kind: "op-log"; limit: number }
  | { kind: "cloud-status" }
  | { kind: "live"; action: "status" | "pause" | "resume" }
  /** Forgets the credential the git credential helpers hold for a bare `host[:port]` (https). */
  | { kind: "auth-logout"; host: string };

const HOST = /^[a-z0-9]([a-z0-9.-]*[a-z0-9])?(:[0-9]{1,5})?$/;

/** The argv of `call`, or null when an input is out of range. */
export function cliArgv(call: CliCall): string[] | null {
  switch (call.kind) {
    case "op-log":
      return Number.isInteger(call.limit) && call.limit >= 1 && call.limit <= OP_LOG_MAX
        ? ["op", "log", "--json", "-n", String(call.limit)]
        : null;
    case "cloud-status":
      return ["cloud", "status", "--json"];
    case "live":
      if (call.action === "status") return ["live", "status", "--json"];
      return call.action === "pause" || call.action === "resume" ? ["live", call.action] : null;
    case "auth-logout":
      return typeof call.host === "string" && call.host.length <= 259 && HOST.test(call.host)
        ? ["auth", "logout", "--host", `https://${call.host}`]
        : null;
    default:
      return null;
  }
}

/** Variables copied to the child; nothing else is, so a stray token or `GIT_*` setting never leaks in. */
const KEEP = [
  "PATH",
  "HOME",
  "USERPROFILE",
  "SystemRoot",
  "SYSTEMROOT",
  "TEMP",
  "TMP",
  "TMPDIR",
  "APPDATA",
  "LOCALAPPDATA",
  "ProgramData",
  "ComSpec",
  "PATHEXT",
  "USERNAME",
  "USER",
  "LANG",
  "LC_ALL",
  "XDG_CONFIG_HOME",
  "XDG_DATA_HOME",
  "XDG_CACHE_HOME",
  "XDG_RUNTIME_DIR",
  "SSH_AUTH_SOCK",
  "ASC_HOME",
  "ASC_REMOTE",
  "ASC_SERVER",
  "ASC_SERVER_URL",
  "ASC_TOKEN",
  "ASC_NO_DAEMON",
  "ASC_DAEMON_ENDPOINT",
  "ASC_HOST_CACHE",
  "ASC_BRANCHLESS",
] as const;

/** The child's environment from `source`, with the fixed values that stop every prompt. */
export function childEnv(
  source: Record<string, string | undefined>,
  extra: Record<string, string | undefined> = {},
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const k of KEEP) {
    const v = source[k];
    if (v !== undefined) out[k] = v;
  }
  for (const [k, v] of Object.entries(extra)) if (v !== undefined && v !== "") out[k] = v;
  out.ASC_NO_ALIASES = "1";
  out.GIT_TERMINAL_PROMPT = "0";
  out.GIT_EDITOR = "true";
  out.GCM_INTERACTIVE = "never";
  return out;
}

export interface CliResult {
  exit: number;
  stdout: string;
  stderr: string[];
}

export interface CliOptions {
  /** The asc executable, resolved on every call so a changed `asc.path` takes effect. */
  command: () => string;
  env?: () => Record<string, string | undefined>;
  /** Extra variables for the child, such as a token the host holds (`ASC_TOKEN`). */
  extraEnv?: () => Record<string, string | undefined>;
  timeoutMs?: number;
  /** Arguments placed before the verb, for a launcher such as `node script.js` (tests); empty for asc. */
  prefixArgs?: readonly string[];
}

export interface CliDirect {
  run(root: string, call: CliCall, signal?: AbortSignal): Promise<CliResult>;
}

export function createCliDirect(o: CliOptions): CliDirect {
  return {
    run(root, call, signal) {
      const argv = cliArgv(call);
      if (argv === null) return Promise.reject(new RangeError("asc: the call is out of range"));
      if (root === "" || root.includes("\0")) {
        return Promise.reject(new RangeError("asc: the root is not a path"));
      }
      return new Promise<CliResult>((resolve, reject) => {
        if (signal?.aborted) {
          reject(new Error("asc: cancelled"));
          return;
        }
        const child = spawn(o.command(), [...(o.prefixArgs ?? []), ...argv], {
          cwd: root,
          env: childEnv(o.env?.() ?? process.env, o.extraEnv?.()),
          stdio: ["ignore", "pipe", "pipe"],
          shell: false,
          windowsHide: true,
        });
        const out: Buffer[] = [];
        let outBytes = 0;
        let tooBig = false;
        const err: string[] = [];
        let errBytes = 0;
        let settled = false;
        const finish = (fn: () => void) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          signal?.removeEventListener("abort", onAbort);
          fn();
        };
        const kill = () => {
          if (!child.killed) child.kill();
        };
        const timer = setTimeout(() => {
          kill();
          finish(() => reject(new Error("asc: the verb timed out")));
        }, o.timeoutMs ?? CLI_TIMEOUT_MS);
        const onAbort = () => {
          kill();
          finish(() => reject(new Error("asc: cancelled")));
        };
        signal?.addEventListener("abort", onAbort, { once: true });
        child.stdout.on("data", (b: Buffer) => {
          outBytes += b.length;
          if (outBytes > CLI_MAX_STDOUT) {
            tooBig = true;
            kill();
            return;
          }
          out.push(b);
        });
        child.stderr.on("data", (b: Buffer) => {
          if (errBytes >= MAX_STDERR) return;
          errBytes += b.length;
          for (const l of b.toString("utf8").split(/\r?\n/)) if (l.trim() !== "") err.push(l);
        });
        child.on("error", () => finish(() => reject(new Error("asc: cannot start the asc child"))));
        child.on("close", (code) =>
          finish(() => {
            if (tooBig) reject(new Error("asc: the verb's result is too large"));
            else
              resolve({
                exit: code ?? -1,
                stdout: Buffer.concat(out).toString("utf8"),
                stderr: err.slice(-50),
              });
          }),
        );
      });
    },
  };
}
