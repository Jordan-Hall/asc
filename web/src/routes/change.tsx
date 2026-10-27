// Change review: the description and review state, the versions, the files of the newest version
// and the threads. Reads only; comments and approvals are made with `asc review` or on the forge.
import type { ReactNode } from "react";
import type { ThreadView } from "../api/review";
import { useApp, useQ } from "../api/queries";
import { Badge, Empty, Page, Panel } from "../components/bits";
import { ErrorState } from "../components/error-state";
import { RowsSkeleton } from "../components/skeleton";
import { changesHref } from "../lib/paths";
import { Link, useParams } from "../lib/router";
import { ago } from "../lib/time";
import { useTitle } from "../lib/title";

const STATUS: Record<string, string> = {
  A: "Added",
  M: "Modified",
  D: "Deleted",
  R: "Renamed",
  C: "Copied",
  T: "Type changed",
};

function Thread(props: { thread: ThreadView }): ReactNode {
  const { thread } = props;
  return (
    <li className="border-b border-line px-4 py-3 last:border-b-0">
      <div className="mb-1 flex flex-wrap items-center gap-2 text-xs text-fg-muted">
        <Badge tone={thread.resolved ? "good" : "warn"}>
          {thread.resolved ? "Resolved" : "Open"}
        </Badge>
        {thread.path ? (
          <code className="font-mono">
            {thread.path}
            {thread.startLine ? `:${thread.startLine}` : ""} (v{thread.version})
          </code>
        ) : (
          <span>On the change</span>
        )}
      </div>
      {thread.comments.map((c) => (
        <div key={c.id} className="mt-2">
          <div className="text-xs text-fg-muted">
            <strong className="text-fg-strong">{c.author}</strong> · {ago(c.created)}
          </div>
          <p className="m-0 whitespace-pre-wrap text-sm">{c.body}</p>
        </div>
      ))}
    </li>
  );
}

export default function ChangePage(): ReactNode {
  const { owner = "", repo = "", id = "" } = useParams();
  const name = `${owner}/${repo}`;
  const { queries } = useApp();
  const list = useQ(queries.changes(name));
  const files = useQ(queries.changeFiles(name, id));
  const threads = useQ(queries.threads(name, id));
  const row = list.data?.stacks.flatMap((s) => s.changes).find((c) => c.id === id);
  useTitle(`${row?.title ?? `Change ${id.slice(0, 8)}`} · ${owner}/${repo}`);
  if (list.status === "error") return <ErrorState error={list.error} />;
  if (list.data && !row) {
    return (
      <Page title="Change not found">
        <Empty>
          No open change {id.slice(0, 12)} in this repository.{" "}
          <Link to={changesHref(owner, repo)}>All changes</Link>
        </Empty>
      </Page>
    );
  }
  return (
    <Page
      wide
      title={row?.title ?? `Change ${id.slice(0, 8)}`}
      sub={
        row ? (
          <>
            <code>{row.short}</code> · v{row.version} · {row.author || "unknown author"}
            {row.time ? ` · ${ago(row.time)}` : ""} · change {row.position + 1} of {row.stackSize}{" "}
            on {row.ref.replace(/^refs\/heads\//, "")} ·{" "}
            <Link to={changesHref(owner, repo)}>All changes</Link>
          </>
        ) : undefined
      }
    >
      {row ? (
        <div className="mb-4 flex flex-wrap items-center gap-2">
          {row.landed ? <Badge tone="note">Landed</Badge> : null}
          {row.divergent ? <Badge tone="warn">Divergent</Badge> : null}
          {row.needsRestack ? <Badge tone="warn">Needs restack</Badge> : null}
          {row.review?.draft ? <Badge tone="plain">Draft</Badge> : null}
          {row.review ? (
            <Badge tone="plain">{`${row.review.state} #${row.review.number}`}</Badge>
          ) : null}
          {row.review && row.review.approvals > 0 ? (
            <Badge tone="good">{`${row.review.approvals} approved`}</Badge>
          ) : null}
          {row.review?.checks === "success" ? <Badge tone="good">Checks passed</Badge> : null}
          {row.review?.checks === "failure" ? <Badge tone="bad">Checks failed</Badge> : null}
          {row.review?.checks === "pending" ? <Badge tone="warn">Checks running</Badge> : null}
          {row.description.riskArea ? (
            <Badge tone="warn">{`Touches ${row.description.riskArea}`}</Badge>
          ) : null}
          {row.description.agent ? (
            <Badge tone="note">{`Agent: ${row.description.agent}`}</Badge>
          ) : null}
          {row.stat ? (
            <Badge tone="plain">{`+${row.stat.added} −${row.stat.removed} in ${row.stat.files} files`}</Badge>
          ) : null}
        </div>
      ) : null}
      {row?.description.intent ? (
        <p className="mb-2 whitespace-pre-wrap text-sm">{row.description.intent}</p>
      ) : null}
      {row?.description.risk ? (
        <p className="mb-4 text-sm">
          <strong>Look at:</strong> {row.description.risk}
        </p>
      ) : null}

      <h2 className="mb-2 mt-4 text-lg font-semibold text-fg-strong">Files</h2>
      <Panel label="Files">
        {files.status === "error" ? (
          <ErrorState error={files.error} />
        ) : !files.data ? (
          <RowsSkeleton rows={3} rowHeight={44} label="Loading files" />
        ) : files.data.files.length === 0 ? (
          <Empty>The newest version changes no files.</Empty>
        ) : (
          <ul className="m-0 list-none p-0">
            {files.data.files.map((f) => (
              <li
                key={f.path}
                className="flex min-h-11 flex-wrap items-center gap-x-3 border-b border-line px-4 py-1 last:border-b-0"
              >
                <Badge tone={f.status === "D" ? "bad" : f.status === "A" ? "good" : "plain"}>
                  {STATUS[f.status.slice(0, 1)] ?? f.status}
                </Badge>
                <span className="min-w-0 break-all font-mono text-sm">{f.path}</span>
                {f.oldPath ? <span className="text-xs text-fg-muted">from {f.oldPath}</span> : null}
              </li>
            ))}
          </ul>
        )}
      </Panel>

      <h2 className="mb-2 mt-6 text-lg font-semibold text-fg-strong">Versions</h2>
      <Panel label="Versions">
        {files.data && files.data.versions.length > 0 ? (
          <ul className="m-0 list-none p-0">
            {files.data.versions.map((v) => (
              <li
                key={v.v}
                className="flex min-h-11 flex-wrap items-center gap-x-4 border-b border-line px-4 py-1 last:border-b-0"
              >
                <strong className="text-fg-strong">v{v.v}</strong>
                <span className="text-sm text-fg-muted">
                  {v.actor || "unknown"} · {v.at ? ago(v.at) : "unknown time"}
                </span>
                <code className="ml-auto font-mono text-xs text-fg-muted">{v.cs.slice(0, 10)}</code>
              </li>
            ))}
          </ul>
        ) : files.data ? (
          <Empty>No versions are recorded.</Empty>
        ) : (
          <RowsSkeleton rows={2} rowHeight={44} label="Loading versions" />
        )}
      </Panel>

      <h2 className="mb-2 mt-6 text-lg font-semibold text-fg-strong">
        Threads
        {threads.data ? (
          <span className="ml-2 text-sm font-normal text-fg-muted">
            {threads.data.openThreads} open
          </span>
        ) : null}
      </h2>
      <Panel label="Threads">
        {threads.status === "error" ? (
          <ErrorState error={threads.error} />
        ) : !threads.data ? (
          <RowsSkeleton rows={2} rowHeight={56} label="Loading threads" />
        ) : threads.data.threads.length === 0 ? (
          <Empty>No review threads.</Empty>
        ) : (
          <ul className="m-0 list-none p-0">
            {threads.data.threads.map((t) => (
              <Thread key={t.id} thread={t} />
            ))}
          </ul>
        )}
      </Panel>
    </Page>
  );
}
