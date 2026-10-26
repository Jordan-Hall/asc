import { describe, expect, test } from "bun:test";
import {
  configureAgents,
  definitions,
  installArgs,
  isSafeServerUrl,
  type McpApi,
  type McpStdio,
  registerMcp,
} from "../src/mcp";

const ASC = "/bin/asc";

function providerApi() {
  const st = { provide: (() => []) as () => McpStdio[], fired: 0, disposed: 0, id: "" };
  const api: McpApi = {
    registerProvider: (id, provide, changed) => {
      st.id = id;
      st.provide = provide;
      changed(() => {
        st.fired += 1;
      });
      return { dispose: () => void (st.disposed += 1) };
    },
    cursorRegister: null,
  };
  return { api, st };
}

describe("mcp definitions", () => {
  test("one stdio `asc mcp` per root, no env or token", () => {
    const d = definitions(ASC, ["/a", "/b"], "https://forge.example");
    expect(d).toEqual([
      { label: "asc (1)", command: ASC, args: ["mcp"], cwd: "/a" },
      { label: "asc (2)", command: ASC, args: ["mcp"], cwd: "/b" },
    ]);
    expect(JSON.stringify(d)).not.toMatch(/token|env|Authorization/i);
  });
  test("remote only without roots, never alongside stdio", () => {
    expect(definitions(ASC, [], "https://forge.example")).toEqual([
      { label: "asc-remote", command: ASC, args: ["mcp", "--remote", "https://forge.example"] },
    ]);
    expect(
      definitions(ASC, ["/a"], "https://forge.example").some((x) => x.args.includes("--remote")),
    ).toBe(false);
  });
  test("refuses http, credentials, junk and a missing asc", () => {
    expect(isSafeServerUrl("http://forge.example")).toBe(false);
    expect(isSafeServerUrl("https://u:p@forge.example")).toBe(false);
    expect(isSafeServerUrl("nope")).toBe(false);
    expect(definitions(ASC, [], "http://x")).toEqual([]);
    expect(definitions(null, ["/a"], "")).toEqual([]);
  });
});

describe("registerMcp", () => {
  test("provider path serves live definitions and fires on refresh", () => {
    const { api, st } = providerApi();
    let roots = ["/a"];
    const reg = registerMcp({ api, ascPath: () => ASC, roots: () => roots, serverUrl: () => "" });
    expect(reg.mode).toBe("provider");
    expect(st.id).toBe("asc");
    expect(st.provide()).toHaveLength(1);
    roots = [];
    reg.refresh();
    expect(st.fired).toBe(1);
    expect(st.provide()).toEqual([]);
    reg.dispose();
    expect(st.disposed).toBe(1);
  });
  test("Cursor fallback registers and unregisters", () => {
    const live = new Set<string>();
    const api: McpApi = {
      registerProvider: null,
      cursorRegister: (d) => {
        live.add(d.label);
        return { dispose: () => void live.delete(d.label) };
      },
    };
    let roots = ["/a"];
    const reg = registerMcp({ api, ascPath: () => ASC, roots: () => roots, serverUrl: () => "" });
    expect(reg.mode).toBe("cursor");
    expect([...live]).toEqual(["asc"]);
    roots = ["/a", "/b"];
    reg.refresh();
    expect([...live].toSorted()).toEqual(["asc (1)", "asc (2)"]);
    reg.dispose();
    expect(live.size).toBe(0);
  });
  test("no API is a no-op", () => {
    const reg = registerMcp({
      api: { registerProvider: null, cursorRegister: null },
      ascPath: () => ASC,
      roots: () => ["/a"],
      serverUrl: () => "",
    });
    expect(reg.mode).toBe("none");
    reg.refresh();
    reg.dispose();
  });
});

describe("configure agents", () => {
  test("runs `asc mcp install --scope project` in the root", async () => {
    const calls: [string, string[], string][] = [];
    const msg = await configureAgents(ASC, ["/a"], async (c, a, w) => void calls.push([c, a, w]));
    expect(calls).toEqual([[ASC, installArgs(), "/a"]]);
    expect(installArgs()).toEqual(["mcp", "install", "--scope", "project"]);
    expect(msg).toContain("configured");
  });
  test("reports failure without throwing or leaking the error", async () => {
    const msg = await configureAgents(ASC, ["/a"], async () => {
      throw new Error("secret-path");
    });
    expect(msg).not.toContain("secret-path");
    expect(await configureAgents(null, ["/a"])).toContain("asc.path");
    expect(await configureAgents(ASC, [])).toContain("workspace");
  });
});
