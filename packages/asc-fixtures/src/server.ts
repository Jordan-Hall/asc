// The server side of the `review` scenario: a real `asc serve` child process (the same binary the
// client is) on a loopback port. SPEC section 24.7.2 names `--db sqlite::memory: --store
// memory://`. The object store is in memory, as named, but the database is a SQLite file in the
// fixture directory: `asc serve` never migrates at start, and a token created by the separate
// `asc serve token create` process has to be visible to the server, and neither is possible with
// a private in-memory database.
// Known gap (K-track): move to the in-process server when it has a bootstrap-token entry point.
//
// The server speaks plain http on the loopback interface only, so the client runs with
// ASC_ALLOW_LOOPBACK_HTTP=1 (the opt-in of the transport rule); nothing leaves the machine.
import { type ChildProcess, spawn } from "node:child_process";
import { createServer } from "node:net";
import { closeSync, mkdirSync, openSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Asc } from "./asc";

export interface ReviewServer {
  /** `http://127.0.0.1:<port>` of the Repo/Changes services. */
  readonly url: string;
  readonly adminUrl: string;
  /** A write token for `repo`, never logged and passed to asc only through the environment. */
  readonly token: string;
  readonly repo: string;
  stop(): Promise<void>;
}

async function freePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const s = createServer();
    s.once("error", reject);
    s.listen(0, "127.0.0.1", () => {
      const addr = s.address();
      const port = typeof addr === "object" && addr ? addr.port : 0;
      s.close(() => resolve(port));
    });
  });
}

const tomlPath = (p: string) => p.replaceAll("\\", "/");

async function waitReady(url: string, child: ChildProcess, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let exited: number | null = null;
  child.once("exit", (code) => (exited = code ?? -1));
  while (Date.now() < deadline) {
    if (exited !== null) throw new Error(`asc serve exited ${exited} before it was ready`);
    try {
      const r = await fetch(`${url}/readyz`);
      if (r.ok) return;
    } catch {
      // not listening yet
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error("asc serve was not ready in time");
}

/** Starts a server with one repository `repo` (`owner/name`) and a token that may write to it. */
export async function startReviewServer(
  asc: Asc,
  dir: string,
  repo = "asc/demo",
): Promise<ReviewServer> {
  mkdirSync(dir, { recursive: true });
  const port = await freePort();
  const adminPort = await freePort();
  const config = join(dir, "asc-serve.toml");
  writeFileSync(
    config,
    [
      `listen = "127.0.0.1:${port}"`,
      `store = "memory:///"`,
      `db = "sqlite:${tomlPath(join(dir, "asc.db"))}?mode=rwc"`,
      `[admin]`,
      `listen = "127.0.0.1:${adminPort}"`,
      `[auth]`,
      `provider = "token_store"`,
      `token_key_file = "${tomlPath(join(dir, "token.key"))}"`,
      "",
    ].join("\n"),
  );
  const server = asc.at(dir);
  server.ok(["serve", "migrate", "--config", config]);
  const made = server.run([
    "serve",
    "token",
    "create",
    "--repo",
    repo,
    "--user",
    "fixture",
    "--write",
    "--config",
    config,
  ]);
  if (made.code !== 0)
    throw new Error(`asc serve token create exited ${made.code}: ${made.stderr.trim()}`);
  const token = made.stdout.trim();

  const logPath = join(dir, "serve.log");
  const log = openSync(logPath, "w");
  const child = spawn(asc.bin, ["serve", "--config", config], {
    cwd: dir,
    env: server.env({ ASC_ALLOW_LOOPBACK_HTTP: "1" }),
    stdio: ["ignore", log, log],
    windowsHide: true,
  });
  closeSync(log);
  const url = `http://127.0.0.1:${port}`;
  const adminUrl = `http://127.0.0.1:${adminPort}`;
  const stop = async () => {
    if (child.exitCode !== null || child.signalCode !== null) return;
    const done = new Promise<void>((r) => child.once("exit", () => r()));
    child.kill();
    await done;
  };
  try {
    await waitReady(adminUrl, child, 30_000);
  } catch (e) {
    await stop();
    const why = e instanceof Error ? e.message : String(e);
    throw new Error(`${why}: ${readFileSync(logPath, "utf8").trim().slice(-2000)}`, { cause: e });
  }
  return { url, adminUrl, token, repo, stop };
}
