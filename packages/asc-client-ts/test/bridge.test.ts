// P09 acceptance: the bridgeNode transport against a real `asc api --stdio` on a fixture workspace (ASC_BIN,
// default target/release/asc), plus a scripted bridge for the cases a real one cannot produce on demand
// (a crash mid-stream, a stream that never ends).
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import {
  type BridgeNode,
  BridgeError,
  bridgeNode,
  callUnary,
  changeIdFromString,
  changeIdToString,
  Code,
  encodeFrame,
  FrameError,
  FrameReader,
  fromHex,
  MAX_FRAME,
  resumable,
  toHex,
} from "../src/index";
import * as bridge from "../src/gen/asc/v1/bridge_pb";
import * as workspace from "../src/gen/asc/v1/workspace_pb";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../../..");
const exe = process.platform === "win32" ? ".exe" : "";
const ascBin = process.env.ASC_BIN ?? join(repoRoot, "target", "release", `asc${exe}`);
const haveAsc = existsSync(ascBin);

let tmp: string;
let ws: string;
let sandboxEnv: Record<string, string | undefined>;
const nodes: BridgeNode[] = [];

function track<T extends BridgeNode>(n: T): T {
  nodes.push(n);
  return n;
}

beforeAll(() => {
  tmp = mkdtempSync(join(tmpdir(), "asc-client-"));
  const user = join(tmp, "user");
  mkdirSync(user, { recursive: true });
  ws = join(tmp, "ws");
  mkdirSync(ws);
  sandboxEnv = {
    ...process.env,
    ASC_HOME: join(tmp, "asc-home"),
    HOME: user,
    USERPROFILE: user,
    XDG_CONFIG_HOME: join(user, ".config"),
    GIT_CONFIG_GLOBAL: join(user, ".gitconfig"),
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_TERMINAL_PROMPT: "0",
    ASC_NO_DAEMON: "1",
    NO_COLOR: "1",
  };
  if (haveAsc) {
    const init = spawnSync(ascBin, ["init", "-q"], {
      cwd: ws,
      env: sandboxEnv as NodeJS.ProcessEnv,
    });
    if (init.status !== 0) throw new Error(`asc init failed: ${init.stderr.toString()}`);
  }
});

afterAll(() => {
  for (const n of nodes) n.close();
  try {
    // Windows keeps a closing child's directory busy for a moment.
    rmSync(tmp, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  } catch {
    // A leftover temp directory is harmless.
  }
});

describe("framing", () => {
  test("a frame round-trips and arrives in pieces", () => {
    const frame = create(bridge.BridgeFrameSchema, {
      id: 7,
      f: { case: "reply", value: new Uint8Array([1, 2, 3]) },
    });
    const wire = encodeFrame(frame);
    expect(new DataView(wire.buffer).getUint32(0, true)).toBe(wire.length - 4);
    const r = new FrameReader();
    const out = [
      ...r.push(wire.subarray(0, 3)),
      ...r.push(wire.subarray(3, 6)),
      ...r.push(wire.subarray(6)),
    ];
    expect(out).toHaveLength(1);
    expect(out[0]?.id).toBe(7);
    // Two frames in one chunk.
    expect(new FrameReader().push(new Uint8Array([...wire, ...wire]))).toHaveLength(2);
  });

  test("an oversize length is refused before any body is read", () => {
    const head = new Uint8Array(4);
    new DataView(head.buffer).setUint32(0, MAX_FRAME + 1, true);
    expect(() => new FrameReader().push(head)).toThrow(FrameError);
  });
});

describe("ids", () => {
  test("hex and change ids", () => {
    expect(toHex(fromHex("00ff10"))).toBe("00ff10");
    expect(() => fromHex("zz")).toThrow();
    const id = new Uint8Array(16);
    id[0] = 0x0f;
    id[15] = 0xf0;
    const text = changeIdToString(id);
    expect(text).toHaveLength(32);
    expect(text.startsWith("zk")).toBe(true);
    expect(text.endsWith("kz")).toBe(true);
    expect(changeIdFromString(text)).toEqual(id);
    expect(() => changeIdFromString("abc")).toThrow();
  });
});

describe.skipIf(!haveAsc)("a real asc api --stdio", () => {
  test("Hello reports features and the fixture workspace; the spawn argv is fixed", async () => {
    const t = track(bridgeNode({ command: ascBin, cwd: ws, env: sandboxEnv }));
    expect(t.state).toBe("idle");
    const hello = await callUnary(t, bridge.Bridge.method.hello, { roots: [ws] });
    expect(t.state).toBe("up");
    expect(hello.features).toContain("plugin.v1");
    expect(hello.ascVersion).not.toBe("");
    expect(hello.roots[0]?.wsId).not.toBe("");
  });

  test("routing is deny-by-default and errors keep their code", async () => {
    const t = track(bridgeNode({ command: ascBin, cwd: ws, env: sandboxEnv }));
    const denied = t.unary({ method: "asc.v1.Workspace/Shutdown", request: new Uint8Array(0) });
    await expect(denied).rejects.toMatchObject({ code: 7 });
    const unknown = t.unary({ method: "asc.v1.Repo/NoSuchMethod", request: new Uint8Array(0) });
    await expect(unknown).rejects.toBeInstanceOf(BridgeError);
    // The same bridge still answers.
    const again = await callUnary(t, bridge.Bridge.method.hello, {});
    expect(again.features).toContain("plugin.v1");
  });

  test("a killed bridge fails the calls in flight and restarts on the next call", async () => {
    const exits: (number | null)[] = [];
    const t = track(
      bridgeNode({
        command: ascBin,
        cwd: ws,
        env: sandboxEnv,
        backoffMs: [20],
        onExit: (c) => exits.push(c),
      }),
    );
    await callUnary(t, bridge.Bridge.method.hello, {});
    const first = t.pid;
    expect(first).toBeDefined();
    process.kill(first as number);
    await new Promise((r) => setTimeout(r, 300));
    expect(t.state).toBe("reconnecting");
    expect(exits).toHaveLength(1);
    const hello = await callUnary(t, bridge.Bridge.method.hello, {});
    expect(hello.features).toContain("plugin.v1");
    expect(t.pid).not.toBe(first);
  });

  test("Run is refused for a verb outside the schema (no shell, no free argv)", async () => {
    const t = track(bridgeNode({ command: ascBin, cwd: ws, env: sandboxEnv }));
    const run = t.stream({
      method: "asc.v1.Bridge/Run",
      request: toBinary(
        bridge.RunRequestSchema,
        create(bridge.RunRequestSchema, { verb: "no-such-verb", root: ws }),
      ),
    });
    await expect(run.next()).rejects.toBeInstanceOf(BridgeError);
  });
});

describe("a scripted bridge", () => {
  let script: string;
  const frameModule = join(repoRoot, "packages/asc-client-ts/src/transport/frame.ts").replaceAll(
    "\\",
    "/",
  );
  const genModule = join(repoRoot, "packages/asc-client-ts/src/gen/asc/v1").replaceAll("\\", "/");

  beforeAll(() => {
    script = join(tmp, "fake-bridge.ts");
    writeFileSync(
      script,
      `
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { encodeFrame, FrameReader, makeFrame } from "${frameModule}";
import { WatchEventSchema, WatchRequestSchema } from "${genModule}/workspace_pb";
const reader = new FrameReader();
const cancelled = [];
const send = (id, f, cb) => process.stdout.write(encodeFrame(makeFrame(id, f)), cb);
process.stdin.on("data", (d) => {
  for (const f of reader.push(d)) {
    if (f.f.case === "cancel") { cancelled.push(f.id); continue; }
    if (f.f.case !== "call") continue;
    const c = f.f.value;
    if (c.method === "asc.v1.Workspace/Watch") {
      const since = fromBinary(WatchRequestSchema, c.request).sinceToken;
      const ev = toBinary(WatchEventSchema, create(WatchEventSchema, { token: since + "+1" }));
      send(f.id, { case: "reply", value: ev }, () => {
        if (since === "") process.exit(1);
        send(f.id, { case: "end", value: { code: 0, message: "" } });
      });
    } else if (c.method === "x/Cancelled") {
      send(f.id, { case: "reply", value: new TextEncoder().encode(cancelled.join(",")) });
      send(f.id, { case: "end", value: { code: 0, message: "" } });
    }
    else if (c.method === "x/Hang") send(f.id, { case: "reply", value: new Uint8Array([1]) });
    // Then it never ends.
  }
});
`,
    );
  });

  const fake = () =>
    track(bridgeNode({ command: process.execPath, args: [script], backoffMs: [20] }));

  test("a dropped stream and an aborted stream send a cancel frame", async () => {
    const t = fake();
    for await (const _ of t.stream({ method: "x/Hang", request: new Uint8Array(0) })) break;

    const ac = new AbortController();
    const it = t.stream({ method: "x/Hang", request: new Uint8Array(0), signal: ac.signal });
    await it.next();
    const aborted = it.next();
    ac.abort();
    await expect(aborted).rejects.toBeDefined();
    await new Promise((r) => setTimeout(r, 100));

    const seen = new TextDecoder().decode(
      await t.unary({ method: "x/Cancelled", request: new Uint8Array(0) }),
    );
    expect(seen.split(",").filter(Boolean)).toHaveLength(2);
  });

  test("Watch resubscribes from its last token after the bridge dies mid-stream", async () => {
    const t = fake();
    const sinceTokens: string[] = [];
    const tokens: string[] = [];
    const events = resumable(t, {
      method: "asc.v1.Workspace/Watch",
      backoffMs: [10],
      request: (token) => {
        sinceTokens.push(token ?? "");
        return toBinary(
          workspace.WatchRequestSchema,
          create(workspace.WatchRequestSchema, { wsId: "w", sinceToken: token ?? "" }),
        );
      },
      tokenOf: (r) => fromBinary(workspace.WatchEventSchema, r).token || undefined,
    });
    for await (const r of events) tokens.push(fromBinary(workspace.WatchEventSchema, r).token);
    expect(tokens).toEqual(["+1", "+1+1"]);
    expect(sinceTokens).toEqual(["", "+1"]);
  });

  test("calls in flight fail with UNAVAILABLE when the bridge dies", async () => {
    const t = fake();
    const it = t.stream({ method: "x/Hang", request: new Uint8Array(0) });
    await it.next();
    const hang = it.next().catch((e: unknown) => e);
    await new Promise((r) => setTimeout(r, 50));
    process.kill(t.pid as number);
    expect(await hang).toMatchObject({ code: Code.Unavailable, lost: true });
  });

  test("a bridge that cannot start fails the call without hanging", async () => {
    const t = track(bridgeNode({ command: join(tmp, "no-such-asc"), backoffMs: [10] }));
    await expect(t.unary({ method: "x/Y", request: new Uint8Array(0) })).rejects.toMatchObject({
      code: Code.Unavailable,
    });
  });
});

describe("bundle", () => {
  async function size(source: string): Promise<number> {
    const entry = join(tmp, `entry-${Math.random().toString(36).slice(2)}.ts`);
    writeFileSync(entry, source);
    const out = await Bun.build({
      entrypoints: [entry],
      target: "node",
      format: "esm",
      minify: true,
    });
    expect(out.success).toBe(true);
    return out.outputs.reduce((n, o) => n + o.size, 0);
  }
  const src = join(repoRoot, "packages/asc-client-ts/src").replaceAll("\\", "/");

  test("the minified client and a workspace consumer are at most 120 KB", async () => {
    const core = await size(`export * from "${src}/index";`);
    const consumer = await size(
      `export * from "${src}/index"; export { Workspace } from "${src}/gen/asc/v1/workspace_pb";`,
    );
    expect(core).toBeLessThanOrEqual(120 * 1024);
    expect(consumer).toBeLessThanOrEqual(120 * 1024);
  });
});
