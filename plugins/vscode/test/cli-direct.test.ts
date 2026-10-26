import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  CLI_MAX_STDOUT,
  childEnv,
  cliArgv,
  createCliDirect,
  OP_LOG_MAX,
} from "../src/cli-direct";
import {
  type ConnectivityApi,
  type ConnectivityEvent,
  connState,
  followConnectivity,
} from "../src/connectivity";

describe("cli-direct: argv", () => {
  test("each call has one fixed argv", () => {
    expect(cliArgv({ kind: "op-log", limit: 50 })).toEqual(["op", "log", "--json", "-n", "50"]);
    expect(cliArgv({ kind: "cloud-status" })).toEqual(["cloud", "status", "--json"]);
    expect(cliArgv({ kind: "live", action: "status" })).toEqual(["live", "status", "--json"]);
    expect(cliArgv({ kind: "live", action: "pause" })).toEqual(["live", "pause"]);
    expect(cliArgv({ kind: "live", action: "resume" })).toEqual(["live", "resume"]);
  });
  test("an out-of-range limit or an unknown call has no argv", () => {
    for (const limit of [0, -1, 1.5, OP_LOG_MAX + 1, Number.NaN, 1e12]) {
      expect(cliArgv({ kind: "op-log", limit })).toBeNull();
    }
    expect(cliArgv({ kind: "live", action: "run" } as never)).toBeNull();
    expect(cliArgv({ kind: "live", action: "--exec=x" } as never)).toBeNull();
    expect(cliArgv({ kind: "rm" } as never)).toBeNull();
  });
});

describe("cli-direct: environment", () => {
  test("only the listed variables pass, plus the fixed values", () => {
    const e = childEnv({
      PATH: "/bin",
      ASC_HOME: "/h",
      GIT_DIR: "/evil",
      GIT_SSH_COMMAND: "evil",
      NODE_OPTIONS: "--require x",
      LD_PRELOAD: "x",
      ASC_ALLOW_LOOPBACK_HTTP: "1",
    });
    expect(e.PATH).toBe("/bin");
    expect(e.ASC_HOME).toBe("/h");
    expect(Object.keys(e)).not.toContain("GIT_DIR");
    expect(Object.keys(e)).not.toContain("GIT_SSH_COMMAND");
    expect(Object.keys(e)).not.toContain("NODE_OPTIONS");
    expect(Object.keys(e)).not.toContain("LD_PRELOAD");
    expect(Object.keys(e)).not.toContain("ASC_ALLOW_LOOPBACK_HTTP");
    expect(e.GIT_TERMINAL_PROMPT).toBe("0");
    expect(e.GCM_INTERACTIVE).toBe("never");
    expect(e.ASC_NO_ALIASES).toBe("1");
  });
  test("a host-supplied token wins over an inherited one", () => {
    expect(childEnv({ ASC_TOKEN: "old" }, { ASC_TOKEN: "new" }).ASC_TOKEN).toBe("new");
    expect(childEnv({ ASC_TOKEN: "old" }, { ASC_TOKEN: undefined }).ASC_TOKEN).toBe("old");
  });
});

/** A killed child can hold its working directory for a moment on Windows. */
function cleanup(dir: string): void {
  try {
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  } catch {
    // a temporary directory that cannot be removed now is removed with the temp folder later.
  }
}

// A real child process: a node script stands in for asc, so spawning, the null stdin, the working
// directory, the output cap and the time limit are exercised for real.
function fakeAsc(body: string): { dir: string; command: string; prefixArgs: string[] } {
  const dir = mkdtempSync(join(tmpdir(), "asc-cli-"));
  const script = join(dir, "fake.cjs");
  writeFileSync(script, body);
  return { dir, command: process.execPath, prefixArgs: [script] };
}

describe("cli-direct: run", () => {
  test("runs in the root with the fixed argv and reads stdout", async () => {
    const f = fakeAsc(
      `process.stdout.write(JSON.stringify({argv: process.argv.slice(2), cwd: process.cwd(), tok: process.env.ASC_TOKEN ?? null, git: process.env.GIT_DIR ?? null}));`,
    );
    try {
      const cli = createCliDirect({
        command: () => f.command,
        prefixArgs: f.prefixArgs,
        env: () => ({ ...process.env, GIT_DIR: "/evil", ASC_TOKEN: "inherited" }),
        extraEnv: () => ({ ASC_TOKEN: "from-host" }),
      });
      const r = await cli.run(f.dir, { kind: "op-log", limit: 7 });
      expect(r.exit).toBe(0);
      const got = JSON.parse(r.stdout);
      expect(got.argv).toEqual(["op", "log", "--json", "-n", "7"]);
      expect(got.tok).toBe("from-host");
      expect(got.git).toBeNull();
      expect(got.cwd.toLowerCase()).toBe(f.dir.toLowerCase());
    } finally {
      cleanup(f.dir);
    }
  });
  test("stdin is the null device", async () => {
    const f = fakeAsc(
      `let n = 0; process.stdin.on("data", (b) => (n += b.length)); process.stdin.on("end", () => process.stdout.write(String(n)));`,
    );
    try {
      const r = await createCliDirect({ command: () => f.command, prefixArgs: f.prefixArgs }).run(f.dir, {
        kind: "cloud-status",
      });
      expect(r.stdout).toBe("0");
    } finally {
      cleanup(f.dir);
    }
  });
  test("a failing verb returns its exit code and stderr lines", async () => {
    const f = fakeAsc(`console.error("asc: the daemon is not running"); process.exit(3);`);
    try {
      const r = await createCliDirect({ command: () => f.command, prefixArgs: f.prefixArgs }).run(f.dir, {
        kind: "live",
        action: "status",
      });
      expect(r.exit).toBe(3);
      expect(r.stderr).toEqual(["asc: the daemon is not running"]);
    } finally {
      cleanup(f.dir);
    }
  });
  test("output over the cap is an error, never a partial parse", async () => {
    const f = fakeAsc(
      `const chunk = "x".repeat(65536); for (let i = 0; i < ${Math.ceil(CLI_MAX_STDOUT / 65536) + 4}; i++) process.stdout.write(chunk);`,
    );
    try {
      await expect(
        createCliDirect({ command: () => f.command, prefixArgs: f.prefixArgs }).run(f.dir, { kind: "cloud-status" }),
      ).rejects.toThrow("too large");
    } finally {
      cleanup(f.dir);
    }
  });
  test("a slow verb is killed at the time limit", async () => {
    const f = fakeAsc("setTimeout(() => {}, 60000);");
    try {
      const t0 = Date.now();
      await expect(
        createCliDirect({ command: () => f.command, prefixArgs: f.prefixArgs, timeoutMs: 300 }).run(f.dir, {
          kind: "cloud-status",
        }),
      ).rejects.toThrow("timed out");
      expect(Date.now() - t0).toBeLessThan(10_000);
    } finally {
      cleanup(f.dir);
    }
  });
  test("an abort cancels the child", async () => {
    const f = fakeAsc("setTimeout(() => {}, 60000);");
    try {
      const ac = new AbortController();
      const p = createCliDirect({ command: () => f.command, prefixArgs: f.prefixArgs }).run(
        f.dir,
        { kind: "cloud-status" },
        ac.signal,
      );
      setTimeout(() => ac.abort(), 100);
      await expect(p).rejects.toThrow("cancelled");
      await expect(
        createCliDirect({ command: () => f.command, prefixArgs: f.prefixArgs }).run(f.dir, { kind: "cloud-status" }, ac.signal),
      ).rejects.toThrow("cancelled");
    } finally {
      cleanup(f.dir);
    }
  });
  test("a missing executable and a bad root are errors", async () => {
    const cli = createCliDirect({ command: () => join(tmpdir(), "no-such-asc-binary") });
    await expect(cli.run(tmpdir(), { kind: "cloud-status" })).rejects.toThrow("cannot start");
    await expect(cli.run("", { kind: "cloud-status" })).rejects.toThrow("root");
    await expect(cli.run("a\0b", { kind: "cloud-status" })).rejects.toThrow("root");
    await expect(cli.run(tmpdir(), { kind: "op-log", limit: 0 })).rejects.toThrow("range");
  });
});

describe("connectivity", () => {
  const ev = (server: ConnectivityEvent["server"]): ConnectivityEvent => ({
    root: "/w",
    daemon: "up",
    server,
    remote: "up",
  });
  test("states map from the wire enum", () => {
    expect(connState(1)).toBe("up");
    expect(connState(2)).toBe("down");
    expect(connState(3)).toBe("not-configured");
    expect(connState(0)).toBe("unknown");
    expect(connState(99)).toBe("unknown");
  });
  test("a dropped stream reconnects, and an abort stops the loop", async () => {
    let n = 0;
    const api: ConnectivityApi = {
      async *events() {
        n++;
        if (n === 1) throw new Error("bridge gone");
        yield ev("down");
        yield ev("up");
        await new Promise((r) => setTimeout(r, 30));
      },
    };
    const ac = new AbortController();
    const got: string[] = [];
    const errs: unknown[] = [];
    const p = followConnectivity(api, ac.signal, (e) => got.push(e.server), (e) => errs.push(e), [1]);
    await new Promise((r) => setTimeout(r, 20));
    ac.abort();
    await p;
    expect(errs).toHaveLength(1);
    expect(got.slice(0, 2)).toEqual(["down", "up"]);
  });
});
