// Code view: the file streams in and is shown through a virtual list, so a million-line file
// costs about 40 DOM rows. Line numbers are anchors (#L12); the anchored line is highlighted.
import type { ReactNode } from "react";
import type { FileView } from "../api/types";
import { useApp, useQ } from "../api/queries";
import { ErrorState } from "../components/error-state";
import { Bar } from "../components/skeleton";
import { VirtualList } from "../components/virtual-list";
import { Link, useHash, useParams } from "../lib/router";
import { treeHref } from "../lib/paths";
import { useTitle } from "../lib/title";

// 24 px: the line-number links are touch and pointer targets (WCAG 2.2 2.5.8 needs 24 px).
const LINE_HEIGHT = 24;
const MAX_ROWS = 28;

export function parseLineHash(hash: string): number | undefined {
  const m = /^#L(\d{1,9})$/.exec(hash);
  return m ? Number(m[1]) : undefined;
}

export function CodeLines(props: { file: FileView; selected: number | undefined }): ReactNode {
  const { file, selected } = props;
  const digits = String(Math.max(file.count, 1)).length;
  return (
    <VirtualList
      count={file.count}
      rowHeight={LINE_HEIGHT}
      maxRows={MAX_ROWS}
      // Space for a full window while the file streams in, so nothing moves; a short file then fits.
      minRows={file.done ? Math.min(file.count, MAX_ROWS) : MAX_ROWS}
      label="File contents"
      borderless
      className="bg-surface font-mono text-[13px]"
      revealIndex={selected === undefined ? undefined : selected - 1}
      renderRow={(i) => (
        <div
          className={`flex h-full whitespace-pre ${selected === i + 1 ? "bg-warn-bg" : ""}`}
          style={{ lineHeight: `${LINE_HEIGHT}px` }}
        >
          <a
            id={`L${i + 1}`}
            href={`#L${i + 1}`}
            aria-label={`Line ${i + 1}`}
            className="sticky left-0 select-none bg-inherit pr-3 text-right text-fg-muted no-underline"
            style={{ width: `${digits + 3}ch`, flex: "none" }}
          >
            {i + 1}
          </a>
          <span>{file.lines[i]}</span>
        </div>
      )}
    />
  );
}

function crumbs(path: string): { name: string; path: string }[] {
  const parts = path.split("/");
  return parts.map((name, i) => ({ name, path: parts.slice(0, i + 1).join("/") }));
}

export default function BlobPage(): ReactNode {
  const { owner = "", repo = "", ref = "", path = "" } = useParams();
  const { queries } = useApp();
  const hash = useHash();
  useTitle(`${path} · ${owner}/${repo}`);
  const file = useQ(queries.file(`${owner}/${repo}`, ref, path));
  if (file.status === "error") return <ErrorState error={file.error} />;
  const data = file.data;
  const parts = crumbs(path);
  return (
    <div className="mx-auto w-full max-w-6xl p-4">
      <h1 className="sr-only">{`${owner}/${repo}: ${path}`}</h1>
      <nav aria-label="Path" className="mb-3 flex flex-wrap items-center gap-1 text-sm">
        <Link to={treeHref(owner, repo, ref, "")}>{repo}</Link>
        {parts.map((c, i) => (
          <span key={c.path} className="flex items-center gap-1">
            <span aria-hidden="true">/</span>
            {i === parts.length - 1 ? (
              <strong aria-current="page" className="text-fg-strong">
                {c.name}
              </strong>
            ) : (
              <Link to={treeHref(owner, repo, ref, c.path)}>{c.name}</Link>
            )}
          </span>
        ))}
      </nav>
      <div className="overflow-hidden rounded-lg border border-line">
        <div
          className="flex items-center gap-4 border-b border-line bg-subtle px-4 text-sm text-fg-muted"
          style={{ height: 40 }}
        >
          {data ? (
            <>
              <span>
                {data.binary
                  ? "Binary file"
                  : `${data.count.toLocaleString("en")} ${data.count === 1 ? "line" : "lines"}`}
              </span>
              <span>{data.size.toLocaleString("en")} bytes</span>
              {data.truncated ? (
                <span role="status">Showing the first part of a large file</span>
              ) : null}
              {!data.done ? <span role="status">Loading…</span> : null}
            </>
          ) : (
            <Bar width="14rem" />
          )}
        </div>
        {data?.binary ? (
          <p className="p-6 text-fg-muted" style={{ minHeight: 120 }}>
            This file is binary and is not shown.
          </p>
        ) : data ? (
          <CodeLines file={data} selected={parseLineHash(hash)} />
        ) : (
          <div
            style={{ height: MAX_ROWS * LINE_HEIGHT }}
            role="status"
            aria-busy="true"
            aria-label="Loading file"
          />
        )}
      </div>
    </div>
  );
}
