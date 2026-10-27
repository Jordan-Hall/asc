// Blame: each run of lines is shown with the commit that last changed it.
import type { ReactNode } from "react";
import { useApp, useQ } from "../api/queries";
import { Empty, Page, Panel } from "../components/bits";
import { ErrorState } from "../components/error-state";
import { RowsSkeleton } from "../components/skeleton";
import { blobHref, commitHref, historyHref } from "../lib/paths";
import { Link, useParams } from "../lib/router";
import { agoShort } from "../lib/time";
import { useTitle } from "../lib/title";

export default function BlamePage(): ReactNode {
  const { owner = "", repo = "", ref = "", path = "" } = useParams();
  const { queries } = useApp();
  useTitle(`Blame ${path} · ${owner}/${repo}`);
  const blame = useQ(queries.blame(`${owner}/${repo}`, ref, path));
  if (blame.status === "error") return <ErrorState error={blame.error} />;
  const groups = blame.data;
  return (
    <Page
      wide
      title="Blame"
      sub={
        <>
          <code>{path}</code> at <code>{ref}</code> ·{" "}
          <Link to={blobHref(owner, repo, ref, path)}>File</Link> ·{" "}
          <Link to={historyHref(owner, repo, ref, path)}>History</Link>
        </>
      }
    >
      <Panel label="Blame">
        {!groups ? (
          <RowsSkeleton rows={8} rowHeight={44} label="Loading blame" />
        ) : groups.length === 0 ? (
          <Empty>This file has no lines to blame.</Empty>
        ) : (
          <ul className="m-0 list-none p-0">
            {groups.map((g) => (
              <li
                key={g.start}
                className="flex min-h-11 flex-wrap items-center gap-x-4 gap-y-1 border-b border-line px-4 py-2 last:border-b-0"
              >
                <code className="w-28 font-mono text-xs text-fg-muted">
                  {g.len === 1 ? `L${g.start}` : `L${g.start}-${g.start + g.len - 1}`}
                </code>
                <Link
                  to={commitHref(owner, repo, g.id)}
                  className="min-w-0 flex-1 basis-48 truncate text-fg-strong no-underline hover:underline"
                >
                  {g.subject || g.short}
                </Link>
                <span className="text-xs text-fg-muted">
                  {g.author}
                  {g.time ? ` · ${agoShort(g.time)}` : ""}
                </span>
                <Link to={`${blobHref(owner, repo, ref, path)}#L${g.start}`} className="text-xs">
                  Lines
                </Link>
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </Page>
  );
}
