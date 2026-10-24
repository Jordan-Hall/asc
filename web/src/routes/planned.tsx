// Routes that are in the route table (so links and shortcuts are final) but not built yet.
import type { ReactNode } from "react";
import { Link, useParams, useRouteId } from "../lib/router";
import { repoHref } from "../lib/paths";
import { useTitle } from "../lib/title";

export const PLANNED: Readonly<Record<string, { title: string; since: string }>> = {
  inbox: { title: "Inbox", since: "v1.2" },
  search: { title: "Search", since: "v1.3" },
  repoSearch: { title: "Search", since: "v1.3" },
  blame: { title: "Blame", since: "v1.1" },
  history: { title: "History", since: "v1.1" },
  commit: { title: "Commit", since: "v1.1" },
  change: { title: "Change review", since: "v1.2" },
  actions: { title: "Actions", since: "v1.3" },
  run: { title: "Run", since: "v1.3" },
  repoSettings: { title: "Repository settings", since: "v1.3" },
};

export default function PlannedPage(): ReactNode {
  const id = useRouteId() ?? "";
  const { owner, repo } = useParams();
  const info = PLANNED[id] ?? { title: "This page", since: "a later release" };
  useTitle(info.title);
  return (
    <div className="mx-auto max-w-xl p-8 text-center">
      <h1 className="mb-2 text-2xl font-semibold text-fg-strong">{info.title}</h1>
      <p className="mb-4 text-fg-muted">Planned for {info.since}. See docs/WEB-UI.md.</p>
      {owner && repo ? (
        <Link to={repoHref(owner, repo)}>Back to code</Link>
      ) : (
        <Link to="/">Back to dashboard</Link>
      )}
    </div>
  );
}
