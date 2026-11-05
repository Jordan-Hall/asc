// The build cache in the editor: the replies of `asc cache ... --json` read defensively, the status
// item, the per-task difference, the admin verbs' argv and the task detection.
import { describe, expect, test } from "bun:test";
import { type CliCall, type CliDirect, callTimeout, cliArgv } from "../src/cli-direct";
import {
  CacheModel,
  type CacheStatus,
  cliCacheApi,
  delta,
  humanSize,
  parseEntries,
  parseEnv,
  parseStatus,
  statusView,
} from "../src/cache/model";
import {
  bundlerScripts,
  declaredRun,
  detectBuilds,
  detectSetupTools,
  insideRoot,
  packageManager,
  type RootFiles,
} from "../src/cache/tasks";

const KEY = "ab".repeat(32);

const STATUS = JSON.stringify({
  root: "/home/u/.cache/asc/build",
  bytes: 3 * 1024 * 1024,
  max_size: 10 * 1024 * 1024 * 1024,
  entries: 4,
  blobs: 9,
  tools: {
    vite: {
      day: { hits: 3, misses: 1, puts: 1, restored: 3, hit_rate: 0.75 },
      week: { hits: 6, misses: 2, puts: 2, restored: 6, hit_rate: 0.75 },
    },
    cargo: {
      day: { hits: 1, misses: 3, puts: 3, restored: 1, hit_rate: 0.25 },
      week: { hits: 1, misses: 3, puts: 3, restored: 1, hit_rate: 0.25 },
    },
    "Bad Name": { day: {}, week: {} },
  },
  remote: { mode: "read-write", queued: 2, last: { at: 1000, error: null } },
});

const status = (o: Partial<Record<string, unknown>> = {}): CacheStatus =>
  parseStatus(JSON.stringify({ ...JSON.parse(STATUS), ...o })) as CacheStatus;

describe("parsers", () => {
  test("status: tools sorted, bad names dropped, numbers clamped", () => {
    const s = parseStatus(STATUS);
    expect(s?.tools.map((t) => t.name)).toEqual(["cargo", "vite"]);
    expect(s?.tools[1]?.day).toEqual({ hits: 3, misses: 1, puts: 1, restored: 3, hitRate: 0.75 });
    expect(s?.remote).toEqual({ mode: "read-write", queued: 2, last: { at: 1000, error: null } });
    const odd = parseStatus(
      JSON.stringify({
        root: "/r",
        bytes: -5,
        tools: { x: { day: { hits: "9", hit_rate: 7 } } },
        remote: { mode: "?" },
      }),
    );
    expect(odd?.bytes).toBe(0);
    expect(odd?.tools[0]?.day.hits).toBe(0);
    expect(odd?.tools[0]?.day.hitRate).toBe(1);
    expect(odd?.remote).toEqual({ mode: "off", queued: 0, last: null });
  });

  test("status: anything but the object is null", () => {
    for (const t of [null, "", "[]", "{}", "nope", '{"root":1}']) expect(parseStatus(t)).toBeNull();
  });

  test("entries: rows without a hex key are dropped, text clipped", () => {
    const rows = parseEntries(
      JSON.stringify([
        {
          key: KEY,
          tool: "vite",
          label: "web",
          size: 10,
          outputs: 2,
          exit: 0,
          created: 5,
          last_use: 7,
          worktree: "/w",
          workflow: null,
        },
        { key: "not-hex", tool: "vite" },
        { key: KEY, label: "x".repeat(900) },
        7,
      ]),
    );
    expect(rows.length).toBe(2);
    expect(rows[0]).toEqual({
      key: KEY,
      tool: "vite",
      label: "web",
      size: 10,
      outputs: 2,
      exit: 0,
      created: 5,
      lastUse: 7,
      worktree: "/w",
      workflow: "",
    });
    expect(rows[1]?.label.length).toBe(503);
    expect(parseEntries("{}")).toEqual([]);
  });

  test("env: only upper-case names with string values", () => {
    expect(parseEnv(JSON.stringify({ TURBO_API: "http://127.0.0.1:1", bad: "x", N: 1 }))).toEqual({
      TURBO_API: "http://127.0.0.1:1",
    });
    expect(parseEnv("[]")).toEqual({});
  });

  test("sizes", () => {
    expect(humanSize(512)).toBe("512 B");
    expect(humanSize(1536)).toBe("1.5 KiB");
    expect(humanSize(3 * 1024 * 1024)).toBe("3.0 MiB");
    expect(humanSize(10 * 1024 ** 3)).toBe("10 GiB");
  });
});

describe("status item", () => {
  test("today's hit rate over every tool, and the detail in the tooltip", () => {
    const v = statusView(parseStatus(STATUS), 1060);
    expect(v.visible).toBe(true);
    expect(v.warning).toBe(false);
    expect(v.text).toBe("$(database) 50% hits today");
    expect(v.tooltip).toContain("Build cache: 3.0 MiB of 10 GiB, 4 entries");
    expect(v.tooltip).toContain("vite: 3 hits, 1 misses today; 75% this week");
    expect(v.tooltip).toContain("Server: read-write, 2 queued, reached 1m ago");
  });

  test("a failed server exchange is a warning, unless the server tier is off", () => {
    const failed = status({
      remote: { mode: "read", queued: 0, last: { at: 1000, error: "connection refused" } },
    });
    const v = statusView(failed, 1000);
    expect(v.warning).toBe(true);
    expect(v.text.startsWith("$(warning)")).toBe(true);
    expect(v.tooltip).toContain("failed 0s ago: connection refused");
    const off = status({ remote: { mode: "off", queued: 0, last: { at: 1, error: "old" } } });
    expect(statusView(off, 1000).warning).toBe(false);
  });

  test("the last build's hits and misses, once a task ran", () => {
    const v = statusView(parseStatus(STATUS), 1000, { label: "cargo build", hits: 12, misses: 2 });
    expect(v.text).toBe("$(database) 12 hits, 2 misses");
    expect(v.tooltip).toContain("Last build: cargo build, 12 hits, 2 misses");
  });

  test("no status hides it; no builds says so", () => {
    expect(statusView(null, 0).visible).toBe(false);
    expect(statusView(status({ tools: {} }), 0).text).toBe("$(database) no builds today");
  });

  test("the difference between two readings", () => {
    const before = parseStatus(STATUS);
    const after = status({
      tools: {
        cargo: { day: { hits: 11, misses: 4 }, week: {} },
        vite: { day: { hits: 3, misses: 1 }, week: {} },
      },
    });
    expect(delta(before, after)).toEqual({ hits: 10, misses: 1 });
    expect(delta(null, after)).toEqual({ hits: 0, misses: 0 });
  });
});

describe("cache verbs", () => {
  test("the argv of each call", () => {
    expect(cliArgv({ kind: "cache-status" })).toEqual(["cache", "status", "--json"]);
    expect(cliArgv({ kind: "cache-ls", tool: "vite", limit: 200 })).toEqual([
      "cache",
      "ls",
      "--json",
      "-n",
      "200",
      "--tool",
      "vite",
    ]);
    expect(cliArgv({ kind: "cache-show", key: KEY })).toEqual(["cache", "show", "--json", KEY]);
    expect(cliArgv({ kind: "cache-rm", keys: [KEY, "abcd"] })).toEqual([
      "cache",
      "rm",
      "--json",
      KEY,
      "abcd",
    ]);
    expect(cliArgv({ kind: "cache-clear" })).toEqual(["cache", "clear", "--json"]);
    expect(cliArgv({ kind: "cache-clear", tool: "cargo" })).toEqual([
      "cache",
      "clear",
      "--json",
      "--tool",
      "cargo",
    ]);
    expect(cliArgv({ kind: "cache-setup", tool: "go", undo: true })).toEqual([
      "cache",
      "setup",
      "go",
      "--undo",
    ]);
    expect(cliArgv({ kind: "cache-env" })).toEqual(["cache", "env", "--shell", "json"]);
  });

  test("out-of-range input gives no argv", () => {
    const bad: CliCall[] = [
      { kind: "cache-ls", limit: 0 },
      { kind: "cache-ls", limit: 10, tool: "--all" },
      { kind: "cache-show", key: "--json" },
      { kind: "cache-show", key: "ABCD" },
      { kind: "cache-rm", keys: [] },
      { kind: "cache-rm", keys: [KEY, "-rf"] },
      { kind: "cache-clear", tool: "a b" },
      { kind: "cache-setup", tool: "npm" as never, undo: false },
    ];
    for (const c of bad) expect(cliArgv(c)).toBeNull();
  });

  test("warm, setup and gc get a longer time limit", () => {
    expect(callTimeout({ kind: "cache-warm" }, 15_000)).toBe(300_000);
    expect(callTimeout({ kind: "cache-status" }, 15_000)).toBe(15_000);
  });

  test("the api reads replies and turns a failure into its last stderr line", async () => {
    const calls: CliCall[] = [];
    const replies: Record<string, { exit: number; stdout: string; stderr: string[] }> = {
      "cache-status": { exit: 0, stdout: STATUS, stderr: [] },
      "cache-rm": { exit: 0, stdout: JSON.stringify({ removed: [KEY, "zz"] }), stderr: [] },
      "cache-gc": { exit: 0, stdout: JSON.stringify({ entries: 3, freed: 2048 }), stderr: [] },
      "cache-warm": { exit: 1, stdout: "", stderr: ["asc: the build cache server is off"] },
    };
    const cli: CliDirect = {
      run: async (_root, call) => {
        calls.push(call);
        const r = replies[call.kind];
        if (r === undefined) throw new Error("unexpected");
        return r;
      },
    };
    const api = cliCacheApi(cli);
    expect((await api.status("/r"))?.entries).toBe(4);
    expect(await api.remove("/r", [KEY])).toEqual([KEY]);
    expect(await api.gc("/r")).toEqual({ entries: 3, freed: 2048 });
    await expect(api.warm("/r")).rejects.toThrow("asc cache warm: the build cache server is off");
    expect(calls.map((c) => c.kind)).toEqual([
      "cache-status",
      "cache-rm",
      "cache-gc",
      "cache-warm",
    ]);
  });

  test("the model hides on a failed read and lists a tool once per refresh", async () => {
    let fail = false;
    let lists = 0;
    const model = new CacheModel({
      root: "/r",
      api: {
        status: async () => {
          if (fail) throw new Error("gone");
          return parseStatus(STATUS);
        },
        list: async () => {
          lists++;
          return [];
        },
      } as never,
    });
    let changes = 0;
    model.onChange(() => changes++);
    expect((await model.refresh())?.entries).toBe(4);
    await model.entries("vite");
    await model.entries("vite");
    expect(lists).toBe(1);
    fail = true;
    expect(await model.refresh()).toBeNull();
    expect(model.status).toBeNull();
    await model.entries("vite");
    expect(lists).toBe(2);
    expect(changes).toBe(2);
    model.dispose();
  });
});

function files(map: Record<string, string>): RootFiles {
  return {
    exists: async (rel) => Object.hasOwn(map, rel),
    read: async (rel) => map[rel] ?? null,
  };
}

describe("tasks", () => {
  test("cargo, go and the bundler scripts, with the lockfile's package manager", async () => {
    const got = await detectBuilds(
      files({
        "Cargo.toml": "",
        "go.mod": "",
        "pnpm-lock.yaml": "",
        "package.json": JSON.stringify({
          scripts: {
            build: "vite build",
            lib: "tsc && rslib build --watch=false",
            dev: "vite",
            test: "bun test",
          },
        }),
      }),
      "/opt/asc",
    );
    expect(got.map((s) => [s.id, s.label])).toEqual([
      ["cargo", "cargo build"],
      ["go", "go build ./..."],
      ["script:build", "pnpm run build"],
      ["script:lib", "pnpm run lib"],
    ]);
    expect(got[0]?.env).toEqual({ RUSTC_WRAPPER: "/opt/asc", CARGO_INCREMENTAL: "0" });
    expect(got[1]?.env).toEqual({ GOCACHEPROG: "/opt/asc cache go" });
    expect(got[0]?.matchers).toEqual(["$asc-rustc"]);
  });

  test("package managers and scripts", async () => {
    expect(await packageManager(files({ "bun.lock": "" }))).toBe("bun");
    expect(await packageManager(files({ "yarn.lock": "" }))).toBe("yarn");
    expect(await packageManager(files({}))).toBe("npm");
    expect(bundlerScripts("not json")).toEqual([]);
    expect(
      bundlerScripts(JSON.stringify({ scripts: { b: "rsbuild build", "bad name": "vite build" } })),
    ).toEqual([{ name: "b", tool: "rsbuild" }]);
  });

  test("a declared task runs through asc cache run", () => {
    const s = declaredRun({
      type: "asc-build",
      name: "docs",
      inputs: ["docs", "./mkdocs.yml"],
      outputs: ["site/"],
      command: ["mkdocs", "build"],
      env: ["SITE_URL"],
    });
    expect(s?.command).toBe("asc");
    expect(s?.args).toEqual([
      "cache",
      "run",
      "--name",
      "docs",
      "--inputs",
      "docs",
      "mkdocs.yml",
      "--outputs",
      "site",
      "--env",
      "SITE_URL",
      "--",
      "mkdocs",
      "build",
    ]);
    expect(
      declaredRun({ name: "d", outputs: ["out"], command: ["make"] })?.args.slice(4, 6),
    ).toEqual(["--inputs", "."]);
  });

  test("a declared task with a path outside the root, or a bad field, is refused", () => {
    const ok = { name: "d", outputs: ["out"], command: ["make"] };
    for (const d of [
      { ...ok, name: "a b" },
      { ...ok, outputs: ["../x"] },
      { ...ok, outputs: ["."] },
      { ...ok, outputs: [] },
      { ...ok, inputs: ["/etc"] },
      { ...ok, inputs: ["C:\\x"] },
      { ...ok, command: [] },
      { ...ok, command: [""] },
      { ...ok, env: ["A-B"] },
      { ...ok, command: "make" },
    ]) {
      expect(declaredRun(d as never)).toBeUndefined();
    }
    expect(insideRoot("a\\b\\..\\c")).toBe("a/c");
    expect(insideRoot("a/../..")).toBeUndefined();
  });

  test("setup offers the tools the root uses", async () => {
    expect(
      await detectSetupTools(
        files({
          "Cargo.toml": "",
          "nx.json": "",
          "MODULE.bazel": "",
          "build.gradle.kts": "",
          "CMakeLists.txt": "",
        }),
      ),
    ).toEqual(["cargo", "nx", "bazel", "gradle", "ccache"]);
    expect(await detectSetupTools(files({}))).toEqual([]);
  });
});
