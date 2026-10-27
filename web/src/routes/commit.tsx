// One commit: its message, the files it changed and the patch in git format.
import type { ReactNode } from "react";
import { useApp, useQ } from "../api/queries";
import { Badge, Empty, Page, Panel } from "../components/bits";
import { ErrorState } from "../components/error-state";
import { Bar } from "../components/skeleton";
import { blobHref, commitHref, historyHref } from "../lib/paths";
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

function lineTone(l: string): string {
  if (l.startsWith("+") && !l.startsWith("+++")) return "bg-good-bg";
  if (l.startsWith("-") && !l.startsWith("---")) return "bg-danger-bg";
  if (l.startsWith("@@") || l.startsWith("diff ")) return "bg-subtle text-fg-muted";
  return "";
}

/** A patch in git format, one row per line. */
export function Patch(props: { text: string }): ReactNode {
  const lines = props.text.replace(/\n$/, "").split("\n");
  return (
    <div
      className="overflow-x-auto bg-surface font-mono text-[13px]"
      role="region"
      tabIndex={0}
      aria-label="Patch"
    >
      {lines.map((l, i) => (
        // The lines of a patch have no identity other than their position.
        // oxlint-disable-next-line react/no-array-index-key
        <div key={i} className={`min-h-5 whitespace-pre px-4 ${lineTone(l)}`}>
          {l || " "}
        </div>
      ))}
    </div>
  );
}

export default function CommitPage(): ReactNode {
  const { owner = "", repo = "", id = "" } = useParams();
  const { queries } = useApp();
  useTitle(`Commit ${id.slice(0, 10)} · ${owner}/${repo}`);
  const commit = useQ(queries.commit(`${owner}/${repo}`, id));
  if (commit.status === "error") return <ErrorState error={commit.error} />;
  const c = commit.data;
  if (commit.status !== "pending" && !c) {
    return (
      <Page title="Commit not found">
        <Empty>No commit matches {id}.</Empty>
      </Page>
    );
  }
  return (
    <Page
      wide
      title={c ? c.subject || "(no message)" : "Commit"}
      sub={
        c ? (
          <>
            {c.author} · {c.time ? ago(c.time) : "unknown time"} · <code>{c.short}</code>
            {c.parents.map((p) => (
              <span key={p}>
                {" "}
                · parent <Link to={commitHref(owner, repo, p)}>{p.slice(0, 10)}</Link>
              </span>
            ))}
          </>
        ) : (
          <Bar width="20rem" />
        )
      }
    >
      {c?.body ? <p className="mb-4 whitespace-pre-wrap text-sm">{c.body}</p> : null}
      <h2 className="mb-2 text-lg font-semibold text-fg-strong">
        {c ? `${c.files.length} ${c.files.length === 1 ? "file" : "files"} changed` : "Files"}
      </h2>
      {c ? (
        <>
          <Panel label="Changed files">
            {c.files.length === 0 ? (
              <Empty>This commit changes no files.</Empty>
            ) : (
              <ul className="m-0 list-none p-0">
                {c.files.map((f) => (
                  <li
                    key={f.path}
                    className="flex min-h-11 flex-wrap items-center gap-x-3 gap-y-1 border-b border-line px-4 py-1 last:border-b-0"
                  >
                    <Badge tone={f.status === "D" ? "bad" : f.status === "A" ? "good" : "plain"}>
                      {STATUS[f.status.slice(0, 1)] ?? f.status}
                    </Badge>
                    {f.status === "D" ? (
                      <span className="min-w-0 break-all font-mono text-sm">{f.path}</span>
                    ) : (
                      <Link
                        to={blobHref(owner, repo, c.id, f.path)}
                        className="min-w-0 break-all font-mono text-sm"
                      >
                        {f.path}
                      </Link>
                    )}
                    {f.oldPath ? (
                      <span className="text-xs text-fg-muted">from {f.oldPath}</span>
                    ) : null}
                    <Link
                      to={historyHref(owner, repo, c.id, f.path)}
                      className="ml-auto whitespace-nowrap text-xs"
                    >
                      History
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </Panel>
          {c.patch ? (
            <div className="mt-4 overflow-hidden rounded-lg border border-line">
              <Patch text={c.patch} />
              {c.truncated ? (
                <p role="status" className="border-t border-line bg-subtle p-3 text-sm">
                  The patch is cut off. Run <code>asc show {c.short}</code> for all of it.
                </p>
              ) : null}
            </div>
          ) : null}
        </>
      ) : (
        <div role="status" aria-busy="true" aria-label="Loading commit" style={{ height: 160 }} />
      )}
    </Page>
  );
}
