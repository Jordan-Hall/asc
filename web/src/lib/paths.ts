// Route patterns and the links built from them. One place so a rename cannot leave a stale link.
import { buildPath } from "./match";

export const PATTERNS = {
  home: "/",
  signin: "/signin",
  inbox: "/inbox",
  settings: "/settings",
  search: "/search",
  repo: "/:owner/:repo",
  tree: "/:owner/:repo/tree/:ref/*path",
  blob: "/:owner/:repo/blob/:ref/*path",
  blame: "/:owner/:repo/blame/:ref/*path",
  history: "/:owner/:repo/commits/:ref/*path",
  commit: "/:owner/:repo/commit/:id",
  changes: "/:owner/:repo/changes",
  change: "/:owner/:repo/changes/:id",
  actions: "/:owner/:repo/actions",
  run: "/:owner/:repo/actions/runs/:run",
  repoSearch: "/:owner/:repo/search",
  repoSettings: "/:owner/:repo/settings",
} as const;

export function repoHref(owner: string, repo: string): string {
  return buildPath(PATTERNS.repo, { owner, repo });
}

export function treeHref(owner: string, repo: string, ref: string, path: string): string {
  return buildPath(PATTERNS.tree, { owner, repo, ref, path });
}

export function blobHref(owner: string, repo: string, ref: string, path: string): string {
  return buildPath(PATTERNS.blob, { owner, repo, ref, path });
}

export function changesHref(owner: string, repo: string): string {
  return buildPath(PATTERNS.changes, { owner, repo });
}

export function changeHref(owner: string, repo: string, id: string): string {
  return buildPath(PATTERNS.change, { owner, repo, id });
}
