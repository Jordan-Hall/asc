import { expect, test } from "bun:test";
import { createForgejoClient } from "@asc/asc-client/forgejo";
import { createMockForgejo, seedState } from "../src/mock-forgejo";

const client = () => {
  const app = createMockForgejo(seedState());
  return createForgejoClient({
    baseUrl: "https://forge.test",
    fetch: (r) => Promise.resolve(app.fetch(r)),
    repos: ["asc/demo"],
  });
};

test("user and inbox", async () => {
  const c = client();
  expect((await c.api.GET("/user")).data?.login).toBe("demo");
  const inbox = await c.api.GET("/user/asc/inbox", { params: { query: { section: "ready" } } });
  expect(inbox.data?.map((e) => e.id)).toEqual([2]);
});

test("pin through PUT updates the entry", async () => {
  const c = client();
  const r = await c.api.PUT("/user/asc/inbox/{id}", {
    params: { path: { id: 1 } },
    body: { pinned: true },
  });
  expect(r.data?.pinned).toBe(true);
});

test("actions jobs", async () => {
  const c = client();
  const jobs = await c.api.GET("/repos/{o}/{r}/actions/runs/{id}/jobs", {
    params: { path: { o: "asc", r: "demo", id: 10 } },
  });
  expect(jobs.data).toBeDefined();
});

test("unknown repo is 404", async () => {
  const app = createMockForgejo();
  expect((await app.request("/api/v1/repos/x/y/actions/runs")).status).toBe(404);
});
