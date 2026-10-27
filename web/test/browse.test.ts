import { describe, expect, test } from "bun:test";
import { toHex } from "@asc/asc-client";
import { CiStatus } from "@asc/asc-client/ci";
import {
  findFiles,
  loadAgentRuns,
  loadBlame,
  loadChangeFiles,
  loadCommit,
  loadHistory,
  loadJobLog,
  loadRepoSettings,
  searchCode,
} from "../src/api/browse";
import { listRuns } from "../src/api/ci";
import { changeIdBytes, DEMO_REPO, demoTransport, fakeId } from "../src/api/fixtures";
import { changeIdToString } from "@asc/asc-client";

const t = demoTransport();
const tip = toHex(fakeId(`${DEMO_REPO}:history:0`));

describe("browse reads over the demo transport", () => {
  test("history lists commits newest first and reports more", async () => {
    const all = await loadHistory(t, DEMO_REPO, "main", "", 50);
    expect(all.rows.length).toBe(6);
    expect(all.more).toBe(false);
    expect(all.rows[0]?.id).toBe(tip);
    const some = await loadHistory(t, DEMO_REPO, "main", "", 2);
    expect(some.rows.length).toBe(2);
    expect(some.more).toBe(true);
  });

  test("a commit has its message, files and patch", async () => {
    const c = await loadCommit(t, DEMO_REPO, tip.slice(0, 10));
    expect(c?.id).toBe(tip);
    expect(c?.parents.length).toBe(1);
    expect(c?.files.map((f) => f.path)).toEqual(["src/index.ts", "docs/guide.md"]);
    expect(c?.patch).toContain("diff --git a/src/index.ts");
    expect(await loadCommit(t, DEMO_REPO, "ffffffffff")).toBeUndefined();
  });

  test("blame covers the file with ordered groups", async () => {
    const groups = await loadBlame(t, DEMO_REPO, "main", "README.md");
    expect(groups.length).toBeGreaterThan(0);
    expect(groups[0]?.start).toBe(1);
    const next = groups[1];
    if (next) expect(next.start).toBe((groups[0]?.start ?? 0) + (groups[0]?.len ?? 0));
    expect(groups[0]?.author).not.toBe("");
  });

  test("code search and the file finder", async () => {
    const hits = await searchCode(t, DEMO_REPO, "HEAD", "hello", "literal");
    expect(hits.files.some((f) => f.path === "src/index.ts")).toBe(true);
    expect(hits.files[0]?.matches[0]?.line).toBeGreaterThan(0);
    const found = await findFiles(t, DEMO_REPO, "HEAD", "INDEX");
    expect(found.paths).toContain("src/index.ts");
  });

  test("change files list versions newest first and the files of the newest", async () => {
    const id = changeIdToString(changeIdBytes("refs/heads/feature-auth", 0));
    const v = await loadChangeFiles(t, DEMO_REPO, id);
    expect(v.versions.map((x) => x.v)).toEqual([2, 1]);
    expect(v.files.map((f) => f.status)).toEqual(["M", "A", "D"]);
  });

  test("runs, job logs, settings and agent runs", async () => {
    const runs = await listRuns(t, DEMO_REPO);
    expect(runs.runs.map((r) => r.status)).toEqual([
      CiStatus.RUNNING,
      CiStatus.FAILURE,
      CiStatus.SUCCESS,
    ]);
    const log = await loadJobLog(t, DEMO_REPO, 2, 21);
    expect(log.text).toContain("FAILED");
    const s = await loadRepoSettings(t, DEMO_REPO, "HEAD");
    expect(s.policy?.approvals).toBe(1);
    expect(s.autolinks[0]?.prefix).toBe("ASC-");
    const agents = await loadAgentRuns(t, DEMO_REPO);
    expect(agents[0]?.title).toBe("Add retry policy");
    expect(await loadAgentRuns(t, "acme/web")).toEqual([]);
  });
});
