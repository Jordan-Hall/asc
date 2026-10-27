// Actions: the CI runs recorded for the repository, newest first.
import { CiStatus } from "@asc/asc-client/ci";
import type { ReactNode } from "react";
import { useApp, useQ } from "../api/queries";
import { Badge, Empty, Page, Panel, type Tone } from "../components/bits";
import { ErrorState } from "../components/error-state";
import { RowsSkeleton } from "../components/skeleton";
import { changeHref, runHref } from "../lib/paths";
import { Link, useParams } from "../lib/router";
import { ago } from "../lib/time";
import { useTitle } from "../lib/title";

export const STATUS_LABEL: Record<number, { text: string; tone: Tone }> = {
  [CiStatus.UNSPECIFIED]: { text: "Unknown", tone: "plain" },
  [CiStatus.QUEUED]: { text: "Queued", tone: "plain" },
  [CiStatus.RUNNING]: { text: "Running", tone: "warn" },
  [CiStatus.SUCCESS]: { text: "Passed", tone: "good" },
  [CiStatus.FAILURE]: { text: "Failed", tone: "bad" },
  [CiStatus.CANCELLED]: { text: "Cancelled", tone: "plain" },
  [CiStatus.SKIPPED]: { text: "Skipped", tone: "plain" },
};

export function StatusBadge(props: { status: CiStatus }): ReactNode {
  const s = STATUS_LABEL[props.status] ?? STATUS_LABEL[CiStatus.UNSPECIFIED];
  return <Badge tone={s?.tone ?? "plain"}>{s?.text ?? "Unknown"}</Badge>;
}

export default function ActionsPage(): ReactNode {
  const { owner = "", repo = "" } = useParams();
  const { queries } = useApp();
  useTitle(`Actions · ${owner}/${repo}`);
  const runs = useQ(queries.runs(`${owner}/${repo}`));
  if (runs.status === "error") return <ErrorState error={runs.error} />;
  const data = runs.data;
  return (
    <Page wide title="Actions" sub="Runs reported by the repository's CI, newest first.">
      <Panel label="Runs">
        {!data ? (
          <RowsSkeleton rows={6} rowHeight={56} label="Loading runs" />
        ) : data.runs.length === 0 ? (
          <Empty>
            No runs yet. A runner or forge adapter reports them with the Ci service; the service is
            described in docs/api.md.
          </Empty>
        ) : (
          <ul className="m-0 list-none p-0">
            {data.runs
              .toSorted((a, b) => b.created - a.created)
              .map((r) => (
                <li
                  key={r.id}
                  className="flex min-h-14 flex-wrap items-center gap-x-4 gap-y-1 border-b border-line px-4 py-2 last:border-b-0"
                >
                  <div className="min-w-0 flex-1 basis-60">
                    <Link
                      to={runHref(owner, repo, r.id)}
                      className="block truncate font-medium text-fg-strong no-underline hover:underline"
                    >
                      {r.name}
                    </Link>
                    <div className="text-xs text-fg-muted">
                      Run {r.id} · {r.jobs.length} {r.jobs.length === 1 ? "job" : "jobs"}
                      {r.created ? ` · ${ago(r.created)}` : ""}
                      {r.change ? (
                        <>
                          {" · "}
                          <Link to={changeHref(owner, repo, r.change)} className="underline">
                            change {r.change.slice(0, 8)} v{r.version}
                          </Link>
                        </>
                      ) : null}
                    </div>
                  </div>
                  <StatusBadge status={r.status} />
                </li>
              ))}
          </ul>
        )}
      </Panel>
    </Page>
  );
}
