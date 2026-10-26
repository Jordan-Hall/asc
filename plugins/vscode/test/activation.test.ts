import { afterAll, afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { create, toBinary } from "@bufbuild/protobuf";
import { type BridgeNode, bridge } from "@asc/asc-client";
import { checkFeatures, createBridgeManager, resolveAsc } from "../src/bridge";
import { activateWith, GIT_PROMPT_BUTTON, type Host } from "../src/extension";
import { detectAscFolders, nodeDetectFs, resolveGitDir } from "../src/repo";

const dirs: string[] = [];
// The bridge starts only from an absolute executable, so `asc.path` names a real (empty) file.
const exeDir = mkdtempSync(join(tmpdir(), "asc-vsc-exe-"));
const FAKE_EXE = join(exeDir, process.platform === "win32" ? "asc.exe" : "asc");
writeFileSync(FAKE_EXE, "");
afterAll(() => rmSync(exeDir, { recursive: true, force: true }));
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
    ascPathSetting: () => FAKE_EXE,
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
    expect(resolveAsc({ ...base, setting: "/x/asc", path: "/a", isFile: () => true })).toBe(
      "/x/asc",
    );
    expect(resolveAsc({ ...base, setting: "", path: "/a:/b", isFile: has(["/b/asc"]) })).toBe(
      "/b/asc",
    );
    expect(
      resolveAsc({ ...base, setting: "", path: "/a", ascHome: "/c", isFile: has(["/c/bin/asc"]) }),
    ).toBe("/c/bin/asc");
  });

  test("none found is null, never a bare name", () => {
    expect(resolveAsc({ ...base, setting: "", path: "/a", isFile: () => false })).toBeNull();
    expect(resolveAsc({ ...base, setting: "/x/asc", path: "/a", isFile: () => false })).toBeNull();
    const win = { ...base, platform: "win32" as const, home: "C:\\Users\\u" };
    expect(resolveAsc({ ...win, setting: "", path: "C:\\bin", isFile: () => false })).toBeNull();
  });

  test("relative settings and PATH entries are never searched", () => {
    const anyFile = () => true;
    expect(resolveAsc({ ...base, setting: "./asc", path: "/a", isFile: anyFile })).toBeNull();
    expect(resolveAsc({ ...base, setting: "bin/asc", path: "/a", isFile: anyFile })).toBeNull();
    // "." and an empty entry are the current directory; they are skipped, absolute ones are kept.
    expect(
      resolveAsc({ ...base, setting: "", path: ".::rel/bin", home: "", isFile: anyFile }),
    ).toBeNull();
    expect(resolveAsc({ ...base, setting: "", path: ".:/ok", isFile: has(["/ok/asc"]) })).toBe(
      "/ok/asc",
    );
    const win = { ...base, platform: "win32" as const, home: "" };
    const winFile = (p: string) => /asc\.exe$/i.test(p);
    for (const setting of [".\\asc.exe", "..\\asc.exe", "\\asc.exe", "C:asc.exe", "sub\\asc.exe"]) {
      expect(resolveAsc({ ...win, setting, path: "C:\\ok", isFile: winFile })).toBeNull();
    }
    expect(resolveAsc({ ...win, setting: "", path: ".;;rel;C:\\ok", isFile: winFile })).toBe(
      "C:\\ok\\asc.exe",
    );
    expect(resolveAsc({ ...win, setting: "asc", path: "C:\\ok", isFile: winFile })).toBe(
      "C:\\ok\\asc.exe",
    );
  });

  test("a planted asc in the workspace is never chosen, even on PATH or as the setting", () => {
    const ws = "C:\\work\\repo";
    const win = { ...base, platform: "win32" as const, home: "", workspaceRoots: [ws] };
    const files = (p: string) =>
      p.toLowerCase().startsWith("c:\\work\\repo\\") || p === "C:\\ok\\asc.exe";
    // PATH lists the workspace (and a sub-folder of it, in another case) ahead of a real install.
    expect(
      resolveAsc({ ...win, setting: "", path: `${ws};C:\\WORK\\Repo\\bin;C:\\ok`, isFile: files }),
    ).toBe("C:\\ok\\asc.exe");
    expect(
      resolveAsc({ ...win, setting: `${ws}\\asc.exe`, path: "C:\\ok", isFile: files }),
    ).toBeNull();
    expect(resolveAsc({ ...win, setting: "", path: ws, isFile: files })).toBeNull();
    const nix = { ...base, home: "", workspaceRoots: ["/w/repo"] };
    expect(
      resolveAsc({ ...nix, setting: "", path: "/w/repo:/w/repo/bin", isFile: () => true }),
    ).toBeNull();
    // A sibling folder that shares the prefix is not inside it.
    expect(resolveAsc({ ...nix, setting: "", path: "/w/repo2", isFile: () => true })).toBe(
      "/w/repo2/asc",
    );
  });
});

describe("a bare or planted asc is never spawned", () => {
  test("the bridge manager refuses a bare name and starts nothing", () => {
    let spawned = 0;
    for (const command of [
      null,
      "asc",
      "asc.exe",
      "./asc",
      "..\\asc.exe",
      "\\asc.exe",
      "C:asc.exe",
    ]) {
      const m = createBridgeManager({
        command: () => command,
        create: () => {
          spawned += 1;
          throw new Error("must not spawn");
        },
      });
      expect(() => m.node()).toThrow("asc was not found");
      expect(m.started).toBe(false);
    }
    expect(spawned).toBe(0);
  });

  test("an activation whose asc is missing reports it and never reaches the transport", async () => {
    const f = fakeHost([ascFolder()]);
    f.host.ascPathSetting = () => "asc";
    f.host.env = { PATH: "", ASC_HOME: join(tmp(), "none") };
    let spawned = 0;
    const a = await activateWith(
      f.host,
      fakeNode(FULL, () => void (spawned += 1)),
    );
    expect(await a.started).toBeNull();
    expect(spawned).toBe(0);
  });
});
