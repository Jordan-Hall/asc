import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import {
  checkArgs,
  checkPackage,
  checkTag,
  checkVsixFiles,
  checkWorkflow,
  WORKFLOW_PATH,
  zipEntries,
} from "../scripts/release-check";

const workflow = () =>
  Bun.YAML.parse(readFileSync(WORKFLOW_PATH, "utf8")) as {
    jobs: { release: { steps: { uses?: string; run?: string }[] } };
  };

describe("release-check", () => {
  test("the shipped workflow passes", () => {
    expect(checkWorkflow(workflow())).toEqual([]);
  });

  test("a token in argv or a secret in a run line fails", () => {
    const wf = workflow();
    const steps = wf.jobs.release.steps;
    const vsce = steps.find((s) => s.run?.includes("vsce publish"));
    if (vsce)
      vsce.run =
        "bunx vsce publish --packagePath plugins/vscode/asc.vsix --pat ${{ secrets.VSCE_PAT }}";
    const problems = checkWorkflow(wf).join("\n");
    expect(problems).toContain("secret interpolated");
    expect(problems).toContain("token in argv");
  });

  test("unpinned actions and publish before the check fail", () => {
    const wf = workflow();
    const steps = wf.jobs.release.steps;
    if (steps[0]) steps[0].uses = "actions/checkout@v4";
    wf.jobs.release.steps = [
      ...steps.filter((s) => s.run?.includes("publish")),
      ...steps.filter((s) => !s.run?.includes("publish")),
    ];
    const problems = checkWorkflow(wf).join("\n");
    expect(problems).toContain("not pinned");
    expect(problems).toContain("must run before a publish");
  });

  test("tag must equal the version and have a changelog entry", () => {
    const log = "## [Unreleased]\n\n## [1.2.3] - 2026-01-01\n";
    expect(checkTag("vscode-v1.2.3", "1.2.3", log)).toEqual([]);
    expect(checkTag("vscode-v1.2.4", "1.2.4", log)).toHaveLength(1);
    expect(checkTag("vscode-v1.2.4", "1.2.3", "")).toHaveLength(2);
    expect(checkTag("v1.2.3", "1.2.3", log)).toHaveLength(1);
  });

  test("package metadata", () => {
    const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
    expect(checkPackage(pkg)).toEqual([]);
    expect(checkPackage({ ...pkg, publisher: "x", enabledApiProposals: ["a"] })).toHaveLength(2);
  });

  test("vsix contents", () => {
    const ok = [
      "package.json",
      "README.md",
      "CHANGELOG.md",
      "icon.png",
      "dist/extension.cjs",
      "dist/ui/index.js",
    ];
    expect(checkVsixFiles(ok)).toEqual([]);
    expect(checkVsixFiles([...ok.slice(1), "src/extension.ts", ".env"])).toHaveLength(3);
  });

  test("argv never holds a token", () => {
    expect(checkArgs("asc.vsix", { VSCE_PAT: "secret-token" })).toEqual([]);
    expect(checkArgs("secret-token.vsix", { VSCE_PAT: "secret-token" })).not.toEqual([]);
  });

  test("zip directory reader rejects non-zips", () => {
    expect(() => zipEntries(new Uint8Array(64))).toThrow();
  });
});
