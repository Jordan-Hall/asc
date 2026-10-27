// Runs inside the editor's extension host (extensionTestsPath). Not part of the shipped VSIX.
// Reads ASC_SMOKE_MODE ("asc" | "plain") and ASC_SMOKE_OUT, writes a JSON report there, and throws
// on a failed assertion so the editor exits non-zero.
const fs = require("node:fs");
const vscode = require("vscode");

const EXT_ID = "asc.asc";
const REQUIRED_APIS = [
  ["scm.createSourceControl", () => typeof vscode.scm?.createSourceControl === "function"],
  [
    "workspace.registerFileSystemProvider",
    () => typeof vscode.workspace.registerFileSystemProvider === "function",
  ],
  [
    "comments.createCommentController",
    () => typeof vscode.comments?.createCommentController === "function",
  ],
  ["window.createWebviewPanel", () => typeof vscode.window.createWebviewPanel === "function"],
  ["window.createTreeView", () => typeof vscode.window.createTreeView === "function"],
  ["tasks.registerTaskProvider", () => typeof vscode.tasks?.registerTaskProvider === "function"],
  [
    "languages.registerCodeLensProvider",
    () => typeof vscode.languages.registerCodeLensProvider === "function",
  ],
  [
    "window.registerFileDecorationProvider",
    () => typeof vscode.window.registerFileDecorationProvider === "function",
  ],
];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(fn, ms) {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) return null;
    await sleep(250);
  }
}

exports.run = async function run() {
  const mode = process.env.ASC_SMOKE_MODE;
  const out = process.env.ASC_SMOKE_OUT;
  const report = {
    mode,
    editorVersion: vscode.version,
    appName: vscode.env.appName,
    failures: [],
    notes: [],
  };
  const fail = (m) => report.failures.push(m);
  try {
    report.missingApis = REQUIRED_APIS.filter(([, ok]) => !ok()).map(([n]) => n);
    if (report.missingApis.length) fail(`missing stable APIs: ${report.missingApis.join(", ")}`);

    const ext = vscode.extensions.getExtension(EXT_ID);
    if (!ext) {
      fail(`${EXT_ID} is not installed`);
    } else if (mode === "asc") {
      await ext.activate();
      if (!ext.isActive) fail("extension did not activate in the asc folder");
      const hasProbe = await waitFor(
        async () => (await vscode.commands.getCommands(true)).includes("asc.probe"),
        3000,
      );
      if (hasProbe) {
        const state = await waitFor(async () => {
          const s = await vscode.commands.executeCommand("asc.probe");
          return s && s.ready ? s : null;
        }, 30000);
        report.probe = state;
        if (!state) fail("asc.probe never reported ready");
        else {
          const want = JSON.parse(process.env.ASC_SMOKE_EXPECT || "{}");
          for (const [k, v] of Object.entries(want)) {
            if (state.groups?.[k] !== v)
              fail(`SCM group ${k}: want ${v}, got ${state.groups?.[k]}`);
          }
        }
      } else {
        // An older build registers no `asc.probe` command: activation is asserted, counts are not.
        report.notes.push("asc.probe not registered: SCM group counts not asserted");
        report.probeMissing = true;
      }
    } else {
      // Non-asc folder: the extension may stay unactivated or activate and stand down; either way
      // it must not report asc folders. The driver also checks no workspace settings were written.
      await sleep(3000);
      const hasProbe = (await vscode.commands.getCommands(true)).includes("asc.probe");
      if (hasProbe) {
        const s = await vscode.commands.executeCommand("asc.probe");
        report.probe = s;
        if (s && (s.ascFolders?.length ?? 0) !== 0) fail("asc folders reported in a non-asc folder");
      } else {
        report.probeMissing = true;
      }
    }
  } catch (e) {
    fail(`exception: ${e && e.stack ? e.stack : e}`);
  }
  fs.writeFileSync(out, JSON.stringify(report, null, 2));
  if (report.failures.length) throw new Error(report.failures.join("; "));
};
