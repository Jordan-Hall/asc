// Change list: stacks of changes, each row a change with its review, checks and restack state.
import type { ReactNode } from "react";
import type { ChangeListView, ChangeRow, StackView } from "../api/types";
import { useApp, useQ } from "../api/queries";
import { Badge } from "../components/bits";
import { ErrorState } from "../components/error-state";
import { RowsSkeleton } from "../components/skeleton";
import { VirtualList } from "../components/virtual-list";
import { Link, useParams } from "../lib/router";
import { changeHref } from "../lib/paths";
import { useTitle } from "../lib/title";

const ROW_HEIGHT = 56;
const MAX_ROWS = 14;

export type Item = { kind: "stack"; stack: StackView } | { kind: "change"; row: ChangeRow };

/** Stack headers and their changes as one flat list, bottom of each stack first. */
export function flatten(view: ChangeListView): Item[] {
  return view.stacks.flatMap((stack) => [
    { kind: "stack" as const, stack },
    ...stack.changes.map((row) => ({ kind: "change" as const, row })),
  ]);
}

function ChecksBadge(props: { checks: string }): ReactNode {
  if (props.checks === "success") return <Badge tone="good">Checks passed</Badge>;
  if (props.checks === "failure") return <Badge tone="bad">Checks failed</Badge>;
  if (props.checks === "pending") return <Badge tone="warn">Checks running</Badge>;
  return null;
}

export function ChangeItem(props: {
  owner: string;
  repo: string;
  row: ChangeRow;
  focusable: boolean;
}): ReactNode {
  const { row } = props;
  return (
    <div
      className="flex h-full items-center gap-3 px-4"
      style={{ paddingLeft: 16 + Math.min(row.position, 6) * 12 }}
    >
      <div className="min-w-0 flex-1">
        <Link
          to={changeHref(props.owner, props.repo, row.id)}
          tabIndex={props.focusable ? 0 : -1}
          className="block truncate font-medium text-fg-strong no-underline hover:underline"
        >
          {row.title}
        </Link>
        <div className="text-xs text-fg-muted">
          <code className="font-mono">{row.short}</code> · v{row.version}
          {row.review ? ` · #${row.review.number}` : ""}
        </div>
      </div>
      <div className="flex flex-wrap items-center justify-end gap-2">
        {row.landed ? <Badge tone="note">Landed</Badge> : null}
        {row.divergent ? <Badge tone="warn">Divergent</Badge> : null}
        {row.needsRestack ? <Badge tone="warn">Needs restack</Badge> : null}
        {row.review?.draft ? <Badge tone="plain">Draft</Badge> : null}
        {row.review && row.review.approvals > 0 ? (
          <Badge tone="good">{`${row.review.approvals} approved`}</Badge>
        ) : null}
        {row.review ? <ChecksBadge checks={row.review.checks} /> : null}
      </div>
    </div>
  );
}

export function ChangeList(props: {
  owner: string;
  repo: string;
  view: ChangeListView;
}): ReactNode {
  const items = flatten(props.view);
  if (items.length === 0) {
    return (
      <p className="p-8 text-center text-fg-muted">
        No open changes. Push a stack with <code>asc submit</code>.
      </p>
    );
  }
  return (
    <VirtualList
      count={items.length}
      rowHeight={ROW_HEIGHT}
      maxRows={MAX_ROWS}
      minRows={Math.min(items.length, 6)}
      label="Changes"
      renderRow={(i, focusable) => {
        const item = items[i];
        if (!item) return null;
        if (item.kind === "stack") {
          return (
            <div className="flex h-full items-center gap-2 bg-subtle px-4 text-sm font-semibold text-fg-strong">
              <span>{item.stack.ref.replace(/^refs\/heads\//, "")}</span>
              <span className="font-normal text-fg-muted">
                {`${item.stack.changes.length} ${item.stack.changes.length === 1 ? "change" : "changes"} onto ${item.stack.trunk}`}
              </span>
            </div>
          );
        }
        return (
          <ChangeItem owner={props.owner} repo={props.repo} row={item.row} focusable={focusable} />
        );
      }}
    />
  );
}

export default function ChangesPage(): ReactNode {
  const { owner = "", repo = "" } = useParams();
  const { queries } = useApp();
  useTitle(`Changes · ${owner}/${repo}`);
  const changes = useQ(queries.changes(`${owner}/${repo}`));
  if (changes.status === "error") return <ErrorState error={changes.error} />;
  return (
    <div className="mx-auto w-full max-w-6xl p-4">
      <h1 className="mb-4 text-xl font-semibold text-fg-strong" style={{ lineHeight: "32px" }}>
        Changes
      </h1>
      <div className="overflow-hidden rounded-lg border border-line bg-surface">
        {changes.data ? (
          <ChangeList owner={owner} repo={repo} view={changes.data} />
        ) : (
          <RowsSkeleton rows={6} rowHeight={ROW_HEIGHT} label="Loading changes" />
        )}
      </div>
    </div>
  );
}
