import { fileURLToPath } from "node:url";
import { defineConfig } from "@rsbuild/core";
import { pluginReact } from "@rsbuild/plugin-react";

// The app is mounted by `asc serve` under /ui (docs/WEB-UI.md section 8). Hashed assets live in
// /ui/static; index.html and theme-init.js are the only unhashed files.
export const BASE = "/ui";

// The demo transport is bundled only for development or an explicit ASC_WEB_MOCK=1 build.
const mock = process.env.ASC_WEB_MOCK === "1" || process.env.NODE_ENV !== "production";

export default defineConfig({
  plugins: [pluginReact()],
  source: {
    entry: { index: "./src/main.tsx" },
    define: { MOCK_BUILD: JSON.stringify(mock) },
  },
  tools: {
    rspack: (config, { rspack }) => {
      // `@asc/asc-client` re-exports a node-only transport; the browser never calls it, so its one
      // node import is replaced by a stub and the transport is tree-shaken away.
      const stub = fileURLToPath(new URL("./src/stubs/child-process.ts", import.meta.url));
      config.plugins ??= [];
      config.plugins.push(new rspack.NormalModuleReplacementPlugin(/^node:child_process$/, stub));
    },
  },
  server: { base: BASE },
  html: {
    template: "./src/index.html",
    title: "asc",
    inject: "head",
    scriptLoading: "defer",
    tags: [
      // External, blocking, before first paint: sets the theme class. No inline script (CSP).
      {
        tag: "script",
        attrs: { src: `${BASE}/theme-init.js` },
        head: true,
        append: false,
        publicPath: false,
      },
    ],
  },
  output: {
    target: "web",
    assetPrefix: `${BASE}/`,
    sourceMap: false,
    legalComments: "none",
    filenameHash: true,
    inlineScripts: false,
    inlineStyles: false,
    distPath: {
      root: process.env.ASC_WEB_OUT ?? "dist",
      js: "static/js",
      css: "static/css",
      svg: "static/img",
      image: "static/img",
      font: "static/font",
      media: "static/media",
    },
    copy: [{ from: "public", to: "." }],
  },
  performance: { chunkSplit: { strategy: "split-by-experience" }, removeConsole: false },
});
