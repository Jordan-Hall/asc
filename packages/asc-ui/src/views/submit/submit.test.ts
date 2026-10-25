import { describe, expect, test } from "bun:test";
import { createForgejoClient, ForgejoDeniedError } from "@asc/asc-client/forgejo";
import { editPull, loadAssignees } from "./forgejo";
import {
  addReviewer,
  defaultPullText,
  initSubmit,
  pullEdits,
  removeReviewer,
  reviewerBlockedReason,
  setAutoMerge,
  setMode,
  setStack,
  setText,
  submitBlockedReason,
  submitRun,
  textOf,
  validReviewer,
} from "./model";

const changes = [
  { id: "aaaa", message: "Add thing\n\nLong words.", pull: { number: 7, title: "Add thing", body: "Long words.", draft: true } },
  { id: "bbbb", message: "Second" },
];
const base = () => initSubmit(changes, ["ann", "Bob", "carol"]);

describe("submit model", () => {
  test("the PR text defaults to the change message; existing PRs show their own", () => {
    expect(defaultPullText("T\r\n\r\nbody\nmore")).toEqual({ title: "T", body: "body\nmore" });
    const s = base();
    expect(textOf(s, "aaaa")).toEqual({ title: "Add thing", body: "Long words." });
    expect(textOf(s, "bbbb")).toEqual({ title: "Second", body: "" });
  });

  test("argv carries the chosen options and nothing else", () => {
    let s = setMode(base(), "publish");
    s = addReviewer(addReviewer(s, "ann"), "bob");
    s = setAutoMerge(s, true);
    expect(submitRun(s)).toEqual({
      verb: "submit",
      args: ["-s", "--publish", "--reviewer=ann", "--reviewer=Bob", "--auto-merge"],
    });
    expect(submitRun(setStack(setMode(base(), "draft"), false), "aaaa").args).toEqual(["--draft", "aaaa"]);
  });

  test("a draft cannot auto-merge", () => {
    const s = setAutoMerge(setMode(base(), "draft"), true);
    expect(submitBlockedReason(s)).toBe("A draft cannot auto-merge");
    expect(() => submitRun(s)).toThrow();
  });

  test("reviewers come from the assignable list, once, and are valid handles", () => {
    const s = base();
    expect(reviewerBlockedReason(s, "mallory")).toBe("Not assignable in this repository");
    expect(addReviewer(s, "mallory")).toBe(s);
    expect(reviewerBlockedReason(initSubmit(changes), "ann")).toBe("Reviewers are not loaded");
    const a = addReviewer(s, "ANN");
    expect(a.reviewers).toEqual(["ann"]);
    expect(reviewerBlockedReason(a, "ann")).toBe("Already added");
    expect(removeReviewer(a, "ann").reviewers).toEqual([]);
  });

  test("option-like or odd reviewer names never reach argv", () => {
    for (const n of ["--force", "-x", ".a", "a b", "a,b", "a=b", "", "x".repeat(41)]) expect(validReviewer(n)).toBe(false);
    const evil = { ...base(), candidates: ["--force"], reviewers: ["--force"] };
    expect(submitBlockedReason(evil)).toBe("Not a valid user name");
    expect(() => submitRun(evil)).toThrow();
    expect(() => submitRun(base(), "--force")).toThrow("Invalid change id");
  });

  test("only edited fields of existing PRs are patched", () => {
    let s = base();
    expect(pullEdits(s)).toEqual([]);
    s = setText(s, "aaaa", { title: "New title", body: "Long words." });
    s = setText(s, "bbbb", { title: "Edited", body: "" });
    expect(pullEdits(s)).toEqual([{ change: "aaaa", number: 7, patch: { title: "New title" } }]);
    s = setText(s, "aaaa", { title: "Add thing", body: "changed" });
    expect(pullEdits(s)[0]?.patch).toEqual({ body: "changed" });
  });

  test("PR text is validated", () => {
    const t = (title: string, body = "x") => submitBlockedReason(setText(base(), "aaaa", { title, body }));
    expect(t("  ")).toBe("A PR title cannot be empty");
    expect(t("a\nb")).toBe("A PR title is one line");
    expect(t("a".repeat(256))).toBe("A PR title is too long");
    expect(t("ok", "b".repeat(65 * 1024))).toBe("A PR description is too long");
    expect(t("ok")).toBeUndefined();
  });
});

describe("forgejo adapter", () => {
  function harness(reply: () => Response) {
    const seen: Array<{ method: string; url: string; body: string }> = [];
    const client = createForgejoClient({
      baseUrl: "https://forge.example",
      fetch: async (req) => {
        seen.push({ method: req.method, url: req.url, body: await req.text() });
        return reply();
      },
    });
    return { client, seen };
  }

  test("assignees are read, de-duplicated and bounded to logins", async () => {
    const { client, seen } = harness(() => Response.json([{ login: "ann" }, { login: "ann" }, {}, { login: "bob" }]));
    expect(await loadAssignees(client, "o", "r")).toEqual(["ann", "bob"]);
    expect(seen[0]?.url).toBe("https://forge.example/api/v1/repos/o/r/assignees");
  });

  test("a PR edit sends title and body only", async () => {
    const { client, seen } = harness(() => Response.json({}, { status: 201 }));
    await editPull(client, "o", "r", 7, { title: "T" });
    expect(seen[0]?.method).toBe("PATCH");
    expect(seen[0]?.url).toBe("https://forge.example/api/v1/repos/o/r/pulls/7");
    expect(JSON.parse(seen[0]?.body ?? "")).toEqual({ title: "T" });
  });

  test("the guard denies fields outside title and body", async () => {
    const { client } = harness(() => Response.json({}));
    let denied: unknown;
    try {
      await client.api.PATCH("/repos/{o}/{r}/pulls/{n}", {
        params: { path: { o: "o", r: "r", n: 7 } },
        body: { state: "closed" },
      });
    } catch (e) {
      denied = e;
    }
    expect(denied).toBeInstanceOf(ForgejoDeniedError);
  });

  test("server failures and bad numbers surface as errors", async () => {
    const { client } = harness(() => new Response("no", { status: 403 }));
    await expect(editPull(client, "o", "r", 7, { title: "T" })).rejects.toThrow("403");
    await expect(editPull(client, "o", "r", 0, { title: "T" })).rejects.toThrow("Invalid PR number");
    await expect(loadAssignees(client, "o", "r")).rejects.toThrow("403");
  });
});
