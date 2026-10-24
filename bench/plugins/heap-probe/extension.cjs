// Test-only extension (SPEC 24.4.10, P44a). Loaded with --extensionDevelopmentPath next to
// plugins/vscode in a window started with --js-flags=--expose-gc. It waits for `asc.asc`, times
// its activation, forces GC, reads the used heap and writes one JSON file named by ASC_PROBE_OUT,
// then quits the window. It never touches the network and reads no secrets.
const vscode = require("vscode");
const v8 = require("node:v8");
const fs = require("node:fs");

async function settle(ms) {
  await new Promise((r) => setTimeout(r, ms));
}

function usedHeap() {
  if (typeof globalThis.gc === "function") {
    globalThis.gc();
    globalThis.gc();
  }
  return v8.getHeapStatistics().used_heap_size;
}

exports.activate = async function activate() {
  const out = process.env.ASC_PROBE_OUT;
  if (!out) return;
  const result = { gc: typeof globalThis.gc === "function", activateMs: null, heapBytes: 0, ascActive: false };
  try {
    const ext = vscode.extensions.getExtension("asc.asc");
    if (ext) {
      const t0 = performance.now();
      await ext.activate();
      result.activateMs = performance.now() - t0;
      result.ascActive = ext.isActive;
    }
    await settle(2000);
    result.heapBytes = usedHeap();
  } catch (e) {
    result.error = String(e && e.message ? e.message : e);
  }
  fs.writeFileSync(out, JSON.stringify(result));
  await vscode.commands.executeCommand("workbench.action.quit");
};
