// Bundles the extension host into dist/extension.cjs with Bun's bundler (swappable to esbuild: the
// options below map one to one). `vscode` is provided by the host. Fails if the bundle is over the
// 350 KB host budget (SPEC 24.4.10).
import { exit } from "node:process";
const BUDGET = 350 * 1024;

const result = await Bun.build({
  entrypoints: ["src/extension.ts"],
  outdir: "dist",
  naming: "extension.cjs",
  target: "node",
  format: "cjs",
  external: ["vscode"],
  minify: true,
  sourcemap: "none",
});

if (!result.success) {
  for (const log of result.logs) process.stderr.write(`${log.message}\n`);
  exit(1);
}
const out = result.outputs[0];
if (!out) {
  process.stderr.write("bundle produced no output\n");
  exit(1);
}
process.stdout.write(`dist/extension.cjs ${out.size} bytes\n`);
if (out.size > BUDGET) {
  process.stderr.write(`host bundle ${out.size} bytes is over the ${BUDGET} byte budget\n`);
  exit(1);
}
