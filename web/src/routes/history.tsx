// History: the commits that reach a revision, newest first, optionally limited to one path.
import { type ReactNode, useState } from "react";
import { useApp, useQ } from "../api/queries";
import { Empty, Page, Panel } from "../components/bits";
import { ErrorState } from "../components/error-state";
import { RowsSkeleton } from "../components/skeleton";
import { commitHref, treeHref } from "../lib/paths";
import { Link, useParams } from "../lib/router";
import { ago } from "../lib/time";
import { useTitle } from "../lib/title";

const PAGE = 50;

export default function HistoryPage(): ReactNode {
  const { owner = "", repo = "", ref = "", path = "" } = useParams();
  const { queries } = useApp();
  const [limit, setLimit] = useState(PAGE);
  const where = path ? `${path} on ${ref}` : ref;
  useTitle(`History of ${where} · ${owner}/${repo}`);
  const history = useQ(queries.history(`${owner}/${repo}`, ref, path, limit));
  if (history.status === "error") return <ErrorState error={history.error} />;
  const data = history.data;
  return (
    <Page
      wide
      title="History"
      sub={
        <>
          Commits that reach <code>{ref}</code>
          {path ? (
            <>
              {" "}
              and touch <code>{path}</code>
            </>
          ) : null}
          . <Link to={treeHref(owner, repo, ref, path)}>Back to the files</Link>
        </>
      }
    >
      <Panel label="Commits">
        {!data ? (
          <RowsSkeleton rows={8} rowHeight={56} label="Loading history" />
        ) : data.rows.length === 0 ? (
          <Empty>No commits reach this revision.</Empty>
        ) : (
          <ul className="m-0 list-none p-0">
            {data.rows.map((c) => (
              <li
                key={c.id}
                className="flex min-h-14 flex-wrap items-center gap-x-4 gap-y-1 border-b border-line px-4 py-2 last:border-b-0"
              >
                <div className="min-w-0 flex-1 basis-60">
                  <Link
                    to={commitHref(owner, repo, c.id)}
                    className="block truncate font-medium text-fg-strong no-underline hover:underline"
                  >
                    {c.subject || "(no message)"}
                  </Link>
                  <div className="text-xs text-fg-muted">
                    {c.author} · {c.time ? ago(c.time) : "unknown time"}
                  </div>
                </div>
                <code className="font-mono text-xs text-fg-muted">{c.short}</code>
              </li>
            ))}
          </ul>
        )}
      </Panel>
      {data?.more ? (
        <div className="mt-4 text-center">
          <button type="button" className="btn-secondary" onClick={() => setLimit((n) => n + PAGE)}>
            Show older commits
          </button>
        </div>
      ) : null}
    </Page>
  );
}
