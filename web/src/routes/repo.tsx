// Repo home and directory view: branch picker, last commit, virtualised file table, README.
import { lazy, type ReactNode, Suspense, useState } from "react";
import type { CommitView, RefsView, TreeRow, TreeView } from "../api/types";
import { useApp, useQ } from "../api/queries";
import { ErrorState } from "../components/error-state";
import { Icon } from "../components/icons";
import { Bar, RowsSkeleton } from "../components/skeleton";
import { VirtualList } from "../components/virtual-list";
import { Link, useNavigate, useParams } from "../lib/router";
import { blobHref, treeHref } from "../lib/paths";
import { ago } from "../lib/time";
import { useTitle } from "../lib/title";

export { ago };

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
              headingOffset={2}
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

function CopyField(props: { id: string; label: string; value: string }): ReactNode {
  const [copied, setCopied] = useState(false);
  return (
    <div>
      <label htmlFor={props.id} className="mb-1 block text-xs font-medium text-fg-muted">
        {props.label}
      </label>
      <div className="flex gap-1">
        <input
          id={props.id}
          readOnly
          value={props.value}
          onFocus={(e) => e.currentTarget.select()}
          className="h-8 min-w-0 flex-1 rounded-md border border-control bg-page px-2 font-mono text-xs"
        />
        <button
          type="button"
          aria-label={`Copy ${props.label}`}
          onClick={() => {
            void navigator.clipboard
              ?.writeText(props.value)
              .then(() => {
                setCopied(true);
                setTimeout(() => setCopied(false), 1500);
              })
              .catch(() => undefined);
          }}
          className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-control hover:bg-subtle-hover"
        >
          <Icon name="copy" size={14} />
        </button>
      </div>
      <span role="status" className="sr-only">
        {copied ? "Copied" : ""}
      </span>
    </div>
  );
}

function CloneFields(props: { prefix: string; owner: string; repo: string }): ReactNode {
  const origin = typeof window === "undefined" ? "" : String(window.location.origin || "");
  const url = `${origin}/${props.owner}/${props.repo}`;
  return (
    <div className="space-y-2">
      <CopyField id={`${props.prefix}-url`} label="Clone URL" value={url} />
      <p className="text-xs text-fg-muted">
        Use it with <code className="font-mono">asc clone</code>.
      </p>
    </div>
  );
}

/** Below lg the clone commands are a disclosure in the toolbar, as GitHub's Code button is. */
function CloneMenu(props: { owner: string; repo: string }): ReactNode {
  return (
    <details className="relative ml-auto lg:hidden">
      <summary className="inline-flex h-9 cursor-pointer list-none items-center rounded-md bg-brand px-3 text-sm font-medium text-on-brand">
        Clone
      </summary>
      <div className="absolute right-0 z-20 mt-1 w-[min(22rem,calc(100vw-2rem))] rounded-lg border border-line bg-surface p-4 shadow-lg">
        <CloneFields prefix="menu" owner={props.owner} repo={props.repo} />
      </div>
    </details>
  );
}

/** About and clone, beside the root listing (GitHub puts the same two cards there). */
function RepoSidebar(props: { owner: string; repo: string; refs: RefsView }): ReactNode {
  const { owner, repo, refs } = props;
  const branches = refs.refs.filter((r) => r.kind === "branch").length;
  const tags = refs.refs.filter((r) => r.kind === "tag").length;
  return (
    <aside aria-label="Repository" className="hidden space-y-4 lg:block">
      <section aria-labelledby="about-h" className="rounded-lg border border-line bg-surface p-4">
        <h2 id="about-h" className="mb-3 text-base font-semibold text-fg-strong">
          About
        </h2>
        <ul className="space-y-2 text-sm text-fg-muted">
          <li className="flex items-center gap-2">
            <Icon name="branch" size={14} />
            Default branch <strong className="text-fg">{refs.defaultBranch}</strong>
          </li>
          <li className="flex items-center gap-2">
            <Icon name="branch" size={14} />
            <strong className="text-fg">{branches}</strong> {branches === 1 ? "branch" : "branches"}
          </li>
          <li className="flex items-center gap-2">
            <Icon name="tag" size={14} />
            <strong className="text-fg">{tags}</strong> {tags === 1 ? "tag" : "tags"}
          </li>
        </ul>
      </section>
      <section aria-labelledby="clone-h" className="rounded-lg border border-line bg-surface p-4">
        <h2 id="clone-h" className="mb-3 text-base font-semibold text-fg-strong">
          Clone
        </h2>
        <CloneFields prefix="side" owner={owner} repo={repo} />
      </section>
    </aside>
  );
}

/** Listing on the left, repository cards on the right from the lg breakpoint; one column below. */
function RepoLayout(props: { children: ReactNode; aside?: ReactNode }): ReactNode {
  return (
    <div className="mx-auto w-full max-w-6xl p-4">
      <div className={props.aside ? "grid gap-6 lg:grid-cols-[minmax(0,1fr)_18rem]" : ""}>
        <div className="min-w-0">{props.children}</div>
        {props.aside}
      </div>
    </div>
  );
}

function PathCrumbs(props: { owner: string; repo: string; rev: string; path: string }): ReactNode {
  const { owner, repo, rev } = props;
  const parts = props.path.split("/");
  return (
    <nav aria-label="Path" className="flex min-w-0 flex-wrap items-center gap-1 text-sm">
      <Link to={treeHref(owner, repo, rev, "")}>{repo}</Link>
      {parts.map((name, i) => (
        <span key={parts.slice(0, i + 1).join("/")} className="flex items-center gap-1">
          <span aria-hidden="true" className="text-fg-muted">
            /
          </span>
          {i === parts.length - 1 ? (
            <strong aria-current="page" className="text-fg-strong">
              {name}
            </strong>
          ) : (
            <Link to={treeHref(owner, repo, rev, parts.slice(0, i + 1).join("/"))}>{name}</Link>
          )}
        </span>
      ))}
    </nav>
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
    <RepoLayout
      aside={path === "" ? <RepoSidebar owner={owner} repo={repo} refs={refs} /> : undefined}
    >
      <h1 className="sr-only">{path ? `${name}: ${path}` : name}</h1>
      <div className="mb-4 flex flex-wrap items-center gap-3">
        <BranchPicker
          refs={refs}
          current={rev}
          onPick={(r) => navigate(treeHref(owner, repo, r, path))}
        />
        {path ? <PathCrumbs owner={owner} repo={repo} rev={rev} path={path} /> : null}
        {path === "" ? <CloneMenu owner={owner} repo={repo} /> : null}
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
    </RepoLayout>
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
      <RepoLayout aside={path === "" ? <div aria-hidden="true" /> : undefined}>
        <div style={{ height: 36 }} className="mb-4">
          <Bar width="12rem" height={36} />
        </div>
        <div className="rounded-lg border border-line bg-surface">
          <div style={{ height: 44 }} />
          <RowsSkeleton rows={8} rowHeight={ROW_HEIGHT} label="Loading files" />
        </div>
      </RepoLayout>
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
