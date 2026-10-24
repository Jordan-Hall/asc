// After rsbuild: pre-compress every text asset (br and gzip siblings), write dist/manifest.json, and
// refuse a build whose index.html carries inline script or style (the CSP forbids both).
import { createHash } from "node:crypto";
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { extname, join, relative } from "node:path";
import { brotliCompressSync, constants, gzipSync } from "node:zlib";
import { BASE } from "../src/base";

const dist = new URL("../dist", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");

const TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".txt": "text/plain; charset=utf-8",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
  ".webmanifest": "application/manifest+json",
};
const COMPRESSIBLE = new Set([".html", ".js", ".css", ".json", ".svg", ".txt", ".webmanifest"]);

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}

export function inlineViolations(html: string): string[] {
  const out: string[] = [];
  for (const m of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
    if (!/\ssrc=/i.test(m[1] ?? "") || (m[2] ?? "").trim() !== "") out.push("inline <script>");
  }
  if (/<style\b/i.test(html)) out.push("<style> element");
  if (/\sstyle\s*=/i.test(html)) out.push("style attribute");
  if (/\son[a-z]+\s*=/i.test(html)) out.push("event handler attribute");
  if (/javascript:/i.test(html)) out.push("javascript: URL");
  return out;
}

const files: {
  path: string;
  size: number;
  sha256: string;
  type: string;
  hashed: boolean;
  gz?: number;
  br?: number;
}[] = [];

for (const abs of walk(dist)) {
  const path = relative(dist, abs).replaceAll("\\", "/");
  if (path === "manifest.json" || path.endsWith(".br") || path.endsWith(".gz")) continue;
  const ext = extname(path);
  const body = readFileSync(abs);
  const entry: (typeof files)[number] = {
    path,
    size: body.length,
    sha256: createHash("sha256").update(body).digest("hex"),
    type: TYPES[ext] ?? "application/octet-stream",
    hashed: path.startsWith("static/"),
  };
  if (COMPRESSIBLE.has(ext) && body.length > 256) {
    const gz = gzipSync(body, { level: 9 });
    const br = brotliCompressSync(body, { params: { [constants.BROTLI_PARAM_QUALITY]: 11 } });
    writeFileSync(`${abs}.gz`, gz);
    writeFileSync(`${abs}.br`, br);
    entry.gz = gz.length;
    entry.br = br.length;
  }
  files.push(entry);
}

const index = readFileSync(join(dist, "index.html"), "utf8");
const bad = inlineViolations(index);
if (bad.length > 0) {
  process.stderr.write(`${`index.html breaks the CSP: ${bad.join(", ")}`}
`);
  process.exit(1);
}

files.sort((a, b) => a.path.localeCompare(b.path));
writeFileSync(join(dist, "manifest.json"), `${JSON.stringify({ base: BASE, files }, null, 2)}\n`);
process.stdout.write(`${`post-build: ${files.length} files, manifest written`}
`);
