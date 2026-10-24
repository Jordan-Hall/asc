import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { create, toBinary } from "@bufbuild/protobuf";
import { type BridgeNode, bridge } from "@asc/asc-client";
import { checkFeatures, resolveAsc } from "../src/bridge";
import { activateWith, GIT_PROMPT_BUTTON, type Host } from "../src/extension";
import { detectAscFolders, nodeDetectFs, resolveGitDir } from "../src/repo";

const dirs: string[] = [];
function tmp(): string {
  const d = mkdtempSync(join(tmpdir(), "asc-vsc-"));
  dirs.push(d);
  return d;
}
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

function ascFolder(): string {
  const d = tmp();
  mkdirSync(join(d, ".git"));
  writeFileSync(join(d, ".git", "asc-workspace"), "");
  return d;
}

interface Fake {
  host: Host;
  contexts: Record<string, boolean>;
  notes: string[];
  commands: string[];
  gitDisabled: number;
  memo: Map<string, boolean>;
}
function fakeHost(folders: string[], pick?: string): Fake {
  const f: Fake = {
    contexts: {},
    notes: [],
    commands: [],
    gitDisabled: 0,
    memo: new Map(),
    host: undefined as unknown as Host,
  };
  f.host = {
    workspaceFolders: () => folders,
    ascPathSetting: () => "",
    gitEnabled: () => true,
    disableBuiltinGit: async () => {
      f.gitDisabled += 1;
    },
    notify: async (m, ...b) => {
      f.notes.push(m);
      return b.length > 0 ? pick : undefined;
    },
    registerCommand: (id) => {
      f.commands.push(id);
      return { dispose() {} };
    },
    setContext: async (k, v) => {
      f.contexts[k] = v;
    },
    log() {},
    showOutput() {},
    memento: { get: (k) => f.memo.get(k), set: async (k, v) => void f.memo.set(k, v) },
    env: {},
    platform: process.platform,
  };
  return f;
}

function fakeNode(features: string[], onCreate?: () => void) {
  return (): BridgeNode => {
    onCreate?.();
    const reply = toBinary(
      bridge.HelloResponseSchema,
      create(bridge.HelloResponseSchema, { ascVersion: "9.9", features }),
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
}

const has = (s: string[]) => (p: string) => s.includes(p.replaceAll("\\", "/"));
const FULL = ["plugin.v1", "json-verbs.v1"];

describe("activation detection", () => {
  test("a .git directory with the marker is an asc workspace", async () => {
    const d = ascFolder();
    expect(await detectAscFolders([d])).toEqual([d]);
  });
  test("a plain git folder and an empty folder are not", async () => {
    const plain = tmp();
    mkdirSync(join(plain, ".git"));
    expect(await detectAscFolders([plain, tmp()])).toEqual([]);
  });
  test("a .git file points at the git dir (worktree layout)", async () => {
    const d = tmp();
    const real = join(d, "store", "wt");
    mkdirSync(real, { recursive: true });
    writeFileSync(join(real, "asc-workspace"), "");
    writeFileSync(join(d, ".git"), "gitdir: store/wt\n");
    expect(await resolveGitDir(d, nodeDetectFs)).toBe(real);
    expect(await detectAscFolders([d])).toEqual([d]);
  });
});

describe("activation in a non-asc folder", () => {
  test("registers nothing, spawns nothing, within 5 ms", async () => {
    const f = fakeHost([tmp()]);
    let spawned = 0;
    const t0 = performance.now();
    const a = await activateWith(
      f.host,
      fakeNode(FULL, () => void (spawned += 1)),
    );
    const ms = performance.now() - t0;
    expect(a.ascFolders).toEqual([]);
    expect(a.bridge).toBeNull();
    expect(f.commands).toEqual([]);
    expect(f.notes).toEqual([]);
    expect(spawned).toBe(0);
    expect(ms).toBeLessThan(5);
  });
});

describe("activation in an asc folder", () => {
  test("registers, starts the bridge lazily, within 30 ms", async () => {
    const f = fakeHost([ascFolder()]);
    let spawned = 0;
    const t0 = performance.now();
    const a = await activateWith(
      f.host,
      fakeNode(FULL, () => void (spawned += 1)),
    );
    const ms = performance.now() - t0;
    expect(ms).toBeLessThan(30);
    expect(f.commands).toContain("asc.restartBridge");
    expect(await a.started).toEqual({ level: "full" });
    expect(spawned).toBe(1);
    expect(f.contexts["asc.mutating"]).toBe(true);
    a.dispose();
  });

  test("the git.enabled prompt writes the setting once, only on accept", async () => {
    const f = fakeHost([ascFolder()], GIT_PROMPT_BUTTON);
    const a = await activateWith(f.host, fakeNode(FULL));
    await a.started;
    await new Promise((r) => setTimeout(r, 0));
    expect(f.gitDisabled).toBe(1);
    const again = await activateWith(f.host, fakeNode(FULL));
    await again.started;
    expect(f.gitDisabled).toBe(1);
    expect(f.notes.filter((n) => n.includes("built-in git"))).toHaveLength(1);
  });

  test("dismissing the prompt leaves git.enabled alone", async () => {
    const f = fakeHost([ascFolder()], undefined);
    const a = await activateWith(f.host, fakeNode(FULL));
    await a.started;
    await new Promise((r) => setTimeout(r, 0));
    expect(f.gitDisabled).toBe(0);
  });

  test("an old asc shows Update asc and disables mutating commands", async () => {
    const f = fakeHost([ascFolder()]);
    f.memo.set("asc.gitPromptShown", true);
    const a = await activateWith(f.host, fakeNode(["plugin.v1"]));
    expect((await a.started)?.level).toBe("read-only");
    expect(f.contexts["asc.mutating"]).toBe(false);
    expect(f.notes.some((n) => n.startsWith("Update asc"))).toBe(true);
  });

  test("a bridge that cannot run is reported, not thrown", async () => {
    const f = fakeHost([ascFolder()]);
    const a = await activateWith(f.host, () => {
      throw new Error("spawn failed");
    });
    expect(await a.started).toBeNull();
  });
});

describe("minimum-feature check", () => {
  test("gates on features, not versions", () => {
    expect(checkFeatures([...FULL, "x"]).level).toBe("full");
    expect(checkFeatures(["plugin.v1"]).level).toBe("read-only");
    expect(checkFeatures(["json-verbs.v1"]).level).toBe("too-old");
    expect(checkFeatures([]).level).toBe("too-old");
  });
});

describe("asc binary resolution", () => {
  const base = { ascHome: undefined, home: "/h", platform: "linux" as const };

  test("setting wins, then PATH, then ASC_HOME/bin", () => {
    expect(resolveAsc({ ...base, setting: "/x/asc", path: "/a", isFile: () => true })).toBe("/x/asc");
    expect(resolveAsc({ ...base, setting: "", path: "/a:/b", isFile: has(["/b/asc"]) })).toBe(
      "/b/asc",
    );
    expect(
      resolveAsc({ ...base, setting: "", path: "/a", ascHome: "/c", isFile: has(["/c/bin/asc"]) }),
    ).toBe("/c/bin/asc");
    expect(resolveAsc({ ...base, setting: "", path: "/a", isFile: () => false })).toBe("asc");
  });
});
