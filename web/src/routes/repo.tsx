// Repo home and directory view: branch picker, last commit, virtualised file table, README.
import { lazy, type ReactNode, Suspense } from "react";
import type { CommitView, RefsView, TreeRow, TreeView } from "../api/types";
import { useApp, useQ } from "../api/queries";
import { ErrorState } from "../components/error-state";
import { Icon } from "../components/icons";
import { Bar, RowsSkeleton } from "../components/skeleton";
import { VirtualList } from "../components/virtual-list";
import { Link, useNavigate, useParams } from "../lib/router";
import { blobHref, treeHref } from "../lib/paths";
import { useTitle } from "../lib/title";

const Markdown = lazy(() => import("../components/markdown"));

const ROW_HEIGHT = 36;
const MAX_ROWS = 18;

const stripDotSlash = (p: string) =>
  p.startsWith("./") ? p.slice(2) : p.startsWith("/") ? p.slice(1) : p;

function parentOf(path: string): string {
  const i = path.lastIndexOf("/");
  return i < 0 ? "" : path.slice(0, i);
}

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export function ago(seconds: number, now = Date.now()): string {
  const d = Math.max(0, Math.round(now / 1000 - seconds));
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
  if (d < 86400 * 60) return `${Math.floor(d / 86400)} d ago`;
  return new Date(seconds * 1000).toISOString().slice(0, 10);
}

export interface FileTableProps {
  owner: string;
  repo: string;
  rev: string;
  path: string;
  rows: readonly TreeRow[];
}

/** The virtualised directory listing. */
export function FileTable(props: FileTableProps): ReactNode {
  const { owner, repo, rev, path, rows } = props;
  const up = path !== "";
  const count = rows.length + (up ? 1 : 0);
  return (
    <VirtualList
      count={count}
      rowHeight={ROW_HEIGHT}
      maxRows={MAX_ROWS}
      minRows={Math.min(count, 8)}
      label="Files"
      renderRow={(i, focusable) => {
        const tab = focusable ? 0 : -1;
        if (up && i === 0) {
          return (
            <Link
              to={treeHref(owner, repo, rev, parentOf(path))}
              tabIndex={tab}
              className="flex h-full items-center gap-3 px-4 no-underline"
            >
              <Icon name="folder" />
              <span>..</span>
            </Link>
          );
        }
        const row = rows[i - (up ? 1 : 0)];
        if (!row) return null;
        const dir = row.kind === "dir";
        return (
          <Link
            to={dir ? treeHref(owner, repo, rev, row.path) : blobHref(owner, repo, rev, row.path)}
            tabIndex={tab}
            className="flex h-full items-center gap-3 px-4 text-fg no-underline hover:bg-subtle"
          >
            <span className={dir ? "text-accent" : "text-fg-muted"}>
              <Icon name={dir ? "folder" : "file"} />
            </span>
            <span className="min-w-0 flex-1 truncate">{row.name}</span>
            {dir ? null : <span className="text-sm text-fg-muted">{formatSize(row.size)}</span>}
          </Link>
        );
      }}
    />
  );
}

function CommitBar(props: { commit: CommitView | undefined; loading: boolean }): ReactNode {
  return (
    <div
      className="flex items-center gap-3 border-b border-line bg-subtle px-4 text-sm"
      style={{ height: 44 }}
    >
      {props.commit ? (
        <>
          <span className="font-medium text-fg-strong">{props.commit.author}</span>
          <span className="min-w-0 flex-1 truncate text-fg-muted">{props.commit.subject}</span>
          <code className="font-mono text-xs text-fg-muted">{props.commit.short}</code>
          <span className="whitespace-nowrap text-fg-muted">{ago(props.commit.time)}</span>
        </>
      ) : props.loading ? (
        <Bar width="50%" />
      ) : null}
    </div>
  );
}

export function BranchPicker(props: {
  refs: RefsView;
  current: string;
  onPick: (ref: string) => void;
}): ReactNode {
  const heads = props.refs.refs.filter((r) => r.kind === "branch");
  const tags = props.refs.refs.filter((r) => r.kind === "tag");
  return (
    <label className="inline-flex items-center gap-2 text-sm">
      <Icon name="branch" />
      <span className="sr-only">Branch or tag</span>
      <select
        value={props.current}
        onChange={(e) => props.onPick(e.target.value)}
        className="h-9 rounded-md border border-control bg-surface px-2"
      >
        <optgroup label="Branches">
          {heads.map((r) => (
            <option key={r.name} value={r.short}>
              {r.short}
            </option>
          ))}
        </optgroup>
        {tags.length > 0 ? (
          <optgroup label="Tags">
            {tags.map((r) => (
              <option key={r.name} value={r.short}>
                {r.short}
              </option>
            ))}
          </optgroup>
        ) : null}
      </select>
    </label>
  );
}

function findReadme(tree: TreeView | undefined): TreeRow | undefined {
  return tree?.rows.find((r) => r.kind !== "dir" && /^readme(\.(md|markdown|txt))?$/i.test(r.name));
}

function Readme(props: { owner: string; repo: string; rev: string; row: TreeRow }): ReactNode {
  const { queries } = useApp();
  const file = useQ(queries.file(`${props.owner}/${props.repo}`, props.rev, props.row.path));
  const data = file.data;
  return (
    <section aria-label="README" className="mt-6 rounded-lg border border-line bg-surface">
      <h2
        className="border-b border-line px-4 text-sm font-semibold"
        style={{ lineHeight: "40px" }}
      >
        {props.row.name}
      </h2>
      <div className="p-6" style={{ minHeight: 160 }}>
        {data && !data.binary ? (
          <Suspense fallback={<Bar width="60%" />}>
            <Markdown
              source={data.lines.slice(0, data.count).join("\n")}
              link={(target, children, key) =>
                target.kind === "relative" ? (
                  <Link
                    key={key}
                    to={blobHref(props.owner, props.repo, props.rev, stripDotSlash(target.href))}
                  >
                    {children}
                  </Link>
                ) : (
                  <a
                    key={key}
                    href={target.href}
                    rel="noreferrer noopener"
                    target={target.kind === "external" ? "_blank" : undefined}
                  >
                    {children}
                  </a>
                )
              }
            />
          </Suspense>
        ) : (
          <Bar width="60%" />
        )}
      </div>
    </section>
  );
}

function TreePane(props: {
  owner: string;
  repo: string;
  rev: string;
  path: string;
  refs: RefsView;
}): ReactNode {
  const { owner, repo, rev, path, refs } = props;
  const name = `${owner}/${repo}`;
  const { queries } = useApp();
  const navigate = useNavigate();
  const tree = useQ(queries.tree(name, rev, path));
  const commit = useQ(queries.lastCommit(name, rev, path));
  const readme = path === "" ? findReadme(tree.data) : undefined;
  if (tree.status === "error") return <ErrorState error={tree.error} />;
  return (
    <div className="mx-auto w-full max-w-6xl p-4">
      <div className="mb-4 flex flex-wrap items-center gap-3">
        <BranchPicker
          refs={refs}
          current={rev}
          onPick={(r) => navigate(treeHref(owner, repo, r, path))}
        />
        {path ? <span className="font-mono text-sm text-fg-muted">/{path}</span> : null}
      </div>
      <div className="overflow-hidden rounded-lg border border-line bg-surface">
        <CommitBar commit={commit.data} loading={commit.status === "pending"} />
        {tree.data ? (
          <FileTable owner={owner} repo={repo} rev={rev} path={path} rows={tree.data.rows} />
        ) : (
          <RowsSkeleton rows={8} rowHeight={ROW_HEIGHT} label="Loading files" />
        )}
      </div>
      {readme ? <Readme owner={owner} repo={repo} rev={rev} row={readme} /> : null}
    </div>
  );
}

export default function RepoPage(): ReactNode {
  const { owner = "", repo = "", ref, path = "" } = useParams();
  const { queries } = useApp();
  useTitle(`${owner}/${repo}`);
  const refs = useQ(queries.refs(`${owner}/${repo}`));
  if (refs.status === "error") return <ErrorState error={refs.error} />;
  if (!refs.data) {
    return (
      <div className="mx-auto w-full max-w-6xl p-4">
        <div style={{ height: 36 }} className="mb-4">
          <Bar width="12rem" height={36} />
        </div>
        <div className="rounded-lg border border-line bg-surface">
          <div style={{ height: 44 }} />
          <RowsSkeleton rows={8} rowHeight={ROW_HEIGHT} label="Loading files" />
        </div>
      </div>
    );
  }
  return (
    <TreePane
      owner={owner}
      repo={repo}
      rev={ref ?? refs.data.defaultBranch}
      path={path}
      refs={refs.data}
    />
  );
}
