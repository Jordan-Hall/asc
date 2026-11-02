// Route patterns and the links built from them. One place so a rename cannot leave a stale link.
import { buildPath } from "./match";

export const PATTERNS = {
  home: "/",
  signin: "/signin",
  inbox: "/inbox",
  settings: "/settings",
  search: "/search",
  newRepo: "/new",
  newChange: "/new/change",
  stacks: "/stacks",
  reviews: "/reviews",
  repos: "/repos",
  me: "/people/me",
  agents: "/agents",
  agentsNew: "/agents/new",
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

export function blameHref(owner: string, repo: string, ref: string, path: string): string {
  return buildPath(PATTERNS.blame, { owner, repo, ref, path });
}

export function historyHref(owner: string, repo: string, ref: string, path: string): string {
  return buildPath(PATTERNS.history, { owner, repo, ref, path });
}

export function commitHref(owner: string, repo: string, id: string): string {
  return buildPath(PATTERNS.commit, { owner, repo, id });
}

export function actionsHref(owner: string, repo: string): string {
  return buildPath(PATTERNS.actions, { owner, repo });
}

export function runHref(owner: string, repo: string, run: number): string {
  return buildPath(PATTERNS.run, { owner, repo, run: String(run) });
}

export function searchHref(owner: string, repo: string, q = ""): string {
  const base = buildPath(PATTERNS.repoSearch, { owner, repo });
  return q ? `${base}?q=${encodeURIComponent(q)}` : base;
}

export function settingsHref(owner: string, repo: string): string {
  return buildPath(PATTERNS.repoSettings, { owner, repo });
}
