// One run: its jobs, and the tail of the log of the job picked with ?job=.
import type { ReactNode } from "react";
import { useApp, useQ } from "../api/queries";
import { Empty, Page, Panel } from "../components/bits";
import { ErrorState } from "../components/error-state";
import { RowsSkeleton } from "../components/skeleton";
import { actionsHref, runHref } from "../lib/paths";
import { Link, useParams, useSearchParams } from "../lib/router";
import { useTitle } from "../lib/title";
import { StatusBadge } from "./actions";

function Log(props: { repo: string; run: number; job: number }): ReactNode {
  const { queries } = useApp();
  const log = useQ(queries.jobLog(props.repo, props.run, props.job));
  if (log.status === "error") return <ErrorState error={log.error} />;
  if (!log.data) {
    return <div role="status" aria-busy="true" aria-label="Loading log" style={{ height: 160 }} />;
  }
  return (
    <>
      {log.data.truncated ? (
        <p role="status" className="border-b border-line bg-subtle p-3 text-sm">
          Showing the end of a log of {log.data.size.toLocaleString("en")} bytes.
        </p>
      ) : null}
      {log.data.text === "" ? (
        <Empty>This job has written no log yet.</Empty>
      ) : (
        <pre
          className="m-0 max-h-[32rem] overflow-auto bg-surface p-4 font-mono text-[13px]"
          tabIndex={0}
          aria-label="Job log"
        >
          {log.data.text}
        </pre>
      )}
    </>
  );
}

export default function RunPage(): ReactNode {
  const { owner = "", repo = "", run = "" } = useParams();
  const picked = Number(useSearchParams().get("job") ?? "");
  const { queries } = useApp();
  const id = Number(run);
  useTitle(`Run ${run} · ${owner}/${repo}`);
  const runs = useQ(queries.runs(`${owner}/${repo}`));
  if (runs.status === "error") return <ErrorState error={runs.error} />;
  const found = runs.data?.runs.find((r) => r.id === id);
  if (runs.data && !found) {
    return (
      <Page title={`Run ${run}`}>
        <Empty>
          This repository has no run {run}. <Link to={actionsHref(owner, repo)}>All runs</Link>
        </Empty>
      </Page>
    );
  }
  const job = found?.jobs.find((j) => j.id === picked) ?? found?.jobs[0];
  return (
    <Page
      wide
      title={found?.name ?? `Run ${run}`}
      sub={<Link to={actionsHref(owner, repo)}>All runs</Link>}
    >
      <Panel label="Jobs">
        {!found ? (
          <RowsSkeleton rows={3} rowHeight={48} label="Loading run" />
        ) : found.jobs.length === 0 ? (
          <Empty>This run has no jobs.</Empty>
        ) : (
          <ul className="m-0 list-none p-0">
            {found.jobs.map((j) => (
              <li
                key={j.id}
                className="flex min-h-12 items-center gap-3 border-b border-line px-4 last:border-b-0"
              >
                <Link
                  to={`${runHref(owner, repo, id)}?job=${j.id}`}
                  aria-current={job?.id === j.id ? "true" : undefined}
                  className={`min-w-0 flex-1 truncate no-underline hover:underline ${
                    job?.id === j.id ? "font-semibold text-fg-strong" : ""
                  }`}
                >
                  {j.name}
                </Link>
                <StatusBadge status={j.status} />
              </li>
            ))}
          </ul>
        )}
      </Panel>
      {job ? (
        <section className="mt-4" aria-label={`Log of ${job.name}`}>
          <h2 className="mb-2 text-lg font-semibold text-fg-strong">{job.name}</h2>
          <div className="overflow-hidden rounded-lg border border-line">
            <Log repo={`${owner}/${repo}`} run={id} job={job.id} />
          </div>
        </section>
      ) : null}
    </Page>
  );
}
