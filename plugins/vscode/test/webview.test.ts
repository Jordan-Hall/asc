import { describe, expect, test } from "bun:test";
import { AvatarFetcher, isPublicIp } from "../src/webview/avatar";
import { buildCsp, mapTheme, pageHtml } from "../src/webview/csp";
import {
  type Disposable,
  type PanelLike,
  registerWebviews,
  type WebviewHostApi,
  type WebviewLike,
} from "../src/webview/host";

const PNG = new Uint8Array([137, 80, 78, 71]);
const png = (extra: Record<string, string> = {}) =>
  new Response(PNG, { headers: { "content-type": "image/png", ...extra } });
const pub = async () => ["93.184.216.34"];

describe("webview csp", () => {
  test("matches the SPEC directive", () => {
    expect(buildCsp("vscode-webview://x")).toBe(
      "default-src 'none'; script-src vscode-webview://x; style-src vscode-webview://x; font-src vscode-webview://x; img-src vscode-webview://x data:; connect-src 'none'",
    );
  });
  test("rejects a cspSource that could break the directive", () => {
    expect(() => buildCsp("a; script-src *")).toThrow();
    expect(() => buildCsp("")).toThrow();
  });
  test("page escapes attributes and has no inline script", () => {
    const h = pageHtml({ cspSource: "s", scriptUri: 'u"><x', view: "graph", root: '/a"b' });
    expect(h).not.toContain('"><x');
    expect(h).not.toMatch(/<script>/);
    expect(h).toContain("&quot;");
  });
  test("theme map skips empty values", () => {
    const t = mapTheme((v) => (v === "--vscode-foreground" ? " #fff " : ""));
    expect(t).toEqual({ "--asc-fg": "#fff" });
  });
});

describe("avatar", () => {
  test("public address classification", () => {
    for (const ip of [
      "127.0.0.1",
      "10.0.0.1",
      "192.168.1.1",
      "172.16.0.1",
      "169.254.1.1",
      "::1",
      "fe80::1",
      "fd00::1",
      "::ffff:127.0.0.1",
      "::ffff:7f00:1",
      "0.0.0.0",
      "not-an-ip",
    ])
      expect(isPublicIp(ip)).toBe(false);
    for (const ip of ["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"])
      expect(isPublicIp(ip)).toBe(true);
  });
  test("returns a data URI and caches", async () => {
    let n = 0;
    const f = new AvatarFetcher({
      resolve: pub,
      fetch: (async () => (n++, png())) as unknown as typeof fetch,
    });
    const a = await f.get("https://a.example/x.png");
    expect(a).toBe("data:image/png;base64,iVBORw==");
    await f.get("https://a.example/x.png");
    expect(n).toBe(1);
  });
  test("refuses http, private hosts, svg and oversize", async () => {
    const mk = (res: () => Response, resolve = pub) =>
      new AvatarFetcher({ resolve, fetch: (async () => res()) as unknown as typeof fetch });
    expect(await mk(png).get("http://a.example/x.png")).toBeNull();
    expect(await mk(png, async () => ["10.0.0.5"]).get("https://a.example/x.png")).toBeNull();
    expect(
      await mk(png, async () => ["93.184.216.34", "127.0.0.1"]).get("https://a.example/x.png"),
    ).toBeNull();
    expect(await mk(png).get("https://127.0.0.1/x.png")).toBeNull();
    expect(await mk(png).get("https://u:p@a.example/x.png")).toBeNull();
    expect(
      await mk(() => new Response("<svg/>", { headers: { "content-type": "image/svg+xml" } })).get(
        "https://a.example/x.svg",
      ),
    ).toBeNull();
    expect(
      await mk(
        () =>
          new Response(new Uint8Array(256 * 1024 + 1), {
            headers: { "content-type": "image/png" },
          }),
      ).get("https://a.example/big.png"),
    ).toBeNull();
  });
  test("redirects are re-checked and capped at 3", async () => {
    const hosts: string[] = [];
    const redirectTo = (loc: string) =>
      new Response(null, { status: 302, headers: { location: loc } });
    const f = new AvatarFetcher({
      resolve: async (h) => (
        hosts.push(h), h === "evil.example" ? ["192.168.0.1"] : ["93.184.216.34"]
      ),
      fetch: (async (u: URL) =>
        u.hostname === "a.example"
          ? redirectTo("https://evil.example/p")
          : png()) as unknown as typeof fetch,
    });
    expect(await f.get("https://a.example/x")).toBeNull();
    expect(hosts).toEqual(["a.example", "evil.example"]);

    let hops = 0;
    const loop = new AvatarFetcher({
      resolve: pub,
      fetch: (async () => (hops++, redirectTo("https://a.example/n"))) as unknown as typeof fetch,
    });
    expect(await loop.get("https://a.example/x")).toBeNull();
    expect(hops).toBe(4);
  });
  test("LRU evicts past 200 entries", async () => {
    const f = new AvatarFetcher({
      resolve: pub,
      fetch: (async () => png()) as unknown as typeof fetch,
    });
    for (let i = 0; i < 205; i++) await f.get(`https://a.example/${i}.png`);
    expect(f.size).toBe(200);
  });
});

function fakeApi() {
  const st = {
    view: null as ((w: WebviewLike) => void) | null,
    serializers: new Map<string, (p: PanelLike, s: unknown) => Promise<void>>(),
    logs: [] as string[],
    created: [] as string[],
    listeners: [] as Array<(m: unknown) => void>,
    posted: [] as unknown[],
    revealed: 0,
  };
  const webview = (): WebviewLike => ({
    html: "",
    cspSource: "vscode-webview://t",
    options: {},
    distUri: (f) => `vscode-webview://t/${f}`,
    postMessage: async (m) => (st.posted.push(m), true),
    onDidReceiveMessage: (l) => (st.listeners.push(l), { dispose() {} }),
  });
  const panel = (): PanelLike => ({
    webview: webview(),
    title: "",
    reveal: () => void st.revealed++,
    onDidDispose: () => ({ dispose() {} }),
    dispose() {},
  });
  const api: WebviewHostApi = {
    distRoot: "dist",
    distHasStyle: false,
    log: (l) => st.logs.push(l),
    createPanel: (vt) => (st.created.push(vt), panel()),
    registerViewProvider: (_id, r) => ((st.view = r), { dispose() {} } as Disposable),
    registerSerializer: (vt, r) => (st.serializers.set(vt, r), { dispose() {} }),
  };
  return { api, st, webview, panel };
}

describe("webview host", () => {
  test("smartlog view gets scripts, local root and CSP", () => {
    const { api, st, webview } = fakeApi();
    registerWebviews({ api, roots: ["/r"] });
    const w = webview();
    st.view?.(w);
    expect(w.options).toEqual({ enableScripts: true, localResourceRoots: ["dist"] });
    expect(w.html).toContain("Content-Security-Policy");
    expect(w.html).toContain('data-asc-view="smartlog"');
  });
  test("open creates once then reveals; serializer restores only known roots", async () => {
    const { api, st, panel } = fakeApi();
    const h = registerWebviews({ api, roots: ["/r", "/s"] });
    expect(h.open("graph")).toBe(true);
    expect(h.open("graph")).toBe(true);
    expect(st.created).toEqual(["asc.graph"]);
    expect(st.revealed).toBe(1);
    const p = panel();
    await st.serializers.get("asc.review")?.(p, { root: "/etc" });
    expect(p.webview.html).toContain('data-asc-root="/r"');
    const q = panel();
    await st.serializers.get("asc.history")?.(q, { root: "/s" });
    expect(q.webview.html).toContain('data-asc-root="/s"');
  });
  test("no roots: open refuses", () => {
    const { api } = fakeApi();
    expect(registerWebviews({ api, roots: [] }).open("graph")).toBe(false);
  });
  test("avatar requests are served; anything else is dropped unless the relay takes it", async () => {
    const { api, st, webview } = fakeApi();
    const avatars = new AvatarFetcher({
      resolve: pub,
      fetch: (async () => png()) as unknown as typeof fetch,
    });
    const seen: unknown[] = [];
    registerWebviews({
      api,
      roots: ["/r"],
      avatars,
      onRequest: (_w, m) => (seen.push(m), (m as { ok?: boolean }).ok === true),
    });
    st.view?.(webview());
    const deliver = st.listeners[0] as (m: unknown) => void;
    deliver({ id: 1, avatar: "https://a.example/x.png" });
    deliver({ id: 2, method: "/asc.Repo/Merge", bytes: new Uint8Array() });
    deliver({ id: 3, ok: true });
    await new Promise((r) => setTimeout(r, 20));
    expect(st.posted).toEqual([{ id: 1, avatar: "data:image/png;base64,iVBORw==" }]);
    expect(st.logs).toHaveLength(1);
    expect(seen).toHaveLength(2);
  });
  test("without a relay every non-avatar message is dropped", async () => {
    const { api, st, webview } = fakeApi();
    registerWebviews({ api, roots: ["/r"] });
    st.view?.(webview());
    (st.listeners[0] as (m: unknown) => void)({
      id: 1,
      hostApi: { provider: "p", method: "DELETE", path: "/x" },
    });
    await new Promise((r) => setTimeout(r, 10));
    expect(st.logs).toHaveLength(1);
    expect(st.posted).toEqual([]);
  });
});
