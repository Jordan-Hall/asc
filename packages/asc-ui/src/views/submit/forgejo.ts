// Forgejo calls of the submit dialog, over the allowlist-only client (P11): the assignable users
// (reviewer candidates, `GET …/assignees`) and the PR text edit (`PATCH …/pulls/{n}`, body `title` and
// `body` only). The client's guard denies anything else.
import type { ForgejoClient } from "@asc/asc-client/forgejo";
import type { PullPatch } from "./model";

const MAX_CANDIDATES = 1000;

function failure(what: string, status: number): Error {
  return new Error(`${what} failed (${status})`);
}

/** Logins that can be assigned in `owner/repo`; bounded and de-duplicated. */
export async function loadAssignees(
  client: ForgejoClient,
  owner: string,
  repo: string,
): Promise<string[]> {
  const { data, response } = await client.api.GET("/repos/{o}/{r}/assignees", {
    params: { path: { o: owner, r: repo } },
  });
  if (!response.ok || !Array.isArray(data)) throw failure("Loading reviewers", response.status);
  const seen = new Set<string>();
  for (const u of data) {
    const login = typeof u?.login === "string" ? u.login : "";
    if (login !== "" && seen.size < MAX_CANDIDATES) seen.add(login);
  }
  return [...seen];
}

/** Edit the title and description of PR `number`. */
export async function editPull(
  client: ForgejoClient,
  owner: string,
  repo: string,
  number: number,
  patch: PullPatch,
): Promise<void> {
  if (!Number.isSafeInteger(number) || number <= 0) throw new Error("Invalid PR number");
  const body: { title?: string; body?: string } = {};
  if (patch.title !== undefined) body.title = patch.title;
  if (patch.body !== undefined) body.body = patch.body;
  const { response } = await client.api.PATCH("/repos/{o}/{r}/pulls/{n}", {
    params: { path: { o: owner, r: repo, n: number } },
    body,
  });
  if (!response.ok) throw failure("Editing the PR", response.status);
}
