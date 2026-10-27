// Inbox: everything that waits for the viewer across the repositories this browser has opened,
// from the same reads as the dashboard (there is no cross-repository inbox RPC).
import type { ReactNode } from "react";
import { useHome } from "../api/home";
import { type NeedKind, type NeedRow } from "../api/needs";
import { useApp } from "../api/queries";
import { Badge, Empty, Page, Panel } from "../components/bits";
import { RowsSkeleton } from "../components/skeleton";
import { useToken } from "../lib/auth";
import { Link, useRouteId } from "../lib/router";
import { agoShort } from "../lib/time";
import { useTitle } from "../lib/title";

const KINDS: readonly { kind: NeedKind; title: string }[] = [
  { kind: "review", title: "Review requested" },
  { kind: "reply", title: "Replies" },
  { kind: "mention", title: "Mentions" },
  { kind: "conflict", title: "Conflicts" },
  { kind: "checks", title: "Checks failing" },
  { kind: "merge", title: "Ready to merge" },
];

function Row(props: { row: NeedRow }): ReactNode {
  const { row } = props;
  return (
    <li className="border-b border-line last:border-b-0">
      <Link
        to={row.href}
        className="flex min-h-14 flex-wrap items-center gap-x-4 gap-y-1 px-4 py-2 no-underline"
      >
        <div className="min-w-0 flex-1 basis-60">
          <div className="font-medium text-fg-strong">{row.headline}</div>
          <div className="text-xs text-fg-muted">
            {row.repo}
            {row.detail ? ` · ${row.detail}` : ""}
          </div>
          {row.quote ? <div className="mt-1 text-sm text-fg-muted">{row.quote}</div> : null}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {row.agent ? <Badge tone="note">{`Agent: ${row.agent}`}</Badge> : null}
          {row.chip ? <Badge tone="warn">{row.chip}</Badge> : null}
          {row.stat ? <Badge tone="plain">{row.stat}</Badge> : null}
          {row.at ? (
            <span className="text-xs text-fg-muted">
              {`${row.atVerb} ${agoShort(row.at)}`.trim()}
            </span>
          ) : null}
        </div>
      </Link>
    </li>
  );
}

export default function InboxPage(): ReactNode {
  const reviews = useRouteId() === "reviews";
  const heading = reviews ? "Review requests" : "Inbox";
  useTitle(heading);
  const { auth } = useApp();
  const signedIn = useToken(auth) !== undefined;
  const home = useHome(signedIn);
  if (!signedIn) {
    return (
      <Page title={heading}>
        <Empty>
          <Link to="/signin">Sign in</Link> to see the changes that wait for you.
        </Empty>
      </Page>
    );
  }
  return (
    <Page
      title={heading}
      sub={
        home.repos.length === 0
          ? "Review requests, replies and merges from the repositories you open appear here."
          : `From ${home.repos.join(", ")}.`
      }
    >
      {home.loading && home.needs.length === 0 ? (
        <Panel>
          <RowsSkeleton rows={4} rowHeight={56} label="Loading inbox" />
        </Panel>
      ) : home.needs.length === 0 ? (
        <Panel>
          <Empty>
            {home.repos.length === 0
              ? "No repositories opened yet. Open one from the dashboard."
              : "Nothing waits for you."}
          </Empty>
        </Panel>
      ) : (
        KINDS.filter((k) => !reviews || k.kind === "review").map(({ kind, title }) => {
          const rows = home.needs.filter((n) => n.kind === kind);
          if (rows.length === 0) return null;
          return (
            <section key={kind} className="mb-6" aria-labelledby={`inbox-${kind}`}>
              <h2 id={`inbox-${kind}`} className="mb-2 text-lg font-semibold text-fg-strong">
                {title} <span className="text-sm font-normal text-fg-muted">{rows.length}</span>
              </h2>
              <Panel>
                <ul className="m-0 list-none p-0">
                  {rows.map((r) => (
                    <Row key={r.id} row={r} />
                  ))}
                </ul>
              </Panel>
            </section>
          );
        })
      )}
      {home.failed ? (
        <p role="status" className="text-sm text-fg-muted">
          Some repositories could not be read, so this list may be incomplete.
        </p>
      ) : null}
    </Page>
  );
}
