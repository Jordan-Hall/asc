import { describe, expect, test } from "bun:test";
import { type HelloInfo, serverRepoFor } from "../src/bridge";

const hello = (roots: HelloInfo["roots"]) => ({
  hello: async () => ({ ascVersion: "1", features: [], roots, compat: { level: "full" } as const }),
});

describe("serverRepoFor", () => {
  test("names the repo of a root that has an asc server", async () => {
    const m = hello([{ root: "/w", wsId: "ws1", repo: "o/r", remoteKind: "asc", remoteUrl: "" }]);
    expect(await serverRepoFor(m, "/w")).toBe("o/r");
  });
  test("is null for a git remote, a local-only root, an unknown root or an empty repo", async () => {
    const m = hello([
      { root: "/git", wsId: "ws1", repo: "o/r", remoteKind: "git", remoteUrl: "" },
      { root: "/none", wsId: "ws2", repo: "", remoteKind: "none", remoteUrl: "" },
      { root: "/empty", wsId: "ws3", repo: "", remoteKind: "asc", remoteUrl: "" },
      { root: "/nows", wsId: "", repo: "o/r", remoteKind: "asc", remoteUrl: "" },
    ]);
    for (const root of ["/git", "/none", "/empty", "/nows", "/other"]) {
      expect(await serverRepoFor(m, root)).toBeNull();
    }
  });
});
