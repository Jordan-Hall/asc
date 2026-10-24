// Runs the asc binary for the fixtures. Every fixture is built only through `ASC_BIN`, with a
// scrubbed environment (no inherited GIT_* or ASC_* variable, an isolated HOME and git config), a
// fixed identity and a clock that moves one second per call, so two builds of a scenario are
// byte-identical (SPEC section 24.7.2). Nothing here prompts: stdin is closed.
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

/** 2024-01-01T00:00:00Z. Every fixture date is this plus a count of seconds. */
export const EPOCH = 1_704_067_200;

export const IDENTITY = { name: "Fixture Author", email: "fixture@example.invalid" } as const;

const repoRoot = resolve(dirname(import.meta.dir), "..", "..");

/** `ASC_BIN`, else the release then the debug build under `target/`. */
export function ascBinary(): string {
  const fromEnv = process.env.ASC_BIN;
  if (fromEnv) return fromEnv;
  const exe = process.platform === "win32" ? ".exe" : "";
  for (const profile of ["release", "debug"]) {
    const p = join(repoRoot, "target", profile, `asc${exe}`);
    if (existsSync(p)) return p;
  }
  throw new Error("asc-fixtures: set ASC_BIN to the asc binary");
}

export function ascAvailable(): boolean {
  try {
    return existsSync(ascBinary());
  } catch {
    return false;
  }
}

const READ_ONLY = new Set([
  "rev-parse",
  "change-id",
  "for-each-ref",
  "status",
  "sl",
  "rev-list",
  "log",
  "cat-file",
  "ls-files",
  "blame",
  "show",
]);

export interface RunResult {
  readonly code: number;
  readonly stdout: string;
  readonly stderr: string;
}

export interface RunOptions {
  readonly cwd?: string;
  readonly input?: string | Uint8Array;
  /** Extra variables; they win over the fixed ones except the clock. */
  readonly env?: Record<string, string>;
}

/** An asc invocation context: one binary, one asc home, one clock. */
export class Asc {
  readonly bin: string;
  readonly home: string;
  readonly cwd: string;
  private readonly clock: { step: number };

  constructor(opts: { bin?: string; home: string; cwd: string; clock?: { step: number } }) {
    this.bin = opts.bin ?? ascBinary();
    this.home = resolve(opts.home);
    this.cwd = resolve(opts.cwd);
    this.clock = opts.clock ?? { step: 0 };
    const user = join(this.home, "user");
    mkdirSync(user, { recursive: true });
    const gitconfig = join(user, ".gitconfig");
    if (!existsSync(gitconfig)) {
      writeFileSync(
        gitconfig,
        `[user]\n\tname = ${IDENTITY.name}\n\temail = ${IDENTITY.email}\n[init]\n\tdefaultBranch = master\n[core]\n\tautocrlf = false\n\tfilemode = true\n`,
      );
    }
  }

  /** The same clock and home, another directory. */
  at(cwd: string): Asc {
    return new Asc({ bin: this.bin, home: this.home, cwd, clock: this.clock });
  }

  /** The clock's next value (seconds since the epoch). */
  tick(): number {
    this.clock.step += 1;
    return EPOCH + this.clock.step;
  }

  env(extra: Record<string, string> = {}, when = EPOCH + this.clock.step): Record<string, string> {
    const env: Record<string, string> = {};
    for (const [k, v] of Object.entries(process.env)) {
      if (v === undefined) continue;
      const up = k.toUpperCase();
      if (up.startsWith("GIT_") || up.startsWith("ASC_")) continue;
      env[k] = v;
    }
    const user = join(this.home, "user");
    const date = `${when} +0000`;
    Object.assign(env, {
      ASC_HOME: join(this.home, "asc-home"),
      HOME: user,
      USERPROFILE: user,
      XDG_CONFIG_HOME: join(user, ".config"),
      GIT_CONFIG_GLOBAL: join(user, ".gitconfig"),
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_TERMINAL_PROMPT: "0",
      GIT_AUTHOR_NAME: IDENTITY.name,
      GIT_AUTHOR_EMAIL: IDENTITY.email,
      GIT_AUTHOR_DATE: date,
      GIT_COMMITTER_NAME: IDENTITY.name,
      GIT_COMMITTER_EMAIL: IDENTITY.email,
      GIT_COMMITTER_DATE: date,
      NO_COLOR: "1",
      TZ: "UTC",
      ...extra,
    });
    return env;
  }

  run(args: readonly string[], opts: RunOptions = {}): RunResult {
    const when = this.tick();
    const r = spawnSync(this.bin, [...args], {
      cwd: opts.cwd ?? this.cwd,
      env: this.env(opts.env, when),
      input: opts.input ?? "",
      encoding: "utf8",
      maxBuffer: 1 << 30,
      windowsHide: true,
    });
    if (r.error) throw r.error;
    return { code: r.status ?? -1, stdout: r.stdout, stderr: r.stderr };
  }

  /** Runs `asc args` and returns stdout; any non-zero exit throws. */
  ok(args: readonly string[], opts: RunOptions = {}): string {
    let r = this.run(args, opts);
    // An asc process that dies at start-up on a starved host exits non-zero with no output at all.
    // Only a read-only verb is tried again (twice at most): a mutating one may have done its work.
    for (
      let again = 0;
      again < 2 &&
      READ_ONLY.has(args[0] ?? "") &&
      r.code !== 0 &&
      r.stderr === "" &&
      r.stdout === "";
      again++
    ) {
      r = this.run(args, opts);
    }
    if (r.code !== 0) {
      throw new Error(
        `asc ${args.join(" ")} exited ${r.code}: ${r.stderr.trim() || r.stdout.trim()}`,
      );
    }
    return r.stdout;
  }

  json<T = unknown>(args: readonly string[], opts: RunOptions = {}): T {
    return JSON.parse(this.ok(args, opts)) as T;
  }
}
