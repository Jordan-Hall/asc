// Production-like static server for the Playwright suite: builds the mock-transport bundle into
// dist-e2e, then serves it under /ui with the same rules and security headers as `asc serve`
// (docs/WEB-UI.md section 8): extension-less paths get index.html, unknown files 404, and the CSP is
// read from crates/asc-serve/src/web/mod.rs so the suite cannot drift from the server.
import { existsSync, readFileSync, statSync } from "node:fs";
import { join, resolve, sep } from "node:path";
import { extname, normalize } from "node:path/posix";

const webDir = new URL("..", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");
const out = join(webDir, "dist-e2e");
const port = Number(process.env.PORT ?? 4180);

function cspFromServer(): string {
  const src = readFileSync(join(webDir, "..", "crates/asc-serve/src/web/mod.rs"), "utf8");
  const m = /const CSP: &str = "((?:[^"\\]|\\.)*)";/s.exec(src);
  if (!m?.[1]) throw new Error("CSP constant not found in asc-serve web/mod.rs");
  // A Rust string continuation (backslash, newline) drops the newline and the next line's indent.
  return m[1].replace(/\\\r?\n\s*/g, "");
}

if (process.env.ASC_E2E_SKIP_BUILD !== "1") {
  const build = Bun.spawnSync(["bun", "x", "rsbuild", "build"], {
    cwd: webDir,
    env: { ...process.env, ASC_WEB_MOCK: "1", ASC_WEB_OUT: "dist-e2e", NODE_ENV: "production" },
    stdout: "inherit",
    stderr: "inherit",
  });
  if (build.exitCode !== 0) throw new Error("rsbuild build failed");
}

const csp = cspFromServer();
const types: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".json": "application/json",
};
const headers = (type: string, cache: string): Record<string, string> => ({
  "content-type": type,
  "cache-control": cache,
  "content-security-policy": csp,
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  "x-frame-options": "DENY",
});

Bun.serve({
  hostname: "127.0.0.1",
  port,
  fetch(req) {
    const { pathname } = new URL(req.url);
    if (pathname === "/ui") return Response.redirect("/ui/", 308);
    if (!pathname.startsWith("/ui/")) return new Response("not found", { status: 404 });
    let decoded: string;
    try {
      decoded = decodeURIComponent(pathname.slice(4));
    } catch {
      return new Response("bad request", { status: 400 });
    }
    // No backslashes, NUL or ".." segments: nothing that can leave the build directory.
    if (/[\\\0]/.test(decoded) || decoded.split("/").includes("..")) {
      return new Response("not found", { status: 404 });
    }
    const rel = normalize(decoded).replace(/^\/+/, "");
    const ext = extname(rel);
    // Same rule as asc-serve: a nested source-file path that a browser opens is a client route.
    const navigation = (req.headers.get("accept") ?? "").includes("text/html");
    const sourceFile = ext !== "" && rel.includes("/") && !rel.startsWith("static/") && navigation;
    if (ext && !sourceFile) {
      const file = resolve(out, rel);
      if (!file.startsWith(resolve(out) + sep)) return new Response("not found", { status: 404 });
      if (!existsSync(file) || !statSync(file).isFile()) {
        return new Response("not found", { status: 404 });
      }
      const hashed = rel.startsWith("static/");
      return new Response(Bun.file(file), {
        headers: headers(
          types[ext] ?? "application/octet-stream",
          hashed ? "public, max-age=31536000, immutable" : "no-cache",
        ),
      });
    }
    return new Response(Bun.file(join(out, "index.html")), {
      headers: headers("text/html; charset=utf-8", "no-cache"),
    });
  },
});
