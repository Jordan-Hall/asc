import { beforeAll, describe, expect, test } from "bun:test";
import { renderToString } from "react-dom/server";
import { DEMO_REPO, demoTransport } from "../src/api/fixtures";
import { type AppCtx, AppProvider, createQueries } from "../src/api/queries";
import { loadChangeList } from "../src/api/changes";
import { listRefs, readFile, readTree } from "../src/api/repo";
import { ChangeList, flatten } from "../src/routes/changes";
import { CodeLines, parseLineHash } from "../src/routes/blob";
import { FileTable } from "../src/routes/repo";
import { createAuthStore } from "../src/lib/auth";
import { CacheProvider, QueryCache } from "../src/lib/query";
import { compileRoutes, matchRoutes, RouterProvider } from "../src/lib/router";
import { createRoutes } from "../src/routes";

// Components that render links read window.location; give them a minimal one.
beforeAll(() => {
  Object.assign(globalThis, {
    window: {
      location: { pathname: "/ui/asc/demo", search: "", hash: "" },
      history: {},
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
    },
  });
});

function setup(): { ctx: AppCtx; t: ReturnType<typeof demoTransport> } {
  const t = demoTransport();
  const cache = new QueryCache();
  const ctx: AppCtx = {
    transport: t,
    cache,
    auth: createAuthStore(undefined),
    queries: createQueries({ transport: t, cache }),
  };
  return { ctx, t };
}

function page(ctx: AppCtx, children: React.ReactNode): string {
  const routes = compileRoutes(createRoutes(ctx));
  return renderToString(
    <AppProvider value={ctx}>
      <CacheProvider value={ctx.cache}>
        <RouterProvider routes={routes} base="/ui">
          {children}
        </RouterProvider>
      </CacheProvider>
    </AppProvider>,
  );
}

describe("api over the mock transport", () => {
  test("refs: default branch, branches and tags", async () => {
    const { t } = setup();
    const refs = await listRefs(t, DEMO_REPO);
    expect(refs.defaultBranch).toBe("main");
    expect(refs.refs.map((r) => `${r.kind}:${r.short}`)).toEqual([
      "branch:main",
      "branch:feature-auth",
      "branch:feature-cache",
      "tag:v0.1.0",
    ]);
  });

  test("tree streams, directories first", async () => {
    const { t } = setup();
    const partial: number[] = [];
    const tree = await readTree(t, DEMO_REPO, "main", "", (v) => partial.push(v.rows.length));
    expect(tree.done).toBe(true);
    expect(tree.rows.map((r) => r.name)).toEqual([
      "assets",
      "data",
      "docs",
      "src",
      "package.json",
      "README.md",
    ]);
    expect(tree.rows.filter((r) => r.kind === "dir")).toHaveLength(4);
    expect(partial.length).toBeGreaterThan(0);
  });

  test("a 50,000 line file streams in chunks", async () => {
    const { t } = setup();
    const counts: number[] = [];
    const file = await readFile(t, DEMO_REPO, "main", "data/big.txt", (v) => counts.push(v.count));
    expect(file.count).toBe(50_000);
    expect(file.lines[0]).toStartWith("line 1:");
    expect(counts.length).toBeGreaterThan(5);
    expect(file.done).toBe(true);
  });

  test("a binary file is flagged and not decoded", async () => {
    const { t } = setup();
    const file = await readFile(t, DEMO_REPO, "main", "assets/logo.bin", () => undefined);
    expect(file.binary).toBe(true);
    expect(file.count).toBe(0);
  });

  test("a missing file is NOT_FOUND", async () => {
    const { t } = setup();
    await expect(readFile(t, DEMO_REPO, "main", "nope", () => undefined)).rejects.toMatchObject({
      code: 5,
    });
  });

  test("change list: stacks bottom to top with titles, review and checks", async () => {
    const { t } = setup();
    const view = await loadChangeList(t, DEMO_REPO);
    expect(view.trunk).toBe("main");
    expect(view.stacks.map((s) => s.changes.length)).toEqual([3, 1]);
    const rows = view.stacks[0]?.changes ?? [];
    expect(rows.map((r) => r.title)).toEqual([
      "auth: add token store",
      "auth: sign-in screen",
      "auth: remember for this tab",
    ]);
    expect(rows.map((r) => r.position)).toEqual([0, 1, 2]);
    expect(rows[0]?.review).toMatchObject({ number: 41, approvals: 2, checks: "success" });
    expect(rows[2]?.review?.draft).toBe(true);
    expect(flatten(view)).toHaveLength(6);
  });

  test("queries resolve a ref once, then read by id from the cache", async () => {
    const { ctx, t } = setup();
    const q = ctx.queries.tree(DEMO_REPO, "main", "src");
    const a = await ctx.cache.fetch(q.key, q.run, q.opts);
    const callsAfterFirst = t.calls.length;
    const b = await ctx.cache.fetch(ctx.queries.tree(DEMO_REPO, "main", "src").key, q.run, q.opts);
    expect(a.rows.map((r) => r.name)).toEqual(["lib", "index.ts"]);
    expect(b).toEqual(a);
    expect(t.calls.length).toBe(callsAfterFirst);
    expect(t.calls.filter((c) => c.endsWith("/Resolve"))).toHaveLength(1);
  });
});

describe("routes", () => {
  test("match the documented screens", () => {
    const { ctx } = setup();
    const routes = compileRoutes(createRoutes(ctx));
    const id = (p: string) => matchRoutes(routes, p)?.route.def.id;
    expect(id("/")).toBe("home");
    expect(id("/asc/demo")).toBe("repo");
    expect(id("/asc/demo/tree/main/src")).toBe("tree");
    expect(id("/asc/demo/blob/main/src/index.ts")).toBe("blob");
    expect(id("/asc/demo/changes")).toBe("changes");
    expect(id("/asc/demo/changes/abc")).toBe("change");
    expect(id("/inbox")).toBe("inbox");
    expect(id("/settings")).toBe("settings");
    expect(id("/a/b/c/d/e/f/g")).toBeUndefined();
  });

  test("repo and tree share one lazy view so navigation does not remount", () => {
    const { ctx } = setup();
    const routes = compileRoutes(createRoutes(ctx));
    expect(matchRoutes(routes, "/asc/demo")?.route.View).toBe(
      matchRoutes(routes, "/asc/demo/tree/main/src")?.route.View,
    );
  });
});

describe("screens", () => {
  test("file table renders a window of rows with links", async () => {
    const { ctx } = setup();
    const q = ctx.queries.tree(DEMO_REPO, "main", "");
    const tree = await ctx.cache.fetch(q.key, q.run, q.opts);
    const html = page(
      ctx,
      <FileTable owner="asc" repo="demo" rev="main" path="" rows={tree.rows} />,
    );
    expect(html).toContain('href="/ui/asc/demo/tree/main/src"');
    expect(html).toContain('href="/ui/asc/demo/blob/main/README.md"');
    expect(html).toContain('aria-label="Files"');
  });

  test("a directory view starts with a parent row", async () => {
    const { ctx } = setup();
    const q = ctx.queries.tree(DEMO_REPO, "main", "src/lib");
    const tree = await ctx.cache.fetch(q.key, q.run, q.opts);
    const html = page(
      ctx,
      <FileTable owner="asc" repo="demo" rev="main" path="src/lib" rows={tree.rows} />,
    );
    expect(html).toContain('href="/ui/asc/demo/tree/main/src"');
    expect(html).toContain("hello.ts");
  });

  test("a huge file renders a bounded number of lines", async () => {
    const { t } = setup();
    const file = await readFile(t, DEMO_REPO, "main", "data/big.txt", () => undefined);
    const html = renderToString(<CodeLines file={file} selected={undefined} />);
    const rendered = html.match(/data-row=/g)?.length ?? 0;
    expect(rendered).toBeGreaterThan(20);
    expect(rendered).toBeLessThan(80);
    expect(html).toContain('id="L1"');
  });

  test("change list shows stack headers, badges and links", async () => {
    const { ctx } = setup();
    const q = ctx.queries.changes(DEMO_REPO);
    const view = await ctx.cache.fetch(q.key, q.run, q.opts);
    const html = page(ctx, <ChangeList owner="asc" repo="demo" view={view} />);
    expect(html).toContain("feature-auth");
    expect(html).toContain("3 changes onto main");
    expect(html).toContain("auth: add token store");
    expect(html).toContain("2 approved");
    expect(html).toContain("Checks passed");
  });

  test("an empty change list says how to start", () => {
    const { ctx } = setup();
    const html = page(
      ctx,
      <ChangeList owner="asc" repo="demo" view={{ trunk: "main", stacks: [] }} />,
    );
    expect(html).toContain("No open changes");
  });

  test("line anchors parse strictly", () => {
    expect(parseLineHash("#L42")).toBe(42);
    expect(parseLineHash("#L")).toBeUndefined();
    expect(parseLineHash("#L1;alert(1)")).toBeUndefined();
  });
});
