// Commit-time smoke test: activation registers every command and view that package.json contributes,
// and every command in package.json has a handler. The editor API is a permissive recording fake, so
// this exercises the real registration code (`vscodeHost`) without VS Code.
import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { create, toBinary } from "@bufbuild/protobuf";
import { type BridgeNode, bridge, BridgeError } from "@asc/asc-client";
import type * as vscodeTypes from "vscode";
import { activateWith, vscodeHost } from "../src/extension";

interface PackageJson {
  contributes: {
    commands: { command: string }[];
    views: Record<string, { id: string; type?: string }[]>;
    menus: Record<string, { command: string }[]>;
    keybindings: { command: string }[];
    mcpServerDefinitionProviders: { id: string }[];
    configuration: { properties: Record<string, unknown> };
  };
  activationEvents: string[];
}
const pkg = JSON.parse(
  readFileSync(new URL("../package.json", import.meta.url), "utf8"),
) as PackageJson;
const contributed = pkg.contributes.commands.map((c) => c.command);

/** Registered on purpose without a package.json entry: arguments come from a tree item, a hover link
 * or another command, so the palette would be useless. Keep this list explicit. */
const INTERNAL = new Set([
  "asc.blame",
  "asc.blame.copyChangeId",
  "asc.blame.openChanges",
  "asc.blame.openPr",
  "asc.blame.previous",
  "asc.diffAgainstStackParent",
]);

interface Stub {
  (...a: unknown[]): Stub;
  [k: string]: Stub;
}

/** Anything you read, call or construct is another stub. Awaiting one resolves to itself. */
function stub(): Stub {
  return new Proxy(function () {}, {
    get: (_t, k) => (k === "then" ? undefined : k === Symbol.toPrimitive ? () => "" : stub()),
    set: () => true,
    apply: () => stub(),
    construct: () => stub(),
  }) as unknown as Stub;
}

function over(o: Record<string, unknown>): Stub {
  return new Proxy(stub(), {
    get: (t, k) => (typeof k === "string" && Object.hasOwn(o, k) ? o[k] : Reflect.get(t, k)),
    set: () => true,
  });
}

interface Recorder {
  commands: string[];
  treeViews: string[];
  webviewViews: string[];
  contexts: Record<string, unknown>;
  logs: string[];
}

function fakeUri(p: string) {
  return { scheme: "file", fsPath: p, path: p, toString: () => `file://${p}` };
}

function fakeVscode(folder: string, rec: Recorder) {
  const config = {
    get: (_k: string, d: unknown) => d,
    inspect: () => undefined,
    update: async () => {},
    has: () => false,
  };
  return over({
    commands: over({
      registerCommand: (id: string) => {
        rec.commands.push(id);
        return { dispose() {} };
      },
      executeCommand: async (c: string, k?: string, v?: unknown) => {
        if (c === "setContext" && k) rec.contexts[k] = v;
      },
      getCommands: async () => [],
    }),
    window: over({
      createTreeView: (id: string) => {
        rec.treeViews.push(id);
        return { visible: false, dispose() {}, onDidChangeVisibility: stub() };
      },
      registerWebviewViewProvider: (id: string) => {
        rec.webviewViews.push(id);
        return { dispose() {} };
      },
      state: { focused: false },
      activeTextEditor: undefined,
      visibleTextEditors: [],
    }),
    workspace: over({
      getConfiguration: () => config,
      workspaceFolders: [{ uri: fakeUri(folder) }],
      textDocuments: [],
      isTrusted: true,
    }),
    Uri: {
      file: fakeUri,
      parse: fakeUri,
      from: (c: { path: string }) => fakeUri(c.path),
      joinPath: (b: { fsPath: string }, ...p: string[]) => fakeUri([b.fsPath, ...p].join("/")),
    },
    env: over({ uriScheme: "vscode" }),
  }) as unknown as typeof vscodeTypes;
}

const ctx = () =>
  ({
    subscriptions: [],
    extensionUri: fakeUri("/ext"),
    workspaceState: { get: () => undefined, update: async () => {} },
    secrets: { get: async () => undefined, store: async () => {}, delete: async () => {} },
  }) as unknown as vscodeTypes.ExtensionContext;

const dirs: string[] = [];
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});
function ascFolder(): string {
  const d = mkdtempSync(join(tmpdir(), "asc-wiring-"));
  dirs.push(d);
  mkdirSync(join(d, ".git"));
  writeFileSync(join(d, ".git", "asc-workspace"), "");
  return d;
}

/** A bridge whose `Hello` is full; every other call is unsupported. */
function fakeNode(root: string) {
  return (): BridgeNode => {
    const hello = toBinary(
      bridge.HelloResponseSchema,
      create(bridge.HelloResponseSchema, {
        ascVersion: "9.9",
        features: ["plugin.v1", "json-verbs.v1"],
        roots: [{ root, wsId: "ws1" }],
      }),
    );
    return {
      state: "up",
      pid: undefined,
      stderrTail: () => [],
      unary: async (o) => {
        if (o.method.endsWith("/Hello")) return hello;
        throw new BridgeError(12, "unsupported in this test");
      },
      stream: async function* () {},
      close() {},
    };
  };
}

async function activateFake() {
  const root = ascFolder();
  const rec: Recorder = { commands: [], treeViews: [], webviewViews: [], contexts: {}, logs: [] };
  const host = vscodeHost(fakeVscode(root, rec), ctx());
  host.memento.set("asc.gitPromptShown", true);
  host.log = (l) => void rec.logs.push(l);
  const a = await activateWith(host, fakeNode(root));
  await a.started;
  expect(rec.logs).toEqual([]);
  return { a, rec };
}

describe("activation wiring", () => {
  test("registers every command that package.json contributes", async () => {
    const { a, rec } = await activateFake();
    try {
      expect(contributed.filter((c) => !rec.commands.includes(c))).toEqual([]);
    } finally {
      a.dispose();
    }
  });

  test("registers no command twice and none that is neither contributed nor listed as internal", async () => {
    const { a, rec } = await activateFake();
    try {
      expect(rec.commands.filter((c, i) => rec.commands.indexOf(c) !== i)).toEqual([]);
      const known = new Set([...contributed, ...INTERNAL]);
      expect(rec.commands.filter((c) => !known.has(c))).toEqual([]);
    } finally {
      a.dispose();
    }
  });

  test("provides every contributed tree view and the smartlog webview view", async () => {
    const { a, rec } = await activateFake();
    try {
      const views = Object.values(pkg.contributes.views).flat();
      const trees = views.filter((v) => v.type !== "webview").map((v) => v.id);
      const webviews = views.filter((v) => v.type === "webview").map((v) => v.id);
      expect(trees.filter((v) => !rec.treeViews.includes(v))).toEqual([]);
      expect(webviews.filter((v) => !rec.webviewViews.includes(v))).toEqual([]);
    } finally {
      a.dispose();
    }
  });
});

describe("package.json", () => {
  test("menus and keybindings name only contributed commands", () => {
    const known = new Set([...contributed, ...INTERNAL]);
    const named = [
      ...Object.values(pkg.contributes.menus).flat(),
      ...pkg.contributes.keybindings,
    ].map((m) => m.command);
    expect(named.filter((c) => !known.has(c))).toEqual([]);
  });

  test("contributed commands are unique", () => {
    expect(contributed.filter((c, i) => contributed.indexOf(c) !== i)).toEqual([]);
  });

  test("declares the MCP provider and the server setting", () => {
    expect(pkg.contributes.mcpServerDefinitionProviders.map((p) => p.id)).toEqual(["asc"]);
    expect(Object.keys(pkg.contributes.configuration.properties)).toContain("asc.mcp.serverUrl");
  });

  test("never activates on *", () => {
    expect(pkg.activationEvents).not.toContain("*");
  });
});

describe("neutrality", () => {
  // The plugin talks to asc only (bridge and asc serve RPCs). A forge is one backend behind asc serve,
  // so no source file or manifest may name one or hold its URL, token or REST client.
  const walk = (dir: string): string[] =>
    readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
      e.isDirectory() ? walk(join(dir, e.name)) : [join(dir, e.name)],
    );
  const srcDir = join(import.meta.dir, "..", "src");

  test("src names no forge product", () => {
    const hits = walk(srcDir).filter((f) => /forgejo|gitea/i.test(readFileSync(f, "utf8")));
    expect(hits).toEqual([]);
  });

  test("package.json names no forge product", () => {
    const text = readFileSync(join(import.meta.dir, "..", "package.json"), "utf8");
    expect(/forgejo|gitea/i.test(text)).toBe(false);
  });
});
